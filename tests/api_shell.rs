//! `/api/shell` endpoints over the same session table the shell tool uses.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox::config::Config;
use ox::runner::{Tool, ToolContext, ToolRegistry};
use ox::tool::rtk::RtkRewriter;
use ox::tool::shell::ShellTool;
use salvo::Listener;

/// Boot the server with the real built-in toolset on a free port.
async fn server() -> (String, tempfile::TempDir, BootGuard) {
    // The shell table is process-wide and its event bridge is (re)pointed at
    // the newest bus by every boot, so tests in this binary boot one at a
    // time; the guard keeps the lock until the test ends.
    static BOOT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = BootGuard {
        _lock: BOOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port")
        .local_addr()
        .expect("addr")
        .port();
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        db_path: dir.path().join("ox.db"),
        listen: format!("127.0.0.1:{port}"),
        ..Config::default()
    };
    let state = ox::build_state_with(config.clone(), chat(), ToolRegistry::with_builtins())
        .await
        .expect("build state");
    let router = ox::build_router(state);
    let listen = config.listen.clone();
    tokio::spawn(async move {
        let acceptor = salvo::conn::TcpListener::new(listen).bind().await;
        salvo::Server::new(acceptor).serve(router).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client.get(format!("{base}/api/info")).send().await.is_ok() {
            return (base, dir, guard);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

/// Holds the boot lock for the lifetime of one test.
struct BootGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

/// The shell endpoints never call the model, so an empty chat transport is
/// enough. Building one needs the trait in scope; the tests never reach it.
fn chat() -> std::sync::Arc<dyn ox::runner::Chat> {
    std::sync::Arc::new(UnusedChat)
}

struct UnusedChat;

impl ox::runner::Chat for UnusedChat {
    fn stream_chat<'a>(
        &'a self,
        _request: &'a ox::cardinal::wire::ChatRequest,
    ) -> ox::cardinal::chat::ChatFuture<'a> {
        Box::pin(async { Err(ox::cardinal::CardinalError::EarlyClose) })
    }
}

async fn start_session() -> String {
    start_session_in("ses_api", std::env::temp_dir()).await
}

async fn start_session_in(session_id: &str, directory: std::path::PathBuf) -> String {
    let ctx = ToolContext {
        session_id: session_id.to_owned(),
        directory,
        call_id: "call_api".to_owned(),
        cancellation: CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
    let output = tool
        .execute(
            ctx,
            json!({ "command": "echo first; sleep 30", "yield_time_ms": 100 }),
        )
        .await;
    output
        .metadata
        .as_ref()
        .and_then(|metadata| metadata["shellID"].as_str())
        .expect("session id")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_and_pages_a_live_session() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let id = start_session().await;

    let body: Value = client
        .get(format!("{base}/api/shell"))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let sessions = body["data"].as_array().expect("data array");
    assert!(
        sessions.iter().any(|item| item["id"] == id.as_str()),
        "session {id} missing from {body}"
    );
    let location = body["location"]["directory"].as_str().expect("location");
    assert!(!location.is_empty());

    let info: Value = client
        .get(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(info["data"]["id"], id.as_str());
    assert_eq!(info["data"]["status"], "running");
    assert!(info["data"]["command"].is_string());

    // Startup latency: poll the output page with bounded waits until the
    // session has echoed its first line.
    let mut page: Value = serde_json::json!({});
    for _ in 0..50 {
        page = client
            .get(format!("{base}/api/shell/{id}/output?cursor=0&limit=4096"))
            .send()
            .await
            .expect("output")
            .json()
            .await
            .expect("json");
        if page["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("first")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(page["data"]["output"].as_str().unwrap().contains("first"));
    assert!(page["data"]["size"].as_u64().unwrap() >= 6);
    assert_eq!(page["data"]["truncated"], false);

    // Cursor paging: reading from the reported cursor returns no repeats.
    let cursor = page["data"]["cursor"].as_u64().unwrap();
    let next: Value = client
        .get(format!("{base}/api/shell/{id}/output?cursor={cursor}"))
        .send()
        .await
        .expect("output")
        .json()
        .await
        .expect("json");
    assert_eq!(next["data"]["output"], "");
    assert!(next["data"]["cursor"].as_u64().unwrap() >= cursor);

    // Upstream `shell.remove` ends a running shell as killed, then forgets
    // it: 204, and the id is gone afterwards.
    let removed = client
        .delete(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(removed.status(), 204);
    assert!(ox::tool::shell::sessions::shared().get(&id).is_none());
    let gone = client
        .get(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("get");
    assert_eq!(gone.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sessions_answer_404_except_idempotent_delete() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    for path in ["/api/shell/sh_missing", "/api/shell/sh_missing/output"] {
        let response = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), 404, "{path}");
        let body: Value = response.json().await.expect("json");
        assert_eq!(body["_tag"], "ShellNotFoundError");
    }
    // Unlike get/output, upstream `shell.remove` is idempotent: its handler
    // (packages/server/src/handlers/shell.ts, `shell.remove`) maps
    // `Shell.NotFoundError` to `Effect.void` and the endpoint declares no
    // error, so an unknown id answers 204.
    let deleted = client
        .delete(format!("{base}/api/shell/sh_missing"))
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 204);
}

/// Read SSE frames until one satisfies `matches`, bounded by a deadline.
async fn next_frame(
    response: &mut reqwest::Response,
    buffer: &mut String,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        while let Some(boundary) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..boundary + 2).collect();
            if let Ok(event) =
                serde_json::from_str::<Value>(frame.trim().trim_start_matches("data: "))
                && matches(&event)
            {
                return event;
            }
        }
        let Some(chunk) = response.chunk().await.expect("chunk") else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
    panic!("expected SSE frame never arrived");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_started_shells_reach_the_tui_store_over_sse() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = workspace.path().to_string_lossy().into_owned();

    let mut sse = client
        .get(format!("{base}/api/event"))
        .send()
        .await
        .expect("sse");
    assert_eq!(sse.status(), 200);
    let mut buffer = String::new();

    let id = start_session_in("ses_events", workspace.path().to_path_buf()).await;
    let is = |kind: &'static str, id: String| {
        move |event: &Value| {
            event["type"] == kind
                && (event["data"]["id"] == id.as_str()
                    || event["data"]["info"]["id"] == id.as_str())
        }
    };

    // shell.created { info } with the composer's sessionID tag and a location.
    let created = next_frame(&mut sse, &mut buffer, is("shell.created", id.clone())).await;
    assert_eq!(created["location"]["directory"], directory.as_str());
    assert_eq!(created["data"]["info"]["status"], "running");
    assert_eq!(
        created["data"]["info"]["metadata"]["sessionID"],
        "ses_events"
    );

    // The running shell is listed; it carries the same tag.
    let listed: Value = client
        .get(format!("{base}/api/shell"))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    let entry = listed["data"]
        .as_array()
        .expect("array")
        .iter()
        .find(|item| item["id"] == id.as_str())
        .expect("running shell listed");
    assert_eq!(entry["metadata"]["sessionID"], "ses_events");

    // shell.exited { id, exit?, status } once stopped.
    ox::tool::shell::sessions::shared()
        .get(&id)
        .expect("session")
        .stop(true)
        .await;
    let exited = next_frame(&mut sse, &mut buffer, is("shell.exited", id.clone())).await;
    assert_eq!(exited["location"]["directory"], directory.as_str());
    assert_eq!(exited["data"]["status"], "killed");
    assert!(exited["data"].get("info").is_none());

    // A settled shell is no longer listed but is still readable.
    let listed: Value = client
        .get(format!("{base}/api/shell"))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    assert!(
        !listed["data"]
            .as_array()
            .expect("array")
            .iter()
            .any(|item| item["id"] == id.as_str()),
        "settled shell must not be listed: {listed}"
    );
    let got: Value = client
        .get(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_ne!(got["data"]["status"], "running");

    // DELETE announces shell.deleted { id } with the same location.
    let deleted = client
        .delete(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 204);
    let frame = next_frame(&mut sse, &mut buffer, is("shell.deleted", id.clone())).await;
    assert_eq!(frame["location"]["directory"], directory.as_str());
    assert_eq!(frame["data"], json!({ "id": id }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_running_shell_kills_it_and_announces_exited_then_deleted() {
    let (base, _dir, _guard) = server().await;
    let client = reqwest::Client::new();
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = workspace.path().to_string_lossy().into_owned();

    let mut sse = client
        .get(format!("{base}/api/event"))
        .send()
        .await
        .expect("sse");
    assert_eq!(sse.status(), 200);
    let mut buffer = String::new();

    let id = start_session_in("ses_kill", workspace.path().to_path_buf()).await;
    let is = |kind: &'static str, id: String| {
        move |event: &Value| {
            event["type"] == kind
                && (event["data"]["id"] == id.as_str()
                    || event["data"]["info"]["id"] == id.as_str())
        }
    };
    let created = next_frame(&mut sse, &mut buffer, is("shell.created", id.clone())).await;
    assert_eq!(created["location"]["directory"], directory.as_str());

    // The composer's kill key (shell-tab.tsx) calls shell.remove on a
    // running shell; upstream answers 204.
    let removed = client
        .delete(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(removed.status(), 204);

    let exited = next_frame(&mut sse, &mut buffer, is("shell.exited", id.clone())).await;
    assert_eq!(exited["location"]["directory"], directory.as_str());
    assert_eq!(exited["data"], json!({ "id": id, "status": "killed" }));
    let deleted = next_frame(&mut sse, &mut buffer, is("shell.deleted", id.clone())).await;
    assert_eq!(deleted["location"]["directory"], directory.as_str());
    assert_eq!(deleted["data"], json!({ "id": id }));

    // Deleting again is a quiet no-op: no second deleted announcement.
    let again = client
        .delete(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(again.status(), 204);
}
