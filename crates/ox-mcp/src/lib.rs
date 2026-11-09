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
mod sync;
pub(crate) mod tool;
pub(crate) mod transport;
mod transports;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use ox_core::config::{McpConfig, McpServerConfig, McpTimeout};
use ox_tools::contract::Tool;

use client::Client;
use transport::{CallError, Rpc};

pub use transports::Transports;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
    metadata: RwLock<BTreeMap<String, ConnectionMetadata>>,
    /// Live connections, kept alive independently of their tools: a server
    /// that exposes no tools must not be torn down (its resources and
    /// instructions are still part of the session).
    connections: RwLock<BTreeMap<String, Arc<dyn Rpc>>>,
    resources: RwLock<Vec<ResourceInfo>>,
    templates: RwLock<Vec<TemplateInfo>>,
    /// Names of the tools the owner registered from this hub, per server; see
    /// [`Hub::record_server_tools`].
    tool_names: RwLock<BTreeMap<String, Vec<String>>>,
    transports: Arc<Transports>,
}

/// The tools one server returned, so the owner can record registrations
/// against the server that owns them.
pub struct ServerTools {
    pub server: String,
    pub tools: Vec<Arc<dyn Tool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionMetadata {
    title: Option<String>,
    instructions: Option<String>,
}

/// Appended after the upstream-rendered `<mcp_instructions>` block: upstream
/// `mcp/instructions.ts:22` names each server's tools per server ("through
/// `execute` under `tools[\"<server>\"]`"), but that is the codemode bridge —
/// ox registers MCP tools directly under their qualified `<server>_<tool>`
/// names (`crate::tool::qualified_name`). The line is the ox adaptation of
/// that naming guidance; the upstream bytes before it are untouched (batch I,
/// recorded as a deviation in `tests/fixtures/prompt/PROVENANCE.md`).
const TOOL_NAMING_LINE: &str = "Use tools from these servers directly by their `<server>_<tool>` names (sanitized server name, underscore, sanitized tool name).";

impl Hub {
    /// A hub with no servers, whose transport cleanup belongs to
    /// `transports`. A composition creates one owner and passes it to every
    /// hub it builds, so a hub that is dropped before shutdown cannot orphan a
    /// spawned child or an open pipe.
    #[must_use]
    pub fn with_transports(transports: Arc<Transports>) -> Self {
        Self {
            servers: RwLock::new(BTreeMap::new()),
            metadata: RwLock::new(BTreeMap::new()),
            connections: RwLock::new(BTreeMap::new()),
            resources: RwLock::new(Vec::new()),
            templates: RwLock::new(Vec::new()),
            tool_names: RwLock::new(BTreeMap::new()),
            transports,
        }
    }

    /// A hub with no servers. `Runner::new` uses this so tests that build a
    /// runner directly get the pre-MCP behavior; it owns a private transport
    /// scope, which [`Hub::shutdown`] still seals.
    #[must_use]
    pub fn empty() -> Self {
        Self::with_transports(Arc::new(Transports::new()))
    }

    /// The transport owner this hub's connections register their cleanup
    /// with.
    #[must_use]
    pub fn transports(&self) -> &Arc<Transports> {
        &self.transports
    }

    /// Tear down every connection this hub still holds and await each
    /// transport's shutdown. Idempotent: a second call finds nothing to drain.
    /// The transport owner is *not* sealed here — the composition seals it
    /// after every hub is shut down, because a dropped hub may have handed it
    /// cleanup that no live hub accounts for.
    pub async fn shutdown(&self) -> Result<(), String> {
        let connections: Vec<Arc<dyn Rpc>> = std::mem::take(&mut *sync::write(&self.connections))
            .into_values()
            .collect();
        let mut failure = None;
        for rpc in connections {
            let transports = Arc::clone(&self.transports);
            let cleanup = tokio::spawn(async move {
                rpc.shutdown().await;
            });
            match transports.adopt(cleanup) {
                Ok(()) => {}
                Err(handle) => {
                    if let Err(error) = handle.await {
                        failure.get_or_insert_with(|| format!("transport cleanup failed: {error}"));
                    }
                }
            }
        }
        sync::write(&self.resources).clear();
        sync::write(&self.templates).clear();
        sync::write(&self.tool_names).clear();
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Connect every configured server concurrently and return the tools to
    /// register. Failures become statuses; this function never returns Err.
    pub async fn connect_all(&self, config: &McpConfig, directory: &Path) -> Vec<ServerTools> {
        let mut pending = FuturesUnordered::new();
        for (name, server) in &config.servers {
            pending.push(connect_one(name, server, directory));
        }
        let mut outcomes = Vec::new();
        while let Some(outcome) = pending.next().await {
            outcomes.push(outcome);
        }
        outcomes.sort_by(|left, right| left.name.cmp(&right.name));

        let mut tools: Vec<ServerTools> = Vec::new();
        let mut seen = HashSet::new();
        for outcome in outcomes {
            let name = outcome.name.clone();
            let mut owned: Vec<Arc<dyn Tool>> = Vec::new();
            for tool in install(self, outcome) {
                let tool_name = tool.name().to_owned();
                if seen.insert(tool_name.clone()) {
                    owned.push(tool);
                } else {
                    tracing::warn!(
                        tool = %tool_name,
                        server = %name,
                        "MCP tool name collides with an earlier tool; skipped"
                    );
                }
            }
            tools.push(ServerTools {
                server: name,
                tools: owned,
            });
        }
        tools
    }

    /// Record which of the tools a server offered the owner actually
    /// registered (a name that collided with an existing tool is skipped and
    /// must not be listed). Replaces the server's previous record; an empty
    /// list drops it.
    pub fn record_server_tools(&self, server: &str, names: Vec<String>) {
        let mut tool_names = sync::write(&self.tool_names);
        if names.is_empty() {
            tool_names.remove(server);
        } else {
            tool_names.insert(server.to_owned(), names);
        }
    }

    /// Names recorded by [`Self::record_server_tools`], flattened: exactly
    /// the tools a reload may remove from a registry without touching a
    /// builtin.
    pub fn tool_names(&self) -> Vec<String> {
        sync::read(&self.tool_names)
            .values()
            .flatten()
            .cloned()
            .collect()
    }

    /// Whether the hub tracks a server under this name, including disabled
    /// and failed ones.
    pub fn has_server(&self, server: &str) -> bool {
        sync::read(&self.servers).contains_key(server)
    }

    /// Force a status, e.g. `disabled` after a manual disconnect.
    pub fn set_status(&self, server: &str, status: Status) {
        sync::write(&self.servers).insert(server.to_owned(), status);
    }

    /// Tear down one server. Does nothing (and returns `None`) unless it was
    /// connected; otherwise shuts the transport down, drops its tools,
    /// resources, templates and metadata, and returns the registered tool
    /// names the owner must remove from the registry. The status is left to
    /// the caller, matching upstream `stopServer`.
    pub async fn stop(&self, server: &str) -> Option<Vec<String>> {
        let rpc = sync::write(&self.connections).remove(server);
        let rpc = rpc?;
        rpc.shutdown().await;
        sync::write(&self.metadata).remove(server);
        sync::write(&self.resources).retain(|entry| entry.server != server);
        sync::write(&self.templates).retain(|entry| entry.server != server);
        let names = sync::write(&self.tool_names)
            .remove(server)
            .unwrap_or_default();
        Some(names)
    }

    /// Connect one already-known server, bypassing the config `disabled`
    /// flag (upstream `startServer` is the manual override). Returns the
    /// tools to register, or `None` when the connection failed: a failure is
    /// a `failed` status and no tools, never an error. Any previous state
    /// for the server is replaced.
    pub async fn start(
        &self,
        server: &str,
        config: &McpServerConfig,
        directory: &Path,
    ) -> Option<Vec<Arc<dyn Tool>>> {
        let outcome = connect_enabled(server, config, directory).await;
        let connected = outcome.rpc.is_some();
        let tools = install(self, outcome);
        connected.then_some(tools)
    }

    pub fn servers(&self) -> Vec<ServerView> {
        let servers = sync::read(&self.servers);
        let metadata = sync::read(&self.metadata);
        servers
            .iter()
            .map(|(name, status)| {
                let connection = metadata.get(name);
                ServerView {
                    name: name.clone(),
                    status: status.clone(),
                    title: connection.and_then(|m| m.title.clone()),
                }
            })
            .collect()
    }

    pub fn resources(&self) -> (Vec<ResourceInfo>, Vec<TemplateInfo>) {
        (
            sync::read(&self.resources).clone(),
            sync::read(&self.templates).clone(),
        )
    }

    /// `<mcp_instructions>` for the system prompt; `None` when no server
    /// supplied instructions, so the zero-server prompt stays byte-identical.
    /// The upstream block bytes are a strict prefix; [`TOOL_NAMING_LINE`]
    /// follows as the one documented deviation.
    pub fn instructions_block(&self) -> Option<String> {
        let metadata = sync::read(&self.metadata);
        let instructions = metadata.iter().filter_map(|(name, connection)| {
            connection
                .instructions
                .as_deref()
                .map(|text| (name.as_str(), text))
        });
        let mut block = String::from("<mcp_instructions>");
        let mut found = false;
        for (name, text) in instructions {
            found = true;
            block.push_str(&format!("\n  <server name=\"{}\">\n", escape(name)));
            for line in text.lines() {
                block.push_str("    ");
                block.push_str(line);
                block.push('\n');
            }
            block.push_str("  </server>");
        }
        if !found {
            return None;
        }
        block.push_str("\n</mcp_instructions>");
        block.push('\n');
        block.push_str(TOOL_NAMING_LINE);
        Some(block)
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Move one connect outcome into the hub: status, instructions, catalog and
/// live connection. A failure clears the previous connection and metadata so
/// a reconnect can never leave the old server's instructions or connection
/// behind. The previous catalog entries must already be gone (connect_all
/// starts empty, [`Hub::stop`] clears them for a restart).
fn install(hub: &Hub, outcome: Connected) -> Vec<Arc<dyn Tool>> {
    let name = outcome.name.clone();
    sync::write(&hub.servers).insert(name.clone(), outcome.status);
    let mut metadata = sync::write(&hub.metadata);
    match outcome.metadata {
        Some(connection) => metadata.insert(name.clone(), connection),
        None => metadata.remove(&name),
    };
    drop(metadata);
    sync::write(&hub.resources).extend(outcome.resources);
    sync::write(&hub.templates).extend(outcome.templates);
    let mut connections = sync::write(&hub.connections);
    match outcome.rpc {
        Some(rpc) => connections.insert(name, rpc),
        None => connections.remove(&name),
    };
    drop(connections);
    outcome.tools
}

struct Connected {
    name: String,
    status: Status,
    rpc: Option<Arc<dyn Rpc>>,
    tools: Vec<Arc<dyn Tool>>,
    resources: Vec<ResourceInfo>,
    templates: Vec<TemplateInfo>,
    metadata: Option<ConnectionMetadata>,
}

impl Connected {
    fn disabled(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            status: Status::Disabled,
            rpc: None,
            tools: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
            metadata: None,
        }
    }

    fn failed(name: &str, error: String) -> Self {
        Self {
            name: name.to_owned(),
            status: Status::Failed { error },
            rpc: None,
            tools: Vec::new(),
            resources: Vec::new(),
            templates: Vec::new(),
            metadata: None,
        }
    }

    fn from_client(name: &str, client: Client, execution: Option<Duration>) -> Self {
        let Client {
            rpc,
            title,
            instructions,
            tools,
            resources,
            templates,
            ..
        } = client;
        let metadata = ConnectionMetadata {
            title,
            instructions,
        };
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
            tools,
            resources,
            templates,
            metadata: Some(metadata),
        }
    }
}

async fn connect_one(name: &str, server: &McpServerConfig, directory: &Path) -> Connected {
    if server.disabled() {
        return Connected::disabled(name);
    }
    connect_enabled(name, server, directory).await
}

/// Spawn or open the transport and handshake, ignoring the `disabled` flag.
async fn connect_enabled(name: &str, server: &McpServerConfig, directory: &Path) -> Connected {
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
                Ok(rpc) => handshake(name, Arc::new(rpc), timeout, server.protocol()).await,
                Err(error) => Connected::failed(
                    name,
                    format!("failed to start MCP server `{name}`: {error}"),
                ),
            }
        }
        McpServerConfig::Remote(remote) => match http::HttpRpc::new(remote) {
            Ok(rpc) => handshake(name, Arc::new(rpc), timeout, server.protocol()).await,
            Err(error) => Connected::failed(name, error),
        },
    }
}

async fn handshake(
    name: &str,
    rpc: Arc<dyn Rpc>,
    timeout: &McpTimeout,
    protocol: ox_core::config::McpProtocol,
) -> Connected {
    let cancel = CancellationToken::new();
    let catalog = Duration::from_millis(timeout.catalog);
    let startup = Duration::from_millis(timeout.startup);
    match tokio::time::timeout(
        startup,
        Client::connect(Arc::clone(&rpc), catalog, &cancel, protocol),
    )
    .await
    {
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
        CallError::Unsupported(message) => format!("MCP server `{name}`: {message}"),
    }
}
