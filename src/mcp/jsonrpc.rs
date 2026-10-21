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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_without_params_has_no_params_key() {
        let request = Request {
            jsonrpc: "2.0",
            id: 7,
            method: "tools/list".to_owned(),
            params: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/list"})
        );
    }

    #[test]
    fn encode_appends_exactly_one_newline() {
        let bytes = encode(&Notification {
            jsonrpc: "2.0",
            method: "notifications/initialized".to_owned(),
            params: None,
        });
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes[..bytes.len() - 1]).unwrap(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        );
    }

    #[test]
    fn incoming_result_error_and_notification() {
        let result: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#).unwrap();
        assert_eq!(result.id, Some(json!(3)));
        assert!(result.method.is_none());
        assert_eq!(result.result, Some(json!({"tools": []})));

        let error: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"no"}}"#,
        )
        .unwrap();
        assert_eq!(error.error.unwrap().code, -32601);

        let note: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#,
        )
        .unwrap();
        assert!(note.id.is_none());
        assert_eq!(note.method.as_deref(), Some("notifications/message"));
    }

    #[test]
    fn server_request_error_response_keeps_the_original_id() {
        let reply = error_response(&json!("abc"), METHOD_NOT_FOUND, "unsupported");
        assert_eq!(reply["id"], json!("abc"));
        assert_eq!(reply["error"]["code"], json!(METHOD_NOT_FOUND));
    }
}
