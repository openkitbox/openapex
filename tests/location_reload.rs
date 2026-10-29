//! `POST /api/location/reload` end to end: a tempdir `ox.toml` is rewritten
//! and re-read, hot keys take effect on the next request, pinned keys keep
//! their running value, failures leave the running services in place, and
//! old generations are freed.

use std::future::poll_fn;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_core::Stream;
use ox::api::AppState;
use ox::cardinal::client::CardinalClient;
use ox::cardinal::router::EndpointRouter;
use ox::config::Config;
use ox::runner::{Services, ToolRegistry};
use salvo::prelude::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

const OK_SSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

const MCP_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
  esac
done
"#;

// ---------------------------------------------------------------------------
// Mock gateway: GET /v1/models and POST /v1/chat/completions, request log.
// ---------------------------------------------------------------------------

struct Gateway {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Gateway {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let request = read_request(&mut socket).await;
                    let response = if request.starts_with("GET /v1/models") {
                        let body = r#"{"data":[{"id":"wire"}]}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        OK_SSE.to_owned()
                    };
                    log.lock().unwrap().push(request);
                    socket.write_all(response.as_bytes()).await.ok();
                    socket.shutdown().await.ok();
                });
            }
        });
        Self {
            addr,
            requests,
            task,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn chat_requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("POST /v1/chat/completions"))
            .cloned()
            .collect()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One HTTP request (head + content-length body) from the socket.
async fn read_request(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while socket.read_exact(&mut byte).await.is_ok() {
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if socket.read_exact(&mut body).await.is_ok() {
        buf.extend_from_slice(&body);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------------------------------------------------------------------------
// App harness
// ---------------------------------------------------------------------------

struct App {
    base: String,
    client: reqwest::Client,
    state: AppState,
    config_path: PathBuf,
    dir: tempfile::TempDir,
    _server: JoinHandle<()>,
}

/// An `ox.toml` body: the gateway as the implicit endpoint plus `extra`.
fn toml(gateway: &Gateway, extra: &str) -> String {
    format!(
        "cardinal_url = \"{}\"\n{extra}\n[magic_context]\nenabled = false\n",
        gateway.url()
    )
}

impl App {
    /// Boot on `initial` (the written `ox.toml`), serving over HTTP.
    async fn start(initial: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("ox.toml");
        std::fs::write(&config_path, initial).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut config = Config::load_from(config_path.clone()).expect("initial config");
        config.db_path = dir.path().join("ox.db");
        config.listen = format!("127.0.0.1:{port}");
        let cardinal = CardinalClient::from_config(&config.llm).unwrap();
        let chat = Arc::new(EndpointRouter::from_config(&config.llm, cardinal).unwrap());
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut state =
            ox::build_state_with_home(config, chat, ToolRegistry::with_builtins(), home)
                .await
                .expect("build state");
        state.config_path = config_path.clone();
        let router = ox::build_router(state.clone());
        let listen = format!("127.0.0.1:{port}");
        let server = tokio::spawn(async move {
            let acceptor = salvo::conn::TcpListener::new(listen).bind().await;
            Server::new(acceptor).serve(router).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let mut ready = false;
        for _ in 0..100 {
            if client.get(format!("{base}/api/info")).send().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "server never became ready");
        Self {
            base,
            client,
            state,
            config_path,
            dir,
            _server: server,
        }
    }

    fn write_config(&self, body: &str) {
        std::fs::write(&self.config_path, body).unwrap();
    }

    async fn reload(&self) -> reqwest::Response {
        self.client
            .post(format!("{}/api/location/reload", self.base))
            .send()
            .await
            .expect("reload request")
    }

    fn generation(&self) -> u64 {
        self.state.live.generation()
    }

    async fn get_json(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post_json(&self, path: &str, body: Value) -> Value {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "POST {path}");
        response.json().await.unwrap_or(Value::Null)
    }
}

/// Poll `cond` for up to 10 s.
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn stub_script(dir: &Path) -> String {
    let path = dir.join("server.sh");
    std::fs::write(&path, MCP_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unchanged_file_reloads_with_204_and_bumps_the_generation() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;
    assert_eq!(app.generation(), 0);

    let response = app.reload().await;

    assert_eq!(response.status(), 204);
    assert_eq!(app.generation(), 1);
}

#[tokio::test]
async fn a_provider_added_to_the_file_receives_the_next_chat_request() {
    let old = Gateway::start().await;
    let new = Gateway::start().await;
    let app = App::start(&toml(&old, "")).await;

    app.write_config(&toml(
        &old,
        &format!(
            "[providers.x]\nbase_url = \"{}\"\n[models.m]\ncandidates = [{{ provider = \"x\", model = \"wire\" }}]\n",
            new.url()
        ),
    ));
    assert_eq!(app.reload().await.status(), 204);

    let created = app
        .post_json(
            "/api/session",
            json!({ "title": "reload", "model": { "id": "m", "providerID": "x" } }),
        )
        .await;
    let session = created["data"]["id"].as_str().expect("session id");
    app.post_json(
        &format!("/api/session/{session}/prompt"),
        json!({ "text": "hello" }),
    )
    .await;

    wait_until("the new gateway to see the chat request", || {
        new.chat_requests()
            .iter()
            .any(|request| request.contains("\"model\":\"wire\""))
    })
    .await;
    assert!(
        old.chat_requests().is_empty(),
        "old gateway must not serve it"
    );
}

#[tokio::test]
async fn invalid_toml_answers_503_and_keeps_the_running_services() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;
    let before = app.state.services();

    app.write_config("this is = [not valid toml");
    let response = app.reload().await;

    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["_tag"], "ServiceUnavailableError");
    assert_eq!(body["service"], "location");
    assert!(body["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert_eq!(app.generation(), 0);
    assert!(Arc::ptr_eq(&before, &app.state.services()));
    let info = app
        .client
        .get(format!("{}/api/info", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(info.status(), 200);
}

#[tokio::test]
async fn pinned_keys_keep_their_running_value() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;
    let listen = app.state.services().config.listen.clone();
    let depth = app.state.services().config.subagent.depth;
    let urls = app.get_json("/api/info").await["urls"].clone();

    // `listen` sits before the first table so it stays a top-level key.
    app.write_config(&format!(
        "listen = \"127.0.0.1:1\"\n{}\n[subagent]\ndepth = {}\n",
        toml(&gateway, ""),
        depth + 5
    ));
    assert_eq!(app.reload().await.status(), 204);

    let config = app.state.services().config.clone();
    assert_eq!(config.listen, listen);
    assert_eq!(config.subagent.depth, depth);
    assert_eq!(config.db_path, app.dir.path().join("ox.db"));
    assert_eq!(app.get_json("/api/info").await["urls"], urls);
    assert_eq!(app.generation(), 1);
}

#[tokio::test]
async fn refresh_frames_reach_event_subscribers() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;

    let response = reqwest::get(format!("{}/api/event", app.base))
        .await
        .unwrap();
    let mut body = response.bytes_stream();
    let mut buffer = String::new();
    let connected = next_event(&mut buffer, &mut body).await;
    assert_eq!(connected["type"], "server.connected");

    assert_eq!(app.reload().await.status(), 204);

    let mut seen = Vec::new();
    while !["location.shutdown", "provider.updated", "model.updated"]
        .iter()
        .all(|kind| seen.iter().any(|seen: &String| seen == kind))
    {
        let event = next_event(&mut buffer, &mut body).await;
        seen.push(event["type"].as_str().unwrap_or_default().to_owned());
    }
}

/// The next `data:` frame of an SSE body as JSON; fails after 10 s of silence.
async fn next_event(
    buffer: &mut String,
    body: &mut (impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
) -> Value {
    loop {
        if let Some(position) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..position + 2).collect();
            if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
                return serde_json::from_str(data).expect("frame JSON");
            }
            continue;
        }
        let chunk = tokio::time::timeout(
            Duration::from_secs(10),
            poll_fn(|cx| Pin::new(&mut *body).poll_next(cx)),
        )
        .await
        .expect("SSE frame within 10s")
        .expect("SSE stream open")
        .expect("SSE chunk");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

#[tokio::test]
async fn mcp_servers_follow_the_file_and_leave_builtins_alone() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;
    assert!(app.state.services().tools.get("read").is_some());

    let script = stub_script(app.dir.path());
    app.write_config(&toml(
        &gateway,
        &format!(
            "[mcp.servers.stub]\ntype = \"local\"\ncommand = [\"{script}\"]\nprotocol = \"legacy\"\n"
        ),
    ));
    assert_eq!(app.reload().await.status(), 204);

    let mcp = app.get_json("/api/mcp").await;
    assert_eq!(
        mcp["data"],
        json!([{ "name": "stub", "status": { "status": "connected" } }])
    );
    assert!(app.state.services().tools.get("stub_echo").is_some());
    let old_services: Weak<Services> = Arc::downgrade(&app.state.services());
    let old_hub = Arc::downgrade(&app.state.services().mcp);

    app.write_config(&toml(&gateway, ""));
    assert_eq!(app.reload().await.status(), 204);

    let services = app.state.services();
    assert!(services.tools.get("stub_echo").is_none());
    assert!(services.tools.get("read").is_some());
    assert_eq!(app.get_json("/api/mcp").await["data"], json!([]));
    drop(services);
    wait_until("the old generation to be freed", || {
        old_services.upgrade().is_none() && old_hub.upgrade().is_none()
    })
    .await;
}

#[tokio::test]
async fn twenty_reloads_leave_no_old_generation_alive() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;
    let first: Weak<Services> = Arc::downgrade(&app.state.services());

    for _ in 0..20 {
        assert_eq!(app.reload().await.status(), 204);
    }

    assert_eq!(app.generation(), 20);
    wait_until("generation 0 to be freed", || first.upgrade().is_none()).await;
}

#[tokio::test]
async fn concurrent_reloads_serialize() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;

    let (a, b) = tokio::join!(app.reload(), app.reload());

    assert_eq!(a.status(), 204);
    assert_eq!(b.status(), 204);
    assert_eq!(app.generation(), 2);
}

#[tokio::test]
async fn parse_errors_answer_503_without_echoing_the_source_line() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;

    app.write_config("cardinal_key = \"sk-SECRET-TOKEN\" oops\n");
    let response = app.reload().await;

    assert_eq!(response.status(), 503);
    let text = response.text().await.unwrap();
    assert!(!text.contains("sk-SECRET-TOKEN"), "leaked: {text}");
    assert!(text.contains("ox.toml"), "{text}");
    assert!(text.contains("line 1"), "{text}");
    assert_eq!(app.generation(), 0);
}

#[tokio::test]
async fn every_endpoint_down_answers_503_and_keeps_the_generation() {
    let gateway = Gateway::start().await;
    let app = App::start(&toml(&gateway, "")).await;
    let before = app.state.services();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = closed.local_addr().unwrap();
    drop(closed);

    app.write_config(&format!(
        "cardinal_url = \"http://{dead}\"\n[magic_context]\nenabled = false\n"
    ));
    let response = app.reload().await;

    assert_eq!(response.status(), 503);
    assert_eq!(app.generation(), 0);
    assert!(Arc::ptr_eq(&before, &app.state.services()));
}
