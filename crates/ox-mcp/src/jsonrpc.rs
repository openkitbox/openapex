//! MCP's JSON-RPC 2.0 wire types.
//!
//! MCP stdio framing is one JSON object per line (no `Content-Length`
//! headers); HTTP reuses the same objects inside a POST body or an SSE
//! `data:` payload. Both transports share these types.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const METHOD_NOT_FOUND: i64 = -32601;

/// A client → server request.
#[derive(Debug, Serialize)]
pub(crate) struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A client → server notification (no id, no response).
#[derive(Debug, Serialize)]
pub(crate) struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Any inbound message. A response carries `id` plus `result`/`error`; a
/// server → client request carries both `id` and `method`; a server
/// notification carries `method` only.
#[derive(Debug, Deserialize)]
pub(crate) struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// Serialize one message and append the newline that delimits it on stdio.
pub(crate) fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    // These are plain structs of owned values; serialization cannot fail.
    let mut bytes = serde_json::to_vec(message).expect("JSON-RPC message serializes");
    bytes.push(b'\n');
    bytes
}

/// The JSON-RPC error object answering an unsupported server → client
/// request. The id must be echoed verbatim: it may be a string.
pub(crate) fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
