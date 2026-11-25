//! End-to-end V2 permissions: real HTTP turns drive the real gate, the SSE
//! event route exposes its ephemeral frames, and the mock LLM only supplies
//! tool calls — no gate or runner internals are called to approve anything.

use std::future::poll_fn;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ox_core::config::{
    CandidateConfig, Config, ModelRouteConfig, Permission, PermissionsConfig, ProviderConfig,
};
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_llm::chat_completions::router::EndpointRouter;
use ox_schema::event::EventEnvelope;
use ox_schema::permission::{Effect, Rule};
use salvo::http::Body;
use salvo::http::ResBody;
use salvo::http::StatusCode;
use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ox_server::api::AppState;
use ox_server::{build_router, build_state_on_with_bus, project, runner};

const TOOL_CALL_ID: &str = "call_e2e";

const TEXT_CHUNKS: &str = concat!(
    "data: {\"id\":\"chatcmpl-e2e\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-e2e\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{}, \"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

const MODEL_LIST: &str = r#"{"data":[{"id":"mock","owned_by":"mock"}]}"#;

/// The first turn asks for one scripted tool call; any turn carrying a tool
/// result finishes the conversation. Every request body is retained so tests
/// can prove the model actually saw (or never saw) a tool result.
fn tool_chunks(tool: &str, args: &Value) -> String {
    let arguments = serde_json::to_string(args).expect("serialize scripted tool arguments");
    let call = json!({
        "id": "chatcmpl-e2e",
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
                    "function": { "name": tool, "arguments": arguments }
                }]
            },
            "finish_reason": null
        }]
    });
    let finish = json!({
        "id": "chatcmpl-e2e",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
    });
    let usage = json!({
        "id": "chatcmpl-e2e",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock",
        "choices": [],
        "usage": { "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12 }
    });
    format!("data: {call}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
}

async fn spawn_llm(tool: &str, args: &Value) -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local mock LLM endpoint");
    let address = listener.local_addr().expect("mock LLM address");
    let scripted = tool_chunks(tool, args);
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

fn ask_shell() -> PermissionsConfig {
    PermissionsConfig {
        default: Permission::Allow,
        rules: vec![Rule {
            action: "shell".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Ask,
        }],
        ..PermissionsConfig::default()
    }
}

struct Fixture {
    state: AppState,
    router: Arc<Router>,
    bus: EventBus,
    directory: String,
    project_id: String,
    home: PathBuf,
    work: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    llm: JoinHandle<()>,
}

impl Fixture {
    async fn new(tool: &str, args: Value, permissions: PermissionsConfig) -> Self {
        let (base_url, requests, llm) = spawn_llm(tool, &args).await;
        let home =
            std::env::temp_dir().join(format!("ox-permission-e2e-{}", ox_schema::id::id("t")));
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
        let project_id = project::resolve_project(&state.db, std::path::Path::new(&directory))
            .await
            .expect("resolve requested directory")
            .id;
        let router = Arc::new(build_router(state.clone()));
        Self {
            state,
            router,
            bus,
            directory,
            project_id,
            home,
            work,
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
            .json(&json!({ "id": session_id, "title": "permission e2e" }))
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
                    "text": format!("run the tool in {session_id}")
                }))
                .send(Arc::clone(&self.router))
                .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "prompt for {session_id} must be admitted"
        );
    }

    async fn asked_on_bus(&self, session_id: &str) -> Value {
        let mut events = self.bus.subscribe();
        self.prompt(session_id).await;
        let mut seen = Vec::new();
        let asked = timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.expect("event bus is open");
                seen.push(format!("{} {}", event.kind, event.data));
                if event.kind == "permission.asked" && event.data["sessionID"] == session_id {
                    return event.data;
                }
            }
        })
        .await;
        match asked {
            Ok(data) => data,
            Err(_) => {
                let requests = self.requests.lock().expect("capture lock");
                panic!(
                    "permission.asked never arrived for {session_id}; seen={seen:?}; requests={requests:?}"
                );
            }
        }
    }

    async fn subscribe_sse(&self) -> Response {
        let response = TestClient::get(self.location_url("/api/event"))
            .query("location[directory]", &self.directory)
            .send(Arc::clone(&self.router))
            .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "SSE route opens"
        );
        response
    }

    async fn reply(
        &self,
        session_id: &str,
        request_id: &str,
        decision: &str,
        message: Option<&str>,
    ) {
        let mut payload = json!({ "decision": decision });
        if let Some(message) = message {
            payload["message"] = json!(message);
        }
        let response = TestClient::post(self.location_url(&format!(
            "/api/session/{session_id}/permission/{request_id}/reply"
        )))
        .query("location[directory]", &self.directory)
        .json(&payload)
        .send(Arc::clone(&self.router))
        .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::NO_CONTENT),
            "reply `{decision}` for {request_id} must settle over HTTP"
        );
        assert!(
            self.state
                .runner
                .permission_service()
                .pending_one(request_id)
                .is_none(),
            "a settled request leaves the shared gate"
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
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "session log opens"
        );
        let body = timeout(Duration::from_secs(5), response.take_string())
            .await
            .expect("finite log ends within the bounded wait")
            .expect("finite log body");
        let frames: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).expect("SSE frame is JSON"))
            .collect();
        assert!(
            frames.iter().any(|frame| frame.get("durable").is_some()),
            "the log route must return durable frames for {session_id}"
        );
        frames
    }

    fn tool_result_request(&self) -> String {
        let requests = self.requests.lock().expect("capture lock");
        requests
            .iter()
            .find(|body| body.contains("\"role\":\"tool\""))
            .cloned()
            .expect("the model received a tool result")
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

/// A wire-level check: the request that carries a tool result still has one
/// result for every call the assistant made, with the original ids. The
/// aggregate output budget may replace content, never pairing.
fn assert_wire_tool_pairing(body: &str) {
    let request: Value = serde_json::from_str(body).expect("captured request is JSON");
    let messages = request["messages"].as_array().expect("wire messages");
    let mut pending: Vec<&str> = Vec::new();
    for message in messages {
        match message["role"].as_str() {
            Some("assistant") => {
                if let Some(calls) = message["tool_calls"].as_array() {
                    pending.extend(calls.iter().filter_map(|call| call["id"].as_str()));
                }
            }
            Some("tool") => {
                let id = message["tool_call_id"]
                    .as_str()
                    .expect("tool result keeps its call id");
                assert!(
                    pending.contains(&id),
                    "tool result {id} answers a prior call"
                );
                pending.retain(|earlier| *earlier != id);
            }
            _ => {}
        }
    }
    assert!(
        pending.is_empty(),
        "every assistant call still has its tool result after the aggregate budget pass"
    );
}

async fn sse_frame(body: &mut ResBody) -> Value {
    let frame = timeout(
        Duration::from_secs(10),
        poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)),
    )
    .await
    .expect("SSE frame arrives within the bounded wait")
    .expect("SSE frame exists")
    .expect("SSE frame is valid")
    .into_data()
    .expect("SSE frame carries data");
    let text = String::from_utf8(frame.to_vec()).expect("SSE frame is UTF-8");
    let line = text.strip_prefix("data: ").expect("SSE data frame");
    serde_json::from_str(line.trim()).expect("SSE frame is JSON")
}

async fn sse_kind(body: &mut ResBody, kind: &str) -> Value {
    timeout(Duration::from_secs(10), async {
        loop {
            let frame = sse_frame(body).await;
            if frame["type"] == kind {
                return frame;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{kind} arrives over SSE within the bounded wait"))
}

fn assert_no_permission_frames(frames: &[Value]) {
    for frame in frames {
        let kind = frame["type"].as_str().unwrap_or_default();
        assert!(
            kind != "permission.asked" && kind != "permission.replied",
            "ephemeral permission frame `{kind}` was persisted"
        );
    }
}

fn assert_no_asked_in_broadcast(events: &mut broadcast::Receiver<EventEnvelope>) {
    // The receiver subscribed before the turn; draining it after `wait_idle`
    // is a bounded check, not a fixed sleep.
    while let Ok(event) = events.try_recv() {
        assert_ne!(
            event.kind, "permission.asked",
            "an already-approved call must not ask again"
        );
    }
}

async fn saved_rows(fix: &Fixture) -> Vec<(String, String)> {
    let conn = fix.state.db.acquire().await.expect("database connection");
    let mut stmt = conn
        .prepare("SELECT action, resource FROM permission_saved WHERE project_id = ?1")
        .await
        .expect("prepare saved-row query");
    let mut rows = stmt
        .query((fix.project_id.as_str(),))
        .await
        .expect("query V25 saved rows");
    let mut found = Vec::new();
    while let Some(row) = rows.next().await.expect("read saved row") {
        found.push((
            row.get::<String>(0).expect("action"),
            row.get::<String>(1).expect("resource"),
        ));
    }
    found
}

#[tokio::test]
async fn once_reply_runs_tool_after_sse_ask_without_persisting_frames() {
    let marker = std::env::temp_dir().join(format!("ox-e2e-once-{}", ox_schema::id::id("m")));
    let command = format!("touch {}", marker.display());
    let fix = Fixture::new("shell", json!({ "command": command }), ask_shell()).await;
    fix.create_session("ses_once").await;

    let mut sse = fix.subscribe_sse().await;
    let mut body = sse.take_body();
    let connected = sse_frame(&mut body).await;
    assert_eq!(connected["type"], "server.connected");

    fix.prompt("ses_once").await;
    let asked = sse_kind(&mut body, "permission.asked").await;
    let request_id = asked["data"]["id"]
        .as_str()
        .expect("asked carries an id")
        .to_owned();
    assert_eq!(asked["data"]["sessionID"], json!("ses_once"));
    assert_eq!(asked["data"]["action"], json!("shell"));
    assert!(
        asked.get("durable").is_none(),
        "asked is ephemeral on the wire"
    );
    assert!(
        !marker.exists(),
        "the tool has not run while the ask is pending"
    );

    fix.reply("ses_once", &request_id, "once", None).await;
    let replied = sse_kind(&mut body, "permission.replied").await;
    assert_eq!(replied["data"]["requestID"], json!(request_id));
    assert_eq!(replied["data"]["reply"], json!("once"));
    assert!(
        replied.get("durable").is_none(),
        "replied is ephemeral on the wire"
    );
    fix.settle("ses_once").await;
    drop(body);

    assert!(marker.exists(), "the tool ran after the once approval");
    let result = fix.tool_result_request();
    assert_eq!(result.matches("\"role\":\"tool\"").count(), 1);
    assert!(
        result.contains(TOOL_CALL_ID),
        "the result is paired with its call"
    );
    assert_wire_tool_pairing(&result);
    let frames = fix.durable_frames("ses_once").await;
    assert_no_permission_frames(&frames);
    assert!(
        frames
            .iter()
            .any(|frame| frame["type"] == "session.tool.success"),
        "the completed tool is a durable event"
    );

    fix.finish().await;
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn always_reply_persists_v25_and_suppresses_later_ask() {
    let fix = Fixture::new("shell", json!({ "command": "true" }), ask_shell()).await;
    fix.create_session("ses_first").await;
    fix.create_session("ses_next").await;

    let asked = fix.asked_on_bus("ses_first").await;
    let request_id = asked["id"]
        .as_str()
        .expect("asked carries an id")
        .to_owned();
    fix.reply("ses_first", &request_id, "always", None).await;
    fix.settle("ses_first").await;

    assert!(
        fix.state
            .db
            .table_exists("permission_saved")
            .await
            .expect("V25 table")
    );
    assert_eq!(
        saved_rows(&fix).await,
        vec![("shell".to_owned(), "true".to_owned())],
        "always writes one V25 saved approval in the requested project"
    );

    let mut events = fix.bus.subscribe();
    fix.prompt("ses_next").await;
    fix.settle("ses_next").await;
    assert_no_asked_in_broadcast(&mut events);
    assert!(
        fix.state.runner.permission_service().pending().is_empty(),
        "the second call was allowed without a pending request"
    );
    assert!(fix.tool_result_request().contains(TOOL_CALL_ID));
    assert_no_permission_frames(&fix.durable_frames("ses_next").await);

    fix.finish().await;
}

#[tokio::test]
async fn reject_message_fails_tool_and_reaches_the_model() {
    let marker = std::env::temp_dir().join(format!("ox-e2e-reject-{}", ox_schema::id::id("m")));
    let command = format!("touch {}", marker.display());
    let fix = Fixture::new("shell", json!({ "command": command }), ask_shell()).await;
    fix.create_session("ses_reject").await;

    let asked = fix.asked_on_bus("ses_reject").await;
    let request_id = asked["id"]
        .as_str()
        .expect("asked carries an id")
        .to_owned();
    fix.reply("ses_reject", &request_id, "reject", Some("not now"))
        .await;
    fix.settle("ses_reject").await;

    assert!(!marker.exists(), "a rejected tool never runs");
    let result = fix.tool_result_request();
    assert!(
        result.contains("not now"),
        "the rejection message is returned to the model as a tool error"
    );
    assert!(result.contains("permission.denied"));
    let frames = fix.durable_frames("ses_reject").await;
    assert_no_permission_frames(&frames);
    let failed = frames
        .iter()
        .find(|frame| frame["type"] == "session.tool.failed")
        .expect("a rejected call publishes a ToolFailure");
    assert_eq!(failed["data"]["executed"], json!(false));
    assert_eq!(failed["data"]["error"]["type"], json!("permission.denied"));
    assert!(
        failed["data"]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("not now"))
    );

    fix.finish().await;
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn config_deny_rule_prevents_ask_and_tool_execution() {
    let marker = std::env::temp_dir().join(format!("ox-e2e-deny-{}", ox_schema::id::id("m")));
    let command = format!("touch {}", marker.display());
    let permissions = PermissionsConfig {
        default: Permission::Allow,
        rules: vec![Rule {
            action: "shell".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Deny,
        }],
        ..PermissionsConfig::default()
    };
    let fix = Fixture::new("shell", json!({ "command": command }), permissions).await;
    fix.create_session("ses_deny").await;

    let mut events = fix.bus.subscribe();
    fix.prompt("ses_deny").await;
    fix.settle("ses_deny").await;

    assert_no_asked_in_broadcast(&mut events);
    assert!(!marker.exists(), "a denied tool never runs");
    assert!(
        fix.state.runner.permission_service().pending().is_empty(),
        "a deny rule creates no pending request"
    );
    let result = fix.tool_result_request();
    assert!(result.contains("permission.denied"));
    let frames = fix.durable_frames("ses_deny").await;
    assert_no_permission_frames(&frames);
    assert!(
        frames
            .iter()
            .any(|frame| frame["type"] == "session.tool.failed"),
        "the denial is a durable ToolFailure"
    );

    fix.finish().await;
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn agent_base_rules_ask_for_env_and_allow_ordinary_reads() {
    let plain = Fixture::new(
        "read",
        json!({ "path": "note.txt" }),
        PermissionsConfig::default(),
    )
    .await;
    plain.write_work_file("note.txt", "ordinary notes\n");
    plain.create_session("ses_plain").await;
    let mut events = plain.bus.subscribe();
    plain.prompt("ses_plain").await;
    plain.settle("ses_plain").await;
    assert_no_asked_in_broadcast(&mut events);
    assert!(
        plain.tool_result_request().contains("ordinary notes"),
        "the ordinary read ran and reached the model"
    );
    assert_no_permission_frames(&plain.durable_frames("ses_plain").await);
    plain.finish().await;

    let env = Fixture::new(
        "read",
        json!({ "path": ".env" }),
        PermissionsConfig::default(),
    )
    .await;
    env.write_work_file(".env", "SECRET=1\n");
    env.create_session("ses_env").await;
    env.create_session("ses_saved").await;
    let asked = env.asked_on_bus("ses_env").await;
    assert_eq!(asked["action"], json!("read"));
    assert_eq!(asked["resources"], json!([".env"]));
    assert_eq!(
        asked["save"],
        json!(["*"]),
        "read approvals save a wildcard"
    );
    let request_id = asked["id"]
        .as_str()
        .expect("asked carries an id")
        .to_owned();
    env.reply("ses_env", &request_id, "always", None).await;
    env.settle("ses_env").await;
    assert!(
        saved_rows(&env)
            .await
            .contains(&("read".to_owned(), "*".to_owned())),
        "the wildcard read approval is persisted in V25"
    );

    let mut events = env.bus.subscribe();
    env.prompt("ses_saved").await;
    env.settle("ses_saved").await;
    assert_no_asked_in_broadcast(&mut events);
    assert!(
        env.tool_result_request().contains("SECRET=1"),
        "the saved read still reaches the model"
    );
    assert_no_permission_frames(&env.durable_frames("ses_saved").await);

    env.finish().await;
}
