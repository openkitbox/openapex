//! End-to-end: a real stdio MCP stub server is configured through `Config`,
//! ox connects on startup, and the TUI-facing catalogs report it.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use ox::config::{Config, McpConfig, McpLocalConfig, McpServerConfig, McpTimeout};
use salvo::prelude::*;
use serde_json::{Value, json};

const STUB: &str = r#"#!/bin/sh
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn stub(directory: &std::path::Path) -> String {
    let path = directory.join("server.sh");
    std::fs::write(&path, STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

#[tokio::test]
async fn mcp_catalogs_report_a_connected_server() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "stub".to_owned(),
        McpServerConfig::Local(McpLocalConfig {
            command: vec![stub(directory.path())],
            cwd: None,
            environment: None,
            disabled: false,
            timeout: McpTimeout::default(),
        }),
    );
    let port = free_port();
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        listen: format!("127.0.0.1:{port}"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    let router = ox::build_router(state);
    let handle = tokio::spawn(async move {
        let acceptor = TcpListener::new(format!("127.0.0.1:{port}")).bind().await;
        Server::new(acceptor).serve(router).await;
    });

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
    assert!(ready, "server never became ready");

    let body: Value = client
        .get(format!("{base}/api/mcp"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["data"],
        json!([{ "name": "stub", "status": { "status": "connected" } }])
    );
    // The envelope echoes the request's directory (no location query here,
    // so the server's cwd); that rule is pinned in tests/skeleton.rs.
    assert!(body["location"]["directory"].is_string());

    let resources: Value = client
        .get(format!("{base}/api/mcp/resource"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resources["data"],
        json!({ "resources": [], "templates": [] })
    );

    handle.abort();
}
