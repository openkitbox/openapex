//! Integration tests for the session runner: the actor lifecycle, drain
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use ox::cardinal::CardinalError;
use ox::cardinal::chat::{BoxChatStream, ChatFuture};
use ox::cardinal::stream::StreamEvent;
use ox::cardinal::wire::{ChatRequest, FunctionDef, FunctionType, ToolDef, Usage};
use ox::config::Config;
use ox::runner::{Chat, Tool, ToolContext, ToolOutput, ToolRegistry};
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
    /// Emit the head events, hold until the gate fires, then the tail
    /// (tools observe their own execution across the hold).
    Hold {
        head: Vec<StreamEvent>,
        gate: Arc<Notify>,
        tail: Vec<StreamEvent>,
    },
    /// Immediate connect failure (transport error before any output).
    Fail,
    /// A 400 context-length overflow rejection at connect time.
    Overflow,
    /// Emit the head events, then the stream dies mid-flight (transport
    /// read failure after published output — the interrupted-stream class).
    Die(Vec<StreamEvent>),
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

    /// One tool call streamed in two fragments plus
    /// `finish_reason: tool_calls`.
    fn tool_events(id: &str, name: &str, arguments: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some(id.to_owned()),
                name: Some(name.to_owned()),
                arguments: String::new(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: arguments.to_owned(),
            },
            StreamEvent::Finish {
                reason: "tool_calls".to_owned(),
            },
        ]
    }

    /// A complete tool-call turn.
    fn tool_turn(id: &str, name: &str, arguments: &str) -> Script {
        Script::Events(Self::tool_events(id, name, arguments))
    }

    /// A truncated turn: text streamed, `finish_reason: length`.
    fn length(delta: &'static str) -> Script {
        Script::Events(vec![
            StreamEvent::TextDelta(delta.to_owned()),
            StreamEvent::Finish {
                reason: "length".to_owned(),
            },
        ])
    }

    /// A template-filled compaction summary reply (the mock cannot know the
    /// template, so the reply carries one real heading verbatim).
    fn summary(text: &'static str) -> Script {
        Script::Events(vec![
            StreamEvent::TextDelta(format!("## Objective\n- {text}")),
            StreamEvent::Finish {
                reason: "stop".to_owned(),
            },
        ])
    }

    /// Text streamed, then the stream dies mid-flight (no finish, no
    /// `[DONE]`): the canonical interrupted stream after published output.
    fn dies_after_text(delta: &'static str) -> Script {
        Script::Die(vec![StreamEvent::TextDelta(delta.to_owned())])
    }

    /// A tool call whose arguments never completed: input started, one
    /// argument fragment, then the stream dies mid-flight.
    fn dies_after_partial_tool(id: &str, name: &str, partial: &str) -> Script {
        Script::Die(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some(id.to_owned()),
                name: Some(name.to_owned()),
                arguments: String::new(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: partial.to_owned(),
            },
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
                Script::Overflow => Err(CardinalError::Http {
                    status: 400,
                    body: "This model's maximum context length is 4097 tokens".to_owned(),
                }),
                Script::Die(events) => {
                    let mut steps: VecDeque<Step> = events.into_iter().map(Step::Event).collect();
                    steps.push_back(Step::Fail(CardinalError::EarlyClose));
                    Ok(Box::pin(EventsStream {
                        events: steps,
                        gate: None,
                    }) as BoxChatStream)
                }
                Script::Events(mut events) => {
                    events.push(StreamEvent::Done);
                    Ok(Box::pin(EventsStream {
                        events: events.into_iter().map(Step::Event).collect(),
                        gate: None,
                    }) as BoxChatStream)
                }
                Script::TextThenHang(delta, gate) => {
                    let (tx, rx) = oneshot::channel();
                    tokio::spawn(async move {
                        gate.notified().await;
                        let _ = tx.send(());
                    });
                    Ok(Box::pin(EventsStream {
                        events: VecDeque::from(vec![
                            Step::Event(StreamEvent::TextDelta(delta.to_owned())),
                            Step::Wait,
                            Step::Event(StreamEvent::Finish {
                                reason: "stop".to_owned(),
                            }),
                            Step::Event(StreamEvent::Done),
                        ]),
                        gate: Some(rx),
                    }) as BoxChatStream)
                }
                Script::Hold { head, gate, tail } => {
                    let (tx, rx) = oneshot::channel();
                    tokio::spawn(async move {
                        gate.notified().await;
                        let _ = tx.send(());
                    });
                    let mut events: VecDeque<Step> = head.into_iter().map(Step::Event).collect();
                    events.push_back(Step::Wait);
                    events.extend(tail.into_iter().map(Step::Event));
                    events.push_back(Step::Event(StreamEvent::Done));
                    Ok(Box::pin(EventsStream {
                        events,
                        gate: Some(rx),
                    }) as BoxChatStream)
                }
            }
        })
    }
}

/// A stream yielding scripted events; a `Wait` marker holds the stream open
/// until its gate fires, then continues with the remaining events (the
/// interrupt path drops the stream at the hold point instead).
struct EventsStream {
    events: VecDeque<Step>,
    gate: Option<oneshot::Receiver<()>>,
}

/// One scripted stream step.
enum Step {
    /// Yield this event.
    Event(StreamEvent),
    /// Hold until the gate fires (never yields by itself).
    Wait,
    /// Kill the stream with a transport error (the read-failure shape).
    Fail(CardinalError),
}

impl Stream for EventsStream {
    type Item = Result<StreamEvent, CardinalError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.events.front() {
                Some(Step::Event(event)) => {
                    let event = event.clone();
                    this.events.pop_front();
                    return Poll::Ready(Some(Ok(event)));
                }
                Some(Step::Wait) => {
                    let Some(gate) = this.gate.as_mut() else {
                        return Poll::Ready(None);
                    };
                    match Pin::new(gate).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        // Gate fired: drop the marker and continue the script.
                        Poll::Ready(_) => {
                            this.events.pop_front();
                        }
                    }
                }
                Some(Step::Fail(_)) => {
                    if let Some(Step::Fail(error)) = this.events.pop_front() {
                        return Poll::Ready(Some(Err(error)));
                    }
                    unreachable!("Fail marker matched");
                }
                None => return Poll::Ready(None),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool stub
// ---------------------------------------------------------------------------

/// A `test_echo` tool: echoes its input back as `{ "echo": <input> }`, can
/// fail, can delay (long enough to be aborted), and can notify a gate when
/// it finishes (the concurrency test's stream resumes on that gate — a
/// serial implementation deadlocks).
struct EchoTool {
    fail: bool,
    delay: Duration,
    executed: Arc<AtomicBool>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl EchoTool {
    /// Build with knobs; returns the tool and its executed flag.
    fn tool(
        fail: bool,
        delay: Duration,
        gate: Option<Arc<tokio::sync::Notify>>,
    ) -> (Arc<Self>, Arc<AtomicBool>) {
        let executed = Arc::new(AtomicBool::new(false));
        (
            Arc::new(Self {
                fail,
                delay,
                executed: executed.clone(),
                gate,
            }),
            executed,
        )
    }
}

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "test_echo"
    }

    fn definition(&self) -> ToolDef {
        ToolDef {
            kind: FunctionType::Function,
            function: FunctionDef {
                name: "test_echo".to_owned(),
                description: "echoes its input back".to_owned(),
                parameters: json!({ "type": "object", "properties": {} }),
            },
        }
    }

    fn execute(
        &self,
        _ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        let fail = self.fail;
        let delay = self.delay;
        let executed = self.executed.clone();
        let gate = self.gate.clone();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            executed.store(true, Ordering::SeqCst);
            if let Some(gate) = &gate {
                gate.notify_one();
            }
            if fail {
                ToolOutput {
                    content: Value::Null,
                    error: Some("boom".to_owned()),
                }
            } else {
                ToolOutput {
                    content: json!({ "echo": input }),
                    error: None,
                }
            }
        })
    }
}

/// A registry holding exactly one tool.
fn registry_with(tool: Arc<dyn Tool>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(tool).expect("register");
    registry
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
    events: Arc<AsyncMutex<Vec<(String, Value)>>>,
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
    /// Spawn the server with the given chat scripts and an empty registry.
    async fn spawn(scripts: Vec<Script>) -> Self {
        Self::spawn_with(scripts, ToolRegistry::new()).await
    }

    /// Spawn the server with the given chat scripts and tool registry.
    async fn spawn_with(scripts: Vec<Script>, tools: ToolRegistry) -> Self {
        let port = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            db_path: dir.path().join("ox.db"),
            listen: format!("127.0.0.1:{port}"),
            ..Config::default()
        };
        let chat = Arc::new(MockChat::new(scripts));
        let state = ox::build_state_with(config.clone(), chat.clone(), tools)
            .await
            .expect("build state");
        let db = state.db.clone();
        let bus = state.bus.clone();
        let mut events = bus.subscribe();
        let collected = Arc::new(AsyncMutex::new(Vec::<(String, Value)>::new()));
        let sink = collected.clone();
        let collector = tokio::spawn(async move {
            loop {
                let Ok(envelope) = events.recv().await else {
                    break;
                };
                sink.lock()
                    .await
                    .push((envelope.kind, envelope.data.clone()));
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
        let messages = self.messages(session_id).await;
        panic!("timed out waiting for {what}; messages: {messages:?}");
    }

    /// Recorded durable + ephemeral event kinds so far.
    async fn event_kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// The full recorded log: `(envelope type, data)` in arrival order.
    async fn event_log(&self) -> Vec<(String, Value)> {
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
    assert_eq!(assistant["error"]["type"], "provider.transport");
    assert!(
        assistant["error"]["message"]
            .as_str()
            .expect("message")
            .contains("closed before [DONE]")
    );
}

// ---------------------------------------------------------------------------
// Interrupted-stream continuation (upstream Outcome.Continue)
// ---------------------------------------------------------------------------

/// Upstream `CONTINUE_AFTER_INCOMPLETE_STREAM` (runner/llm.ts), verbatim.
const CONTINUE_PROMPT: &str = "The previous response was interrupted. Continue from where you left off without repeating completed content.";

#[tokio::test]
async fn mid_stream_failure_continues_with_upstream_prompt() {
    let harness = Harness::spawn(vec![
        MockChat::dies_after_text("par"),
        MockChat::text("tial answer"),
    ])
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "interrupted stream" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    // The continuation re-ran the attempt against a fresh assistant row.
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 2, "partial attempt + continuation attempt");

    // The continuation prompt rides as a synthetic user message, verbatim.
    let synthetic = harness
        .event_log()
        .await
        .into_iter()
        .find(|(k, _)| k == "session.synthetic")
        .map(|(_, data)| data)
        .expect("continuation prompt published");
    assert_eq!(synthetic["text"], CONTINUE_PROMPT);

    // The continuation attempt observes the original prompt, the partial
    // assistant text, and the continuation prompt.
    let user_texts: Vec<&str> = requests[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            ox::cardinal::wire::ChatMessage::User { content } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["interrupted stream", CONTINUE_PROMPT]);
    let assistant_texts: Vec<&str> = requests[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            ox::cardinal::wire::ChatMessage::Assistant { content, .. } => content.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(assistant_texts, vec!["par"], "partial text replayed");

    // Rows: user, partial assistant (failed), synthetic, completed assistant
    // (plus the actor's idle marker at the end of the busy period).
    let messages = harness.messages(&session_id).await;
    let kinds: Vec<&str> = messages.iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(&kinds[..4], ["user", "assistant", "synthetic", "assistant"]);

    // The partial assistant settles `step.failed` with the stream error
    // (finish `error`, no `step.ended` for it — upstream failAssistant).
    let partial = &messages[1];
    assert_eq!(partial["error"]["type"], "provider.transport");
    assert_eq!(partial["finish"], "error");
    assert_eq!(partial["content"][0]["text"], "par", "partial text flushed");
    let final_row = &messages[3];
    assert!(final_row["error"].is_null(), "fresh row settled cleanly");
    assert_eq!(final_row["content"][0]["text"], "tial answer");
    assert_eq!(final_row["finish"], "stop");

    let log = harness.event_log().await;
    let step_failed = log
        .iter()
        .find(|(k, _)| k == "session.step.failed")
        .map(|(_, data)| data)
        .expect("partial step failed");
    assert_eq!(step_failed["assistantMessageID"], partial["id"]);
    assert_eq!(step_failed["error"]["type"], "provider.transport");
    let step_ended = log
        .iter()
        .find(|(k, _)| k == "session.step.ended")
        .map(|(_, data)| data)
        .expect("continuation step ended");
    assert_eq!(step_ended["assistantMessageID"], final_row["id"]);
    assert_eq!(
        log.iter()
            .filter(|(k, _)| k == "session.step.failed")
            .count(),
        1,
        "only the partial row failed"
    );
    let retry = log
        .iter()
        .find(|(k, _)| k == "session.retry.scheduled")
        .map(|(_, data)| data)
        .expect("retry announced");
    assert_eq!(retry["attempt"], 1);
    assert_eq!(retry["error"]["type"], "provider.transport");
}

#[tokio::test]
async fn mid_stream_failure_settles_partial_tool_with_stream_error() {
    let (echo, _executed) = EchoTool::tool(false, Duration::ZERO, None);
    let harness = Harness::spawn_with(
        vec![
            MockChat::dies_after_partial_tool("call_1", "test_echo", r#"{"q":"par"#),
            MockChat::tool_turn("call_2", "test_echo", r#"{"q":"ok"}"#),
            MockChat::text("done"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "die mid tool" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    // Upstream failAssistant: the call unsettled at the stream's death
    // fails with the stream error itself — not the input-failure text —
    // and never executes.
    let failure = harness
        .event_log()
        .await
        .into_iter()
        .find(|(k, _)| k == "session.tool.failed")
        .map(|(_, data)| data)
        .expect("partial tool settled");
    assert_eq!(failure["id"], "call_1");
    assert_eq!(failure["error"]["type"], "provider.transport");

    // The continuation attempt re-issues the call: the failed call and its
    // error result replay before the continuation prompt.
    let requests = harness.chat.requests();
    assert_eq!(
        requests.len(),
        3,
        "partial attempt + tool continuation + text step"
    );
    let roles: Vec<&str> = requests[1]
        .messages
        .iter()
        .map(|m| match m {
            ox::cardinal::wire::ChatMessage::System { .. } => "system",
            ox::cardinal::wire::ChatMessage::User { .. } => "user",
            ox::cardinal::wire::ChatMessage::Assistant { .. } => "assistant",
            ox::cardinal::wire::ChatMessage::Tool { .. } => "tool",
        })
        .collect();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "tool", "user"],
        "failed call + error result replay, then the continuation prompt"
    );
    let replayed = requests[1]
        .messages
        .iter()
        .find_map(|m| match m {
            ox::cardinal::wire::ChatMessage::Tool {
                tool_call_id,
                content,
            } => Some((tool_call_id.clone(), content.clone())),
            _ => None,
        })
        .expect("failed call's result replayed");
    assert_eq!(replayed.0, "call_1");
    assert!(
        replayed.1.contains("provider.transport"),
        "the error result carries the stream error: {}",
        replayed.1
    );

    // Rows: user, partial row (tool part in error), synthetic prompt,
    // completed tool step, text step.
    let messages = harness.messages(&session_id).await;
    let kinds: Vec<&str> = messages.iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(
        &kinds[..5],
        ["user", "assistant", "synthetic", "assistant", "assistant"]
    );

    let partial = &messages[1];
    let part = &partial["content"][0];
    assert_eq!(part["type"], "tool");
    assert_eq!(part["id"], "call_1");
    assert_eq!(part["state"]["status"], "error");
    assert_eq!(part["state"]["error"]["type"], "provider.transport");
    assert_eq!(partial["error"]["type"], "provider.transport");

    let retried = &messages[3];
    let part = retried["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("reissued tool part");
    assert_eq!(part["id"], "call_2");
    assert_eq!(part["state"]["status"], "completed");
    assert_eq!(retried["finish"], "tool-calls");
}

#[tokio::test]
async fn continuation_exhaustion_fails_the_step() {
    let harness = Harness::spawn(vec![
        MockChat::dies_after_text("a"),
        MockChat::dies_after_text("b"),
        MockChat::dies_after_text("c"),
    ])
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "never finishes" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.failed").await;

    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 3, "three attempts, two continuations");
    let log = harness.event_log().await;
    assert_eq!(
        log.iter().filter(|(k, _)| k == "session.synthetic").count(),
        2,
        "a continuation prompt per retry"
    );
    assert_eq!(
        log.iter()
            .filter(|(k, _)| k == "session.retry.scheduled")
            .count(),
        2
    );

    // Every attempt's partial row settles failed with the stream error.
    let messages = harness.messages(&session_id).await;
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 3, "one row per attempt");
    for (attempt, row) in assistants.iter().enumerate() {
        assert_eq!(row["error"]["type"], "provider.transport", "row {attempt}");
        assert!(
            row["time"]["completed"].is_number(),
            "row {attempt} settled"
        );
    }
    assert_eq!(assistants[0]["content"][0]["text"], "a");
    assert_eq!(assistants[1]["content"][0]["text"], "b");
    assert_eq!(assistants[2]["content"][0]["text"], "c");

    let failed: Vec<&Value> = log
        .iter()
        .filter(|(k, _)| k == "session.step.failed")
        .map(|(_, data)| data)
        .collect();
    assert_eq!(failed.len(), 3, "one step.failed per attempt");
    assert_eq!(failed[2]["assistantMessageID"], assistants[2]["id"]);
    let execution = log
        .iter()
        .find(|(k, _)| k == "session.execution.failed")
        .map(|(_, data)| data)
        .expect("execution failed");
    assert_eq!(execution["error"]["type"], "provider.transport");
}

#[tokio::test]
async fn failing_continuation_retries_then_fails() {
    let harness = Harness::spawn(vec![
        MockChat::dies_after_text("par"),
        Script::Fail,
        Script::Fail,
    ])
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "continuation dies too" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.failed").await;

    let requests = harness.chat.requests();
    assert_eq!(
        requests.len(),
        3,
        "partial attempt + continuation + exhausted retry"
    );
    let log = harness.event_log().await;
    assert_eq!(
        log.iter().filter(|(k, _)| k == "session.synthetic").count(),
        1,
        "one continuation prompt"
    );
    assert_eq!(
        log.iter()
            .filter(|(k, _)| k == "session.retry.scheduled")
            .count(),
        2
    );
    assert_eq!(
        log.iter()
            .filter(|(k, _)| k == "session.step.failed")
            .count(),
        2,
        "the partial row and the exhausted retry row"
    );

    let messages = harness.messages(&session_id).await;
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0]["content"][0]["text"], "par");
    assert_eq!(assistants[0]["error"]["type"], "provider.transport");
    assert!(
        assistants[1]["error"].is_object(),
        "the connect-failed row settles failed"
    );
    assert!(
        assistants[1]["content"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "no content on the connect-failed row"
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

    // Patching an unknown row → 409 with the exact upstream wire string.
    let response = harness
        .client
        .patch(format!("{}/inbox_missing", harness.base.clone() + &inbox))
        .json(&json!({ "delivery": "queue" }))
        .send()
        .await
        .expect("patch");
    assert_eq!(response.status(), 409);
    let conflict: Value = response.json().await.expect("json");
    assert_eq!(
        conflict["message"],
        "Pending input cannot change to queue: inbox_missing"
    );
    assert_eq!(conflict["resource"], "inbox_missing");

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
// Task 6b: tool-call streaming + execution loop + compaction + overflow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_flow_executes_and_continues_with_results() {
    let (echo, _executed) = EchoTool::tool(false, Duration::ZERO, None);
    let harness = Harness::spawn_with(
        vec![
            MockChat::tool_turn("call_1", "test_echo", r#"{"q":"hi"}"#),
            MockChat::text("done"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "use the tool" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 2, "tool step + continuation step");
    let wire = serde_json::to_value(&requests[0]).expect("wire");
    assert_eq!(wire["tools"].as_array().map(Vec::len), Some(1));

    // Event choreography, in order.
    let log = harness.event_log().await;
    let index_of = |kind: &str| log.iter().position(|(k, _)| k == kind).expect(kind);
    let started = index_of("session.tool.input.started");
    let delta = index_of("session.tool.input.delta");
    let ended = index_of("session.tool.input.ended");
    let called = index_of("session.tool.called");
    let success = index_of("session.tool.success");
    assert!(started < delta && delta < ended && ended < called && called < success);

    // The continuation request replays the call and its output.
    let tool_messages = requests[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            ox::cardinal::wire::ChatMessage::Tool {
                tool_call_id,
                content,
            } => Some((tool_call_id.clone(), content.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 1, "one tool result replayed");
    assert_eq!(tool_messages[0].0, "call_1");
    assert!(
        tool_messages[0].1.contains("hi"),
        "output should carry the echoed input: {}",
        tool_messages[0].1
    );

    // The assistant row's tool part completed with the parsed input/output.
    let messages = harness.messages(&session_id).await;
    let part = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row")["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("tool part")
        .clone();
    assert_eq!(part["id"], "call_1");
    assert_eq!(part["state"]["status"], "completed");
    assert_eq!(part["state"]["input"]["q"], "hi");
    assert!(
        part["state"]["content"][0]["text"]
            .as_str()
            .expect("output text")
            .contains("hi")
    );
}

#[tokio::test]
async fn tool_failure_continues_with_error_result() {
    let (echo, _executed) = EchoTool::tool(true, Duration::ZERO, None);
    let harness = Harness::spawn_with(
        vec![
            MockChat::tool_turn("call_1", "test_echo", r#"{}"#),
            MockChat::text("recovered"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "fail it" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    harness.wait_event("session.tool.failed").await;
    let log = harness.event_log().await;
    let failure = log
        .iter()
        .find(|(k, _)| k == "session.tool.failed")
        .expect("failure event");
    assert_eq!(failure.1["error"]["type"], "tool.execution");
    assert_eq!(failure.1["error"]["message"], "boom");

    // The model saw the error result and the turn still continued.
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 2);
    let tool_messages = requests[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            ox::cardinal::wire::ChatMessage::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 1);
    assert!(
        tool_messages[0].contains("boom"),
        "error result replayed: {}",
        tool_messages[0]
    );

    let messages = harness.messages(&session_id).await;
    let part = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row")["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("tool part")
        .clone();
    assert_eq!(part["state"]["status"], "error");
    assert_eq!(part["state"]["error"]["message"], "boom");
}

#[tokio::test]
async fn tool_executes_while_stream_is_open() {
    // The tool notifies the gate when it ran; the stream only finishes its
    // remaining events after that gate — a serial implementation deadlocks
    // and the execution never settles.
    let gate = Arc::new(tokio::sync::Notify::new());
    let (echo, executed) = EchoTool::tool(false, Duration::from_millis(20), Some(gate.clone()));
    let harness = Harness::spawn_with(
        vec![
            Script::Hold {
                head: MockChat::tool_events("call_1", "test_echo", r#"{}"#),
                gate,
                tail: vec![StreamEvent::Usage(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 1,
                    cached_tokens: None,
                    reasoning_tokens: None,
                })],
            },
            MockChat::text("done"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "overlap" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    assert!(executed.load(Ordering::SeqCst), "the tool ran");
    // The step joins its tools before step.ended.
    let log = harness.event_log().await;
    let position = |kind: &str| log.iter().position(|(k, _)| k == kind).expect(kind);
    assert!(position("session.tool.success") < position("session.step.ended"));
}

#[tokio::test]
async fn interrupt_aborts_in_flight_tool() {
    // The tool sleeps long enough to be aborted mid-flight; the stream hold
    // never fires, so the step is waiting on the tool when the interrupt
    // lands.
    let (sleeper, executed) = EchoTool::tool(false, Duration::from_secs(30), None);
    let harness = Harness::spawn_with(
        vec![Script::Hold {
            head: MockChat::tool_events("call_1", "test_echo", r#"{}"#),
            gate: Arc::new(tokio::sync::Notify::new()),
            tail: vec![],
        }],
        registry_with(sleeper),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "abort me" }),
        )
        .await;
    assert_eq!(response.status(), 200);

    // Wait until the call is executing (spawned at Finish).
    harness.wait_event("session.tool.called").await;
    let response = harness
        .post(&format!("/api/session/{session_id}/interrupt"), json!({}))
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["interrupted"], true);

    harness.wait_event("session.execution.interrupted").await;
    let log = harness.event_log().await;
    let failure = log
        .iter()
        .find(|(k, _)| k == "session.tool.failed")
        .expect("aborted tool failure");
    assert_eq!(failure.1["error"]["type"], "aborted");
    assert_eq!(failure.1["error"]["message"], "Tool execution interrupted");
    assert!(
        !executed.load(Ordering::SeqCst),
        "aborted tool never completed"
    );

    let messages = harness.messages(&session_id).await;
    let part = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row")["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("tool part")
        .clone();
    assert_eq!(part["state"]["status"], "error");
    assert_eq!(part["state"]["error"]["type"], "aborted");
    harness.wait_event("session.step.failed").await;
}

#[tokio::test]
async fn overflow_compacts_and_retries_once() {
    let harness = Harness::spawn(vec![
        Script::Overflow,
        MockChat::summary("S2 summary"),
        MockChat::text("resumed"),
    ])
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "trigger overflow" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    harness.wait_event("session.compaction.started").await;
    harness.wait_event("session.compaction.ended").await;
    let log = harness.event_log().await;
    let started = log
        .iter()
        .find(|(k, _)| k == "session.compaction.started")
        .expect("compaction started");
    assert_eq!(started.1["reason"], "auto");

    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 3, "overflow attempt + summary + retry");

    // The summarization request: compaction system prompt, no tools, the
    // summary instruction last.
    let summary = &requests[1];
    assert!(matches!(
        &summary.messages[0],
        ox::cardinal::wire::ChatMessage::System { content } if content.contains("summarize conversations")
    ));
    assert!(matches!(
        summary.messages.last(),
        Some(ox::cardinal::wire::ChatMessage::User { content }) if content.contains("You MUST summarize")
    ));
    let wire = serde_json::to_value(summary).expect("wire");
    assert!(wire.get("tools").is_none(), "compaction never sends tools");

    // The retry sees only the checkpoint: the compaction row replaced the
    // whole transcript.
    let retry = &requests[2];
    assert_eq!(retry.messages.len(), 2, "system + checkpoint");
    assert!(matches!(
        &retry.messages[1],
        ox::cardinal::wire::ChatMessage::User { content }
            if content.contains("<conversation-checkpoint>") && content.contains("S2 summary")
    ));

    // The compaction row landed completed with the summary.
    let messages = harness.messages(&session_id).await;
    let compaction = messages
        .iter()
        .find(|m| m["type"] == "compaction")
        .expect("compaction row");
    assert_eq!(compaction["status"], "completed");
    assert!(
        compaction["summary"]
            .as_str()
            .is_some_and(|text| text.contains("S2 summary")),
        "summary carries the model's text"
    );
}

#[tokio::test]
async fn compaction_item_summarizes_and_checkpoints_context() {
    use ox::schema::event::SessionEventData;
    use ox::schema::inbox::{CompactionPayload, Delivery, Item};

    let harness = Harness::spawn(vec![
        MockChat::summary("S2 summary"),
        MockChat::text("after"),
    ])
    .await;
    let session_id = harness.create_session().await;

    harness
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: "msg_compaction_test".to_owned(),
            item: Item::Compaction {
                payload: CompactionPayload {},
                delivery: Delivery::Steer,
            },
        })
        .await
        .expect("enqueued");

    // Wake the actor with an ordinary prompt; the compaction control sits at
    // the head of the steers, so the drain runs it before the prompt's turn.
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "wake" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.compaction.ended").await;
    harness.wait_event("session.execution.succeeded").await;

    let log = harness.event_log().await;
    let started = log
        .iter()
        .find(|(k, _)| k == "session.compaction.started")
        .expect("compaction started");
    assert_eq!(started.1["reason"], "manual");
    let ended = log
        .iter()
        .find(|(k, _)| k == "session.compaction.ended")
        .expect("compaction ended");
    assert!(
        ended.1["text"]
            .as_str()
            .is_some_and(|text| text.contains("S2 summary")),
        "summary carries the model's text: {:?}",
        ended.1["text"]
    );
    assert!(
        log.iter().any(|(k, _)| k == "session.compaction.delta"),
        "the summary streamed as deltas"
    );

    // The summarization request was captured: no tools, special system
    // prompt, summary instruction last.
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 2, "summary + wake turn");
    let summary = &requests[0];
    assert!(matches!(
        &summary.messages[0],
        ox::cardinal::wire::ChatMessage::System { content } if content.contains("summarize conversations")
    ));
    let wire = serde_json::to_value(summary).expect("wire");
    assert!(wire.get("tools").is_none(), "compaction sends no tools");
    assert!(matches!(
        summary.messages.last(),
        Some(ox::cardinal::wire::ChatMessage::User { content }) if content.contains("You MUST summarize")
    ));

    // The wake turn's context opens with the checkpoint summary.
    assert!(requests[1].messages.iter().any(|m| matches!(
        m,
        ox::cardinal::wire::ChatMessage::User { content }
            if content.contains("<conversation-checkpoint>") && content.contains("S2 summary")
    )));

    // The compaction row is durable.
    let messages = harness.messages(&session_id).await;
    assert!(messages.iter().any(|m| {
        m["type"] == "compaction"
            && m["status"] == "completed"
            && m["summary"]
                .as_str()
                .is_some_and(|text| text.contains("S2 summary"))
    }));
}

#[tokio::test]
async fn stale_running_tools_settle_at_drain_entry() {
    use ox::schema::event::SessionEventData;
    use ox::schema::support::ModelRef;

    let harness = Harness::spawn(vec![MockChat::text("ok")]).await;
    let session_id = harness.create_session().await;

    // A crashed predecessor left an assistant row with a running tool part.
    harness
        .bus
        .publish(SessionEventData::StepStarted {
            session_id: session_id.clone(),
            assistant_message_id: "msg_stale".to_owned(),
            agent: "build".to_owned(),
            model: ModelRef {
                id: "m".to_owned(),
                provider_id: "p".to_owned(),
                variant: None,
            },
            snapshot: None,
            started: 1,
        })
        .await
        .expect("step started");
    harness
        .bus
        .publish(SessionEventData::ToolInputStarted {
            session_id: session_id.clone(),
            assistant_message_id: "msg_stale".to_owned(),
            id: "call_stale".to_owned(),
            name: "test_echo".to_owned(),
        })
        .await
        .expect("input started");
    harness
        .bus
        .publish(SessionEventData::ToolCalled {
            session_id: session_id.clone(),
            assistant_message_id: "msg_stale".to_owned(),
            id: "call_stale".to_owned(),
            input: {
                use serde_json::value::Value as V;
                let mut input = serde_json::Map::new();
                input.insert("a".to_owned(), json!(1));
                input
                    .into_iter()
                    .collect::<std::collections::BTreeMap<String, V>>()
            },
            executed: false,
            state: None,
        })
        .await
        .expect("tool called");

    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "wake" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    // The stale part settled as failed/interrupted.
    let log = harness.event_log().await;
    let failure = log
        .iter()
        .find(|(k, data)| k == "session.tool.failed" && data["assistantMessageID"] == "msg_stale")
        .expect("stale tool settled");
    assert_eq!(failure.1["id"], "call_stale");
    assert_eq!(failure.1["error"]["message"], "Tool execution interrupted");

    let messages = harness.messages(&session_id).await;
    let stale = messages
        .iter()
        .find(|m| m["id"] == "msg_stale")
        .expect("stale row");
    let part = stale["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("tool part");
    assert_eq!(part["state"]["status"], "error");
}

#[tokio::test]
async fn length_truncation_continues_then_exhausts() {
    // Three truncated responses: two continuations, then the turn gives up
    // (upstream Outcome.OutputLimit exhaustion).
    let harness = Harness::spawn(vec![
        MockChat::length("one"),
        MockChat::length("two"),
        MockChat::length("three"),
    ])
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "keep going" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.failed").await;

    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 3, "three truncated attempts");

    // Two synthetic continuation prompts were injected between attempts.
    let log = harness.event_log().await;
    let synthetics = log
        .iter()
        .filter(|(k, data)| {
            k == "session.synthetic"
                && data["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("hit the output token limit"))
        })
        .count();
    assert_eq!(synthetics, 2, "two continuation prompts");

    // The requests between truncations carry the synthetic as a user turn.
    assert!(requests[1].messages.iter().any(|m| matches!(
        m,
        ox::cardinal::wire::ChatMessage::User { content } if content.contains("hit the output token limit")
    )));

    // Upstream raises StepFailedError from OutputLimit exhaustion with only
    // execution.failed as the terminal: every truncated assistant keeps
    // `finish: "length"` with no error, and no step.failed is published.
    let log = harness.event_log().await;
    assert!(
        !log.iter().any(|(k, _)| k == "session.step.failed"),
        "exhaustion surfaces only as execution.failed"
    );
    let messages = harness.messages(&session_id).await;
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|m| m["type"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 3);
    for assistant in &assistants {
        assert_eq!(assistant["finish"], "length");
        assert!(assistant["error"].is_null());
    }
}

#[tokio::test]
async fn no_tools_session_omits_tools_key() {
    let harness = Harness::spawn(vec![MockChat::text("hi")]).await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "plain" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    let wire = serde_json::to_value(&requests[0]).expect("wire");
    assert!(wire.get("tools").is_none(), "no tools key on the wire");
    assert!(wire.get("tool_choice").is_none());
}

// ---------------------------------------------------------------------------
// Length truncation with tool calls: continues without a synthetic prompt
// (upstream `Outcome.OutputLimit` + `needsContinuation`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn length_with_tool_calls_continues_without_prompt() {
    let (echo, executed) = EchoTool::tool(false, Duration::ZERO, None);
    let mut truncated = MockChat::tool_events("call_1", "test_echo", r#"{"q":"hi"}"#);
    truncated.pop(); // finish=tool_calls → finish=length
    let harness = Harness::spawn_with(
        vec![
            Script::Events({
                truncated.push(StreamEvent::Finish {
                    reason: "length".to_owned(),
                });
                truncated
            }),
            MockChat::text("after"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "truncated but tooled" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    assert!(
        executed.load(Ordering::SeqCst),
        "the truncated step's tool ran"
    );
    // No synthetic continuation prompt: the tool calls own the boundary.
    let log = harness.event_log().await;
    assert!(
        !log.iter().any(|(k, _)| k == "session.synthetic"),
        "needsContinuation skips the synthetic prompt"
    );
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 2, "truncated step + continuation step");
    let messages = harness.messages(&session_id).await;
    let assistant = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row");
    assert_eq!(assistant["finish"], "length");
    assert_eq!(assistant["content"][0]["state"]["status"], "completed");
}

// ---------------------------------------------------------------------------
// Overflow give-up: a retry that still overflows publishes step.failed
// (fresh failed row forwarding the provider error) then execution.failed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn overflow_give_up_fails_step_forwarding_provider_error() {
    let harness = Harness::spawn(vec![
        Script::Overflow,
        MockChat::summary("S2 summary"),
        Script::Overflow,
    ])
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "still too long" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.failed").await;

    harness.wait_event("session.step.failed").await;
    let log = harness.event_log().await;
    let failed = log
        .iter()
        .find(|(k, _)| k == "session.step.failed")
        .expect("step failed");
    assert_eq!(failed.1["error"]["type"], "provider.invalid-request");
    assert!(
        failed.1["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("maximum context length")),
        "the provider error is forwarded: {:?}",
        failed.1["error"]["message"]
    );
    // One compaction ran, then the give-up.
    assert_eq!(
        log.iter()
            .filter(|(k, _)| k == "session.compaction.started")
            .count(),
        1
    );

    let messages = harness.messages(&session_id).await;
    let assistant = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row");
    assert_eq!(assistant["finish"], "error");
    assert_eq!(assistant["error"]["type"], "provider.invalid-request");
    assert!(
        assistant["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("maximum context length"))
    );
}

// ---------------------------------------------------------------------------
// Compaction nudge: an off-template summary reply gets exactly one re-ask
// (upstream NUDGE), then either succeeds or fails compaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compaction_nudges_off_template_reply_once() {
    use ox::schema::event::SessionEventData;
    use ox::schema::inbox::{CompactionPayload, Delivery, Item};

    let harness = Harness::spawn(vec![
        MockChat::text("I will not follow templates."),
        MockChat::summary("proper summary"),
        MockChat::text("after"),
    ])
    .await;
    let session_id = harness.create_session().await;
    harness
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: "msg_compaction_nudge".to_owned(),
            item: Item::Compaction {
                payload: CompactionPayload {},
                delivery: Delivery::Steer,
            },
        })
        .await
        .expect("enqueued");
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "wake" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.compaction.ended").await;
    harness.wait_event("session.execution.succeeded").await;

    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 3, "off-template ask + nudge + wake turn");
    // The nudge appended the reminder after the summary prompt.
    let nudged = &requests[1];
    let users: Vec<&String> = nudged
        .messages
        .iter()
        .filter_map(|m| match m {
            ox::cardinal::wire::ChatMessage::User { content } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(users.len(), 2, "summary instruction + nudge");
    assert!(
        users[0].contains("You MUST summarize"),
        "the summary instruction precedes the nudge"
    );
    assert!(
        users[1].contains("did not fill in the required summary template"),
        "the nudge re-asks the template"
    );

    // The second reply satisfied the template and became the checkpoint.
    let messages = harness.messages(&session_id).await;
    assert!(messages.iter().any(|m| {
        m["type"] == "compaction"
            && m["status"] == "completed"
            && m["summary"]
                .as_str()
                .is_some_and(|text| text.contains("proper summary"))
    }));
    assert!(!log_contains(&harness, "session.compaction.failed").await);
}

/// Whether the harness log recorded `kind` so far.
async fn log_contains(harness: &Harness, kind: &str) -> bool {
    harness.event_log().await.iter().any(|(k, _)| k == kind)
}

// ---------------------------------------------------------------------------
// Malformed JSON tool input: fails the call with the input excerpt, the
// turn continues (upstream `INPUT_FAILURES.malformed` + `inputFailure`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_json_input_fails_with_excerpt() {
    let (echo, executed) = EchoTool::tool(false, Duration::ZERO, None);
    let harness = Harness::spawn_with(
        vec![
            MockChat::tool_turn("call_1", "test_echo", r#"{"q": "#),
            MockChat::text("retrying"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "break the args" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    let failure = harness
        .event_log()
        .await
        .into_iter()
        .find(|(k, _)| k == "session.tool.failed")
        .expect("malformed failure");
    assert_eq!(failure.1["error"]["type"], "tool.input-json");
    let message = failure.1["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("could not be parsed as JSON"),
        "upstream malformed wording: {message}"
    );
    assert!(
        message.contains("Input excerpt (first 6 of 6 characters):\n{\"q\": "),
        "the input excerpt is appended: {message}"
    );
    assert!(!executed.load(Ordering::SeqCst), "the call never executed");

    // The failure is model-visible and the turn continued.
    let requests = harness.chat.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|m| matches!(
        m,
        ox::cardinal::wire::ChatMessage::Tool { content, .. } if content.contains("could not be parsed as JSON")
    )));
    let messages = harness.messages(&session_id).await;
    let part = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row")["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("tool part")
        .clone();
    assert_eq!(part["state"]["status"], "error");
    assert_eq!(part["state"]["error"]["type"], "tool.input-json");
}

// ---------------------------------------------------------------------------
// Stream ends with no (or an unrecognized) finish: open tool input settles
// tool.input-incomplete with the excerpt, the step fails with
// provider.invalid-output (upstream `unknownFinish` + `failIncompleteTools`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_without_finish_continues_after_output() {
    let (echo, executed) = EchoTool::tool(false, Duration::ZERO, None);
    let harness = Harness::spawn_with(
        vec![
            Script::Events(vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_owned()),
                name: Some("test_echo".to_owned()),
                arguments: "{\"q\": \"par".to_owned(),
            }]),
            MockChat::text("recovered"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "cut off" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    // Upstream `unknownFinish`: a stream that ends without a finish reason
    // is invalid provider output; the unsettled call fails with that error
    // itself (failAssistant — not the input-failure text) and the step
    // continues via the upstream prompt.
    let failure = harness
        .event_log()
        .await
        .into_iter()
        .find(|(k, _)| k == "session.tool.failed")
        .map(|(_, data)| data)
        .expect("unsettled tool failure");
    assert_eq!(failure["error"]["type"], "provider.invalid-output");
    assert_eq!(
        failure["error"]["message"],
        "The provider response ended with an unknown finish reason."
    );
    assert!(!executed.load(Ordering::SeqCst), "the call never executed");

    let synthetic = harness
        .event_log()
        .await
        .into_iter()
        .find(|(k, _)| k == "session.synthetic")
        .map(|(_, data)| data)
        .expect("continuation prompt published");
    assert_eq!(synthetic["text"], CONTINUE_PROMPT);

    let messages = harness.messages(&session_id).await;
    let kinds: Vec<&str> = messages.iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(&kinds[..4], ["user", "assistant", "synthetic", "assistant"]);
    assert_eq!(messages[1]["error"]["type"], "provider.invalid-output");
    assert_eq!(messages[3]["content"][0]["text"], "recovered");
}

// ---------------------------------------------------------------------------
// Unknown tool name: the call settles tool.unknown without executing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_tool_fails_with_tool_unknown() {
    let (echo, _executed) = EchoTool::tool(false, Duration::ZERO, None);
    let harness = Harness::spawn_with(
        vec![
            MockChat::tool_turn("call_1", "no_such_tool", r#"{}"#),
            MockChat::text("noted"),
        ],
        registry_with(echo),
    )
    .await;
    let session_id = harness.create_session().await;
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "call a ghost" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness.wait_event("session.execution.succeeded").await;

    let failure = harness
        .event_log()
        .await
        .into_iter()
        .find(|(k, _)| k == "session.tool.failed")
        .expect("unknown tool failure");
    assert_eq!(failure.1["error"]["type"], "tool.unknown");
    assert_eq!(
        failure.1["error"]["message"],
        "Tool is not available for this request: no_such_tool"
    );

    let messages = harness.messages(&session_id).await;
    let part = messages
        .iter()
        .find(|m| m["type"] == "assistant")
        .expect("assistant row")["content"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|p| p["type"] == "tool")
        .expect("tool part")
        .clone();
    assert_eq!(part["state"]["status"], "error");
    assert_eq!(part["state"]["error"]["type"], "tool.unknown");
}

// ---------------------------------------------------------------------------
// Queue chaining: a queued head re-opens the entry boundary in the same
// busy period (llm.ts:109-112) — no dispatch doorbell required
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queued_items_chain_across_turns_in_one_busy_period() {
    use ox::schema::inbox::{Delivery, Item, UserPayload};

    let gate = Arc::new(Notify::new());
    let harness = Harness::spawn(vec![
        Script::TextThenHang("one", gate.clone()),
        MockChat::text("two"),
    ])
    .await;
    let session_id = harness.create_session().await;

    // Turn 1 starts and blocks mid-stream.
    let response = harness
        .post(
            &format!("/api/session/{session_id}/prompt"),
            json!({ "text": "first", "delivery": "queue" }),
        )
        .await;
    assert_eq!(response.status(), 200);
    harness
        .wait_for(&session_id, "assistant started", |messages| {
            messages
                .iter()
                .any(|m| m["type"] == "assistant" && m["time"]["completed"].is_null())
        })
        .await;

    // A second queued row lands while turn 1 is in flight — published
    // straight onto the bus, so no dispatch message follows: only the
    // drain's own re-opened entry boundary can pick it up.
    harness
        .bus
        .publish(ox::schema::event::SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: "msg_second".to_owned(),
            item: Item::User {
                payload: UserPayload {
                    prompt: ox::schema::support::Prompt {
                        text: "second".to_owned(),
                        files: None,
                        agents: None,
                        skills: None,
                    },
                    metadata: None,
                },
                delivery: Delivery::Queue,
            },
        })
        .await
        .expect("enqueued");

    // Turn 1 finishes; the drain must chain straight into turn 2.
    gate.notify_waiters();
    harness
        .wait_for(&session_id, "second assistant", |messages| {
            messages.iter().filter(|m| m["type"] == "assistant").count() == 2
        })
        .await;

    // Two turns in ONE busy period: one started/succeeded pair, both
    // prompts delivered, inbox empty.
    harness.wait_event("session.execution.succeeded").await;
    let kinds = harness.event_kinds().await;
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session.execution.started")
            .count(),
        1,
        "one busy period"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "session.execution.succeeded")
            .count(),
        1,
        "one terminal"
    );
    assert_eq!(harness.chat.requests().len(), 2, "two model calls");
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
    assert_eq!(pending, 0, "no stranded inbox rows");
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
