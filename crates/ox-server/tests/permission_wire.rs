//! HTTP wire tests for the V2 permission group. Every handler runs inside the
//! real salvo router and reads the same `PermissionService` the runner owns;
//! the LLM transport is a local SSE server so an actual tool call produces the
//! pending request the client would see.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ox_core::config::{
    CandidateConfig, Config, ModelRouteConfig, Permission, PermissionsConfig, ProviderConfig,
};
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_llm::chat_completions::router::EndpointRouter;
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
use ox_server::{build_router, build_state_on_with_bus, project, runner};

const TOOL_CHUNKS: &str = concat!(
    "data: {\"id\":\"chatcmpl-wire\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_wire\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"true\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-wire\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-wire\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n",
    "data: [DONE]\n\n",
);

const TEXT_CHUNKS: &str = concat!(
    "data: {\"id\":\"chatcmpl-wire\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-wire\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-wire\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"mock\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":2,\"total_tokens\":14}}\n\n",
    "data: [DONE]\n\n",
);

const MODEL_LIST: &str = r#"{"data":[{"id":"mock","owned_by":"mock"}]}"#;

/// A tiny chat-completions SSE endpoint: the first turn of a session asks for
/// `shell`, the follow-up after the tool result finishes the turn.
async fn spawn_llm() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local mock LLM endpoint");
    let address = listener.local_addr().expect("mock LLM address");
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
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
                let chunks = if body.contains("\"role\":\"tool\"") {
                    TEXT_CHUNKS
                } else {
                    TOOL_CHUNKS
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
    (format!("http://{address}"), server)
}

struct Fixture {
    state: AppState,
    router: Arc<Router>,
    bus: EventBus,
    directory: String,
    project_id: String,
    home: PathBuf,
    llm: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let (base_url, llm) = spawn_llm().await;
        let home =
            std::env::temp_dir().join(format!("ox-permission-wire-{}", ox_schema::id::id("t")));
        let work = home.join("work");
        std::fs::create_dir_all(&work).expect("create isolated work directory");
        let directory = work.canonicalize().unwrap_or(work).display().to_string();

        let mut config = Config::default();
        config.magic_context.enabled = false;
        config.magic_context.dreamer_enabled = false;
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
        config.permissions = PermissionsConfig {
            default: Permission::Ask,
            rules: vec![Rule {
                action: "*".to_owned(),
                resource: "*".to_owned(),
                effect: Effect::Ask,
            }],
            ..PermissionsConfig::default()
        };
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
        let project_id = project::resolve_project(&state.db, Path::new(&directory))
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
            llm,
        }
    }

    fn location_url(&self, path: &str) -> String {
        format!("http://localhost{path}")
    }

    async fn create_session(&self, session_id: &str) {
        let response = TestClient::post(self.location_url("/api/session"))
            .query("location[directory]", &self.directory)
            .json(&json!({ "id": session_id, "title": "permission wire" }))
            .send(Arc::clone(&self.router))
            .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "session {session_id} must be created over HTTP"
        );
    }

    /// Drive a real turn until the gate publishes `permission.asked`, then read
    /// the shared pending entry the HTTP handlers must use.
    async fn ask(&self, session_id: &str) -> (String, Value) {
        let mut events = self.bus.subscribe();
        let response =
            TestClient::post(self.location_url(&format!("/api/session/{session_id}/prompt")))
                .query("location[directory]", &self.directory)
                .json(&json!({ "id": format!("msg_{session_id}"), "text": "run the tool" }))
                .send(Arc::clone(&self.router))
                .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::OK),
            "prompt for {session_id} must be admitted"
        );
        let envelope = timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.expect("event bus is open");
                if event.kind == "permission.asked" && event.data["sessionID"] == session_id {
                    return event;
                }
            }
        })
        .await
        .expect("permission.asked arrives within the bounded wait");
        let request_id = envelope.data["id"]
            .as_str()
            .expect("asked carries an id")
            .to_owned();
        let shared = self
            .state
            .runner
            .permission_service()
            .pending_one(&request_id)
            .expect("the shared gate owns the pending request");
        let expected = serde_json::to_value(shared.as_ref()).expect("serialize shared request");
        (request_id, expected)
    }

    async fn settle(&self, session_id: &str) {
        timeout(
            Duration::from_secs(10),
            self.state.runner.wait_idle(session_id),
        )
        .await
        .expect("the turn settles within the bounded wait");
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

fn assert_header(response: &Response, directory: &str) {
    assert_eq!(
        response
            .headers()
            .get("x-opencode-directory")
            .and_then(|value| value.to_str().ok()),
        Some(directory),
        "permission endpoints echo the requested directory"
    );
}

fn json_field(body: &Value, field: &str) -> String {
    body.get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("body is missing {field}: {body}"))
        .to_owned()
}

#[tokio::test]
async fn permission_v2_http_endpoints_match_upstream_group() -> anyhow::Result<()> {
    let fix = Fixture::new().await;
    let service = fix.state.runner.permission_service();
    let store = service.saved();

    // --- saved list, filtered by the requested location's project -----------
    store
        .save(&fix.project_id, "shell", &["echo first".to_owned()])
        .await?;
    store
        .save(&fix.project_id, "edit", &["/tmp/file".to_owned()])
        .await?;
    store
        .save("prj_other", "shell", &["other".to_owned()])
        .await?;
    let saved = store.list(&fix.project_id).await?;
    let other = store.list("prj_other").await?.remove(0);
    let first = saved
        .iter()
        .find(|row| row.action == "shell")
        .expect("first saved row")
        .clone();
    let first_id = first.id.clone();
    let second_id = saved
        .iter()
        .find(|row| row.action == "edit")
        .expect("second saved row")
        .id
        .clone();

    let mut response = TestClient::get(fix.location_url("/api/permission/saved"))
        .add_header("x-opencode-directory", &fix.directory, true)
        .send(Arc::clone(&fix.router))
        .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_header(&response, &fix.directory);
    let body: Value = response.take_json().await.expect("saved list JSON");
    let expected: Vec<Value> = saved
        .iter()
        .map(|row| serde_json::to_value(row).expect("serialize saved info"))
        .collect();
    assert_eq!(
        body["data"].as_array().map(|rows| rows.len()),
        Some(expected.len()),
        "only the requested project's saved rows are listed"
    );
    for row in &expected {
        assert!(
            body["data"]
                .as_array()
                .is_some_and(|rows| rows.contains(row))
        );
    }

    let mut response = TestClient::get(fix.location_url("/api/permission/saved"))
        .query("location[directory]", &fix.directory)
        .query("projectID", "prj_other")
        .send(Arc::clone(&fix.router))
        .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_header(&response, &fix.directory);
    let body: Value = response
        .take_json()
        .await
        .expect("project-filtered saved list");
    assert_eq!(
        body["data"],
        json!([serde_json::to_value(&other).expect("other info")])
    );

    // --- saved PATCH/DELETE --------------------------------------------------
    let mut response =
        TestClient::patch(fix.location_url(&format!("/api/permission/saved/{first_id}")))
            .query("location[directory]", &fix.directory)
            .json(&json!({ "resource": "echo second" }))
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_header(&response, &fix.directory);
    let updated: Value = response.take_json().await.expect("saved update JSON");
    assert_eq!(updated["data"]["id"], json!(first_id));
    assert_eq!(updated["data"]["action"], json!("shell"));
    assert_eq!(updated["data"]["resource"], json!("echo second"));
    assert_eq!(
        updated["data"]["time"]["created"],
        json!(first.time.created)
    );
    let stored = store
        .get(&fix.project_id, &first_id)
        .await?
        .expect("updated row remains scoped to the project");
    assert_eq!(stored.resource, "echo second");
    assert_eq!(stored.time.created, first.time.created);

    let response =
        TestClient::patch(fix.location_url(&format!("/api/permission/saved/{second_id}")))
            .query("location[directory]", &fix.directory)
            .json(&json!({ "action": "shell", "resource": "echo second" }))
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(
        response.status_code,
        Some(StatusCode::CONFLICT),
        "a duplicate project/action/resource is a 409"
    );
    assert_header(&response, &fix.directory);
    assert_eq!(
        store
            .get(&fix.project_id, &second_id)
            .await?
            .expect("conflicting row is untouched")
            .action,
        "edit"
    );

    let response = TestClient::patch(fix.location_url("/api/permission/saved/psv_missing"))
        .query("location[directory]", &fix.directory)
        .json(&json!({ "action": "read" }))
        .send(Arc::clone(&fix.router))
        .await;
    assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
    assert_header(&response, &fix.directory);

    let response =
        TestClient::patch(fix.location_url(&format!("/api/permission/saved/{}", other.id)))
            .query("location[directory]", &fix.directory)
            .json(&json!({ "action": "read" }))
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(
        response.status_code,
        Some(StatusCode::NOT_FOUND),
        "an id from another project is not visible here"
    );
    assert!(store.get("prj_other", &other.id).await?.is_some());

    let response =
        TestClient::delete(fix.location_url(&format!("/api/permission/saved/{first_id}")))
            .query("location[directory]", &fix.directory)
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
    assert_header(&response, &fix.directory);
    let response =
        TestClient::delete(fix.location_url(&format!("/api/permission/saved/{first_id}")))
            .query("location[directory]", &fix.directory)
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(
        response.status_code,
        Some(StatusCode::NO_CONTENT),
        "DELETE is idempotent"
    );
    assert!(store.get(&fix.project_id, &first_id).await?.is_none());
    let response =
        TestClient::delete(fix.location_url(&format!("/api/permission/saved/{}", other.id)))
            .query("location[directory]", &fix.directory)
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
    assert!(store.get("prj_other", &other.id).await?.is_some());

    // --- pending list/get read the runner's shared gate ---------------------
    fix.create_session("ses_once").await;
    fix.create_session("ses_always").await;
    fix.create_session("ses_reject").await;
    let (once_id, once_request) = fix.ask("ses_once").await;
    let (always_id, always_request) = fix.ask("ses_always").await;
    let (reject_id, reject_request) = fix.ask("ses_reject").await;
    assert_eq!(service.pending().len(), 3);

    let mut response = TestClient::get(fix.location_url("/api/permission/request"))
        .query("location[directory]", &fix.directory)
        .send(Arc::clone(&fix.router))
        .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_header(&response, &fix.directory);
    let body: Value = response.take_json().await.expect("pending list JSON");
    assert_eq!(
        body["location"],
        json!({ "directory": fix.directory }),
        "Location.response echoes the requested location"
    );
    let mut expected: Vec<Value> = service
        .pending()
        .iter()
        .map(|row| serde_json::to_value(row.as_ref()).expect("serialize pending"))
        .collect();
    expected.sort_by_key(|row| json_field(row, "id"));
    let mut actual = body["data"].as_array().expect("pending data").clone();
    actual.sort_by_key(|row| json_field(row, "id"));
    assert_eq!(
        actual, expected,
        "global list uses the shared pending handles"
    );

    for (session_id, request_id, expected) in [
        ("ses_once", &once_id, &once_request),
        ("ses_always", &always_id, &always_request),
        ("ses_reject", &reject_id, &reject_request),
    ] {
        let mut response =
            TestClient::get(fix.location_url(&format!("/api/session/{session_id}/permission")))
                .query("location[directory]", &fix.directory)
                .send(Arc::clone(&fix.router))
                .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_header(&response, &fix.directory);
        let body: Value = response.take_json().await.expect("session pending list");
        assert_eq!(body["data"], json!([expected]));

        let mut response = TestClient::get(fix.location_url(&format!(
            "/api/session/{session_id}/permission/{request_id}"
        )))
        .query("location[directory]", &fix.directory)
        .send(Arc::clone(&fix.router))
        .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_header(&response, &fix.directory);
        let body: Value = response.take_json().await.expect("session pending get");
        assert_eq!(body["data"], json!(expected));
    }

    // --- reply: three decisions, wrong owner, missing id, duplicate ----------
    let response = TestClient::post(fix.location_url(&format!(
        "/api/session/ses_always/permission/{once_id}/reply"
    )))
    .query("location[directory]", &fix.directory)
    .json(&json!({ "decision": "once" }))
    .send(Arc::clone(&fix.router))
    .await;
    assert_eq!(
        response.status_code,
        Some(StatusCode::NOT_FOUND),
        "a request from another session is not owned by this session"
    );
    assert_header(&response, &fix.directory);

    let response =
        TestClient::post(fix.location_url("/api/session/ses_once/permission/per_missing/reply"))
            .query("location[directory]", &fix.directory)
            .json(&json!({ "decision": "once" }))
            .send(Arc::clone(&fix.router))
            .await;
    assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
    assert_header(&response, &fix.directory);

    let response = TestClient::post(fix.location_url("/api/session/ses_missing/permission/reply"))
        .query("location[directory]", &fix.directory)
        .send(Arc::clone(&fix.router))
        .await;
    assert_ne!(
        response.status_code,
        Some(StatusCode::NO_CONTENT),
        "an unmatched reply route must not settle anything"
    );

    for (session_id, request_id, decision, message) in [
        ("ses_once", &once_id, "once", None),
        ("ses_always", &always_id, "always", None),
        ("ses_reject", &reject_id, "reject", Some("not now")),
    ] {
        let mut payload = json!({ "decision": decision });
        if let Some(message) = message {
            payload["message"] = json!(message);
        }
        let response = TestClient::post(fix.location_url(&format!(
            "/api/session/{session_id}/permission/{request_id}/reply"
        )))
        .query("location[directory]", &fix.directory)
        .json(&payload)
        .send(Arc::clone(&fix.router))
        .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::NO_CONTENT),
            "decision `{decision}` must settle its request"
        );
        assert_header(&response, &fix.directory);

        let response = TestClient::post(fix.location_url(&format!(
            "/api/session/{session_id}/permission/{request_id}/reply"
        )))
        .query("location[directory]", &fix.directory)
        .json(&payload)
        .send(Arc::clone(&fix.router))
        .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::NOT_FOUND),
            "a second reply to the same request is a 404"
        );
        assert_header(&response, &fix.directory);
        assert!(service.pending_one(request_id).is_none());
        fix.settle(session_id).await;
    }

    assert!(
        store
            .list(&fix.project_id)
            .await?
            .iter()
            .any(|row| row.action == "shell" && row.resource == "true"),
        "an `always` reply persists an allow in the requested project"
    );
    fix.finish().await;
    Ok(())
}
