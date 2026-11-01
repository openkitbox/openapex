//! Minimal SSE parser. ox's stream is `data: {json}\n\n` plus `: heartbeat`
//! comments; we deliberately avoid an eventsource dependency (~40 lines).

#[derive(Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; returns every complete `data:` payload decoded in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        // Normalize CRLF so frame detection only has to look for `\n\n`.
        for byte in chunk {
            match *byte {
                b'\r' => {}
                b => self.buf.push(b),
            }
        }

        let mut out = Vec::new();
        while let Some(pos) = find(&self.buf, b"\n\n") {
            let frame = String::from_utf8_lossy(&self.buf[..pos]).to_string();
            self.buf.drain(..pos + 2);

            let mut data_lines = Vec::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
                // `:` comments (heartbeats), `event:`, `id:`, `retry:` are ignored:
                // the envelope JSON already carries id/type.
            }
            if !data_lines.is_empty() {
                out.push(data_lines.join("\n"));
            }
        }
        out
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

use std::time::Duration;

use futures_util::StreamExt;
use ox_schema::event::EventEnvelope;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::Client;

const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Longest slice of a malformed frame echoed into the log.
const LOG_FRAME_BYTES: usize = 200;

/// Pause before the next attempt and the delay to remember afterwards:
/// a connection that was established resets the backoff, a failed attempt
/// doubles it up to [`MAX_RECONNECT_DELAY`].
fn backoff(delay: Duration, connected: bool) -> (Duration, Duration) {
    if connected {
        (MIN_RECONNECT_DELAY, MIN_RECONNECT_DELAY)
    } else {
        (delay, (delay * 2).min(MAX_RECONNECT_DELAY))
    }
}

/// `s` cut to at most `max` bytes without splitting a character.
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(max)]
}

#[derive(Debug)]
pub enum StreamMsg {
    /// The stream is connected again after a drop; the caller should refetch
    /// history. Not sent for the first connection or while the server is down.
    Reconnected,
    Event(Box<EventEnvelope<Value>>),
}

/// Spawn the reconnect loop. `seq_rx` carries the focused session's last
/// durable seq (updated by the store as events arrive; per-session only).
pub fn spawn_events(
    client: Client,
    session: String,
    seq_rx: watch::Receiver<Option<i64>>,
    tx: mpsc::UnboundedSender<StreamMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reconnecting = false;
        let mut delay = MIN_RECONNECT_DELAY;
        loop {
            let mut connected = false;
            let attempt = {
                let seq = *seq_rx.borrow();
                stream_once(&client, &session, seq, &tx, reconnecting, &mut connected).await
            };
            if let Err(err) = attempt {
                tracing::warn!(%err, "event stream dropped");
            } else {
                tracing::info!("event stream closed, reconnecting");
            }
            if tx.is_closed() {
                break; // receiver gone
            }
            reconnecting = true;
            let (pause, next) = backoff(delay, connected);
            delay = next;
            tokio::time::sleep(pause).await;
        }
    })
}

/// The one live event stream of the focused session. Switching sessions
/// aborts the old reconnect loop and starts a new one, so at most one task
/// ever feeds `tx`; the caller resets `seq_rx`'s sender to `None` first so the
/// new stream starts from the live tail instead of the old session's cursor.
pub struct Attachment {
    client: Client,
    seq_rx: watch::Receiver<Option<i64>>,
    tx: mpsc::UnboundedSender<StreamMsg>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Attachment {
    /// Nothing is connected until [`Attachment::attach`].
    pub fn new(
        client: Client,
        seq_rx: watch::Receiver<Option<i64>>,
        tx: mpsc::UnboundedSender<StreamMsg>,
    ) -> Self {
        Self {
            client,
            seq_rx,
            tx,
            task: None,
        }
    }

    /// Follow `session`, dropping whatever stream was attached before.
    pub fn attach(&mut self, session: &str) {
        self.detach();
        self.task = Some(spawn_events(
            self.client.clone(),
            session.to_owned(),
            self.seq_rx.clone(),
            self.tx.clone(),
        ));
    }

    /// True while a stream task is alive (it may be between reconnects).
    pub fn is_attached(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    /// Stop following; the next [`Attachment::attach`] starts a fresh stream.
    pub fn detach(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.detach();
    }
}

async fn stream_once(
    client: &Client,
    session: &str,
    replay_after: Option<i64>,
    tx: &mpsc::UnboundedSender<StreamMsg>,
    reconnecting: bool,
    connected: &mut bool,
) -> crate::Result<()> {
    let url = match replay_after {
        Some(after) => format!("{}/api/event?session={session}&after={after}", client.base),
        None => format!("{}/api/event", client.base),
    };

    let resp = client
        .http
        .get(url)
        .header("x-opencode-directory", &client.directory)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::Error::Status(resp.status()));
    }

    *connected = true;
    if reconnecting && tx.send(StreamMsg::Reconnected).is_err() {
        return Ok(());
    }

    let mut stream = resp.bytes_stream();
    let mut parser = FrameParser::new();

    loop {
        let next = tokio::time::timeout(HEARTBEAT_TIMEOUT, stream.next()).await;
        match next {
            Err(_) => return Err(crate::Error::HeartbeatTimeout),
            Ok(None) => return Err(crate::Error::StreamClosed),
            Ok(Some(Err(err))) => return Err(crate::Error::Http(err)),
            Ok(Some(Ok(bytes))) => {
                for data in parser.feed(&bytes) {
                    match serde_json::from_str::<EventEnvelope<Value>>(&data) {
                        Ok(envelope) => {
                            if tx.send(StreamMsg::Event(Box::new(envelope))).is_err() {
                                return Ok(());
                            }
                        }
                        Err(err) => {
                            tracing::warn!(%err, data = %truncate_on_char_boundary(&data, LOG_FRAME_BYTES), "skipping malformed event frame");
                        }
                    }
                }
            }
        }
    }
}
