#![cfg(unix)]

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use ox_client::Client;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Socket file removed on drop. The path stays short: macOS caps `sun_path`
/// at 104 bytes, so it is not placed under a tempfile directory.
struct SockPath(PathBuf);

impl SockPath {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("oxc-{}-{n}.sock", std::process::id())))
    }
}

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Read up to the end of the request head and return its text.
async fn read_head(stream: &mut UnixStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn over_unix_reaches_the_socket_with_a_json_reply() {
    let sock = SockPath::new();
    let listener = UnixListener::bind(&sock.0).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let head = read_head(&mut stream).await;
        let body = r#"{"ok":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        let _ = stream.shutdown().await;
        head
    });

    let client = Client::over_unix(&sock.0, "/tmp").unwrap();
    assert_eq!(client.base, ox_client::UNIX_BASE);
    let resp = client
        .http
        .get(format!("{}/api/location", client.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["ok"], true);

    let head = server.await.unwrap();
    assert_eq!(head.lines().next(), Some("GET /api/location HTTP/1.1"));
}

#[tokio::test]
async fn over_unix_streams_server_sent_events() {
    let sock = SockPath::new();
    let listener = UnixListener::bind(&sock.0).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_head(&mut stream).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                    transfer-encoding: chunked\r\nconnection: close\r\n\r\n";
        stream.write_all(head.as_bytes()).await.unwrap();
        let frame = "data: hello\n\n";
        let chunk = format!("{:x}\r\n{frame}\r\n", frame.len());
        stream.write_all(chunk.as_bytes()).await.unwrap();
        // Keep the stream open so the client must read the frame incrementally.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });

    let client = Client::over_unix(&sock.0, "/tmp").unwrap();
    let mut resp = client
        .http
        .get(format!("{}/api/event", client.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), resp.chunk())
        .await
        .expect("frame arrives before the stream ends")
        .unwrap()
        .expect("a chunk");
    assert!(String::from_utf8_lossy(&chunk).contains("data: hello"));
}

#[tokio::test]
async fn over_unix_errors_when_the_socket_is_missing() {
    let sock = SockPath::new();
    let client = Client::over_unix(&sock.0, "/tmp").unwrap();
    let result = client
        .http
        .get(format!("{}/api/location", client.base))
        .send()
        .await;
    assert!(result.is_err());
}
