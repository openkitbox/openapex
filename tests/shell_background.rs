//! Background shell completion: a shell that outlives its tool call (background
//! or past its yield window) reports exactly one terminal notification into
//! the session that started it, as a `source=shell` synthetic inbox item, and
//! wakes that session through `dispatch`.
//!
//! The first group runs the real `ShellTool`, database and event bus with a
//! recording waker, so the wake-up contract is observed without a chat
//! transport. The last test runs the real `Runner` and checks that the
//! notification text reaches the next model request.

#![cfg(unix)]

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ox::cardinal::CardinalError;
use ox::cardinal::chat::{BoxChatStream, ChatFuture};
use ox::cardinal::stream::StreamEvent;
use ox::cardinal::wire::{ChatMessage, ChatRequest};
use ox::config::Config;
use ox::db::Db;
use ox::event_bus::EventBus;
use ox::runner::{Chat, Tool, ToolContext, ToolRegistry};
use ox::schema::event::SessionEventData;
use ox::schema::support::LocationRef;
use ox::tool::rtk::RtkRewriter;
use ox::tool::shell::ShellTool;
use ox::tool::shell::completion::{ShellCompletion, ShellWaker};
use ox::tool::shell::sessions::ShellSessions;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Records every `dispatch`; `fail` makes each one return an error.
#[derive(Default)]
struct RecordingWaker {
    dispatched: Mutex<Vec<(String, String)>>,
    fail: bool,
}

impl ShellWaker for RecordingWaker {
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        self.dispatched
            .lock()
            .expect("dispatched")
            .push((session_id.to_owned(), inbox_id.to_owned()));
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                anyhow::bail!("runner refused the wake-up")
            }
            Ok(())
        })
    }
}

/// Process-wide log capture for the warn lines the notifier emits. Installed
/// once; every test in this binary writes through it.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Sink(Arc::clone(&writer)))
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
    .clone()
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    directory: std::path::PathBuf,
    bus: EventBus,
    sessions: Arc<ShellSessions>,
    waker: Arc<RecordingWaker>,
    tool: ShellTool,
}

impl Fixture {
    async fn new(fail_dispatch: bool) -> Self {
        // Installed before any notifier can warn, so no line is missed.
        let _ = captured_logs();
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("ox.db"),
            ..Config::default()
        };
        let db = Db::open(&config.db_path).await.unwrap();
        let bus = EventBus::new(db.clone());
        let completion = Arc::new(ShellCompletion::new(db.clone(), bus.clone()));
        let waker = Arc::new(RecordingWaker {
            dispatched: Mutex::new(Vec::new()),
            fail: fail_dispatch,
        });
        completion.attach(waker.clone());
        let sessions = Arc::new(ShellSessions::default());
        let tool = ShellTool::with_sessions(Arc::clone(&sessions))
            .with_rewriter(RtkRewriter::disabled())
            .with_completion(completion.sink());
        let directory = dir.path().to_path_buf();
        Self {
            _dir: dir,
            directory,
            bus,
            sessions,
            waker,
            tool,
        }
    }

    /// Create a parent session row a notification can be delivered to.
    async fn parent(&self, id: &str) {
        self.bus
            .publish(SessionEventData::SessionCreated {
                session_id: id.to_owned(),
                project_id: "proj_test".to_owned(),
                location: LocationRef {
                    directory: self.directory.to_string_lossy().to_string(),
                    workspace_id: None,
                },
                subpath: None,
                parent_id: None,
                slug: ox::slug::create(),
                title: Some("parent".to_owned()),
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            })
            .await
            .unwrap();
    }

    fn ctx(&self, session_id: &str) -> ToolContext {
        ToolContext {
            session_id: session_id.to_owned(),
            directory: self.directory.clone(),
            call_id: "call_bg".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        }
    }

    async fn run(&self, session_id: &str, input: Value) -> ox::runner::ToolOutput {
        self.tool.execute(self.ctx(session_id), input).await
    }

    /// Every `session.inbox.enqueued` event data of a session, in seq order.
    async fn inbox(&self, session_id: &str) -> Vec<Value> {
        // `event_log` treats `after` as exclusive; -1 starts before seq 0.
        self.bus
            .event_log(session_id, -1, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|envelope| envelope.kind == "session.inbox.enqueued")
            .map(|envelope| envelope.data)
            .collect()
    }

    /// Bounded wait for a condition over the session's inbox; never a fixed sleep.
    async fn wait_inbox<F>(&self, session_id: &str, mut done: F) -> Vec<Value>
    where
        F: FnMut(&[Value]) -> bool,
    {
        for _ in 0..200 {
            let items = self.inbox(session_id).await;
            if done(&items) {
                return items;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.inbox(session_id).await
    }

    fn dispatched(&self) -> Vec<(String, String)> {
        self.waker.dispatched.lock().expect("dispatched").clone()
    }
}

/// The synthetic payload of an inbox event, if it is one.
fn synthetic(item: &Value) -> Option<&Value> {
    (item["item"]["type"] == "synthetic").then(|| &item["item"]["payload"])
}

/// The `source=shell` payloads among the inbox items, in order.
fn shell_payloads(items: &[Value]) -> Vec<&Value> {
    items
        .iter()
        .filter_map(synthetic)
        .filter(|payload| payload["metadata"]["source"] == "shell")
        .collect()
}

fn shell_id(output: &ox::runner::ToolOutput) -> String {
    output.metadata.as_ref().expect("metadata")["shellID"]
        .as_str()
        .expect("shellID")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Tool response and single notification
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_returns_running_and_notifies_once() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": "/bin/sleep 0.3; echo finished", "background": true }),
        )
        .await;
    assert_eq!(output.error, None, "{output:?}");
    assert_eq!(output.metadata.as_ref().unwrap()["status"], "running");
    let id = shell_id(&output);
    let text = output.content.as_str().expect("text");
    assert!(text.contains("notified automatically"), "{text}");
    assert!(text.contains("DO NOT poll"), "{text}");
    assert!(!text.contains("action=\"interact\""), "{text}");

    let items = fixture
        .wait_inbox("ses_parent", |items| !shell_payloads(items).is_empty())
        .await;
    let payloads = shell_payloads(&items);
    assert_eq!(payloads.len(), 1, "{items:?}");
    let payload = payloads[0];
    assert_eq!(payload["metadata"]["shellID"], id.as_str());
    assert_eq!(payload["metadata"]["state"], "completed");
    assert_eq!(payload["metadata"]["exit"], 0);
    let body = payload["text"].as_str().expect("text");
    assert!(
        body.starts_with(&format!(
            "<shell id=\"{id}\" state=\"completed\" command=\"/bin/sleep 0.3; echo finished\">"
        )),
        "{body}"
    );
    assert!(body.contains("finished"), "{body}");
    assert!(body.ends_with("</shell>"), "{body}");

    // Negative check: no second notification for the same shell. A bounded
    // observation window is the only way to assert absence.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(shell_payloads(&fixture.inbox("ses_parent").await).len(), 1);

    let dispatched = fixture.dispatched();
    assert_eq!(dispatched.len(), 1, "{dispatched:?}");
    assert_eq!(dispatched[0].0, "ses_parent");
    assert_eq!(dispatched[0].1, items[0]["inboxID"].as_str().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_settled_inside_the_yield_window_is_not_notified() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    let inline = fixture
        .run("ses_parent", json!({ "command": "echo inline" }))
        .await;
    assert_eq!(inline.error, None, "{inline:?}");
    assert_eq!(inline.metadata.as_ref().unwrap()["status"], "completed");

    // A later background completion is the barrier: the inline shell settled
    // before it was started, so its notifier would have run first.
    let later = fixture
        .run(
            "ses_parent",
            json!({ "command": "/bin/sleep 0.2; echo later", "background": true }),
        )
        .await;
    let later_id = shell_id(&later);
    let items = fixture
        .wait_inbox("ses_parent", |items| {
            shell_payloads(items)
                .iter()
                .any(|payload| payload["metadata"]["shellID"] == later_id.as_str())
        })
        .await;
    let ids: Vec<&str> = shell_payloads(&items)
        .iter()
        .filter_map(|payload| payload["metadata"]["shellID"].as_str())
        .collect();
    assert_eq!(ids, vec![later_id.as_str()]);
    assert_eq!(fixture.dispatched().len(), 1);
}

/// A command that blocks until `gate` exists, so the test decides when it settles.
fn gated(gate: &std::path::Path, tail: &str) -> String {
    format!(
        "while [ ! -f {} ]; do /bin/sleep 0.02; done; {tail}",
        gate.display()
    )
}

/// Opens its gate file on drop, so a failing assertion cannot leave a polling
/// shell behind to hang the runtime shutdown.
struct OpenOnDrop(std::path::PathBuf);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"");
    }
}

/// Shell ids of the `source=shell` payloads in the inbox, in order.
fn notified_ids(items: &[Value]) -> Vec<String> {
    shell_payloads(items)
        .iter()
        .filter_map(|payload| payload["metadata"]["shellID"].as_str())
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreground_command_past_its_window_is_backgrounded_and_notified_once() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;
    let gate = fixture.directory.join("overflow-gate");

    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": gated(&gate, "echo late"), "yield_time_ms": 50 }),
        )
        .await;
    assert_eq!(output.error, None, "{output:?}");
    assert_eq!(output.metadata.as_ref().unwrap()["status"], "running");
    let id = shell_id(&output);
    let text = output.content.as_str().expect("text");
    assert!(text.contains("notified automatically"), "{text}");
    assert!(text.contains("DO NOT poll"), "{text}");
    assert!(!text.contains("action=\"interact\""), "{text}");
    // Still running: nothing delivered before the command settles.
    assert!(shell_payloads(&fixture.inbox("ses_parent").await).is_empty());

    std::fs::write(&gate, b"").unwrap();
    let items = fixture
        .wait_inbox("ses_parent", |items| !shell_payloads(items).is_empty())
        .await;
    let payloads = shell_payloads(&items);
    assert_eq!(payloads.len(), 1, "{items:?}");
    assert_eq!(payloads[0]["metadata"]["shellID"], id.as_str());
    assert_eq!(payloads[0]["metadata"]["state"], "completed");
    assert!(payloads[0]["text"].as_str().unwrap().contains("late"));

    // A later background completion is the barrier: any duplicate for the
    // overflowed shell would have been enqueued before it.
    let later = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo barrier", "background": true }),
        )
        .await;
    let later_id = shell_id(&later);
    let items = fixture
        .wait_inbox("ses_parent", |items| {
            notified_ids(items).contains(&later_id)
        })
        .await;
    assert_eq!(notified_ids(&items), vec![id, later_id], "{items:?}");
    assert_eq!(fixture.dispatched().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreground_command_that_settles_in_the_window_returns_inline_without_notice() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    let inline = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo quick", "yield_time_ms": 20_000 }),
        )
        .await;
    assert_eq!(inline.error, None, "{inline:?}");
    assert_eq!(inline.metadata.as_ref().unwrap()["status"], "completed");
    assert_eq!(inline.content, json!("quick\n"));

    let later = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo barrier", "background": true }),
        )
        .await;
    let later_id = shell_id(&later);
    let items = fixture
        .wait_inbox("ses_parent", |items| {
            notified_ids(items).contains(&later_id)
        })
        .await;
    assert_eq!(notified_ids(&items), vec![later_id], "{items:?}");
}

/// Release a gated command and wait for its single notification, so a test
/// never leaves a polling shell behind.
async fn release_and_drain(fixture: &Fixture, gate: &std::path::Path, id: &str) {
    std::fs::write(gate, b"").unwrap();
    let items = fixture
        .wait_inbox("ses_parent", |items| {
            notified_ids(items).iter().any(|seen| seen == id)
        })
        .await;
    assert_eq!(
        notified_ids(&items)
            .iter()
            .filter(|seen| *seen == id)
            .count(),
        1,
        "{items:?}"
    );
}

/// `interact` on a session whose completion is delivered automatically must
/// not invite polling, but stays useful: new output since the cursor is still
/// returned, with the no-poll guidance, `status=running` and the shell id.
async fn assert_interact_forbids_polling(fixture: &Fixture, id: &str) {
    let mut observed = String::new();
    let mut last = None;
    for _ in 0..50 {
        let reply = fixture
            .run(
                "ses_parent",
                json!({ "action": "interact", "session_id": id, "yield_time_ms": 200 }),
            )
            .await;
        assert_eq!(reply.error, None, "{reply:?}");
        let metadata = reply.metadata.clone().expect("metadata");
        assert_eq!(metadata["status"], "running", "{reply:?}");
        assert_eq!(metadata["shellID"], id);
        let text = reply.content.as_str().expect("text").to_owned();
        assert!(!text.contains("Interact again"), "{text}");
        assert!(text.contains("notified automatically"), "{text}");
        assert!(text.contains("DO NOT poll"), "{text}");
        assert!(text.contains(id), "{text}");
        assert!(text.contains("status: running"), "{text}");
        observed.push_str(&text);
        last = Some(metadata["cursor"].clone());
        if observed.contains("first") {
            break;
        }
    }
    assert!(observed.contains("first"), "new output missing: {observed}");

    // Past the cursor there is nothing new, and the guidance is unchanged.
    let cursor = last.unwrap();
    let quiet = fixture
        .run(
            "ses_parent",
            json!({ "action": "interact", "session_id": id, "cursor": cursor, "yield_time_ms": 50 }),
        )
        .await;
    let text = quiet.content.as_str().expect("text");
    assert!(text.contains("(no new output)"), "{text}");
    assert!(text.contains("DO NOT poll"), "{text}");
    assert!(!text.contains("Interact again"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_on_a_background_session_forbids_polling_but_returns_output() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;
    let gate = fixture.directory.join("interact-bg-gate");
    let _open = OpenOnDrop(gate.clone());
    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": format!("echo first; {}", gated(&gate, "echo done")), "background": true }),
        )
        .await;
    let id = shell_id(&output);
    assert_interact_forbids_polling(&fixture, &id).await;
    release_and_drain(&fixture, &gate, &id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_on_a_handed_back_session_forbids_polling_but_returns_output() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;
    let gate = fixture.directory.join("interact-fg-gate");
    let _open = OpenOnDrop(gate.clone());
    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": format!("echo first; {}", gated(&gate, "echo done")), "yield_time_ms": 50 }),
        )
        .await;
    assert_eq!(output.metadata.as_ref().unwrap()["status"], "running");
    let id = shell_id(&output);
    assert_interact_forbids_polling(&fixture, &id).await;
    release_and_drain(&fixture, &gate, &id).await;

    // Settled: the reply is the ordinary completed one, no running guidance.
    let settled = fixture
        .run(
            "ses_parent",
            json!({ "action": "interact", "session_id": id, "yield_time_ms": 50 }),
        )
        .await;
    assert_eq!(settled.metadata.as_ref().unwrap()["status"], "completed");
    assert!(!settled.content.as_str().unwrap().contains("DO NOT poll"));
}

/// Upstream clears the timeout when a foreground run is backgrounded
/// (`shell.timeout(id, 0)`): the run lasts until it exits or is stopped. The
/// timeout here is far shorter than the observation window, so a surviving
/// deadline would have settled the run as a timeout long before the gate opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handed_back_foreground_run_loses_its_timeout() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;
    let gate = fixture.directory.join("deadline-gate");
    let _open = OpenOnDrop(gate.clone());
    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": gated(&gate, "echo survived"), "yield_time_ms": 50, "timeout": 300 }),
        )
        .await;
    assert_eq!(output.metadata.as_ref().unwrap()["status"], "running");
    let id = shell_id(&output);
    let session = fixture.sessions.get(&id).expect("session");

    // Bounded observation: well past the 300 ms timeout, still alive.
    let until = tokio::time::Instant::now() + Duration::from_millis(900);
    while tokio::time::Instant::now() < until {
        assert!(
            !session.settled(),
            "the foreground timeout survived hand-back"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    release_and_drain(&fixture, &gate, &id).await;
    let items = fixture.inbox("ses_parent").await;
    let payloads = shell_payloads(&items);
    assert_eq!(payloads[0]["metadata"]["state"], "completed");
    assert!(
        payloads[0]["metadata"].get("timeout").is_none(),
        "{items:?}"
    );
    assert!(payloads[0]["text"].as_str().unwrap().contains("survived"));
}

/// STRESS ONLY: repeats the hand-off against a settle that lands right at the
/// window's edge. The interleaving is not controlled, so this proves only that
/// the response and the inbox agree whichever side wins (a `running` response
/// is followed by exactly one notification, a `completed` one by none). Each
/// branch is pinned deterministically by
/// `a_foreground_command_past_its_window_is_backgrounded_and_notified_once`
/// and `a_foreground_command_that_settles_in_the_window_returns_inline_without_notice`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stress_hand_off_racing_settle_agrees_between_response_and_inbox() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    let mut expected: Vec<String> = Vec::new();
    for _ in 0..8 {
        let output = fixture
            .run(
                "ses_parent",
                json!({ "command": "echo edge", "yield_time_ms": 1 }),
            )
            .await;
        assert_eq!(output.error, None, "{output:?}");
        match output.metadata.as_ref().unwrap()["status"].as_str() {
            Some("running") => expected.push(shell_id(&output)),
            Some("completed") => {}
            other => panic!("unexpected status {other:?}: {output:?}"),
        }
    }
    let later = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo barrier", "background": true }),
        )
        .await;
    let later_id = shell_id(&later);
    expected.push(later_id.clone());
    let items = fixture
        .wait_inbox("ses_parent", |items| {
            notified_ids(items).contains(&later_id) && notified_ids(items).len() >= expected.len()
        })
        .await;
    let mut got = notified_ids(&items);
    got.sort();
    expected.sort();
    assert_eq!(got, expected, "{items:?}");
}

// ---------------------------------------------------------------------------
// States
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nonzero_exit_is_completed_with_its_notice() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo boom; exit 3", "background": true }),
        )
        .await;
    let id = shell_id(&output);
    let items = fixture
        .wait_inbox("ses_parent", |items| !shell_payloads(items).is_empty())
        .await;
    let payloads = shell_payloads(&items);
    assert_eq!(payloads.len(), 1, "{items:?}");
    assert_eq!(payloads[0]["metadata"]["shellID"], id.as_str());
    // Upstream `notifyWhenDone`: the state is the job's, so a command that ran
    // and exited nonzero is `completed`; the exit code rides the notice.
    assert_eq!(payloads[0]["metadata"]["state"], "completed");
    assert_eq!(payloads[0]["metadata"]["exit"], 3);
    let body = payloads[0]["text"].as_str().unwrap();
    assert!(body.contains("boom"), "{body}");
    assert!(body.contains("Exited with code 3"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_background_command_is_reported_as_cancelled() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": "/bin/sleep 30", "background": true }),
        )
        .await;
    let id = shell_id(&output);

    let stopped = fixture
        .run(
            "ses_parent",
            json!({ "action": "stop", "session_id": id, "force": true }),
        )
        .await;
    assert_eq!(stopped.error, None, "{stopped:?}");

    let items = fixture
        .wait_inbox("ses_parent", |items| !shell_payloads(items).is_empty())
        .await;
    let payloads = shell_payloads(&items);
    assert_eq!(payloads.len(), 1, "{items:?}");
    assert_eq!(payloads[0]["metadata"]["state"], "cancelled");
    assert_eq!(payloads[0]["metadata"]["shellID"], id.as_str());
    assert!(
        payloads[0]["text"].as_str().unwrap().contains("Cancelled"),
        "{items:?}"
    );
}

// ---------------------------------------------------------------------------
// Failure paths: fail closed, never a false completion
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_parent_drops_the_notification_silently() {
    let fixture = Fixture::new(false).await;
    fixture.parent("ses_parent").await;

    // `ses_gone` is never created, so its completion has no parent to wake.
    let orphan = fixture
        .run(
            "ses_gone",
            json!({ "command": "echo orphan", "background": true }),
        )
        .await;
    let orphan_id = shell_id(&orphan);
    let session = fixture.sessions.get(&orphan_id).expect("orphan session");
    for _ in 0..200 {
        if session.settled() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(session.settled());

    // A live parent's completion proves the orphan's delivery attempt, which
    // started earlier, has been processed.
    let live = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo live", "background": true }),
        )
        .await;
    let live_id = shell_id(&live);
    fixture
        .wait_inbox("ses_parent", |items| {
            shell_payloads(items)
                .iter()
                .any(|payload| payload["metadata"]["shellID"] == live_id.as_str())
        })
        .await;

    assert!(shell_payloads(&fixture.inbox("ses_gone").await).is_empty());
    let dispatched = fixture.dispatched();
    assert_eq!(dispatched.len(), 1, "{dispatched:?}");
    assert_eq!(dispatched[0].0, "ses_parent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_wake_up_is_logged_and_not_reported_as_delivered() {
    let fixture = Fixture::new(true).await;
    fixture.parent("ses_parent").await;

    let output = fixture
        .run(
            "ses_parent",
            json!({ "command": "echo once", "background": true }),
        )
        .await;
    assert_eq!(output.error, None, "{output:?}");
    let id = shell_id(&output);

    // The item is durable and the wake was attempted once; the failed wake is
    // logged, and nothing claims the model was woken.
    let items = fixture
        .wait_inbox("ses_parent", |items| !shell_payloads(items).is_empty())
        .await;
    assert_eq!(shell_payloads(&items).len(), 1, "{items:?}");
    assert_eq!(
        shell_payloads(&items)[0]["metadata"]["shellID"],
        id.as_str()
    );

    let mut logs = String::new();
    for _ in 0..200 {
        logs = String::from_utf8_lossy(&captured_logs().lock().expect("logs")).into_owned();
        if logs.contains("unable to wake parent for shell completion") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        logs.contains("unable to wake parent for shell completion"),
        "{logs}"
    );
    assert_eq!(fixture.dispatched().len(), 1);
}

// ---------------------------------------------------------------------------
// End to end through the real runner: the notice reaches the next model step
// ---------------------------------------------------------------------------

/// A stream over a fixed list of events, written out because futures-util is
/// not a dependency of this test.
mod stream {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Iter<T>(pub VecDeque<T>);

    impl<T: Unpin> futures_core::Stream for Iter<T> {
        type Item = T;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<T>> {
            Poll::Ready(self.get_mut().0.pop_front())
        }
    }
}

/// Answers every turn with `(ack)` and records the messages each request carried.
#[derive(Default)]
struct RecordingChat {
    requests: Mutex<Vec<String>>,
}

impl Chat for RecordingChat {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        let rendered: Vec<String> = request
            .messages
            .iter()
            .map(|message| match message {
                ChatMessage::User { content } => format!("{content:?}"),
                other => format!("{other:?}"),
            })
            .collect();
        self.requests
            .lock()
            .expect("requests")
            .push(rendered.join("\n"));
        Box::pin(async move {
            let events: std::collections::VecDeque<Result<StreamEvent, CardinalError>> = [
                StreamEvent::TextDelta("(ack)".to_owned()),
                StreamEvent::Finish {
                    reason: "stop".to_owned(),
                },
            ]
            .into_iter()
            .map(Ok)
            .collect();
            Ok(Box::pin(stream::Iter(events)) as BoxChatStream)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_notice_reaches_the_next_model_step() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        db_path: dir.path().join("ox.db"),
        listen: "127.0.0.1:0".to_owned(),
        ..Config::default()
    };
    let db = Db::open(&config.db_path).await.unwrap();
    let bus = EventBus::new(db.clone());
    let mut tools = ToolRegistry::with_builtins();
    tools.register_shell_completion(db.clone(), bus.clone());
    let shell = tools.get("shell").expect("shell registered");
    let chat = Arc::new(RecordingChat::default());

    // Building the state attaches the runner to the registry's completion
    // handle; the registry must be the one the tool was taken from.
    let _state = ox::build_state_on_with_bus(
        config,
        chat.clone(),
        tools,
        db,
        bus.clone(),
        dir.path().to_path_buf(),
    )
    .await
    .expect("build state");

    let parent = "ses_e2e";
    bus.publish(SessionEventData::SessionCreated {
        session_id: parent.to_owned(),
        project_id: "proj_test".to_owned(),
        location: LocationRef {
            directory: dir.path().to_string_lossy().to_string(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: ox::slug::create(),
        title: Some("parent".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .await
    .unwrap();

    let ctx = ToolContext {
        session_id: parent.to_owned(),
        directory: dir.path().to_path_buf(),
        call_id: "call_e2e".to_owned(),
        cancellation: CancellationToken::new(),
        registry: Arc::new(ToolRegistry::new()),
    };
    let output = shell
        .execute(
            ctx,
            json!({ "command": "echo e2e-marker", "background": true }),
        )
        .await;
    assert_eq!(output.error, None, "{output:?}");
    let id = shell_id(&output);

    let mut seen = false;
    for _ in 0..400 {
        seen = chat
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| request.contains("e2e-marker") && request.contains(&id));
        if seen {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(seen, "completion notice never reached a model request");
}

// ---------------------------------------------------------------------------
// Pure payload mapping and timeout defaults
// ---------------------------------------------------------------------------

mod payload {
    use ox::tool::shell::completion::payload;
    use ox::tool::shell::sessions::{Completion, Status};

    fn completion(status: Status, exit: Option<i32>, output: &str) -> Completion {
        Completion {
            id: "sh_1".to_owned(),
            command: "cmd".to_owned(),
            status,
            exit,
            signal: None,
            output: output.to_owned(),
            truncated: false,
        }
    }

    #[test]
    fn an_engine_failure_without_an_exit_code_is_error() {
        let payload = payload(&completion(Status::Exited, None, "spawn failed\n"));
        let metadata = payload.metadata.as_ref().unwrap();
        assert_eq!(metadata["state"], "error");
        assert_eq!(metadata["source"], "shell");
        assert_eq!(metadata["shellID"], "sh_1");
        assert!(metadata.get("exit").is_none());
        assert_eq!(
            payload.text,
            "<shell id=\"sh_1\" state=\"error\" command=\"cmd\">\nspawn failed\n\nCommand failed\n</shell>"
        );
    }

    #[test]
    fn a_timeout_is_completed_with_the_timeout_flag_and_notice() {
        let payload = payload(&completion(Status::Timeout, None, "partial\n"));
        let metadata = payload.metadata.as_ref().unwrap();
        assert_eq!(metadata["state"], "completed");
        assert_eq!(metadata["timeout"], true);
        assert_eq!(
            payload.text,
            "<shell id=\"sh_1\" state=\"completed\" command=\"cmd\">\npartial\n\nTimed out before completion\n</shell>"
        );
    }

    #[test]
    fn a_truncated_capture_carries_the_footer_and_flag() {
        let mut truncated = completion(Status::Exited, Some(0), "tail\n");
        truncated.truncated = true;
        let payload = payload(&truncated);
        let metadata = payload.metadata.as_ref().unwrap();
        assert_eq!(metadata["truncated"], true);
        assert_eq!(metadata["exit"], 0);
        assert!(
            payload.text.contains(
                "[output truncated; full output is paged through GET /api/shell/sh_1/output]"
            ),
            "{}",
            payload.text
        );
        assert!(!payload.text.contains("Exited with code"));
    }

    #[test]
    fn line_truncation_sets_the_flag_without_the_producer_flag() {
        let many: String = (0..2_500).map(|n| format!("{n}\n")).collect();
        let payload = payload(&completion(Status::Exited, Some(0), &many));
        assert_eq!(payload.metadata.as_ref().unwrap()["truncated"], true);
        assert!(payload.text.contains("[output truncated;"));
        assert!(!payload.text.contains("\n0\n"), "head lines dropped");
    }

    #[test]
    fn an_untruncated_empty_capture_says_no_output() {
        let payload = payload(&completion(Status::Exited, Some(0), ""));
        assert_eq!(payload.metadata.as_ref().unwrap()["truncated"], false);
        assert!(payload.text.contains("(no output)"));
        assert!(!payload.text.contains("[output truncated;"));
    }
}

#[test]
fn background_runs_default_to_no_timeout_and_foreground_keeps_the_default() {
    use ox::tool::shell::{DEFAULT_TIMEOUT_MS, default_timeout_ms};
    assert_eq!(default_timeout_ms(true), 0, "upstream: timeout ?? 0");
    assert_eq!(default_timeout_ms(false), DEFAULT_TIMEOUT_MS);
    assert_eq!(DEFAULT_TIMEOUT_MS, 120_000);
}
