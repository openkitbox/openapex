//! The seam every MCP transport implements, plus the error type the tool
//! adapter renders.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::jsonrpc;

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
}

impl std::fmt::Display for CallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(formatter, "MCP server is not available"),
            Self::Rpc(error) => write!(formatter, "{error}"),
            Self::Timeout => write!(formatter, "MCP request timed out"),
            Self::Cancelled => write!(formatter, "MCP request cancelled"),
            Self::Transport(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for CallError {}

/// One live connection. Implementations are shared through `Arc` by every
/// tool they expose, so request ids must be unique per connection and
/// in-flight calls must be correlated, not serialized.
pub(crate) trait Rpc: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, CallError>>;

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), CallError>>;

    /// Best-effort rejection of every in-flight call and teardown.
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
}
