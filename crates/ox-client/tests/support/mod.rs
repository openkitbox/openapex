#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// In-process mock of the ox HTTP API. Register responses by path prefix, then
/// point a [`ox_client::Client`] at the address returned by [`MockOx::start`].
#[derive(Clone, Default)]
pub struct MockOx {
    requests: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockOx {
    /// Register a JSON response for every request path starting with `prefix`.
    ///
    /// When several registered prefixes match, the longest one wins (ties go to
    /// the first registration), so `/api/session/ses_1/message` is not shadowed
    /// by `/api/session` regardless of registration order.
    pub fn reply(&self, prefix: impl Into<String>, json_body: &str) {
        self.responses
            .lock()
            .unwrap()
            .push((prefix.into(), http_response(200, "OK", json_body)));
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Bind an ephemeral port and serve until the process exits.
    pub async fn start(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let this = self.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let this = this.clone();
                tokio::spawn(async move {
                    let request = match read_request(&mut stream).await {
                        Some(request) => request,
                        None => {
                            let _ = stream.shutdown().await;
                            return;
                        }
                    };
                    let head = String::from_utf8_lossy(&request).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    this.requests.lock().unwrap().push(head);
                    let response = {
                        let responses = this.responses.lock().unwrap();
                        responses
                            .iter()
                            .filter(|(prefix, _)| path.starts_with(prefix))
                            .fold(None::<&(String, String)>, |best, candidate| match best {
                                // `>=` keeps the first registration on a tie.
                                Some(best) if best.0.len() >= candidate.0.len() => Some(best),
                                _ => Some(candidate),
                            })
                            .map(|(_, response)| response.clone())
                    }
                    .unwrap_or_else(|| http_response(404, "Not Found", r#"{"error":"not found"}"#));
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }
}

/// Read one HTTP request and return its head plus body, decoded lossily.
///
/// The mock only supports `content-length` framed bodies (what `reqwest` sends
/// for `.json()` requests); chunked transfer-encoding is deliberately not
/// implemented. Returns `None` on EOF, I/O error, or an oversized request.
async fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    const MAX_REQUEST: usize = 1024 * 1024;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];

    // Phase 1: read until the header terminator appears anywhere in the buffer.
    let head_end = loop {
        if let Some(idx) = find(&buf, b"\r\n\r\n") {
            break idx + b"\r\n\r\n".len();
        }
        if buf.len() >= MAX_REQUEST {
            return None;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };

    // Phase 2: read exactly `content-length` bytes after the terminator, reusing
    // whatever body bytes already arrived alongside the headers. Missing header
    // (GET, DELETE) means an empty body.
    let content_length = content_length(&buf[..head_end]).unwrap_or(0);
    let wanted = head_end.saturating_add(content_length).min(MAX_REQUEST);
    while buf.len() < wanted {
        let take = chunk.len().min(wanted - buf.len());
        match stream.read(&mut chunk[..take]).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }

    Some(buf)
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse `content-length` case-insensitively from the request head.
fn content_length(head: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(head)
        .split("\r\n")
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
}

fn http_response(status: u16, reason: &str, json_body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\
         \r\n\
         {json_body}",
        json_body.len()
    )
}
