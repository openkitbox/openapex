//! End-to-end: a real stdio MCP stub server is configured through `Config`,
//! ox connects on startup, and the TUI-facing catalogs report it.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use ox::config::{Config, McpConfig, McpLocalConfig, McpProtocol, McpServerConfig, McpTimeout};
use salvo::prelude::*;
use serde_json::{Value, json};

const STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0"}}}\n' "$id"
      ;;
    *'"method":"server/discover"'*)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
  esac
done
"#;

const LEGACY_ONLY_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      exit 1
      ;;
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"legacy-only","version":"0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id"
      ;;
  esac
done
"#;

const DEFAULT_LEGACY_STUB: &str = r#"#!/bin/sh
seen_modern=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      seen_modern=1
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id"
      ;;
    *'"method":"initialize"'*)
      if [ "$seen_modern" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stub","version":"0"}}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"modern discovery was not attempted"}}\n' "$id"
      fi
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id"
      ;;
  esac
done
"#;

const MODERN_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      if printf '%s' "$line" | grep -q '"_meta":{"protocolVersion":"2026-07-28","clientInfo":{"name":"ox"' && printf '%s' "$line" | grep -q '"clientCapabilities":{}'; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2026-07-28","capabilities":{"tools":{}},"serverInfo":{"name":"modern-stub","title":"Modern Stub","version":"1"},"instructions":"Use the modern stub."}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"missing modern metadata"}}\n' "$id"
      fi
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
  esac
done
"#;

const AUTO_ERROR_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"server unavailable"}}\n' "$id"
      ;;
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"unexpected","capabilities":{"tools":{}}}}\n' "$id"
      ;;
  esac
done
"#;

const MODERN_MISSING_CAPABILITIES_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2026-07-28","serverInfo":{"name":"missing-capabilities","version":"1"}}}\n' "$id"
      ;;
  esac
done
"#;

const AUTO_UNSUPPORTED_SUCCESS_STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"server/discover"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{}}}\n' "$id"
      ;;
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id"
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

fn legacy_only_stub(directory: &std::path::Path) -> String {
    let path = directory.join("legacy-only-server.sh");
    std::fs::write(&path, LEGACY_ONLY_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn default_legacy_stub(directory: &std::path::Path) -> String {
    let path = directory.join("default-legacy-server.sh");
    std::fs::write(&path, DEFAULT_LEGACY_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn modern_stub(directory: &std::path::Path) -> String {
    let path = directory.join("modern-server.sh");
    std::fs::write(&path, MODERN_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn auto_error_stub(directory: &std::path::Path) -> String {
    let path = directory.join("auto-error-server.sh");
    std::fs::write(&path, AUTO_ERROR_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn modern_missing_capabilities_stub(directory: &std::path::Path) -> String {
    let path = directory.join("modern-missing-capabilities-server.sh");
    std::fs::write(&path, MODERN_MISSING_CAPABILITIES_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn auto_unsupported_success_stub(directory: &std::path::Path) -> String {
    let path = directory.join("auto-unsupported-success-server.sh");
    std::fs::write(&path, AUTO_UNSUPPORTED_SUCCESS_STUB).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

#[test]
fn mcp_protocol_defaults_to_auto_and_parses_explicit_modes() {
    let default: McpLocalConfig = serde_json::from_value(json!({
        "command": ["server"],
    }))
    .unwrap();
    assert_eq!(default.protocol, McpProtocol::Auto);

    let legacy: McpLocalConfig = serde_json::from_value(json!({
        "command": ["server"],
        "protocol": "legacy",
    }))
    .unwrap();
    assert_eq!(legacy.protocol, McpProtocol::Legacy);

    let modern: McpLocalConfig = serde_json::from_value(json!({
        "command": ["server"],
        "protocol": "2026-07-28",
    }))
    .unwrap();
    assert_eq!(modern.protocol, McpProtocol::Modern);
}

#[tokio::test]
async fn mcp_catalogs_report_a_connected_server() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "stub".to_owned(),
        McpServerConfig::Local(McpLocalConfig {
            command: vec![legacy_only_stub(directory.path())],
            cwd: None,
            environment: None,
            protocol: McpProtocol::Legacy,
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
        json!([{
            "name": "stub",
            "status": { "status": "connected" },
        }])
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

#[tokio::test]
async fn mcp_modern_discovery_propagates_server_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "modern".to_owned(),
        McpServerConfig::Local(
            serde_json::from_value(json!({
                "command": [modern_stub(directory.path())],
            }))
            .unwrap(),
        ),
    );
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    let views = state.services().mcp.servers();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].title.as_deref(), Some("Modern Stub"));
}

#[tokio::test]
async fn mcp_auto_does_not_fallback_for_non_method_not_found_error() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "error".to_owned(),
        McpServerConfig::Local(McpLocalConfig {
            command: vec![auto_error_stub(directory.path())],
            cwd: None,
            environment: None,
            protocol: McpProtocol::Auto,
            disabled: false,
            timeout: McpTimeout::default(),
        }),
    );
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    let view = &state.services().mcp.servers()[0];
    assert!(matches!(view.status, ox::mcp::Status::Failed { .. }));
}

#[tokio::test]
async fn mcp_auto_falls_back_to_legacy_only_on_unsupported_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "legacy".to_owned(),
        McpServerConfig::Local(
            serde_json::from_value(json!({
                "command": [default_legacy_stub(directory.path())],
            }))
            .unwrap(),
        ),
    );
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    let view = &state.services().mcp.servers()[0];
    assert_eq!(view.status, ox::mcp::Status::Connected);
}

#[tokio::test]
async fn mcp_modern_discovery_fails_closed_when_capabilities_are_missing() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "missing-capabilities".to_owned(),
        McpServerConfig::Local(McpLocalConfig {
            command: vec![modern_missing_capabilities_stub(directory.path())],
            cwd: None,
            environment: None,
            protocol: McpProtocol::Modern,
            disabled: false,
            timeout: McpTimeout::default(),
        }),
    );
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    assert!(matches!(
        state.services().mcp.servers()[0].status,
        ox::mcp::Status::Failed { .. }
    ));
}

#[tokio::test]
async fn mcp_auto_falls_back_when_successful_discovery_does_not_offer_modern_version() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "unsupported-success".to_owned(),
        McpServerConfig::Local(McpLocalConfig {
            command: vec![auto_unsupported_success_stub(directory.path())],
            cwd: None,
            environment: None,
            protocol: McpProtocol::Auto,
            disabled: false,
            timeout: McpTimeout::default(),
        }),
    );
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    assert_eq!(
        state.services().mcp.servers()[0].status,
        ox::mcp::Status::Connected
    );
}

#[tokio::test]
async fn mcp_modern_mode_fails_closed_for_legacy_server() {
    let directory = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert(
        "legacy".to_owned(),
        McpServerConfig::Local(McpLocalConfig {
            command: vec![stub(directory.path())],
            cwd: None,
            environment: None,
            protocol: McpProtocol::Modern,
            disabled: false,
            timeout: McpTimeout::default(),
        }),
    );
    let config = Config {
        mcp: McpConfig { servers },
        db_path: directory.path().join("ox.db"),
        ..Config::default()
    };

    let state = ox::build_state(config).await.unwrap();
    assert!(matches!(
        state.services().mcp.servers()[0].status,
        ox::mcp::Status::Failed { .. }
    ));
}
