//! Transport liveness: `CardinalClient::with_stall_timeout` against a TCP
//! mock that withholds bytes. The timeout is a *silence* bound (reqwest
//! `read_timeout`), not a total deadline, and it surfaces as
//! `CardinalError::Transport`.

use std::net::SocketAddr;
use std::time::Duration;

use ox::cardinal::CardinalError;
use ox::cardinal::client::CardinalClient;
use ox::cardinal::stream::StreamEvent;
use ox::cardinal::wire::ChatRequest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
const BOUND: Duration = Duration::from_secs(5);
const STALL: Duration = Duration::from_millis(200);

fn event(text: &str) -> String {
    format!(
        "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n"
    )
}

const FINISH: &str = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// Read one HTTP request (head + content-length body), discarding it.
async fn drain_request(socket: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if socket.read_exact(&mut byte).await.is_err() {
            return;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    socket.read_exact(&mut body).await.ok();
}

/// Accept one connection, drain the request, then run `script` on the socket.
/// The socket stays open until `script` returns (dropping it closes).
async fn mock<F, Fut>(script: F) -> (SocketAddr, JoinHandle<()>)
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        drain_request(&mut socket).await;
        script(socket).await;
    });
    (addr, handle)
}

fn client(addr: SocketAddr, stall: Option<Duration>) -> CardinalClient {
    CardinalClient::with_stall_timeout(format!("http://{addr}"), None, stall).unwrap()
}

fn request() -> ChatRequest {
    ChatRequest::new("m", Vec::new())
}

fn is_transport<T>(result: &Result<T, CardinalError>) -> bool {
    matches!(result, Err(CardinalError::Transport(_)))
}

#[tokio::test]
async fn no_response_headers_within_the_stall_window_is_a_transport_error() {
    // Hold the socket open (silent) well past the stall window.
    let (addr, mock) = mock(|socket| async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(socket);
    })
    .await;
    let result = tokio::time::timeout(BOUND, client(addr, Some(STALL)).stream_chat(&request()))
        .await
        .expect("must fail within the bound, not hang");
    assert!(is_transport(&result), "got {:?}", result.err());
    mock.abort();
}

#[tokio::test]
async fn silence_after_headers_yields_the_event_then_a_transport_error() {
    let (addr, mock) = mock(|mut socket| async move {
        socket
            .write_all(format!("{HEAD}{}", event("hi")).as_bytes())
            .await
            .ok();
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(socket);
    })
    .await;
    let mut stream = tokio::time::timeout(BOUND, client(addr, Some(STALL)).stream_chat(&request()))
        .await
        .expect("headers arrive")
        .expect("stream opens");
    let first = tokio::time::timeout(BOUND, stream.next_event())
        .await
        .expect("first event arrives")
        .expect("an item")
        .expect("a good event");
    assert!(
        matches!(first, StreamEvent::TextDelta(ref t) if t == "hi"),
        "got {first:?}"
    );
    let second = tokio::time::timeout(BOUND, stream.next_event())
        .await
        .expect("the stall must surface within the bound")
        .expect("an item");
    assert!(is_transport(&second), "got {second:?}");
    mock.abort();
}

#[tokio::test]
async fn a_slow_but_alive_stream_is_not_killed() {
    // 6 events, 100ms apart (600ms total) with a 300ms silence bound.
    let (addr, mock) = mock(|mut socket| async move {
        socket.write_all(HEAD.as_bytes()).await.ok();
        for i in 0..6 {
            socket
                .write_all(event(&i.to_string()).as_bytes())
                .await
                .ok();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        socket.write_all(FINISH.as_bytes()).await.ok();
    })
    .await;
    let stall = Some(Duration::from_millis(300));
    let message = tokio::time::timeout(BOUND, async {
        client(addr, stall)
            .stream_chat(&request())
            .await?
            .collect()
            .await
    })
    .await
    .expect("completes within the bound")
    .expect("a total runtime above the stall bound still succeeds");
    assert_eq!(message.text, "012345");
    assert!(message.done);
    mock.abort();
}

#[tokio::test]
async fn without_a_stall_timeout_slow_headers_still_succeed() {
    let (addr, mock) = mock(|mut socket| async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        socket
            .write_all(format!("{HEAD}{}{FINISH}", event("ok")).as_bytes())
            .await
            .ok();
    })
    .await;
    let message = tokio::time::timeout(BOUND, async {
        client(addr, None)
            .stream_chat(&request())
            .await?
            .collect()
            .await
    })
    .await
    .expect("completes within the bound")
    .expect("no stall timeout configured: unchanged behavior");
    assert_eq!(message.text, "ok");
    mock.abort();
}
