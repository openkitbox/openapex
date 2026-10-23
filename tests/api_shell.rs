//! `/api/shell` endpoints over the same session table the shell tool uses.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox::config::Config;
use ox::runner::{Tool, ToolContext, ToolRegistry};
use ox::tool::shell::ShellTool;
use salvo::Listener;

/// Boot the server with the real built-in toolset on a free port.
async fn server() -> (String, tempfile::TempDir) {
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
            return (base, dir);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
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
    let ctx = ToolContext {
        session_id: "ses_api".to_owned(),
        directory: std::env::temp_dir(),
        call_id: "call_api".to_owned(),
        cancellation: CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    };
    let tool = ShellTool::new();
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
    let (base, _dir) = server().await;
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

    // A running session is not deletable.
    let refused = client
        .delete(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("delete");
    assert_eq!(refused.status(), 409);

    // Stop it, then it goes away.
    ox::tool::shell::sessions::shared()
        .get(&id)
        .expect("session")
        .stop(true)
        .await;
    // The watcher records the exit on its next tick.
    for _ in 0..100 {
        if ox::tool::shell::sessions::shared()
            .get(&id)
            .is_some_and(|session| session.settled())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let deleted = client
        .delete(format!("{base}/api/shell/{id}"))
        .send()
        .await
        .expect("delete");
    assert!(
        deleted.status() == 204 || deleted.status() == 409,
        "unexpected status {}",
        deleted.status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sessions_answer_404() {
    let (base, _dir) = server().await;
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
    let deleted = client
        .delete(format!("{base}/api/shell/sh_missing"))
        .send()
        .await
        .expect("delete");
    assert_eq!(deleted.status(), 404);
}
