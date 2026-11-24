//! The seam every MCP transport implements, plus the error type the tool
//! adapter renders.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::jsonrpc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtocolMode {
    Legacy,
    Modern,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestMetadata<'a> {
    pub method: &'a str,
    pub tool_name: Option<&'a str>,
}

impl<'a> RequestMetadata<'a> {
    pub(crate) fn new(method: &'a str) -> Self {
        Self {
            method,
            tool_name: None,
        }
    }

    pub(crate) fn tool(mut self, name: &'a str) -> Self {
        self.tool_name = Some(name);
        self
    }
}

/// A server → client notification, published by each transport to every
/// event-stream listener. Payloads stay raw JSON: only the events layer
/// interprets them, and its consumers treat all of it as untrusted input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ServerNotification {
    pub method: String,
    pub params: Option<Value>,
}

pub(crate) fn modern_params(metadata: RequestMetadata<'_>, params: Option<Value>) -> Option<Value> {
    let mut params = params.unwrap_or_else(|| json!({}));
    let Some(object) = params.as_object_mut() else {
        return Some(params);
    };
    let meta = object
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    let Some(meta) = meta else {
        object.insert("_meta".to_owned(), json!({}));
        return modern_params(metadata, Some(params));
    };
    meta.insert(
        "protocolVersion".to_owned(),
        Value::String(super::client::MODERN_PROTOCOL_VERSION.to_owned()),
    );
    meta.insert(
        "clientInfo".to_owned(),
        json!({
            "name": "ox",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    );
    meta.insert("clientCapabilities".to_owned(), json!({}));
    if let Some(tool_name) = metadata.tool_name {
        meta.insert("toolName".to_owned(), Value::String(tool_name.to_owned()));
    }
    Some(params)
}

#[derive(Debug)]
pub(crate) enum CallError {
    /// The transport is gone (process exited, HTTP 404/closed stream).
    Closed,
    /// The server answered with a JSON-RPC error object.
    Rpc(jsonrpc::RpcError),
    /// The caller's deadline elapsed.
    Timeout,
    /// The turn was interrupted.
    Cancelled,
    /// Anything else (spawn failure, HTTP status, malformed body).
    Transport(String),
    /// The peer answered successfully but does not support the requested
    /// protocol revision.
    Unsupported(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(formatter, "MCP server is not available"),
            Self::Rpc(error) => write!(formatter, "{error}"),
            Self::Timeout => write!(formatter, "MCP request timed out"),
            Self::Cancelled => write!(formatter, "MCP request cancelled"),
            Self::Transport(message) => write!(formatter, "{message}"),
            Self::Unsupported(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CallError {}

/// One live connection. Implementations are shared through `Arc` by every
/// tool they expose, so request ids must be unique per connection and
/// in-flight calls must be correlated, not serialized.
pub(crate) trait Rpc: Send + Sync {
    fn set_protocol(&self, _protocol: ProtocolMode) {}

    fn request<'a>(
        &'a self,
        metadata: RequestMetadata<'a>,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, CallError>>;

    /// Same as [`Self::request`], but reports the allocated JSON-RPC id before
    /// sending. SEP-3415 names a stream's notifications by this id.
    fn request_with_id<'a>(
        &'a self,
        metadata: RequestMetadata<'a>,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancel: &'a CancellationToken,
        observe_id: Option<&'a (dyn Fn(u64) + Send + Sync)>,
    ) -> BoxFuture<'a, Result<Value, CallError>>;

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), CallError>>;

    /// A receiver of server notifications. Created per listener; a lagged
    /// receiver signals that replay from a cursor is required.
    fn subscribe_notifications(&self) -> broadcast::Receiver<ServerNotification>;

    /// Best-effort rejection of every in-flight call and teardown.
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
}
