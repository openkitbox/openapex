//! Turn-bound behaviour through the real runner: the `[turn] max_steps` cap,
//! the tool-less final step, and repeated-tool-call detection.
//!
//! Everything below the `Chat` seam is production code: `Runner::dispatch`, the
//! drain loop, the step machinery, the event bus and an in-memory database.
//! The seam is a scripted `Chat` that records every `ChatRequest` it is handed,
//! plus a counting fake `probe` tool. Assertions read the recorded request
//! sequence and the persisted transcript, never runner internals.
//!
//! The assistant model never gets a title generation (the session is created
//! titled), so every recorded request is a main-loop step.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream;
use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_engine::runner::{Chat, Runner, Tool, ToolContext, ToolOutput, ToolRegistry};
use ox_llm::cardinal::chat::{BoxChatStream, ChatFuture};
use ox_llm::cardinal::wire::ToolChoice;
use ox_llm::cardinal::{CardinalError, ChatMessage, ChatRequest, StreamEvent, ToolDef};
use ox_schema::event::SessionEventData;
use ox_schema::inbox::{Delivery, Item, UserPayload};
use ox_schema::support::{LocationRef, Prompt};
use ox_tools::{object_param, ok_text, param};
use serde_json::{Value, json};

/// `MAX_STEPS_PROMPT` in `runner/step.rs`, which is private. Duplicated on
/// purpose: the test pins the exact text the provider sees.
const MAX_STEPS_PROMPT: &str = "The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.";
const REPEAT_FRAGMENT: &str = "You have issued the same tool call with identical arguments";
const DISABLED_MESSAGE: &str = "Tools are disabled after the maximum agent steps";

const SESSION: &str = "ses_turn_bound";

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/// What the scripted model answers one request with.
enum Reply {
    Call { name: &'static str, args: Value },
    Text(&'static str),
}

type Script = Box<dyn Fn(usize, &ChatRequest) -> Reply + Send + Sync>;

/// A scripted transport. `script(n, request)` picks the n-th (1-based) answer;
/// every request is recorded before it is answered.
struct ScriptedChat {
    requests: Mutex<Vec<ChatRequest>>,
    script: Script,
}

impl Chat for ScriptedChat {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        let index = {
            let mut requests = self.requests.lock().expect("requests lock");
            requests.push(request.clone());
            requests.len()
        };
        let events = match (self.script)(index, request) {
            Reply::Call { name, args } => vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some(format!("call_{index}")),
                    name: Some(name.to_owned()),
                    arguments: args.to_string(),
                },
                StreamEvent::Finish {
                    reason: "tool_calls".to_owned(),
                },
                StreamEvent::Done,
            ],
            Reply::Text(text) => vec![
                StreamEvent::TextDelta(text.to_owned()),
                StreamEvent::Finish {
                    reason: "stop".to_owned(),
                },
                StreamEvent::Done,
            ],
        };
        Box::pin(async move {
            Ok(
                Box::pin(stream::iter(events.into_iter().map(Ok::<_, CardinalError>)))
                    as BoxChatStream,
            )
        })
    }
}

/// A tool that does nothing but remember the arguments it ran with.
struct Probe {
    inputs: Arc<Mutex<Vec<Value>>>,
}

impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "probe",
            "Test probe.",
            object_param(vec![("n", param("integer", "Call number"))], &["n"]),
        )
    }

    fn execute(
        &self,
        _ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            self.inputs.lock().expect("inputs lock").push(input);
            ok_text("probe ok")
        })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Run {
    requests: Vec<ChatRequest>,
    probe_inputs: Vec<Value>,
    db: Db,
}

struct Setup {
    max_steps: u32,
    todowrite: bool,
    script: Script,
}

/// Drive one prompt to the end of its busy period and return what the model
/// saw. Waits by bounded polling on `is_active`, then lets the runner settle
/// for a bounded moment so a request that should NOT happen has time to show.
async fn run(setup: Setup) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    let db = Db::open_in_memory().await.expect("open db");
    let bus = EventBus::new(db.clone());

    let mut config = Config::default();
    config.turn.max_steps = setup.max_steps;
    let config = Arc::new(config);

    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Probe {
            inputs: Arc::clone(&inputs),
        }))
        .expect("register probe");
    if setup.todowrite {
        tools.register_todowrite(db.clone());
    }

    let chat = Arc::new(ScriptedChat {
        requests: Mutex::new(Vec::new()),
        script: setup.script,
    });
    let catalog = ox_llm::model_catalog::ModelCatalog::new(&config.llm);
    let runner = Runner::new_with_mcp_and_home(
        config,
        PathBuf::from(dir.path()),
        db.clone(),
        bus.clone(),
        chat.clone(),
        Arc::new(tools),
        catalog,
        Arc::new(ox_mcp::Hub::empty()),
        home.path().to_path_buf(),
    );

    bus.publish(SessionEventData::SessionCreated {
        session_id: SESSION.to_owned(),
        project_id: "default".to_owned(),
        location: LocationRef {
            directory: dir.path().to_string_lossy().into_owned(),
            workspace_id: None,
        },
        subpath: None,
        parent_id: None,
        slug: "turn-bound".to_owned(),
        // Titled, so no title generation competes for the scripted chat.
        title: Some("turn bound".to_owned()),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        version: "test".to_owned(),
    })
    .await
    .expect("create session");
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: SESSION.to_owned(),
        inbox_id: "msg_prompt".to_owned(),
        item: Item::User {
            payload: UserPayload {
                prompt: Prompt {
                    text: "go".to_owned(),
                    files: None,
                    agents: None,
                    skills: None,
                },
                metadata: None,
            },
            delivery: Delivery::Steer,
        },
    })
    .await
    .expect("enqueue prompt");
    runner
        .dispatch(SESSION, "msg_prompt")
        .await
        .expect("dispatch");

    let deadline = Instant::now() + Duration::from_secs(20);
    while runner.is_active(SESSION) {
        assert!(Instant::now() < deadline, "turn did not finish in 20s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A stray extra request (the bug these tests exist for) would start within
    // this window; the runner would also report itself active again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !runner.is_active(SESSION),
        "runner started more work after the turn ended"
    );

    let requests = chat.requests.lock().expect("requests lock").clone();
    let probe_inputs = inputs.lock().expect("inputs lock").clone();
    Run {
        requests,
        probe_inputs,
        db,
    }
}

fn is_none_choice(request: &ChatRequest) -> bool {
    request.tool_choice == ToolChoice::None
}

/// The request's messages as JSON text, for fragment searches.
fn transcript(request: &ChatRequest) -> String {
    serde_json::to_string(&request.messages).expect("serialize messages")
}

/// Rows of the session transcript whose stored data contains `needle`.
async fn rows_containing(db: &Db, needle: &str) -> i64 {
    let conn = db.acquire().await.expect("conn");
    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM session_message WHERE session_id = ?1 AND data LIKE ?2")
        .await
        .expect("prepare");
    let mut rows = stmt
        .query((SESSION, format!("%{needle}%")))
        .await
        .expect("query");
    let row = rows.next().await.expect("next").expect("count row");
    row.get::<i64>(0).expect("count")
}

/// A distinct-argument probe call for request `index`.
fn distinct_call(index: usize) -> Reply {
    Reply::Call {
        name: "probe",
        args: json!({ "n": index }),
    }
}

/// Probe calls while tools are allowed, text once the model is told to stop.
fn call_unless_final(index: usize, request: &ChatRequest, args: impl Fn(usize) -> Value) -> Reply {
    if is_none_choice(request) {
        Reply::Text("wrap-up report")
    } else {
        Reply::Call {
            name: "probe",
            args: args(index),
        }
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// A: the cap ends the tool loop with exactly one tool-less final step.
#[tokio::test]
async fn step_cap_ends_the_turn_with_a_tool_less_final_step() {
    let run = run(Setup {
        max_steps: 3,
        todowrite: false,
        script: Box::new(|index, request| {
            call_unless_final(index, request, |index| json!({ "n": index }))
        }),
    })
    .await;

    assert_eq!(run.requests.len(), 4, "3 capped steps + 1 final step");
    for request in &run.requests[..3] {
        assert_ne!(request.tool_choice, ToolChoice::None);
    }
    let last = &run.requests[3];
    assert_eq!(last.tool_choice, ToolChoice::None);
    assert!(
        !last.tools.is_empty(),
        "tools stay advertised so the provider cache prefix survives"
    );
    match last.messages.last() {
        Some(ChatMessage::Assistant {
            content: Some(text),
            ..
        }) => assert_eq!(text, MAX_STEPS_PROMPT),
        other => panic!("final request must end with the max-steps prompt, got {other:?}"),
    }
    assert_eq!(run.probe_inputs.len(), 3, "the final step ran no tool");
}

/// A2: a model that ignores `tool_choice: none` gets the call rejected, not run.
#[tokio::test]
async fn tool_call_on_the_final_step_is_rejected_not_executed() {
    let run = run(Setup {
        max_steps: 3,
        todowrite: false,
        script: Box::new(|index, _| distinct_call(index)),
    })
    .await;

    assert_eq!(run.requests.len(), 4, "the rejected call opens no 5th step");
    assert!(is_none_choice(&run.requests[3]));
    assert_eq!(run.probe_inputs.len(), 3, "the 4th call must not execute");
    assert!(
        rows_containing(&run.db, DISABLED_MESSAGE).await >= 1,
        "the rejected call is persisted as a tools-disabled failure"
    );
}

/// B: identical calls get one reminder at the 3rd, then a wrap-up at the 6th.
#[tokio::test]
async fn repeated_identical_calls_warn_once_then_force_a_final_step() {
    let run = run(Setup {
        max_steps: 0,
        todowrite: false,
        script: Box::new(|index, request| call_unless_final(index, request, |_| json!({ "n": 1 }))),
    })
    .await;

    assert_eq!(run.requests.len(), 7, "6 repeated calls + 1 final step");
    for (position, request) in run.requests.iter().enumerate() {
        let reminders = transcript(request).matches(REPEAT_FRAGMENT).count();
        match position + 1 {
            1..=3 => assert_eq!(
                reminders,
                0,
                "request {} is before the warning",
                position + 1
            ),
            n => assert_eq!(reminders, 1, "request {n} carries exactly one reminder"),
        }
    }
    assert!(
        run.requests[..6].iter().all(|r| !is_none_choice(r)),
        "only the 7th request is tool-less"
    );
    assert!(is_none_choice(&run.requests[6]));
    assert_eq!(run.probe_inputs.len(), 6);
}

/// C: the ordinary shape, one tool call then text, is untouched by the bound.
#[tokio::test]
async fn one_tool_call_then_text_runs_two_auto_steps() {
    let run = run(Setup {
        max_steps: 500,
        todowrite: false,
        script: Box::new(|index, _| {
            if index == 1 {
                distinct_call(index)
            } else {
                Reply::Text("all done")
            }
        }),
    })
    .await;

    assert_eq!(run.requests.len(), 2);
    assert!(
        run.requests
            .iter()
            .all(|r| r.tool_choice == ToolChoice::Auto)
    );
    assert_eq!(run.probe_inputs.len(), 1);
    assert!(!transcript(&run.requests[1]).contains(MAX_STEPS_PROMPT));
}

/// D: `max_steps = 0` means unlimited; distinct calls never trigger a wrap-up.
#[tokio::test]
async fn zero_max_steps_is_unlimited() {
    let run = run(Setup {
        max_steps: 0,
        todowrite: false,
        script: Box::new(|index, _| {
            if index <= 12 {
                distinct_call(index)
            } else {
                Reply::Text("all done")
            }
        }),
    })
    .await;

    assert_eq!(run.requests.len(), 13, "12 tool steps + the closing text");
    for request in &run.requests {
        assert!(!is_none_choice(request));
        let text = transcript(request);
        assert!(!text.contains(MAX_STEPS_PROMPT));
        assert!(!text.contains(REPEAT_FRAGMENT));
    }
    assert_eq!(run.probe_inputs.len(), 12);
}

/// E: an open todo list must not turn the final step into a fresh turn. The
/// todo reminder enqueues a steer that would reset the step count and bypass
/// the cap, so the final step skips it.
#[tokio::test]
async fn open_todo_does_not_restart_the_turn_after_the_final_step() {
    let run = run(Setup {
        max_steps: 2,
        todowrite: true,
        script: Box::new(|index, request| {
            if is_none_choice(request) {
                return Reply::Text("wrap-up report");
            }
            if index == 1 {
                Reply::Call {
                    name: "todowrite",
                    args: json!({ "todos": [{ "content": "unfinished", "status": "pending" }] }),
                }
            } else {
                distinct_call(index)
            }
        }),
    })
    .await;

    assert_eq!(
        run.requests.len(),
        3,
        "todowrite step + probe step + final step, and nothing after"
    );
    assert!(is_none_choice(&run.requests[2]));
    assert!(
        rows_containing(&run.db, "unfinished").await >= 1,
        "the todo really is open in the transcript"
    );
}
