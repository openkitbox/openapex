//! The MCP session handshake and discovery steps.
//!
//! One `Client` per server. It owns the transport handle that every tool
//! built from this server shares, plus the catalog discovered once at
//! startup.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{CallError, Rpc};

/// The revision ox opens with. Servers may answer with an older revision;
/// the answer is accepted as-is (see the plan's decision log).
pub(crate) const PROTOCOL_VERSION: &str = "2025-11-25";

/// `tools/list` pages before giving up. A broken server must not hang
/// startup forever, and 100 pages is far beyond any real catalog.
const MAX_TOOL_PAGES: usize = 100;

#[derive(Debug, Clone)]
pub(crate) struct ToolInfo {
    /// The server's own tool name, unqualified. Wire calls use this.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceEntry {
    pub name: String,
    pub uri: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TemplateEntry {
    pub name: String,
    pub uri_template: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

pub(crate) struct Client {
    pub rpc: Arc<dyn Rpc>,
    pub instructions: Option<String>,
    pub tools: Vec<ToolInfo>,
    pub resources: Vec<ResourceEntry>,
    pub templates: Vec<TemplateEntry>,
}

impl Client {
    /// `initialize` → `notifications/initialized` → discovery. Every request
    /// uses `catalog` as its timeout; the caller bounds the whole startup
    /// separately.
    pub(crate) async fn connect(
        rpc: Arc<dyn Rpc>,
        catalog: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self, CallError> {
        let init = rpc
            .request(
                "initialize",
                Some(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "ox",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                })),
                Some(catalog),
                cancel,
            )
            .await?;

        let version = init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if version != PROTOCOL_VERSION {
            tracing::debug!(
                server_version = %version,
                client_version = PROTOCOL_VERSION,
                "MCP protocol version differs; continuing"
            );
        }
        let instructions = init
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let capabilities = init
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({}));

        rpc.notify("notifications/initialized", None).await?;

        let tools = if capabilities.get("tools").is_some() {
            list_tools(Arc::clone(&rpc), catalog, cancel).await?
        } else {
            Vec::new()
        };
        let (resources, templates) = if capabilities.get("resources").is_some() {
            (
                list_resources(Arc::clone(&rpc), catalog, cancel).await,
                list_templates(Arc::clone(&rpc), catalog, cancel).await,
            )
        } else {
            (Vec::new(), Vec::new())
        };

        Ok(Self {
            rpc,
            instructions,
            tools,
            resources,
            templates,
        })
    }
}

async fn list_tools(
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<ToolInfo>, CallError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOL_PAGES {
        let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
        let page = rpc
            .request("tools/list", params, Some(catalog), cancel)
            .await?;
        for item in page
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            tools.push(ToolInfo {
                name: name.to_owned(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_schema: item
                    .get("inputSchema")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                output_schema: item.get("outputSchema").cloned(),
            });
        }
        match page.get("nextCursor").and_then(Value::as_str) {
            Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
            _ => break,
        }
    }
    Ok(tools)
}

async fn list_resources(
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: &CancellationToken,
) -> Vec<ResourceEntry> {
    // A server that advertises resources but fails the list is degraded, not
    // fatal: the toolset is the point of the connection.
    let page = match rpc
        .request("resources/list", None, Some(catalog), cancel)
        .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::debug!(%error, "MCP resources/list failed; no resources listed");
            return Vec::new();
        }
    };
    page.get("resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(ResourceEntry {
                name: item.get("name")?.as_str()?.to_owned(),
                uri: item.get("uri")?.as_str()?.to_owned(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                mime_type: item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

async fn list_templates(
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: &CancellationToken,
) -> Vec<TemplateEntry> {
    // Some servers declare resources without implementing template listing
    // (upstream does `orElseSucceed(() => [])` for the same reason).
    let page = match rpc
        .request("resources/templates/list", None, Some(catalog), cancel)
        .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::debug!(
                %error,
                "MCP resources/templates/list failed; no templates listed"
            );
            return Vec::new();
        }
    };
    page.get("resourceTemplates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(TemplateEntry {
                name: item.get("name")?.as_str()?.to_owned(),
                uri_template: item.get("uriTemplate")?.as_str()?.to_owned(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                mime_type: item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}
