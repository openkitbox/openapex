//! Streamable HTTP transport (MCP 2025-03-26 and later).
//!
//! One POST per JSON-RPC message. The response is either a plain JSON body
//! or an SSE stream carrying the response (plus optional notifications, which
//! v1 ignores). The server may hand out `Mcp-Session-Id` on initialize; it is
//! echoed on every later request.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox_core::config::McpRemoteConfig;

use super::transport::{CallError, Rpc};

/// A response larger than this is refused instead of buffered. MCP tool
/// results are model input, so an unbounded body is a memory hazard.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct HttpRpc {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl HttpRpc {
    pub(crate) fn new(config: &McpRemoteConfig) -> Result<Self, String> {
        if config.url.is_empty() {
            return Err("MCP remote server has an empty url".to_owned());
        }
        let client = ox_core::http_client()
            .build()
            .map_err(|error| format!("MCP HTTP client init failed: {error}"))?;
        Ok(Self {
            client,
            url: config.url.clone(),
            headers: config.headers.clone().unwrap_or_default(),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        })
    }

    fn body(&self, id: Option<u64>, method: &str, params: Option<Value>) -> Value {
        let mut body = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(id) = id {
            body["id"] = json!(id);
        }
        if let Some(params) = params {
            body["params"] = params;
        }
        body
    }

    fn builder(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(body);
        for (key, value) in &self.headers {
            builder = builder.header(key, value);
        }
        if let Some(session) = self.session.lock().unwrap().clone() {
            builder = builder.header("mcp-session-id", session);
        }
        builder
    }

    fn remember_session(&self, response: &reqwest::Response) {
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session.lock().unwrap() = Some(session.to_owned());
        }
    }
}

async fn sleep_until_optional(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn parse_response(value: Value) -> Result<Value, CallError> {
    if let Some(error) = value.get("error") {
        return Err(CallError::Rpc(
            serde_json::from_value(error.clone())
                .map_err(|error| CallError::Transport(format!("malformed MCP error: {error}")))?,
        ));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// Offset and byte length of the blank line that ends the next SSE event.
/// Servers may use either LF or CRLF; a CRLF stream never contains `\n\n`.
fn next_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    match (buffer.find("\n\n"), buffer.find("\r\n\r\n")) {
        (Some(lf), Some(crlf)) if crlf <= lf => Some((crlf, 4)),
        (Some(lf), _) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

async fn read_sse(
    mut response: reqwest::Response,
    id: u64,
    deadline: Option<tokio::time::Instant>,
    cancel: &CancellationToken,
) -> Result<Value, CallError> {
    let mut buffer = String::new();
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => {
                chunk.map_err(|error| CallError::Transport(error.to_string()))?
            }
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            () = sleep_until_optional(deadline) => return Err(CallError::Timeout),
        };
        let Some(chunk) = chunk else {
            return Err(CallError::Closed);
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if buffer.len() > MAX_BODY_BYTES {
            return Err(CallError::Transport(format!(
                "MCP SSE response exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        while let Some((index, separator)) = next_event_boundary(&buffer) {
            let event = buffer[..index].to_owned();
            buffer.drain(..index + separator);
            let data = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return parse_response(value);
            }
        }
    }
}

impl Rpc for HttpRpc {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, CallError>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(CallError::Closed);
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let body = self.body(Some(id), method, params);
            let deadline = timeout.map(|limit| tokio::time::Instant::now() + limit);
            let response = tokio::select! {
                response = self.builder(&body).send() => {
                    response.map_err(|error| CallError::Transport(error.to_string()))?
                }
                () = cancel.cancelled() => return Err(CallError::Cancelled),
                () = sleep_until_optional(deadline) => return Err(CallError::Timeout),
            };
            self.remember_session(&response);
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(CallError::Transport(
                    "MCP server requires OAuth authorization (not supported)".to_owned(),
                ));
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(CallError::Closed);
            }
            if !status.is_success() {
                return Err(CallError::Transport(format!("MCP HTTP status {status}")));
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            if content_type.contains("text/event-stream") {
                read_sse(response, id, deadline, cancel).await
            } else {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| CallError::Transport(error.to_string()))?;
                if bytes.len() > MAX_BODY_BYTES {
                    return Err(CallError::Transport(format!(
                        "MCP HTTP response exceeds {MAX_BODY_BYTES} bytes"
                    )));
                }
                let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                    CallError::Transport(format!("malformed MCP response: {error}"))
                })?;
                parse_response(value)
            }
        })
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            let body = self.body(None, method, params);
            let response = self
                .builder(&body)
                .send()
                .await
                .map_err(|error| CallError::Transport(error.to_string()))?;
            self.remember_session(&response);
            if !response.status().is_success() {
                return Err(CallError::Transport(format!(
                    "MCP HTTP status {}",
                    response.status()
                )));
            }
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
        })
    }
}
