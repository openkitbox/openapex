//! Integration test: [`CardinalClient`] against a mock cardinal that speaks
//! the exact HTTP/SSE dialect of cardinal's gateway (`POST /v1/chat/
//! completions`, Bearer auth, `data:` chunks with `choices[].delta`,
//! usage-only trailing chunk, `data: [DONE]`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ox::cardinal::client::CardinalClient;
use ox::cardinal::stream::StreamEvent;
use ox::cardinal::wire::{ChatMessage, ChatRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// One canned write to the socket, with an optional pause before it.
struct Write {
    bytes: String,
    pause_before: Option<Duration>,
}

impl Write {
    fn now(bytes: impl Into<String>) -> Self {
        Self {
            bytes: bytes.into(),
            pause_before: None,
        }
    }

    fn after_pause(bytes: impl Into<String>, pause: Duration) -> Self {
        Self {
            bytes: bytes.into(),
            pause_before: Some(pause),
        }
    }
}

/// A mock cardinal: captures the first request it receives, replays the
/// scripted writes, then closes the connection.
async fn spawn_mock(
    writes: Vec<Write>,
) -> (SocketAddr, Arc<Mutex<Option<String>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        *capture.lock().await = Some(request);
        for write in writes {
            if let Some(pause) = write.pause_before {
                tokio::time::sleep(pause).await;
            }
            socket.write_all(write.bytes.as_bytes()).await.ok();
        }
        // Dropping the socket closes the connection.
    });
    (addr, captured, handle)
}

/// Read one HTTP request (head + content-length body) from the socket.
async fn read_request(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if socket.read_exact(&mut byte).await.is_err() {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        socket.read_exact(&mut body).await.ok();
    }
    format!("{head}{}", String::from_utf8_lossy(&body))
}

fn sse_head(status: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
}

/// One cardinal-style SSE event.
fn event(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

fn test_request() -> ChatRequest {
    ChatRequest::new("glm-4.7", vec![ChatMessage::user("hello")])
}

#[tokio::test]
async fn streams_chat_assembles_and_sends_canonical_request() {
    // Chunk sequence modeled on cardinal's emission: role+text deltas, a
    // keep-alive comment, a multi-fragment tool call, a finish_reason chunk,
    // the usage-only trailing chunk, and [DONE].
    let head = sse_head("200 OK");
    let writes = vec![
        Write::now(format!(
            "{head}: keep-alive\n\n{}",
            event(
                r#"{"choices":[{"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#
            )
        )),
        Write::now(event(
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#,
        )),
        Write::now(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"p\":"}}]},"finish_reason":null}]}"#,
        )),
        Write::now(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":null}]}"#,
        )),
        Write::now(event(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        )),
        Write::now(event(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        )),
        Write::now(event("[DONE]")),
    ];
    let (addr, captured, server) = spawn_mock(writes).await;

    let client = CardinalClient::new(format!("http://{addr}"), Some("sk-ox-test".into())).unwrap();
    let message = client
        .stream_chat(&test_request())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(message.text, "Hello");
    assert_eq!(message.tool_calls.len(), 1);
    assert_eq!(message.tool_calls[0].id, "call_1");
    assert_eq!(message.tool_calls[0].name, "read");
    assert_eq!(message.tool_calls[0].arguments, "{\"p\":1}");
    assert_eq!(message.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(message.usage.map(|usage| usage.prompt_tokens), Some(10));
    assert_eq!(message.usage.map(|usage| usage.completion_tokens), Some(5));
    assert!(message.done);

    // The request hit cardinal's route with Bearer auth, the SSE accept
    // header, and canonical streaming flags.
    let request = captured.lock().await.clone().unwrap();
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-ox-test")
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("accept: text/event-stream")
    );
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "glm-4.7");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(
        body.get("tool_choice").is_none(),
        "tool_choice is omitted when no tools are offered"
    );

    server.await.unwrap();
}

#[tokio::test]
async fn http_error_maps_to_typed_error_with_body_snippet() {
    // No Content-Length: reqwest reads the error body until EOF.
    let head =
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
    let writes = vec![Write::now(format!(
        "{head}{{\"error\":{{\"message\":\"invalid key\"}}}}"
    ))];
    let (addr, _captured, server) = spawn_mock(writes).await;

    let client = CardinalClient::new(format!("http://{addr}"), Some("wrong".into())).unwrap();
    let error = client
        .stream_chat(&test_request())
        .await
        .expect_err("401 must be an error");
    assert!(
        matches!(&error, ox::cardinal::CardinalError::Http { status, body }
            if *status == 401 && body.contains("invalid key")),
        "unexpected error: {error}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn connection_closed_before_done_is_an_error() {
    // One real delta, then the socket closes without [DONE] (the server
    // script has no further writes, so the connection drops immediately).
    let writes = vec![Write::now(format!(
        "{}{}",
        sse_head("200 OK"),
        event(r#"{"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}"#)
    ))];
    let (addr, _captured, server) = spawn_mock(writes).await;

    let client = CardinalClient::new(format!("http://{addr}"), None).unwrap();
    let mut stream = client.stream_chat(&test_request()).await.unwrap();
    let first = stream.next_event().await.unwrap().unwrap();
    assert_eq!(first, StreamEvent::TextDelta("partial".to_owned()));
    // EOF without [DONE]: the next poll reports the early close.
    let error = stream.next_event().await.unwrap().err().unwrap();
    assert!(
        matches!(error, ox::cardinal::CardinalError::EarlyClose),
        "unexpected error: {error}"
    );
    assert!(stream.next_event().await.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn eof_after_finish_reason_ends_cleanly_without_done() {
    // Cardinal's verdict: a seen finish_reason is a terminal even when
    // [DONE] never arrives. Text + Finish + usage-only chunk, then the
    // socket closes — the assembled message must survive with done=true.
    let head = sse_head("200 OK");
    let writes = vec![Write::now(format!(
        "{head}{}{}{}",
        event(r#"{"choices":[{"delta":{"content":"done text"},"finish_reason":null}]}"#),
        event(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
        event(r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#)
    ))];
    let (addr, _captured, server) = spawn_mock(writes).await;

    let client = CardinalClient::new(format!("http://{addr}"), None).unwrap();
    let message = client
        .stream_chat(&test_request())
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(message.text, "done text");
    assert_eq!(message.finish_reason.as_deref(), Some("stop"));
    assert_eq!(message.usage.map(|usage| usage.completion_tokens), Some(2));
    assert!(message.done, "synthetic Done must close the assembly");
    assert!(message.tool_calls.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn dropping_stream_mid_flight_is_clean() {
    // Server dribbles chunks with pauses; the client drops after the first
    // event. Nothing panics; the server task finishes normally.
    let head = sse_head("200 OK");
    let writes = vec![
        Write::now(format!(
            "{head}{}",
            event(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":null}]}"#)
        )),
        Write::after_pause(
            event(r#"{"choices":[{"delta":{"content":"b"},"finish_reason":null}]}"#),
            Duration::from_millis(50),
        ),
        Write::after_pause(event("[DONE]"), Duration::from_millis(50)),
    ];
    let (addr, _captured, server) = spawn_mock(writes).await;

    let client = CardinalClient::new(format!("http://{addr}"), None).unwrap();
    let mut stream = client.stream_chat(&test_request()).await.unwrap();
    let first = stream.next_event().await.unwrap().unwrap();
    assert_eq!(first, StreamEvent::TextDelta("a".to_owned()));
    drop(stream);
    tokio::time::sleep(Duration::from_millis(150)).await;
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_token_ends_the_stream() {
    let head = sse_head("200 OK");
    let writes = vec![
        Write::now(format!(
            "{head}{}",
            event(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":null}]}"#)
        )),
        Write::after_pause(
            event(r#"{"choices":[{"delta":{"content":"b"},"finish_reason":null}]}"#),
            Duration::from_secs(60),
        ),
    ];
    let (addr, _captured, server) = spawn_mock(writes).await;

    let client = CardinalClient::new(format!("http://{addr}"), None).unwrap();
    let mut stream = client.stream_chat(&test_request()).await.unwrap();
    let token = CancellationToken::new();
    stream.cancel_with(token.clone());

    let first = stream.next_event().await.unwrap().unwrap();
    assert_eq!(first, StreamEvent::TextDelta("a".to_owned()));

    token.cancel();
    // The cancelled stream ends (None) instead of waiting on the next chunk.
    let started = std::time::Instant::now();
    assert!(stream.next_event().await.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancelled stream must not block on the held-open connection"
    );
    server.abort();
}
