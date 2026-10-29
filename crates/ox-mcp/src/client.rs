//! The MCP session handshake and discovery steps.
//!
//! One `Client` per server. It owns the transport handle that every tool
//! built from this server shares, plus the catalog discovered once at
//! startup.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{CallError, ProtocolMode, RequestMetadata, Rpc};
use ox_core::config::McpProtocol;

/// The last legacy revision ox sends in `initialize`.
pub(crate) const PROTOCOL_VERSION: &str = "2025-11-25";
pub(crate) const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

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
    pub title: Option<String>,
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
        protocol: McpProtocol,
    ) -> Result<Self, CallError> {
        if matches!(protocol, McpProtocol::Modern | McpProtocol::Auto) {
            rpc.set_protocol(ProtocolMode::Modern);
        }
        let metadata = match protocol {
            McpProtocol::Legacy => {
                rpc.set_protocol(ProtocolMode::Legacy);
                legacy_initialize(Arc::clone(&rpc), catalog, cancel).await?
            }
            McpProtocol::Modern => {
                rpc.set_protocol(ProtocolMode::Modern);
                modern_discover(Arc::clone(&rpc), catalog, cancel).await?
            }
            McpProtocol::Auto => match modern_discover(Arc::clone(&rpc), catalog, cancel).await {
                Ok(metadata) => metadata,
                Err(error) if is_modern_unsupported(&error) => {
                    rpc.set_protocol(ProtocolMode::Legacy);
                    legacy_initialize(Arc::clone(&rpc), catalog, cancel).await?
                }
                Err(error) => return Err(error),
            },
        };

        if metadata.protocol == "legacy" {
            rpc.notify("notifications/initialized", None).await?;
        }

        let tools = if metadata.capabilities.get("tools").is_some() {
            list_tools(Arc::clone(&rpc), catalog, cancel).await?
        } else {
            Vec::new()
        };
        let (resources, templates) = if metadata.capabilities.get("resources").is_some() {
            (
                list_resources(Arc::clone(&rpc), catalog, cancel).await,
                list_templates(Arc::clone(&rpc), catalog, cancel).await,
            )
        } else {
            (Vec::new(), Vec::new())
        };

        Ok(Self {
            rpc,
            title: metadata.title,
            instructions: metadata.instructions,
            tools,
            resources,
            templates,
        })
    }
}

struct Metadata {
    protocol: &'static str,
    title: Option<String>,
    instructions: Option<String>,
    capabilities: Value,
}

async fn legacy_initialize(
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: &CancellationToken,
) -> Result<Metadata, CallError> {
    let init = rpc
        .request(
            RequestMetadata::new("initialize"),
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

    Ok(Metadata {
        protocol: "legacy",
        title: init
            .get("serverInfo")
            .and_then(|info| info.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        instructions,
        capabilities,
    })
}

async fn modern_discover(
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: &CancellationToken,
) -> Result<Metadata, CallError> {
    let discovery = rpc
        .request(
            RequestMetadata::new("server/discover"),
            Some(json!({
                "_meta": {
                    "protocolVersion": MODERN_PROTOCOL_VERSION,
                    "clientInfo": {
                        "name": "ox",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "clientCapabilities": {},
                },
            })),
            Some(catalog),
            cancel,
        )
        .await?;
    discovery
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|version| *version == MODERN_PROTOCOL_VERSION)
        .or_else(|| {
            discovery
                .get("supportedVersions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|version| *version == MODERN_PROTOCOL_VERSION)
        })
        .ok_or_else(|| {
            CallError::Unsupported(
                "MCP modern discovery did not offer protocol 2026-07-28".to_owned(),
            )
        })?;
    let capabilities = discovery
        .get("capabilities")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| {
            CallError::Transport(
                "MCP modern discovery response must include object capabilities".to_owned(),
            )
        })?;
    Ok(Metadata {
        protocol: "modern",
        title: server_title(&discovery),
        instructions: discovery
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned),
        capabilities,
    })
}

fn server_title(result: &Value) -> Option<String> {
    result
        .get("serverInfo")
        .or_else(|| result.get("_meta").and_then(|meta| meta.get("serverInfo")))
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn is_modern_unsupported(error: &CallError) -> bool {
    matches!(
        error,
        CallError::Rpc(error) if error.code == super::jsonrpc::METHOD_NOT_FOUND
    ) || matches!(error, CallError::Unsupported(_))
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
            .request(
                RequestMetadata::new("tools/list"),
                params,
                Some(catalog),
                cancel,
            )
            .await?;
        tools.extend(parse_tools(&page));
        match page.get("nextCursor").and_then(Value::as_str) {
            Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
            _ => break,
        }
    }
    Ok(tools)
}

/// Validate one `tools/list` page. A malformed entry is skipped with a warning
/// instead of blocking the catalog, matching upstream's per-tool tolerance.
fn parse_tools(page: &Value) -> Vec<ToolInfo> {
    page.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_tool)
        .collect()
}

fn parse_tool(item: &Value) -> Option<ToolInfo> {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let input_schema = item
        .get("inputSchema")
        .filter(|value| {
            value.is_object() && value.get("type").and_then(Value::as_str) == Some("object")
        })
        .or_else(|| {
            tracing::warn!(
                tool = %name,
                "MCP tool has no object inputSchema; skipped"
            );
            None
        })?;
    if name.is_empty() {
        tracing::warn!("MCP tool has an empty name; skipped");
        return None;
    }
    let output_schema = match item.get("outputSchema") {
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => {
            tracing::warn!(
                tool = %name,
                "MCP tool has a non-object outputSchema; skipped"
            );
            return None;
        }
        None => None,
    };
    Some(ToolInfo {
        name: name.to_owned(),
        description: item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        input_schema: input_schema.clone(),
        output_schema,
    })
}

async fn list_resources(
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: &CancellationToken,
) -> Vec<ResourceEntry> {
    // A server that advertises resources but fails the list is degraded, not
    // fatal: the toolset is the point of the connection.
    let page = match rpc
        .request(
            RequestMetadata::new("resources/list"),
            None,
            Some(catalog),
            cancel,
        )
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
        .request(
            RequestMetadata::new("resources/templates/list"),
            None,
            Some(catalog),
            cancel,
        )
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
