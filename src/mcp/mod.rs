//! MCP client integration.
//!
//! The hub owns every configured server: it connects them concurrently at
//! startup, turns their tools into `runner::Tool` values, and keeps the
//! status/resource/instruction surface the HTTP layer and the system prompt
//! read. Connections are process-wide, matching the process-wide tool
//! registry (see the plan's decision log for why ACP per-session servers are
//! not supported).

pub(crate) mod client;
pub(crate) mod http;
pub(crate) mod jsonrpc;
pub(crate) mod stdio;
pub(crate) mod tool;
pub(crate) mod transport;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{McpConfig, McpServerConfig, McpTimeout};
use crate::runner::Tool;

use client::Client;
use transport::{CallError, Rpc};

/// Server status, serialized exactly as upstream `Mcp.Status` (a tagged
/// object: `{"status":"connected"}`, `{"status":"failed","error":"..."}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Connected,
    Pending,
    Disabled,
    Failed { error: String },
}

/// One entry of `GET /api/mcp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerView {
    pub name: String,
    pub status: Status,
}

/// One entry of `GET /api/mcp/resource`'s `resources` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceInfo {
    pub server: String,
    pub name: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "mimeType")]
    pub mime_type: Option<String>,
}

/// One entry of `GET /api/mcp/resource`'s `templates` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TemplateInfo {
    pub server: String,
    pub name: String,
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "mimeType")]
    pub mime_type: Option<String>,
}

pub struct Hub {
    servers: RwLock<BTreeMap<String, Status>>,
    /// Live connections, kept alive independently of their tools: a server
    /// that exposes no tools must not be torn down (its resources and
    /// instructions are still part of the session).
    connections: RwLock<BTreeMap<String, Arc<dyn Rpc>>>,
    instructions: RwLock<Vec<(String, String)>>,
    resources: RwLock<Vec<ResourceInfo>>,
    templates: RwLock<Vec<TemplateInfo>>,
}

impl Hub {
    /// A hub with no servers. `Runner::new` uses this so tests that build a
    /// runner directly get the pre-MCP behavior.
    pub fn empty() -> Self {
        Self {
            servers: RwLock::new(BTreeMap::new()),
            connections: RwLock::new(BTreeMap::new()),
            instructions: RwLock::new(Vec::new()),
            resources: RwLock::new(Vec::new()),
            templates: RwLock::new(Vec::new()),
        }
    }

    /// Connect every configured server concurrently and return the tools to
    /// register. Failures become statuses; this function never returns Err.
    pub async fn connect_all(&self, config: &McpConfig, directory: &Path) -> Vec<Arc<dyn Tool>> {
        let mut pending = FuturesUnordered::new();
        for (name, server) in &config.servers {
            pending.push(connect_one(name, server, directory));
        }
        let mut outcomes = Vec::new();
        while let Some(outcome) = pending.next().await {
            outcomes.push(outcome);
        }
        outcomes.sort_by(|left, right| left.name.cmp(&right.name));

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut seen = HashSet::new();
        for outcome in outcomes {
            let name = outcome.name.clone();
            if let Some(instructions) = outcome.instructions {
                self.instructions
                    .write()
                    .unwrap()
                    .push((name.clone(), instructions));
            }
            self.servers
                .write()
                .unwrap()
                .insert(name.clone(), outcome.status);
            self.resources.write().unwrap().extend(outcome.resources);
            self.templates.write().unwrap().extend(outcome.templates);
            if let Some(rpc) = outcome.rpc {
                self.connections.write().unwrap().insert(name.clone(), rpc);
            }
            for tool in outcome.tools {
                let tool_name = tool.name().to_owned();
                if seen.insert(tool_name.clone()) {
                    tools.push(tool);
                } else {
                    tracing::warn!(
                        tool = %tool_name,
                        server = %name,
                        "MCP tool name collides with an earlier tool; skipped"
                    );
                }
            }
        }
        tools
    }

    pub fn servers(&self) -> Vec<ServerView> {
        self.servers
            .read()
            .unwrap()
            .iter()
            .map(|(name, status)| ServerView {
                name: name.clone(),
                status: status.clone(),
            })
            .collect()
    }

    pub fn resources(&self) -> (Vec<ResourceInfo>, Vec<TemplateInfo>) {
        (
            self.resources.read().unwrap().clone(),
            self.templates.read().unwrap().clone(),
        )
    }

    /// `<mcp_instructions>` for the system prompt; `None` when no server
    /// supplied instructions, so the zero-server prompt stays byte-identical.
    pub fn instructions_block(&self) -> Option<String> {
        let instructions = self.instructions.read().unwrap();
        if instructions.is_empty() {
            return None;
        }
        let mut block = String::from("<mcp_instructions>");
        for (name, text) in instructions.iter() {
            block.push_str(&format!("\n  <server name=\"{}\">\n", escape(name)));
            for line in text.lines() {
                block.push_str("    ");
                block.push_str(line);
                block.push('\n');
            }
            block.push_str("  </server>");
        }
        block.push_str("\n</mcp_instructions>");
        Some(block)
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct Connected {
    name: String,
    status: Status,
    rpc: Option<Arc<dyn Rpc>>,
    instructions: Option<String>,
    tools: Vec<Arc<dyn Tool>>,
    resources: Vec<ResourceInfo>,
    templates: Vec<TemplateInfo>,
}

impl Connected {
    fn disabled(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            status: Status::Disabled,
            rpc: None,
            instructions: None,
            tools: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
        }
    }

    fn failed(name: &str, error: String) -> Self {
        Self {
            name: name.to_owned(),
            status: Status::Failed { error },
            rpc: None,
            instructions: None,
            tools: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
        }
    }

    fn from_client(name: &str, client: Client, execution: Option<Duration>) -> Self {
        let Client {
            rpc,
            instructions,
            tools,
            resources,
            templates,
        } = client;
        let tools = tools
            .into_iter()
            .map(|info| {
                Arc::new(tool::McpTool::new(
                    name.to_owned(),
                    info,
                    Arc::clone(&rpc),
                    execution,
                )) as Arc<dyn Tool>
            })
            .collect();
        let resources = resources
            .into_iter()
            .map(|entry| ResourceInfo {
                server: name.to_owned(),
                name: entry.name,
                uri: entry.uri,
                description: entry.description,
                mime_type: entry.mime_type,
            })
            .collect();
        let templates = templates
            .into_iter()
            .map(|entry| TemplateInfo {
                server: name.to_owned(),
                name: entry.name,
                uri_template: entry.uri_template,
                description: entry.description,
                mime_type: entry.mime_type,
            })
            .collect();
        Self {
            name: name.to_owned(),
            status: Status::Connected,
            rpc: Some(rpc),
            instructions,
            tools,
            resources,
            templates,
        }
    }
}

async fn connect_one(name: &str, server: &McpServerConfig, directory: &Path) -> Connected {
    if server.disabled() {
        return Connected::disabled(name);
    }
    let timeout = server.timeout();
    match server {
        McpServerConfig::Local(local) => {
            let cwd = local.cwd.as_ref().map(|cwd| {
                let path = Path::new(cwd);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    directory.join(path)
                }
            });
            let environment = local.environment.clone().unwrap_or_default();
            match stdio::StdioRpc::spawn(&local.command, &environment, cwd.as_deref()) {
                Ok(rpc) => handshake(name, Arc::new(rpc), timeout).await,
                Err(error) => Connected::failed(
                    name,
                    format!("failed to start MCP server `{name}`: {error}"),
                ),
            }
        }
        McpServerConfig::Remote(remote) => match http::HttpRpc::new(remote) {
            Ok(rpc) => handshake(name, Arc::new(rpc), timeout).await,
            Err(error) => Connected::failed(name, error),
        },
    }
}

async fn handshake(name: &str, rpc: Arc<dyn Rpc>, timeout: &McpTimeout) -> Connected {
    let cancel = CancellationToken::new();
    let catalog = Duration::from_millis(timeout.catalog);
    let startup = Duration::from_millis(timeout.startup);
    match tokio::time::timeout(startup, Client::connect(Arc::clone(&rpc), catalog, &cancel)).await {
        Ok(Ok(client)) => {
            Connected::from_client(name, client, timeout.execution.map(Duration::from_millis))
        }
        Ok(Err(error)) => {
            rpc.shutdown().await;
            Connected::failed(name, friendly(name, &error))
        }
        Err(_) => {
            rpc.shutdown().await;
            Connected::failed(
                name,
                format!("startup timed out after {} ms", timeout.startup),
            )
        }
    }
}

fn friendly(name: &str, error: &CallError) -> String {
    match error {
        CallError::Closed => format!("MCP server `{name}` is not available"),
        CallError::Timeout => format!("MCP server `{name}` timed out during startup"),
        CallError::Cancelled => format!("MCP server `{name}` startup was cancelled"),
        CallError::Rpc(error) => format!("MCP server `{name}` rejected the handshake: {error}"),
        CallError::Transport(message) => format!("MCP server `{name}`: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{McpLocalConfig, McpServerConfig, McpTimeout};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    const STUB: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"stub","version":"0"},"instructions":"stub instructions"}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"resources":[{"name":"notes","uri":"file:///notes"}]}}\n' "$id"
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"resourceTemplates":[]}}\n' "$id"
      ;;
  esac
done
"#;

    fn local(command: Vec<String>) -> McpServerConfig {
        McpServerConfig::Local(McpLocalConfig {
            command,
            cwd: None,
            environment: None,
            disabled: false,
            timeout: McpTimeout::default(),
        })
    }

    fn install(directory: &std::path::Path) -> String {
        let path = directory.join("server.sh");
        std::fs::write(&path, STUB).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[tokio::test]
    async fn connect_all_registers_tools_and_reports_status() {
        let directory = tempfile::tempdir().unwrap();
        let mut servers = BTreeMap::new();
        servers.insert("stub".to_owned(), local(vec![install(directory.path())]));
        let config = McpConfig { servers };
        let hub = Hub::empty();
        let tools = hub.connect_all(&config, directory.path()).await;

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "stub_echo");
        assert_eq!(
            hub.servers(),
            vec![ServerView {
                name: "stub".to_owned(),
                status: Status::Connected,
            }]
        );
        assert_eq!(
            hub.instructions_block().as_deref(),
            Some(
                "<mcp_instructions>\n  <server name=\"stub\">\n    stub instructions\n  </server>\n</mcp_instructions>"
            )
        );
        let (resources, templates) = hub.resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].server, "stub");
        assert_eq!(resources[0].uri, "file:///notes");
        assert!(templates.is_empty());
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_failed_status_not_a_startup_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut servers = BTreeMap::new();
        servers.insert(
            "broken".to_owned(),
            local(vec!["/nonexistent/mcp-server".to_owned()]),
        );
        let hub = Hub::empty();
        let tools = hub
            .connect_all(&McpConfig { servers }, directory.path())
            .await;
        assert!(tools.is_empty());
        match &hub.servers()[0].status {
            Status::Failed { error } => assert!(error.contains("failed to start")),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_servers_are_reported_and_never_spawned() {
        let directory = tempfile::tempdir().unwrap();
        let mut server = local(vec!["/nonexistent/mcp-server".to_owned()]);
        if let McpServerConfig::Local(local) = &mut server {
            local.disabled = true;
        }
        let mut servers = BTreeMap::new();
        servers.insert("off".to_owned(), server);
        let hub = Hub::empty();
        assert!(
            hub.connect_all(&McpConfig { servers }, directory.path())
                .await
                .is_empty()
        );
        assert_eq!(hub.servers()[0].status, Status::Disabled);
    }

    #[test]
    fn instructions_are_absent_without_servers() {
        assert!(Hub::empty().instructions_block().is_none());
    }

    #[tokio::test]
    async fn a_hung_server_times_out_instead_of_blocking_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hung.sh");
        std::fs::write(&path, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let McpServerConfig::Local(mut hung) = local(vec![path.display().to_string()]) else {
            unreachable!()
        };
        hung.timeout.startup = 300;
        let mut servers = BTreeMap::new();
        servers.insert("hung".to_owned(), McpServerConfig::Local(hung));
        let hub = Hub::empty();
        let started = std::time::Instant::now();
        let tools = hub
            .connect_all(&McpConfig { servers }, directory.path())
            .await;
        assert!(tools.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        match &hub.servers()[0].status {
            Status::Failed { error } => assert!(error.contains("startup timed out"), "{error}"),
            other => panic!("expected failed, got {other:?}"),
        }
    }
}
