//! SSE reader for `GET /api/event?session=<id>&after=<seq>`.
//!
//! The endpoint serves the upstream volatile stream when called without
//! parameters; with `session` + `after` it first replays that session's
//! durable log (exclusive cursor) and then continues live with no-dup
//! semantics at the boundary (header of `src/api/event.rs`). Frames are
//! `data: {json}\n\n` with an immediate `server.connected` frame and
//! `: heartbeat` comments every 15s.
//!
//! Reconnect discipline mirrors the server's own contract: a dropped or
//! lagged stream is reopened with `after=<last durable seq seen this turn>`
//! so durable events are not duplicated; ephemeral deltas (text/reasoning)
//! have no seq and a gap across a reconnect matches upstream's volatile
//! contract.

use futures_util::Stream;
use serde_json::Value;

use super::internal;

/// One decoded event envelope: the flattened upstream `V2Event`.
#[derive(Debug)]
pub struct Frame {
    /// Durable sequence number; `None` on ephemeral frames and on the
    /// initial `server.connected` frame.
    pub seq: Option<i64>,
    /// Event type name, e.g. `session.text.delta`.
    pub kind: String,
    /// The event payload.
    pub data: Value,
}

/// A live SSE stream of [`Frame`]s, ending on transport errors (the caller
/// reconnects with its cursor per the server's documented repair path).
pub struct EventStream {
    inner: FrameDecoder,
}

/// Open (or reopen) the event stream for `session_id`.
///
/// `after` replays the durable log beyond the cursor; `None` connects live
/// only. Returns as soon as the response headers are in — never waits for
/// the first frame (the caller POSTs the prompt right after, and the hello
/// frame arrives as part of the stream).
pub async fn open(
    http: &reqwest::Client,
    base: &str,
    session_id: Option<&str>,
    after: Option<i64>,
) -> Result<EventStream, agent_client_protocol::Error> {
    // Scoped streams require the cursor (`session` without `after` is a 400:
    // "pass `after=0`"), so a live-only subscription uses the global stream
    // and filters by session client-side.
    let url = match (session_id, after) {
        (Some(session), Some(seq)) => format!("{base}/api/event?session={session}&after={seq}"),
        _ => format!("{base}/api/event"),
    };
    let response = match http.get(&url).send().await {
        Ok(response) => response,
        Err(error) => return Err(internal(format!("event stream `{url}` failed: {error}"))),
    };
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(internal(format!(
            "event stream `{url}` answered {status}: {body}"
        )));
    }
    let stream = EventStream {
        inner: FrameDecoder {
            inner: Box::pin(response.bytes_stream()),
            buffer: String::new(),
            done: false,
        },
    };
    Ok(stream)
}

impl EventStream {
    /// Next frame, or `None` when the stream ended or errored. Decode errors
    /// are logged and skipped. The initial `server.connected` hello is
    /// yielded like any other frame; consumers drop it (it carries no
    /// `sessionID`).
    pub async fn next(&mut self) -> Option<Frame> {
        use futures_util::StreamExt;
        loop {
            match self.inner.next().await {
                Some(Ok(frame)) => return Some(frame),
                Some(Err(error)) => {
                    tracing::debug!(error = %error, "event stream decode error");
                }
                None => return None,
            }
        }
    }
}

/// Parse one framed SSE event body (`data:` lines joined by newlines).
fn decode(body: &str) -> Option<Frame> {
    let value: Value = serde_json::from_str(body).ok()?;
    let kind = value.get("type")?.as_str()?.to_string();
    let seq = value.get("id").and_then(Value::as_i64);
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    Some(Frame { seq, kind, data })
}

/// SSE framing: events are separated by blank lines; `data:` lines carry the
/// JSON payload; comments (`: heartbeat`) and other fields are ignored.
struct FrameDecoder {
    inner: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: String,
    done: bool,
}

impl Stream for FrameDecoder {
    type Item = anyhow::Result<Frame>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(boundary) = this.buffer.find("\n\n") {
                let event: String = this.buffer.drain(..boundary + 2).collect();
                let body = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .collect::<Vec<_>>()
                    .join("\n");
                return match decode(&body) {
                    Some(frame) => std::task::Poll::Ready(Some(Ok(frame))),
                    // Framed but not an event we understand (comment-only).
                    None => continue,
                };
            }
            if this.done {
                return std::task::Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => this.done = true,
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    match std::str::from_utf8(&chunk) {
                        Ok(text) => this.buffer.push_str(text),
                        Err(error) => {
                            this.done = true;
                            return std::task::Poll::Ready(Some(Err(error.into())));
                        }
                    };
                }
                std::task::Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return std::task::Poll::Ready(Some(Err(error.into())));
                }
            }
        }
    }
}
