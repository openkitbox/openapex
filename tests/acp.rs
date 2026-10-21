//! `ox acp` integration tests: drive the real binary over stdio NDJSON.
//!
//! Three scenarios:
//! 1. own-mode `initialize` — no server at the probed URL, so the translator
//!    boots one in-process on an ephemeral port (stderr logs stay off stdout);
//! 2. attach-mode `initialize` — a real ox server answers, the translator
//!    attaches and answers identically without binding a new port;
//! 3. a full scripted session — `initialize` → `session/new` →
//!    `session/prompt` against an ox server backed by a mock cardinal, which
//!    proves streaming `session/update` notifications and an `end_turn`
//!    settlement end to end.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

/// One NDJSON line into the child's stdin, flushed.
fn send(child: &mut Child, line: &str) {
    let stdin = child.stdin.as_mut().expect("child stdin");
    stdin.write_all(line.as_bytes()).expect("write line");
    stdin.write_all(b"\n").expect("write newline");
    stdin.flush().expect("flush stdin");
}

/// Read one NDJSON frame with a hard timeout.
fn receive(frames: &Receiver<String>) -> Value {
    receive_timeout(frames, Duration::from_secs(30)).expect("timed out waiting for an ACP frame")
}

/// Read one NDJSON frame with a custom timeout; `None` on timeout/close.
fn receive_timeout(frames: &Receiver<String>, timeout: Duration) -> Option<Value> {
    let line = frames.recv_timeout(timeout).ok()?;
    match serde_json::from_str(line.trim()) {
        Ok(value) => Some(value),
        Err(error) => panic!("undecodable ACP frame `{line}`: {error}"),
    }
}

/// Spawn `ox acp` with a custom environment; a dedicated thread pumps the
/// child's stdout lines into a channel (stdout is the NDJSON wire).
fn spawn_acp(envs: &[(&str, &str)]) -> (Child, Receiver<String>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencode-rs"));
    command.arg("acp").envs(envs.iter().copied());
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ox acp");
    let stdout = child.stdout.take().expect("child stdout");
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if sender.send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (child, receiver)
}

fn initialize(child: &mut Child, frames: &Receiver<String>) -> Value {
    send(
        child,
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}"#,
    );
    let response = receive(frames);
    assert_eq!(response["id"], 0);
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert_eq!(response["result"]["agentInfo"]["name"], "ox");
    assert_eq!(response["result"]["agentCapabilities"]["loadSession"], true);
    response
}

/// A temp dir for the isolated ox.db.
fn temp_db() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// A stub gateway: `/v1/models` serves `models_body`; `/v1/chat/completions`
/// answers a minimal streaming reply (one content delta, a usage-carrying
/// finish chunk, `[DONE]`).
async fn spawn_mock_cardinal(models_body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read request head, then content-length body.
                let mut head_bytes = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    if socket.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head_bytes.push(byte[0]);
                    if head_bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&head_bytes).into_owned();
                let content_length: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; content_length];
                if content_length > 0 && socket.read_exact(&mut body).await.is_err() {
                    return;
                }

                if head.starts_with("GET /v1/models") {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{models_body}",
                        models_body.len()
                    );
                    socket.write_all(response.as_bytes()).await.ok();
                    return;
                }
                let sse = concat!(
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好的\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n",
                    "data: [DONE]\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{sse}"
                );
                socket.write_all(response.as_bytes()).await.ok();
            });
        }
    });
    format!("http://{addr}")
}

/// Spawn a real ox server on `listen` with an isolated database.
fn spawn_ox(listen: &str, db: &str, cardinal: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_opencode-rs"));
    command
        .env("OX_LISTEN", listen)
        .env("OX_DB_PATH", db)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cardinal) = cardinal {
        command
            .env("OX_CARDINAL_URL", cardinal)
            .env("OX_CARDINAL_KEY", "test")
            .env("OX_DEFAULT_MODEL", "cardinal/k3-256k");
    }
    command.spawn().expect("spawn ox server")
}

/// Kill a child and reap it (no zombies, clippy-clean).
fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait until `GET /api/info` answers on `base`.
async fn wait_ready(base: &str) {
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if client.get(format!("{base}/api/info")).send().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server at {base} never became ready");
}

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn initialize_answers_in_own_mode_and_logs_off_stdout() {
    let dir = temp_db();
    let db = dir.path().join("ox.db").display().to_string();
    // Nothing listens on the probed port: the translator owns a server.
    let (mut child, frames) = spawn_acp(&[
        ("OX_LISTEN", "127.0.0.1:59997"),
        ("OX_DB_PATH", db.as_str()),
    ]);
    let response = initialize(&mut child, &frames);
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );
    // stdin drop ends the loop; the owned server dies with the process.
    drop(child.stdin.take());
    let status = child.wait().expect("wait for acp");
    assert!(status.success(), "acp exited {status}");
}

#[tokio::test]
async fn initialize_attaches_to_a_running_server_without_binding() {
    let dir = temp_db();
    let db = dir.path().join("ox.db").display().to_string();
    let listen = format!("127.0.0.1:{}", free_port().await);
    let server = spawn_ox(&listen, &db, None);
    wait_ready(&format!("http://{listen}")).await;

    let (mut child, frames) =
        spawn_acp(&[("OX_LISTEN", listen.as_str()), ("OX_DB_PATH", db.as_str())]);
    initialize(&mut child, &frames);
    drop(child.stdin.take());
    let status = child.wait().expect("wait for acp");
    assert!(status.success(), "acp exited {status}");
    stop(server);
}

// The mock cardinal lives on this test's runtime; the frame pump blocks the
// calling thread, so the test needs a second worker to keep serving it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_session_streams_chunks_and_settles_end_turn() {
    let dir = temp_db();
    let db = dir.path().join("ox.db").display().to_string();
    let cardinal = spawn_mock_cardinal(
        r#"{"data":[{"id":"k3-256k","owned_by":"cardinal","context_length":262144}]}"#,
    )
    .await;
    let listen = format!("127.0.0.1:{}", free_port().await);
    let server = spawn_ox(&listen, &db, Some(&cardinal));
    wait_ready(&format!("http://{listen}")).await;

    let (mut child, frames) =
        spawn_acp(&[("OX_LISTEN", listen.as_str()), ("OX_DB_PATH", db.as_str())]);
    initialize(&mut child, &frames);

    // session/new — the editor's cwd carries into the ox session.
    send(
        &mut child,
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    let new_session = receive(&frames);
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("no sessionId in {new_session}"))
        .to_owned();
    assert!(session_id.starts_with("ses_"), "{new_session}");

    // session/prompt — chunk updates stream, then an end_turn settlement.
    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"{session_id}","prompt":[{{"type":"text","text":"只回复两个字：好的"}}]}}}}"#
    );
    send(&mut child, &prompt);

    let mut chunks = 0usize;
    let mut settled = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !settled {
        let Some(frame) = receive_timeout(&frames, Duration::from_secs(5)) else {
            break;
        };
        if frame["method"] == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            chunks += 1;
        }
        if frame["id"] == 2 {
            settled = true;
            assert_eq!(
                frame["result"]["stopReason"], "end_turn",
                "prompt settlement: {frame}"
            );
        }
    }
    assert!(settled, "session/prompt never settled");
    assert!(chunks > 0, "no agent_message_chunk updates streamed");

    // Editor threads and ox sessions are one-to-one in the shared store.
    let client = reqwest::Client::new();
    let listed: Value = client
        .get(format!("http://{listen}/api/session/{}", session_id))
        .header("x-opencode-directory", "/tmp")
        .send()
        .await
        .expect("fetch session")
        .json()
        .await
        .expect("session json");
    assert_eq!(
        listed["data"]["id"].as_str(),
        Some(session_id.as_str()),
        "{listed}"
    );

    drop(child.stdin.take());
    let _ = child.wait();
    stop(server);
}
