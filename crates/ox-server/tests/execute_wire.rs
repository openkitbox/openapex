//! End-to-end wire shape for the model-facing `execute` tool. A real HTTP turn
//! drives the runner, a local SSE endpoint supplies the tool call, and the only
//! observation points are the event bus, the model request and HTTP routes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ox_core::config::{
    CandidateConfig, Config, ModelRouteConfig, Permission, PermissionsConfig, ProviderConfig,
};
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_llm::chat_completions::EndpointRouter;
use ox_schema::event::EventEnvelope;
use ox_schema::permission::{Effect, Rule};
use salvo::http::StatusCode;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ox_server::api::AppState;
use ox_server::{build_router, build_state_on_with_bus, runner};

const TOOL_CALL_ID: &str = "call_execute";
const MODEL_LIST: &str = r#"{"data":[{"id":"mock","owned_by":"mock"}]}"#;

const TEXT_CHUNKS: &str = concat!(
    "data: {\"id\":\"chatcmpl-execute\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-execute\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// The first turn asks for `execute`; any turn carrying a tool result finishes.
/// Request bodies are retained so the test can inspect the model-visible call.
fn tool_chunks(code: &str) -> String {
    let call = json!({
        "id": "chatcmpl-execute",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": TOOL_CALL_ID,
                    "type": "function",
                    "function": { "name": "execute", "arguments": json!({ "code": code }).to_string() }
                }]
            },
            "finish_reason": null
        }]
    });
    let finish = json!({
        "id": "chatcmpl-execute",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
    });
    let usage = json!({
        "id": "chatcmpl-execute",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [],
        "usage": { "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12 }
    });
    format!("data: {call}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
}

async fn spawn_llm(code: &str) -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local mock LLM endpoint");
    let address = listener.local_addr().expect("mock LLM address");
    let scripted = tool_chunks(code);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let scripted = scripted.clone();
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let body_start = loop {
                    let read = stream.read(&mut chunk).await.expect("read LLM request");
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buffer[..body_start]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or_default();
                if buffer.starts_with(b"GET /v1/models") {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        MODEL_LIST.len(),
                        MODEL_LIST
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write model list");
                    let _ = stream.shutdown().await;
                    return;
                }
                while buffer.len() < body_start + content_length {
                    let read = stream.read(&mut chunk).await.expect("read LLM body");
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let body =
                    String::from_utf8_lossy(&buffer[body_start..body_start + content_length]);
                captured
                    .lock()
                    .expect("capture lock")
                    .push(body.to_string());
                let chunks = if body.contains("\"role\":\"tool\"") {
                    TEXT_CHUNKS.to_owned()
                } else {
                    scripted
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    chunks.len(),
                    chunks
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write LLM response");
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{address}"), requests, server)
}

/// `grep` is gated under the `read` permission action (the gate extracts the
/// search pattern as a read resource), so denying `read` denies the script's
/// nested `grep` call.
fn permissions(deny_read: bool) -> PermissionsConfig {
    let rules = if deny_read {
        vec![Rule {
            action: "read".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Deny,
        }]
    } else {
        Vec::new()
    };
    PermissionsConfig {
        default: Permission::Allow,
        rules,
        ..PermissionsConfig::default()
    }
}

struct Fixture {
    state: AppState,
    router: Arc<Router>,
    bus: EventBus,
    directory: String,
    work: PathBuf,
    home: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    llm: JoinHandle<()>,
}

impl Fixture {
    async fn new(code: &str, permissions: PermissionsConfig) -> Self {
        let (base_url, requests, llm) = spawn_llm(code).await;
        let home = std::env::temp_dir().join(format!("ox-execute-wire-{}", ox_schema::id::id("t")));
        let work = home.join("work");
        std::fs::create_dir_all(&work).expect("create isolated work directory");
        let directory = work
            .canonicalize()
            .unwrap_or_else(|_| work.clone())
            .display()
            .to_string();

        let mut config = Config::default();
        config.magic_context.enabled = false;
        config.magic_context.dreamer_enabled = false;
        config.event_heartbeat_ms = 60_000;
        config.llm.providers.insert(
            "mock".to_owned(),
            ProviderConfig {
                base_url,
                api_key: None,
                protocol: None,
                limits: Default::default(),
            },
        );
        config.llm.models.insert(
            "mock".to_owned(),
            ModelRouteConfig {
                candidates: vec![CandidateConfig {
                    provider: "mock".to_owned(),
                    model: "mock".to_owned(),
                }],
            },
        );
        config.llm.default_model = Some("mock".to_owned());
        config.permissions = permissions;
        let db = Db::open_in_memory().await.expect("in-memory database");
        let bus = EventBus::new(db.clone());
        let chat = Arc::new(EndpointRouter::from_config(&config.llm).expect("mock chat router"))
            as Arc<dyn runner::Chat>;
        let state = build_state_on_with_bus(
            config,
            chat,
            runner::ToolRegistry::with_builtins(),
            db,
            bus.clone(),
            home.join("global"),
        )
        .await
        .expect("test state");
        state
            .live
            .current()
            .model_catalog
            .refresh()
            .await
            .expect("mock model catalogue");
        let router = Arc::new(build_router(state.clone()));
        Self {
            state,
            router,
            bus,
            directory,
            work,
            home,
            requests,
            llm,
        }
    }

    fn location_url(&self, path: &str) -> String {
        format!("http://localhost{path}")
    }

    fn write_work_file(&self, name: &str, content: &str) {
        std::fs::write(self.work.join(name), content).expect("write isolated fixture file");
    }

    async fn create_session(&self, session_id: &str) {
        let response = TestClient::post(self.location_url("/api/session"))
            .query("location[directory]", &self.directory)
            .json(&json!({ "id": session_id, "title": "execute wire" }))
            .send(Arc::clone(&self.router))
            .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "session {session_id} must be created over HTTP"
        );
    }

    async fn prompt(&self, session_id: &str) {
        let response =
            TestClient::post(self.location_url(&format!("/api/session/{session_id}/prompt")))
                .query("location[directory]", &self.directory)
                .json(&json!({
                    "id": format!("msg_{session_id}"),
                    "text": "run the scripted tool"
                }))
                .send(Arc::clone(&self.router))
                .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "prompt for {session_id} must be admitted"
        );
    }

    async fn settle(&self, session_id: &str) {
        timeout(
            Duration::from_secs(10),
            self.state.runner.wait_idle(session_id),
        )
        .await
        .expect("the turn settles within the bounded wait");
    }

    async fn durable_frames(&self, session_id: &str) -> Vec<Value> {
        let mut response = TestClient::get(
            self.location_url(&format!("/api/experimental/session/{session_id}/log")),
        )
        .query("location[directory]", &self.directory)
        .send(Arc::clone(&self.router))
        .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        let body = timeout(Duration::from_secs(5), response.take_string())
            .await
            .expect("finite log ends within the bounded wait")
            .expect("finite log body");
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).expect("SSE frame is JSON"))
            .collect()
    }

    async fn finish(self) {
        self.state.runtime.stop().cancel();
        timeout(
            Duration::from_secs(10),
            self.state.runtime.shutdown(&self.state),
        )
        .await
        .expect("shutdown completes within the bounded wait")
        .expect("shutdown succeeds");
        self.llm.abort();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Read the assistant's tool call out of the real request the mock LLM saw.
fn scripted_code(requests: &[String]) -> String {
    for body in requests {
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            continue;
        };
        let Some(messages) = value["messages"].as_array() else {
            continue;
        };
        for message in messages {
            let Some(calls) = message["tool_calls"].as_array() else {
                continue;
            };
            for call in calls {
                if call["function"]["name"] == json!("execute") {
                    let arguments = call["function"]["arguments"]
                        .as_str()
                        .expect("tool arguments are a string");
                    let input: Value =
                        serde_json::from_str(arguments).expect("tool arguments are JSON");
                    assert!(
                        input["code"].is_string(),
                        "execute input carries code: {input}"
                    );
                    return input["code"].as_str().expect("code is a string").to_owned();
                }
            }
        }
    }
    panic!("the model never received an `execute` tool call: {requests:?}");
}

fn tool_rows(metadata: &Value) -> Vec<Value> {
    let rows = metadata["toolCalls"]
        .as_array()
        .expect("metadata carries a toolCalls array");
    for row in rows {
        assert!(
            row["tool"].is_string(),
            "every toolCalls row names a tool: {row}"
        );
        assert!(
            ["running", "completed", "error"].contains(&row["status"].as_str().unwrap_or_default()),
            "unexpected row status: {row}"
        );
        let unexpected = row
            .as_object()
            .expect("row is an object")
            .keys()
            .filter(|key| {
                key.as_str() != "tool" && key.as_str() != "status" && key.as_str() != "input"
            })
            .collect::<Vec<_>>();
        assert!(
            unexpected.is_empty(),
            "unexpected row fields: {unexpected:?}"
        );
    }
    rows.clone()
}

fn progress_events<'events>(
    events: &'events [EventEnvelope],
    session_id: &str,
) -> Vec<&'events EventEnvelope> {
    events
        .iter()
        .filter(|event| {
            event.kind == "session.tool.progress"
                && event.data["sessionID"] == json!(session_id)
                && event.data["id"] == json!(TOOL_CALL_ID)
        })
        .collect()
}

fn terminal(frame: &Value, kind: &str) -> Value {
    frame["data"]["metadata"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .unwrap_or_else(|| panic!("{kind} carries metadata: {frame}"))
}

fn assert_no_synthetic_codemode(frames: &[Value]) {
    for frame in frames {
        let kind = frame["type"].as_str().unwrap_or_default();
        assert!(
            !kind.contains("codemode") && !kind.contains("job"),
            "no synthetic codemode row was emitted: {kind}"
        );
        assert_ne!(
            frame["data"]["metadata"]["source"],
            json!("codemode"),
            "no Script message carries a codemode source"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_progress_and_terminal_metadata_match_the_wire_shape() {
    let code = concat!(
        "await tools.read({ path: \"first.txt\" });",
        "await tools.read({ path: \"second.txt\" });",
        "return { done: true };"
    );
    let fix = Fixture::new(code, permissions(false)).await;
    fix.write_work_file("first.txt", "one\n");
    fix.write_work_file("second.txt", "two\n");
    fix.create_session("ses_execute").await;

    let mut events = fix.bus.subscribe();
    fix.prompt("ses_execute").await;
    fix.settle("ses_execute").await;
    let mut observed = Vec::new();
    while let Ok(event) = events.try_recv() {
        observed.push(event);
    }

    let requests = fix.requests.lock().expect("capture lock").clone();
    let seen = scripted_code(&requests);
    assert_eq!(seen, code, "the model sees the scripted execute code");
    assert_eq!(
        requests.len(),
        2,
        "the tool result is sent back to the model"
    );

    let progress = progress_events(&observed, "ses_execute");
    assert!(
        !progress.is_empty(),
        "nested tool calls publish live session.tool.progress"
    );
    let mut saw_running = false;
    let mut live_rows = 0;
    for event in &progress {
        assert!(
            event.durable.is_none(),
            "progress stays ephemeral without a durable seq"
        );
        assert!(
            event.data["assistantMessageID"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "progress names its assistant message"
        );
        let metadata = &event.data["metadata"];
        assert!(
            metadata
                .get("files")
                .is_none_or(|files| files == &json!([])),
            "progress never invents a file diff: {metadata}"
        );
        let rows = tool_rows(metadata);
        live_rows = live_rows.max(rows.len());
        saw_running |= rows.iter().any(|row| row["status"] == json!("running"));
    }
    assert!(saw_running, "a nested call is observed while it is running");

    let frames = fix.durable_frames("ses_execute").await;
    assert_no_synthetic_codemode(&frames);
    let success = frames
        .iter()
        .find(|frame| {
            frame["type"] == json!("session.tool.success")
                && frame["data"]["id"] == json!(TOOL_CALL_ID)
        })
        .expect("execute settles as a durable tool success");
    assert!(
        success.get("durable").is_some(),
        "the terminal tool part is durable"
    );
    let metadata = terminal(success, "session.tool.success");
    assert_eq!(metadata["files"], json!([]), "no fabricated diff files");
    assert!(
        metadata.get("error").is_none(),
        "the successful script is not flagged as failed"
    );
    let rows = tool_rows(&metadata);
    assert_eq!(rows.len(), 2, "the terminal row set is complete");
    for (row, path) in rows.iter().zip(["first.txt", "second.txt"]) {
        assert_eq!(row["tool"], json!("read"));
        assert_eq!(row["status"], json!("completed"), "running settles: {row}");
        assert_eq!(row["input"], json!({ "path": path }));
    }
    assert!(
        rows.len() >= live_rows,
        "terminal metadata includes every row seen live"
    );

    fix.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaited_tool_promises_keep_the_execute_wire_shape() {
    let code = concat!(
        "const results = await Promise.all([",
        "tools.read({ path: \"first.txt\" }),",
        "tools.read({ path: \"second.txt\" }),",
        "]);",
        "return { state: { count: results.length } };"
    );
    let fix = Fixture::new(code, permissions(false)).await;
    fix.write_work_file("first.txt", "one\n");
    fix.write_work_file("second.txt", "two\n");
    fix.create_session("ses_await").await;

    let mut events = fix.bus.subscribe();
    fix.prompt("ses_await").await;
    fix.settle("ses_await").await;
    let mut observed = Vec::new();
    while let Ok(event) = events.try_recv() {
        observed.push(event);
    }

    assert_eq!(
        scripted_code(&fix.requests.lock().expect("capture lock").clone()),
        code,
        "the model sees the top-level await script"
    );
    let progress = progress_events(&observed, "ses_await");
    let running = progress
        .iter()
        .flat_map(|event| tool_rows(&event.data["metadata"]))
        .filter(|row| row["status"] == json!("running"))
        .count();
    assert!(
        running >= 1,
        "a promised call is observed running: {progress:?}"
    );

    let frames = fix.durable_frames("ses_await").await;
    assert_no_synthetic_codemode(&frames);
    let success = frames
        .iter()
        .find(|frame| {
            frame["type"] == json!("session.tool.success")
                && frame["data"]["id"] == json!(TOOL_CALL_ID)
        })
        .expect("the awaited script settles execute successfully");
    let rows = tool_rows(&terminal(success, "session.tool.success"));
    assert_eq!(rows.len(), 2, "both awaited calls have terminal rows");
    for row in &rows {
        assert_eq!(row["status"], json!("completed"), "promise settled: {row}");
    }
    let inputs: Vec<&str> = rows
        .iter()
        .map(|row| row["input"]["path"].as_str().unwrap_or_default())
        .collect();
    assert!(
        inputs.contains(&"first.txt") && inputs.contains(&"second.txt"),
        "both awaited calls settle, in either completion order: {inputs:?}"
    );

    fix.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_nested_call_flags_execute_without_failing_the_server() {
    let code = "await tools.grep({ pattern: \"needle\" }); return { done: true };";
    let fix = Fixture::new(code, permissions(true)).await;
    fix.create_session("ses_denied").await;

    let mut events = fix.bus.subscribe();
    fix.prompt("ses_denied").await;
    fix.settle("ses_denied").await;
    let mut observed = Vec::new();
    while let Ok(event) = events.try_recv() {
        observed.push(event);
    }

    let progress = progress_events(&observed, "ses_denied");
    assert!(
        progress.iter().any(|event| {
            tool_rows(&event.data["metadata"])
                .iter()
                .any(|row| row["tool"] == json!("grep") && row["status"] == json!("running"))
        }),
        "the denied nested call is visible as running first"
    );
    assert!(
        !observed
            .iter()
            .any(|event| event.kind == "permission.asked"),
        "a deny rule asks nothing"
    );
    assert!(
        fix.state.runner.permission_service().pending().is_empty(),
        "no pending permission survives the denied nested call"
    );

    let frames = fix.durable_frames("ses_denied").await;
    assert_no_synthetic_codemode(&frames);
    let success = frames
        .iter()
        .find(|frame| {
            frame["type"] == json!("session.tool.success")
                && frame["data"]["id"] == json!(TOOL_CALL_ID)
        })
        .expect("a failed script still settles execute successfully");
    let metadata = terminal(success, "session.tool.success");
    assert_eq!(
        metadata["error"],
        json!(true),
        "the script failure is flagged"
    );
    assert_eq!(metadata["files"], json!([]));
    let rows = tool_rows(&metadata);
    assert_eq!(rows.len(), 1, "the denied call has one terminal row");
    assert_eq!(rows[0]["tool"], json!("grep"));
    assert_eq!(rows[0]["status"], json!("error"));
    assert_eq!(rows[0]["input"], json!({ "pattern": "needle" }));
    assert!(
        !frames
            .iter()
            .any(|frame| frame["type"] == json!("session.tool.failed")),
        "the whole execute call did not fail closed"
    );

    let response = TestClient::get(fix.location_url("/api/permission/saved"))
        .query("location[directory]", &fix.directory)
        .send(Arc::clone(&fix.router))
        .await;
    assert_eq!(
        response.status_code,
        Some(StatusCode::OK),
        "the server still serves HTTP after a denied nested call"
    );

    fix.finish().await;
}
