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
    /// Durable aggregate identity, when present.
    pub aggregate_id: Option<String>,
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
        let body = response.text().await.map_err(internal)?;
        return Err(internal(format!(
            "event stream `{url}` answered {status}: {body}"
        )));
    }
    let stream = EventStream {
        inner: FrameDecoder {
            inner: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            done: false,
        },
    };
    Ok(stream)
}

impl EventStream {
    /// Next frame, or `Ok(None)` when the stream ended. Framing and JSON
    /// errors are returned to the turn so it can fail closed rather than
    /// reconnecting forever. The initial `server.connected` hello is yielded
    /// like any other frame; consumers drop it (it carries no `sessionID`).
    pub async fn next(&mut self) -> anyhow::Result<Option<Frame>> {
        use futures_util::StreamExt;
        match self.inner.next().await {
            Some(Ok(frame)) => Ok(Some(frame)),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        }
    }
}

/// Index of the `\n\n` event separator, if a complete frame is buffered.
fn find_boundary(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\n\n")
}

/// Parse one framed SSE event body (`data:` lines joined by newlines).
fn decode(body: &str) -> anyhow::Result<Frame> {
    let mut value: Value = serde_json::from_str(body)
        .map_err(|error| anyhow::anyhow!("malformed SSE data JSON: {error}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("malformed SSE data: expected a JSON object"))?;
    let kind = object
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("malformed SSE data: missing string type"))?;
    let seq = object
        .get("durable")
        .and_then(|durable| durable.get("seq"))
        .and_then(Value::as_i64);
    let aggregate_id = object
        .get("durable")
        .and_then(|durable| durable.get("aggregateID"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let data = object.remove("data").unwrap_or(Value::Null);
    Ok(Frame {
        seq,
        aggregate_id,
        kind,
        data,
    })
}

fn decode_event(event: &[u8]) -> anyhow::Result<Option<Frame>> {
    let mut body = String::new();
    let mut has_data = false;
    for line in event.split(|byte| *byte == b'\n') {
        let line = std::str::from_utf8(line)
            .map_err(|error| anyhow::anyhow!("malformed SSE UTF-8: {error}"))?;
        if let Some(data) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        {
            has_data = true;
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(data);
        }
    }
    if !has_data {
        return Ok(None);
    }
    decode(&body).map(Some)
}

/// SSE framing: events are separated by blank lines; `data:` lines carry the
/// JSON payload; comments (`: heartbeat`) and other fields are ignored.
///
/// The buffer is bytes, not `String`: a transport chunk can split a multibyte
/// UTF-8 sequence, and only a complete framed event is guaranteed valid UTF-8
/// (`data: {json}` is ASCII framing around UTF-8 payload bytes).
struct FrameDecoder {
    inner: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: Vec<u8>,
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
            if let Some(boundary) = find_boundary(&this.buffer) {
                let event: Vec<u8> = this.buffer.drain(..boundary + 2).collect();
                return match decode_event(&event) {
                    Ok(Some(frame)) => std::task::Poll::Ready(Some(Ok(frame))),
                    Ok(None) => continue,
                    Err(error) => std::task::Poll::Ready(Some(Err(error))),
                };
            }
            if this.done {
                return std::task::Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => this.done = true,
                std::task::Poll::Ready(Some(Ok(chunk))) => this.buffer.extend_from_slice(&chunk),
                std::task::Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return std::task::Poll::Ready(Some(Err(error.into())));
                }
            }
            if this.done && !this.buffer.is_empty() {
                let buffered = this.buffer.len();
                this.buffer.clear();
                return std::task::Poll::Ready(Some(Err(anyhow::anyhow!(
                    "incomplete SSE frame at EOF ({buffered} bytes buffered)"
                ))));
            }
        }
    }
}
