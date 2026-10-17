//! Integration test: boot the real router on an ephemeral port and hit the
//! read endpoints over HTTP.

use std::net::TcpListener as StdTcpListener;
use std::time::Instant;

use ox::api::AppState;
use ox::config::Config;
use ox::db::Db;
use salvo::prelude::*;
use tokio::task::JoinHandle;

/// Pick an ephemeral port by binding then releasing a std listener.
fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn the server in the background against a temp DB; returns (base_url, handle).
async fn spawn_server() -> (String, JoinHandle<()>) {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("ox-itest-{}", std::process::id()));
    let db_path = dir.join(format!("ox-{port}.db"));
    let config = Config {
        db_path,
        listen: format!("127.0.0.1:{port}"),
        ..Config::default()
    };
    let db = Db::open(&config.db_path).await.unwrap();
    let state = AppState {
        config: config.clone(),
        db,
        started: Instant::now(),
        directory: std::env::current_dir().unwrap(),
    };
    let router = ox::build_router(state);
    let listen: String = config.listen.clone();
    let handle = tokio::spawn(async move {
        let acceptor = TcpListener::new(listen).bind().await;
        Server::new(acceptor).serve(router).await;
    });

    // Wait until the port accepts connections.
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..50 {
        if client.get(format!("{base}/api/info")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    (base, handle)
}

#[tokio::test]
async fn info_location_project_endpoints_respond() {
    let (base, handle) = spawn_server().await;
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
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("ox-itest-{}", std::process::id()));
    let db_path = dir.join(format!("ox-shared-{port}.db"));
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

    let state = AppState {
        config: config.clone(),
        db,
        started: Instant::now(),
        directory: std::env::current_dir().unwrap(),
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
    let projects = projects.expect("project.list responded");
    let arr = projects.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "prj_abc");
    assert_eq!(arr[0]["canonical"], "/repo/canonical");
    assert_eq!(arr[0]["time"]["created"], 1000);
    assert_eq!(arr[0]["time"]["active"], 3000);
    assert_eq!(arr[0]["sandboxes"], serde_json::json!(["sb1"]));

    handle.abort();
}
