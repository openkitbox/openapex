//! Integration tests for the task 6a executor: the actor lifecycle, drain
//! loop, inbox promotion and the prompt/inbox/interrupt endpoints, driven
//! over the real router with a scripted chat transport.
//!
//! Same harness pattern as `tests/session.rs`: the server runs on an
//! ephemeral port; `ox::build_state_with` injects a [`MockChat`] whose
//! scripts hand out canned `StreamEvent` sequences (or hang) per
//! `stream_chat` call. Every `stream_chat` request is recorded so promotion
//! order and context assembly are asserted against what the model would
//! actually have seen.

use std::collections::VecDeque;
use std::future::Future;
use std::net::TcpListener as StdTcpListener;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use ox::cardinal::CardinalError;
use ox::cardinal::chat::{BoxChatStream, ChatFuture};
use ox::cardinal::stream::StreamEvent;
use ox::cardinal::wire::{ChatRequest, Usage};
use ox::config::Config;
use ox::runner::{Chat, ToolRegistry};
use salvo::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Scripted chat transport
// ---------------------------------------------------------------------------

/// One scripted `stream_chat` call.
enum Script {
    /// A canned event sequence; `[DONE]` is appended automatically.
    Events(Vec<StreamEvent>),
    /// Emit `delta` as a text block, then hold the stream open until the
    /// gate fires (or the abort path drops it — then the hold never ends).
    TextThenHang(&'static str, Arc<Notify>),
    /// Immediate connect failure (transport error before any output).
    Fail,
}

/// A [`Chat`] implementation handing out scripts in order and recording
/// every request.
struct MockChat {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockChat {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Text turn: `delta` streamed, clean stop, no usage.
    fn text(delta: &'static str) -> Script {
        Script::Events(vec![
            StreamEvent::TextDelta(delta.to_owned()),
            StreamEvent::Finish {
                reason: "stop".to_owned(),
            },
        ])
    }

    /// Text turn carrying usage (prompt 100, cached 40, completion 10).
    fn text_with_usage(delta: &'static str) -> Script {
        Script::Events(vec![
            StreamEvent::TextDelta(delta.to_owned()),
            StreamEvent::Finish {
                reason: "stop".to_owned(),
            },
            StreamEvent::Usage(Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
                cached_tokens: Some(40),
                reasoning_tokens: None,
            }),
        ])
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Chat for MockChat {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let script = self
            .scripts
            .lock()
            .expect("scripts lock")
            .pop_front()
            .unwrap_or(Script::Fail);
        Box::pin(async move {
            match script {
                Script::Fail => Err(CardinalError::EarlyClose),
                Script::Events(mut events) => {
                    events.push(StreamEvent::Done);
                    Ok(Box::pin(EventsStream {
                        events: events.into_iter().collect(),
                        hang: None,
                    }) as BoxChatStream)
                }
                Script::TextThenHang(delta, gate) => {
                    let (tx, rx) = oneshot::channel();
                    tokio::spawn(async move {
                        gate.notified().await;
                        let _ = tx.send(());
                    });
                    Ok(Box::pin(EventsStream {
                        events: VecDeque::from(vec![StreamEvent::TextDelta(delta.to_owned())]),
                        hang: Some(rx),
                    }) as BoxChatStream)
                }
            }
        })
    }
}

/// A stream yielding scripted events, then (optionally) holding open until
/// its gate fires — the interrupt target.
struct EventsStream {
    events: VecDeque<StreamEvent>,
    hang: Option<oneshot::Receiver<()>>,
}

impl Stream for EventsStream {
    type Item = Result<StreamEvent, CardinalError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(event) = this.events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
        match this.hang.as_mut() {
            Some(done) => match Pin::new(done).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => Poll::Ready(None),
            },
            None => Poll::Ready(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

struct Harness {
    base: String,
    client: reqwest::Client,
    db: ox::db::Db,
    bus: ox::event_bus::EventBus,
    chat: Arc<MockChat>,
    events: Arc<AsyncMutex<Vec<String>>>,
    handle: JoinHandle<()>,
    collector: JoinHandle<()>,
    _dir: TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.handle.abort();
        self.collector.abort();
    }
}

impl Harness {
    /// Spawn the server with the given chat scripts.
    async fn spawn(scripts: Vec<Script>) -> Self {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            db_path: dir.path().join("ox.db"),
            listen: format!("127.0.0.1:{port}"),
            ..Config::default()
        };
        let chat = Arc::new(MockChat::new(scripts));
        let state =
            ox::build_state_with(config.clone(), chat.clone(), Arc::new(ToolRegistry::new()))
                .await
                .expect("build state");
        let db = state.db.clone();
        let bus = state.bus.clone();
        let mut events = bus.subscribe();
        let collected = Arc::new(AsyncMutex::new(Vec::<String>::new()));
        let sink = collected.clone();
        let collector = tokio::spawn(async move {
            loop {
                let Ok(envelope) = events.recv().await else {
                    break;
                };
                sink.lock().await.push(envelope.kind);
            }
        });
        let router = ox::build_router(state);
        let listen: String = config.listen.clone();
        let handle = tokio::spawn(async move {
            let acceptor = TcpListener::new(listen).bind().await;
            Server::new(acceptor).serve(router).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..50 {
            if client.get(format!("{base}/api/info")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            base,
            client,
            db,
            bus,
            chat,
            events: collected,
            handle,
            collector,
            _dir: dir,
        }
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .expect("request")
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request")
    }

    /// Create a session, returning its id.
    async fn create_session(&self) -> String {
        let response = self
            .post("/api/session", json!({ "title": "runner test" }))
            .await;
        assert_eq!(response.status(), 200, "session create");
        let body: Value = response.json().await.expect("json");
        body["data"]["id"].as_str().expect("session id").to_owned()
    }

    /// `GET /api/session/{id}/message` list body (oldest first).
    async fn messages(&self, session_id: &str) -> Vec<Value> {
        let response = self
            .get(&format!("/api/session/{session_id}/message?order=asc"))
            .await;
        assert_eq!(response.status(), 200, "message list");
        let body: Value = response.json().await.expect("json");
        body["data"].as_array().expect("data array").clone()
    }

    /// Wait until `predicate` holds for the session's message list.
    async fn wait_for(
        &self,
        session_id: &str,
        what: &str,
        mut predicate: impl FnMut(&[Value]) -> bool,
    ) {
        for _ in 0..100 {
            let messages = self.messages(session_id).await;
            if predicate(&messages) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Recorded durable + ephemeral event kinds so far.
    async fn event_kinds(&self) -> Vec<String> {
        self.events.lock().await.clone()
    }

    /// Wait until the event log contains `kind`.
    async fn wait_event(&self, kind: &str) {
        for _ in 0..200 {
            if self.event_kinds().await.iter().any(|k| k == kind) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "timed out waiting for event `{kind}`; saw {:?}",
            self.event_kinds().await
        );
    }
}

// ---------------------------------------------------------------------------
// Text-only e2e: prompt → actor → user + assistant rows, tokens accumulated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_runs_text_turn_and_accumulates_tokens() {
    let harness = Harness::spawn(vec![MockChat::text_with_usage("Hello from the mock.")]).await;
    let session_id = harness.create_session().await;

    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "Say hello", "delivery": "queue" }),
        )
        .await;
    assert_eq!(response.status(), 200, "prompt accepted");
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"]["type"], "user");
    assert_eq!(body["data"]["payload"]["text"], "Say hello");
    assert_eq!(body["data"]["delivery"], "queue");
    assert!(body["data"]["id"].as_str().is_some());

    // The turn completes: assistant row with the streamed text, finish and
    // upstream-shaped tokens; usage accumulated on session_v2.
    harness
        .wait_for(&session_id, "assistant completion", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["time"]["completed"].is_number())
        })
        .await;

    let messages = harness.messages(&session_id).await;
    let user = messages
        .iter()
        .find(|m| m["type"] == "user")
        .expect("user row");
    assert_eq!(user["text"], "Say hello");
    let assistant = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row");
    assert_eq!(assistant["content"][0]["type"], "text");
    assert_eq!(assistant["content"][0]["text"], "Hello from the mock.");
    assert_eq!(assistant["finish"], "stop");
    assert_eq!(assistant["tokens"]["input"], 60);
    assert_eq!(assistant["tokens"]["output"], 10);
    assert_eq!(assistant["tokens"]["reasoning"], 0);
    assert_eq!(assistant["tokens"]["cache"]["read"], 40);
    assert_eq!(assistant["tokens"]["cache"]["write"], 0);

    // session_v2 accumulated the usage (projection of step.ended).
    let sid = session_id.clone();
    let tokens: (i64, i64) = harness
        .db
        .conn()
        .call(move |c| {
            c.query_row(
                "SELECT tokens_input, tokens_output FROM session_v2 WHERE id = ?1",
                [sid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
        .expect("query");
    assert_eq!(tokens, (60, 10));

    // Lifecycle: exactly one terminal opened and closed the busy period.
    harness.wait_event("session.execution.succeeded").await;
    let kinds = harness.event_kinds().await;
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session.execution.started")
            .count(),
        1,
        "one execution.started per busy period"
    );
    assert!(kinds.contains(&"session.step.started".to_owned()));
    assert!(kinds.contains(&"session.step.ended".to_owned()));
    assert!(kinds.contains(&"session.text.started".to_owned()));
    assert!(kinds.contains(&"session.text.delta".to_owned()));
    assert!(kinds.contains(&"session.text.ended".to_owned()));
    assert!(kinds.contains(&"session.inbox.delivered".to_owned()));
    assert!(!kinds.contains(&"session.execution.failed".to_owned()));

    // The delivered inbox row is gone.
    let sid = session_id.clone();
    let pending: i64 = harness
        .db
        .conn()
        .call(move |c| {
            c.query_row(
                "SELECT count(*) FROM session_inbox WHERE session_id = ?1",
                [sid],
                |row| row.get(0),
            )
        })
        .await
        .expect("query");
    assert_eq!(pending, 0, "inbox drained");

    // The model saw the system prompt, then the user turn.
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 1, "one chat request");
    let messages = requests[0].messages.len();
    assert_eq!(messages, 2, "system + user");
}

// ---------------------------------------------------------------------------
// Promotion order: steers (arrival order) before the queued item
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steers_promote_in_order_before_queue() {
    let harness = Harness::spawn(vec![MockChat::text("ack")]).await;
    let session_id = harness.create_session().await;
    let inbox = format!("/api/session/{session_id}/prompt");

    // Enqueue three items without waking the actor…
    for (text, delivery) in [
        ("first steer", "steer"),
        ("second steer", "steer"),
        ("the queue", "queue"),
    ] {
        let response = harness
            .post(
                &inbox,
                json!({ "text": text, "delivery": delivery, "resume": false }),
            )
            .await;
        assert_eq!(response.status(), 200, "enqueue {text}");
    }
    // …then a fourth prompt that wakes the actor (default resume): it joins
    // the same entry boundary, so one turn must see all four in arrival
    // order — steers first (including this wake item), then the queued one.
    let response = harness
        .post(&inbox, json!({ "text": "wake steer", "delivery": "steer" }))
        .await;
    assert_eq!(response.status(), 200, "waking prompt accepted");

    harness
        .wait_for(&session_id, "assistant completion", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["time"]["completed"].is_number())
        })
        .await;

    // All four user rows exist and the single turn saw them in arrival
    // order (entry boundary: every steer, then exactly one queue item).
    let messages = harness.messages(&session_id).await;
    let user_texts: Vec<&str> = messages
        .iter()
        .filter(|m| m["type"] == "user")
        .filter_map(|m| m["text"].as_str())
        .collect();
    assert_eq!(
        user_texts,
        vec!["first steer", "second steer", "wake steer", "the queue"],
        "promotion order: steers in arrival order, then the queued item"
    );
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 1, "one turn for the whole boundary");
    let users: Vec<String> = requests[0]
        .messages
        .iter()
        .filter_map(|m| match m {
            ox::cardinal::wire::ChatMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        vec![
            "first steer".to_owned(),
            "second steer".to_owned(),
            "wake steer".to_owned(),
            "the queue".to_owned(),
        ],
        "context assembly order"
    );
    harness.wait_event("session.execution.succeeded").await;
}

// ---------------------------------------------------------------------------
// Interrupt: blocked stream → step.failed (aborted) + execution.interrupted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_aborts_stream_and_settles_step_failed() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::spawn(vec![Script::TextThenHang("partial answer", gate.clone())]).await;
    let session_id = harness.create_session().await;

    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "long running" }),
        )
        .await;
    assert_eq!(response.status(), 200);

    // Wait until the step has started (assistant row exists, not completed).
    harness
        .wait_for(&session_id, "assistant started", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["time"]["completed"].is_null())
        })
        .await;

    let response = harness
        .post(&format!("/api/session/{session_id}/interrupt"), json!({}))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["interrupted"], true, "busy period was interrupted");

    // The step settles as failed/aborted; the partial text survives.
    harness
        .wait_for(&session_id, "aborted step", |messages| {
            messages.iter().any(|m| {
                m["type"] == "assistant"
                    && m["time"]["completed"].is_number()
                    && m["error"]["type"] == "aborted"
                    && m["error"]["message"] == "Step interrupted"
            })
        })
        .await;

    harness.wait_event("session.execution.interrupted").await;
    harness.wait_event("session.step.failed").await;
    let second: Value = harness
        .post(&format!("/api/session/{session_id}/interrupt"), json!({}))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(second["interrupted"], false, "idle interrupt is a no-op");

    gate.notify_waiters();
}

// ---------------------------------------------------------------------------
// Retry: transport failures before output schedule retries, then fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transport_failures_retry_then_fail_the_step() {
    let harness = Harness::spawn(vec![Script::Fail, Script::Fail, Script::Fail]).await;
    let session_id = harness.create_session().await;

    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "trigger retries" }),
        )
        .await;
    assert_eq!(response.status(), 200);

    harness
        .wait_for(&session_id, "failed assistant", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["error"].is_object())
        })
        .await;

    // Three attempts, two retry announcements, one failed step, one
    // failed execution.
    assert_eq!(harness.chat.requests().len(), 3, "three attempts");
    harness.wait_event("session.execution.failed").await;
    let kinds = harness.event_kinds().await;
    let retries = kinds
        .iter()
        .filter(|k| **k == "session.retry.scheduled")
        .count();
    assert_eq!(retries, 2, "two retry announcements");
    assert!(kinds.contains(&"session.step.failed".to_owned()));
    let messages = harness.messages(&session_id).await;
    let assistant = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row");
    assert_eq!(assistant["error"]["type"], "unknown");
    assert!(
        assistant["error"]["message"]
            .as_str()
            .expect("message")
            .contains("closed before [DONE]")
    );
}

// ---------------------------------------------------------------------------
// Inbox endpoints: list / patch / cancel + events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_endpoints_list_patch_and_cancel() {
    let harness = Harness::spawn(vec![MockChat::text("ack")]).await;
    let session_id = harness.create_session().await;
    let inbox = format!("/api/session/{session_id}/inbox");

    // Unknown session → 404.
    let response = harness.get("/api/session/ses_missing/inbox").await;
    assert_eq!(response.status(), 404);

    // Enqueue (paused) and list it.
    let prompt = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "queued item", "delivery": "queue", "resume": false }),
        )
        .await;
    assert_eq!(prompt.status(), 200);
    let inbox_id: String = prompt.json::<Value>().await.expect("json")["data"]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let response = harness.get(&inbox).await;
    assert_eq!(response.status(), 200);
    let listed: Value = response.json().await.expect("json");
    let items = listed["data"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], inbox_id.as_str());
    assert_eq!(items[0]["delivery"], "queue");
    assert_eq!(items[0]["payload"]["text"], "queued item");

    // Patch queue → steer: 204 + delivery.changed (and a legitimate wake,
    // so the actor consumes the item: wait for the turn to finish).
    let response = harness
        .client
        .patch(format!("{}/{inbox_id}", harness.base.clone() + &inbox))
        .json(&json!({ "delivery": "steer" }))
        .send()
        .await
        .expect("patch");
    assert_eq!(response.status(), 204);
    harness.wait_event("session.inbox.delivery.changed").await;
    harness
        .wait_for(&session_id, "patched item delivered", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["time"]["completed"].is_number())
        })
        .await;

    // Patching an unknown row → 409 (upstream pendingMutation conflict).
    let response = harness
        .client
        .patch(format!("{}/inbox_missing", harness.base.clone() + &inbox))
        .json(&json!({ "delivery": "queue" }))
        .send()
        .await
        .expect("patch");
    assert_eq!(response.status(), 409);

    // Cancel a still-pending item: 204 + inbox.cancelled, row disappears;
    // cancelling again stays 204 (upstream swallows the conflict).
    let second = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "second item", "delivery": "queue", "resume": false }),
        )
        .await;
    assert_eq!(second.status(), 200);
    let second_id: String = second.json::<Value>().await.expect("json")["data"]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let response = harness
        .client
        .delete(format!("{}/{second_id}", harness.base.clone() + &inbox))
        .send()
        .await
        .expect("delete");
    assert_eq!(response.status(), 204);
    harness.wait_event("session.inbox.cancelled").await;
    let response = harness
        .client
        .delete(format!("{}/{second_id}", harness.base.clone() + &inbox))
        .send()
        .await
        .expect("delete");
    assert_eq!(response.status(), 204, "cancel conflict is swallowed");
    let listed: Value = harness.get(&inbox).await.json().await.expect("json");
    assert_eq!(listed["data"].as_array().expect("items").len(), 0);
}

// ---------------------------------------------------------------------------
// Compaction controls settle as not-implemented in 6a
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compaction_item_fails_not_implemented() {
    use ox::schema::event::SessionEventData;
    use ox::schema::inbox::{CompactionPayload, Delivery, Item};

    let harness = Harness::spawn(vec![MockChat::text("ok")]).await;
    let session_id = harness.create_session().await;
    let inbox_id = "msg_compaction_test";

    harness
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: inbox_id.to_owned(),
            item: Item::Compaction {
                payload: CompactionPayload {},
                delivery: Delivery::Steer,
            },
        })
        .await
        .expect("enqueued");

    // Wake the actor with an ordinary prompt; the compaction control sits at
    // the head of the steers, so the drain settles it before the prompt's
    // turn runs.
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "wake" }),
        )
        .await;
    assert_eq!(response.status(), 200);

    harness.wait_event("session.compaction.failed").await;
    let kinds = harness.event_kinds().await;
    assert!(kinds.contains(&"session.inbox.delivered".to_owned()));
    assert!(kinds.contains(&"session.compaction.started".to_owned()));
    harness
        .wait_for(&session_id, "turn after compaction", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["time"]["completed"].is_number())
        })
        .await;
    assert!(kinds.contains(&"session.execution.succeeded".to_owned()));
}

// ---------------------------------------------------------------------------
// Idle dispatch after a completed busy period respawns the actor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_prompt_after_idle_runs_a_fresh_busy_period() {
    let harness = Harness::spawn(vec![MockChat::text("one"), MockChat::text("two")]).await;
    let session_id = harness.create_session().await;
    let prompt = format!("/api/session/{session_id}/prompt");

    let response = harness.post(&prompt, json!({ "text": "first" })).await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    let response = harness.post(&prompt, json!({ "text": "second" })).await;
    assert_eq!(response.status(), 200);
    harness
        .wait_for(&session_id, "second assistant", |messages| {
            messages.iter().filter(|m| m["type"] == "assistant").count() == 2
        })
        .await;
    let kinds = harness.event_kinds().await;
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session.execution.started")
            .count(),
        2,
        "a busy period per prompt burst"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session.execution.succeeded")
            .count(),
        2
    );
}
