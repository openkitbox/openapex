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

use crate::config::McpRemoteConfig;

use super::transport::{CallError, Rpc};

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
        let client = crate::http_client()
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
        while let Some(index) = buffer.find("\n\n") {
            let event = buffer[..index].to_owned();
            buffer.drain(..index + 2);
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
                let value: Value = response
                    .json()
                    .await
                    .map_err(|error| CallError::Transport(error.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpRemoteConfig;
    use salvo::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    #[handler]
    async fn json_endpoint(req: &mut Request, res: &mut Response) {
        let call = CALLS.fetch_add(1, Ordering::SeqCst);
        let body: Value = req.parse_json().await.unwrap();
        if body["method"]
            .as_str()
            .unwrap_or_default()
            .starts_with("notifications/")
        {
            res.status_code(StatusCode::ACCEPTED);
            return;
        }
        let session = req
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        res.headers_mut().insert(
            "mcp-session-id",
            if call == 0 {
                "sess-1"
            } else {
                session.as_str()
            }
            .parse()
            .unwrap(),
        );
        res.render(Json(json!({
            "jsonrpc": "2.0",
            "id": body["id"].clone(),
            "result": { "seenSession": session }
        })));
    }

    #[handler]
    async fn sse_endpoint(req: &mut Request, res: &mut Response) {
        let body: Value = req.parse_json().await.unwrap();
        res.render(Text::Plain(format!(
            "event: message\ndata: {}\n\n",
            json!({
                "jsonrpc": "2.0",
                "id": body["id"].clone(),
                "result": { "ok": true }
            })
        )));
        // Text::Plain stamps its own content-type during render; override it
        // after so the client takes the SSE path.
        res.headers_mut()
            .insert("content-type", "text/event-stream".parse().unwrap());
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let port = free_port();
        let handle = tokio::spawn(async move {
            let acceptor = TcpListener::new(format!("127.0.0.1:{port}")).bind().await;
            Server::new(acceptor).serve(router).await;
        });
        // The listener binds inside the spawned task; wait for the socket so
        // the first request cannot race the bind.
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return (format!("http://127.0.0.1:{port}/mcp"), handle);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test server never became ready");
    }

    fn remote(url: String) -> McpRemoteConfig {
        McpRemoteConfig {
            url,
            headers: None,
            disabled: false,
            timeout: crate::config::McpTimeout::default(),
        }
    }

    #[tokio::test]
    async fn json_response_and_session_header_round_trip() {
        CALLS.store(0, Ordering::SeqCst);
        let router = Router::new().push(Router::with_path("mcp").post(json_endpoint));
        let (url, server) = serve(router).await;
        let rpc = HttpRpc::new(&remote(url)).unwrap();
        let cancel = CancellationToken::new();

        let first = rpc
            .request(
                "initialize",
                Some(json!({})),
                Some(Duration::from_secs(5)),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(first["seenSession"], json!(""));

        let second = rpc
            .request("tools/list", None, Some(Duration::from_secs(5)), &cancel)
            .await
            .unwrap();
        assert_eq!(second["seenSession"], json!("sess-1"));

        rpc.notify("notifications/initialized", None).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn sse_response_is_parsed_until_the_matching_id() {
        let router = Router::new().push(Router::with_path("mcp").post(sse_endpoint));
        let (url, server) = serve(router).await;
        let rpc = HttpRpc::new(&remote(url)).unwrap();
        let cancel = CancellationToken::new();
        let value = rpc
            .request(
                "initialize",
                Some(json!({})),
                Some(Duration::from_secs(5)),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(value, json!({"ok": true}));
        server.abort();
    }
}
