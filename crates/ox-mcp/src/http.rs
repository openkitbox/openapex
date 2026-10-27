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

use super::transport::modern_params;
use super::transport::{CallError, ProtocolMode, RequestMetadata, Rpc};

/// A response larger than this is refused instead of buffered. MCP tool
/// results are model input, so an unbounded body is a memory hazard.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct HttpRpc {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    state: Mutex<ConnectionState>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

struct ConnectionState {
    protocol: ProtocolMode,
    session: Option<String>,
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
            state: Mutex::new(ConnectionState {
                protocol: match config.protocol {
                    ox_core::config::McpProtocol::Modern => ProtocolMode::Modern,
                    ox_core::config::McpProtocol::Legacy | ox_core::config::McpProtocol::Auto => {
                        ProtocolMode::Legacy
                    }
                },
                session: None,
            }),
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

    fn builder(&self, body: &Value, metadata: RequestMetadata<'_>) -> reqwest::RequestBuilder {
        let state = self.state.lock().unwrap();
        let protocol = state.protocol;
        let session = state.session.clone();
        drop(state);
        let mut builder = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(body);
        for (key, value) in &self.headers {
            if protocol == ProtocolMode::Modern && key.eq_ignore_ascii_case("mcp-session-id") {
                continue;
            }
            builder = builder.header(key, value);
        }
        match protocol {
            ProtocolMode::Legacy => {
                if let Some(session) = session {
                    builder = builder.header("mcp-session-id", session);
                }
            }
            ProtocolMode::Modern => {
                builder = builder
                    .header(
                        "mcp-protocol-version",
                        super::client::MODERN_PROTOCOL_VERSION,
                    )
                    .header("mcp-method", metadata.method);
                if let Some(tool_name) = metadata.tool_name {
                    builder = builder.header("mcp-name", tool_name);
                }
            }
        }
        builder
    }

    fn remember_session(&self, response: &reqwest::Response) {
        let mut state = self.state.lock().unwrap();
        if state.protocol != ProtocolMode::Legacy {
            return;
        }
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            state.session = Some(session.to_owned());
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

async fn read_body(
    mut response: reqwest::Response,
    deadline: Option<tokio::time::Instant>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, CallError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(CallError::Transport(format!(
            "MCP HTTP response exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .map_or(0, |length| length.min(MAX_BODY_BYTES as u64) as usize),
    );
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => {
                chunk.map_err(|error| CallError::Transport(error.to_string()))?
            }
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            () = sleep_until_optional(deadline) => return Err(CallError::Timeout),
        };
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(CallError::Transport(format!(
                "MCP HTTP response exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
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
    fn set_protocol(&self, protocol: ProtocolMode) {
        let mut state = self.state.lock().unwrap();
        state.protocol = protocol;
        if protocol == ProtocolMode::Modern {
            state.session.take();
        }
    }

    fn request<'a>(
        &'a self,
        metadata: RequestMetadata<'a>,
        params: Option<Value>,
        timeout: Option<Duration>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, CallError>> {
        Box::pin(async move {
            if self.closed.load(Ordering::SeqCst) {
                return Err(CallError::Closed);
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let protocol = self.state.lock().unwrap().protocol;
            let params = match protocol {
                ProtocolMode::Legacy => params,
                ProtocolMode::Modern => modern_params(metadata, params),
            };
            let body = self.body(Some(id), metadata.method, params);
            let deadline = timeout.map(|limit| tokio::time::Instant::now() + limit);
            let response = tokio::select! {
                response = self.builder(&body, metadata).send() => {
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
                let bytes = read_body(response, deadline, cancel).await?;
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
            let metadata = RequestMetadata::new(method);
            let protocol = self.state.lock().unwrap().protocol;
            let params = match protocol {
                ProtocolMode::Legacy => params,
                ProtocolMode::Modern => modern_params(metadata, params),
            };
            let body = self.body(None, method, params);
            let response = self
                .builder(&body, metadata)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ox_core::config::{McpProtocol, McpRemoteConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap().to_ascii_lowercase()
    }

    async fn read_request_with_body(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(bytes).unwrap().to_ascii_lowercase()
    }

    #[tokio::test]
    async fn modern_request_has_protocol_method_and_tool_headers_without_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(request.contains("mcp-protocol-version: 2026-07-28\r\n"));
            assert!(request.contains("mcp-method: tools/call\r\n"));
            assert!(request.contains("mcp-name: echo\r\n"));
            assert!(!request.contains("mcp-session-id:"));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
                )
                .await
                .unwrap();
        });

        let rpc = HttpRpc::new(&McpRemoteConfig {
            url: format!("http://{address}"),
            headers: Some(BTreeMap::from([(
                "mcp-session-id".to_owned(),
                "stale-session".to_owned(),
            )])),
            disabled: false,
            protocol: McpProtocol::Modern,
            timeout: Default::default(),
        })
        .unwrap();
        let cancel = CancellationToken::new();
        rpc.request(
            RequestMetadata::new("tools/call").tool("echo"),
            Some(json!({ "name": "echo", "arguments": {} })),
            Some(Duration::from_secs(1)),
            &cancel,
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn modern_request_body_has_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request_with_body(&mut socket).await;
            assert!(request.contains("\"_meta\""));
            assert!(request.contains("\"protocolversion\":\"2026-07-28\""));
            assert!(request.contains("\"clientinfo\":{\"name\":\"ox\""));
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
                )
                .await
                .unwrap();
        });

        let rpc = HttpRpc::new(&McpRemoteConfig {
            url: format!("http://{address}"),
            headers: None,
            disabled: false,
            protocol: McpProtocol::Modern,
            timeout: Default::default(),
        })
        .unwrap();
        let cancel = CancellationToken::new();
        rpc.request(
            RequestMetadata::new("tools/call").tool("echo"),
            Some(json!({ "name": "echo", "arguments": {} })),
            Some(Duration::from_secs(1)),
            &cancel,
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn legacy_session_is_echoed_and_modern_does_not_save_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nmcp-session-id: legacy-session\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
                .await
                .unwrap();
            drop(socket);

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(request.contains("mcp-session-id: legacy-session\r\n"));
            assert!(!request.contains("mcp-protocol-version:"));
            assert!(!request.contains("mcp-name:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}")
                .await
                .unwrap();
            drop(socket);

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(!request.contains("mcp-session-id:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nmcp-session-id: must-not-be-saved\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}")
                .await
                .unwrap();
            drop(socket);

            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(!request.contains("mcp-session-id:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{}}")
                .await
                .unwrap();
        });

        let rpc = HttpRpc::new(&McpRemoteConfig {
            url: format!("http://{address}"),
            headers: None,
            disabled: false,
            protocol: McpProtocol::Legacy,
            timeout: Default::default(),
        })
        .unwrap();
        let cancel = CancellationToken::new();
        for metadata in [
            RequestMetadata::new("initialize"),
            RequestMetadata::new("tools/list"),
        ] {
            rpc.request(metadata, None, Some(Duration::from_secs(1)), &cancel)
                .await
                .unwrap();
        }
        rpc.set_protocol(ProtocolMode::Modern);
        for _ in 0..2 {
            rpc.request(
                RequestMetadata::new("tools/list"),
                None,
                Some(Duration::from_secs(1)),
                &cancel,
            )
            .await
            .unwrap();
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn json_body_rejects_declared_oversize_before_buffering() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                        MAX_BODY_BYTES + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let rpc = HttpRpc::new(&McpRemoteConfig {
            url: format!("http://{address}"),
            headers: None,
            disabled: false,
            protocol: McpProtocol::Legacy,
            timeout: Default::default(),
        })
        .unwrap();
        let cancel = CancellationToken::new();
        let result = rpc
            .request(
                RequestMetadata::new("tools/list"),
                None,
                Some(Duration::from_secs(1)),
                &cancel,
            )
            .await;
        assert!(matches!(
            result,
            Err(CallError::Transport(message)) if message.contains("exceeds")
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn json_body_read_honors_cancellation_after_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (headers_sent, headers_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n",
                )
                .await
                .unwrap();
            headers_sent.send(()).unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let rpc = HttpRpc::new(&McpRemoteConfig {
            url: format!("http://{address}"),
            headers: None,
            disabled: false,
            protocol: McpProtocol::Legacy,
            timeout: Default::default(),
        })
        .unwrap();
        let cancel = CancellationToken::new();
        let mut request = Box::pin(rpc.request(
            RequestMetadata::new("tools/list"),
            None,
            Some(Duration::from_secs(2)),
            &cancel,
        ));
        tokio::select! {
            result = &mut request => panic!("body read completed unexpectedly: {result:?}"),
            _ = headers_received => cancel.cancel(),
        }
        assert!(matches!(request.await, Err(CallError::Cancelled)));
        server.abort();
    }

    #[tokio::test]
    async fn json_body_read_honors_deadline_after_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let rpc = HttpRpc::new(&McpRemoteConfig {
            url: format!("http://{address}"),
            headers: None,
            disabled: false,
            protocol: McpProtocol::Legacy,
            timeout: Default::default(),
        })
        .unwrap();
        let cancel = CancellationToken::new();
        let result = rpc
            .request(
                RequestMetadata::new("tools/list"),
                None,
                Some(Duration::from_millis(50)),
                &cancel,
            )
            .await;
        assert!(matches!(result, Err(CallError::Timeout)));
        server.abort();
    }
}
