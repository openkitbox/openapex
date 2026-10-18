//! Integration test: boot the real router on an ephemeral port and hit the
//! read endpoints over HTTP.

use std::net::TcpListener as StdTcpListener;

use ox::api::AppState;
use ox::config::Config;
use salvo::prelude::*;
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// Pick an ephemeral port by binding then releasing a std listener.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn the server in the background against a temp DB.
/// Returns (base_url, handle, tempdir) — the tempdir guard must outlive the
/// server so the DB files are cleaned up on drop.
async fn spawn_server() -> (String, JoinHandle<()>, TempDir) {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ox.db");
    let config = Config {
        db_path,
        listen: format!("127.0.0.1:{port}"),
        ..Config::default()
    };
    let state = ox::build_state(config.clone()).await.unwrap();
    let router = ox::build_router(state);
    let listen: String = config.listen.clone();
    let handle = tokio::spawn(async move {
        let acceptor = TcpListener::new(listen).bind().await;
        Server::new(acceptor).serve(router).await;
    });

    // Poll until the port accepts connections; fail loudly if it never does.
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..50 {
        if client.get(format!("{base}/api/info")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ready, "server at {base} never became ready");
    (base, handle, dir)
}

#[tokio::test]
async fn info_location_project_endpoints_respond() {
    let (base, handle, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // GET /api/info → 200 JSON with a `version` field.
    let info: serde_json::Value = client
        .get(format!("{base}/api/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(info.get("version").and_then(|v| v.as_str()).is_some());
    // Upstream ServerInfo is `additionalProperties: false`: exactly these keys.
    let keys: Vec<&str> = info
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    for key in ["version", "pid", "urls", "paths", "capabilities"] {
        assert!(keys.contains(&key), "missing key {key}");
    }
    assert_eq!(keys.len(), 5, "unexpected extra keys: {keys:?}");
    assert!(info.get("pid").and_then(|v| v.as_u64()).is_some());

    // GET /api/location → 200 JSON with directory + project.
    let location: serde_json::Value = client
        .get(format!("{base}/api/location"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(location.get("directory").and_then(|v| v.as_str()).is_some());
    let project = location.get("project").expect("location.project");
    assert_eq!(project.get("id").and_then(|v| v.as_str()), Some("default"));
    assert!(project.get("canonical").and_then(|v| v.as_str()).is_some());

    // GET /api/project → 200 JSON array with one synthetic project.
    let resp = client
        .get(format!("{base}/api/project"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let projects: serde_json::Value = resp.json().await.unwrap();
    let arr = projects.as_array().expect("project.list returns an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0].get("id").and_then(|v| v.as_str()), Some("default"));
    assert!(arr[0].get("time").is_some());

    // GET /api/agent → 200 envelope with the static `build` agent.
    let agents: serde_json::Value = client
        .get(format!("{base}/api/agent"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = agents
        .get("data")
        .and_then(|v| v.as_array())
        .expect("agent.data");
    assert_eq!(data[0].get("name").and_then(|v| v.as_str()), Some("build"));
    assert_eq!(
        data[0].get("mode").and_then(|v| v.as_str()),
        Some("primary")
    );

    // GET /api/model → 200 envelope (empty static list by default).
    let models: serde_json::Value = client
        .get(format!("{base}/api/model"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models.get("location").is_some());
    assert!(models.get("data").and_then(|v| v.as_array()).is_some());

    handle.abort();
}

#[tokio::test]
async fn project_list_reads_upstream_project_table() {
    // Simulate a shared upstream DB: create the `project` table per
    // upstream's drizzle schema and insert one row.
    use ox::db::Db;

    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ox-shared.db");
    let config = Config {
        db_path,
        listen: format!("127.0.0.1:{port}"),
        ..Config::default()
    };
    let db = Db::open(&config.db_path).await.unwrap();
    db.conn()
        .call(|c| -> rusqlite::Result<()> {
            c.execute_batch(
                "CREATE TABLE project (
                  id text PRIMARY KEY,
                  worktree text NOT NULL,
                  vcs text,
                  name text,
                  icon_url text,
                  icon_url_override text,
                  icon_color text,
                  time_created integer NOT NULL,
                  time_updated integer NOT NULL,
                  time_initialized integer,
                  time_active integer DEFAULT 0 NOT NULL,
                  sandboxes text NOT NULL,
                  commands text
                );
                INSERT INTO project (id, worktree, vcs, name, time_created, time_updated, time_active, sandboxes)
                VALUES ('prj_abc', '/repo/canonical', 'git', 'ox', 1000, 2000, 3000, '[\"sb1\"]');",
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let bus = ox::event_bus::EventBus::new(db.clone());
    let cardinal = ox::cardinal::client::CardinalClient::new(
        config.cardinal_url.clone(),
        config.cardinal_key.clone(),
    )
    .unwrap();
    let runner = ox::runner::Runner::new(
        config.clone(),
        std::env::current_dir().unwrap(),
        db.clone(),
        bus.clone(),
        std::sync::Arc::new(cardinal),
        std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    );
    let state = AppState {
        config: config.clone(),
        bus,
        db,
        directory: std::env::current_dir().unwrap(),
        runner,
    };
    let router = ox::build_router(state);
    let listen: String = config.listen.clone();
    let handle = tokio::spawn(async move {
        let acceptor = TcpListener::new(listen).bind().await;
        Server::new(acceptor).serve(router).await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut projects: Option<serde_json::Value> = None;
    for _ in 0..50 {
        if let Ok(resp) = client.get(format!("{base}/api/project")).send().await {
            projects = resp.json().await.ok();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let projects =
        projects.expect("server never became ready: /api/project gave no response after 50 polls");
    let arr = projects.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "prj_abc");
    assert_eq!(arr[0]["canonical"], "/repo/canonical");
    assert_eq!(arr[0]["time"]["created"], 1000);
    assert_eq!(arr[0]["time"]["active"], 3000);
    assert_eq!(arr[0]["sandboxes"], serde_json::json!(["sb1"]));

    handle.abort();
}

#[tokio::test]
async fn unmatched_route_answers_json_not_html() {
    let (base, handle, _dir) = spawn_server().await;
    let response = reqwest::get(format!("{base}/api/nonexistent"))
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    handle.abort();
    assert_eq!(status, 404);
    assert!(body.contains("NotFoundError"), "body: {body}");
}

#[tokio::test]
async fn fs_list_and_find_resolve_paths_against_location() {
    let (base, handle, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // Build a tree under the system temp dir (the location the requests name).
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
    std::fs::write(root.path().join("top.txt"), "").unwrap();
    let location = format!(
        "location%5Bdirectory%5D={}",
        urlencoding_encode(root.path().to_str().unwrap())
    );

    // Default: relative to the location root.
    let body: serde_json::Value = client
        .get(format!("{base}/api/fs/list?{location}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paths = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"top.txt"), "{paths:?}");
    assert!(paths.contains(&"src/lib.rs"), "{paths:?}");

    // Relative `path` joins the location, not the server process cwd.
    let body: serde_json::Value = client
        .get(format!("{base}/api/fs/list?{location}&path=src"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paths = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["lib.rs"], "{paths:?}");

    // find matches by substring with the location envelope. The envelope
    // echoes the CANONICALIZED location (macOS /var → /private/var).
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let body: serde_json::Value = client
        .get(format!("{base}/api/fs/find?{location}&query=lib"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["data"][0]["path"], "src/lib.rs", "find body: {body}");
    assert_eq!(body["location"]["directory"], canonical.to_str().unwrap());
    handle.abort();
}

/// Minimal percent-encoder for test query values (enough for paths).
fn urlencoding_encode(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn location_get_echoes_requested_directory() {
    let (base, handle, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let body: serde_json::Value = client
        .get(format!(
            "{base}/api/location?location%5Bdirectory%5D=%2Ftmp%2Fsomewhere"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["directory"], "/tmp/somewhere", "{body:?}");
    assert_eq!(body["project"]["canonical"], "/tmp/somewhere");
    handle.abort();
}

#[tokio::test]
async fn catalog_envelope_shape_is_pinned() {
    let (base, handle, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    for path in ["/api/command", "/api/mcp", "/api/plugin", "/api/form"] {
        let body: serde_json::Value = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            body["location"]["directory"].is_string(),
            "{path}: {body:?}"
        );
        assert_eq!(body["data"], serde_json::json!([]), "{path}: {body:?}");
    }
    let config: serde_json::Value = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config, serde_json::json!([]));
    let migration: serde_json::Value = client
        .get(format!("{base}/api/experimental/migration/v1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(migration, serde_json::json!({"status": "completed"}));
    handle.abort();
}

#[tokio::test]
async fn wait_idle_404s_unknown_session_and_204s_when_idle() {
    let (base, handle, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/api/experimental/session/ses_unknown/wait"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    // A real session with no dispatched work is idle: 204, no body.
    let created: serde_json::Value = client
        .post(format!("{base}/api/session"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = created["data"]["id"].as_str().unwrap();
    let response = client
        .post(format!("{base}/api/experimental/session/{session_id}/wait"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    handle.abort();
}
