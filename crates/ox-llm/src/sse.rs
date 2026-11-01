//! SSE framing and the streaming-response driver shared by every chat
//! protocol.
//!
//! ox speaks two wire protocols whose *bytes* are framed identically and whose
//! *payloads* agree on nothing: chat completions sends `choices[].delta`,
//! Messages sends `message_start` / `content_block_delta` / `message_delta`.
//! Both are `data:`-only server-sent events, so framing is shared here while
//! decoding stays protocol-local behind [`SseDecoder`].
//!
//! [`SseFramer`] turns bytes into payloads; [`SseChatStream`] drives a live
//! `reqwest::Response` through a decoder and yields [`StreamEvent`]s with one
//! terminal contract for both protocols: a stream that ends without any
//! terminal signal is [`ChatError::EarlyClose`], and a seen terminal event ends
//! it cleanly even if the explicit terminator never arrives.
//!
//! # Cancellation
//!
//! Dropping an [`SseChatStream`] drops the underlying `reqwest::Response`,
//! which aborts the HTTP connection — no extra shutdown step is needed. For
//! cooperative cancellation from another task, pass a [`CancellationToken`] to
//! [`SseChatStream::cancel_with`].

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::cardinal::stream::StreamEvent;
use crate::error::ChatError;

/// Maximum bytes in one SSE line.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;
/// Maximum bytes accumulated for one SSE event.
pub(crate) const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// The payload name every Messages stream *may* carry in an `event:` field.
/// Kept here because both decoders rely on the payload's own `type`.
const DATA_FIELD: &[u8] = b"data:";

/// Turns one SSE `data:` payload into zero or more [`StreamEvent`]s.
pub trait SseDecoder {
    /// Decode `payload`, appending events to `out`.
    fn decode(&mut self, payload: &str, out: &mut Vec<StreamEvent>) -> Result<(), ChatError>;
}

/// Incremental SSE framer: feed bytes, receive complete `data` payloads.
///
/// Only the `data` field is collected; `event:`/`id:` and any unknown field
/// are ignored (both protocols repeat the type inside the payload), and
/// `:`-prefixed comment lines (keep-alives) are dropped.
#[derive(Debug, Default)]
pub struct SseFramer {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    skip_lf: bool,
    started: bool,
}

impl SseFramer {
    /// A fresh framer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte chunk, appending every completed payload to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<String>) -> Result<(), ChatError> {
        let mut bytes = chunk;
        if !self.started {
            self.started = true;
            if bytes.len() >= 3 && bytes[..3] == [0xEF, 0xBB, 0xBF] {
                bytes = &bytes[3..];
            }
        }
        for byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(out)?,
                b'\r' => {
                    self.end_line(out)?;
                    self.skip_lf = true;
                }
                _ => {
                    self.line.push(*byte);
                    if self.line.len() > MAX_LINE_BYTES {
                        return Err(ChatError::Sse(format!(
                            "SSE line exceeds {MAX_LINE_BYTES} bytes"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Flush a trailing partial line/event (a connection that closed without a
    /// final blank line). Returns any final payload via `out`.
    pub fn finish(&mut self, out: &mut Vec<String>) {
        if !self.line.is_empty() {
            // Ignore framing errors on the flush path: the event boundary is
            // best-effort, and an oversized trailing line is reported on the
            // next `push` anyway.
            let _ = self.end_line(out);
        }
        if !self.data.is_empty() {
            out.push(self.take_event());
        }
    }

    fn end_line(&mut self, out: &mut Vec<String>) -> Result<(), ChatError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(self.take_event());
            }
            return Ok(());
        }
        // Comment (keep-alive) lines: a leading `:` means the whole line is a
        // comment.
        if line[0] == b':' {
            return Ok(());
        }
        let value = match line.iter().position(|byte| *byte == b':') {
            Some(position) => {
                let mut value = &line[position + 1..];
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                value
            }
            // A line without a colon is a field with an empty value; for a
            // `data`-only consumer it carries nothing.
            None => return Ok(()),
        };
        if line[0] != b'd' || !line.starts_with(DATA_FIELD) {
            // `event:`, `id:`, and unknown fields are ignored.
            return Ok(());
        }
        let value = std::str::from_utf8(value)
            .map_err(|_| ChatError::Sse("SSE payload is not valid UTF-8".to_owned()))?;
        self.event_bytes += value.len() + 1;
        if self.event_bytes > MAX_EVENT_BYTES {
            return Err(ChatError::Sse(format!(
                "SSE event exceeds {MAX_EVENT_BYTES} bytes"
            )));
        }
        self.data.extend_from_slice(value.as_bytes());
        self.data.push(b'\n');
        Ok(())
    }

    fn take_event(&mut self) -> String {
        if self.data.last() == Some(&b'\n') {
            self.data.pop();
        }
        self.event_bytes = 0;
        let payload = String::from_utf8_lossy(&self.data).into_owned();
        self.data.clear();
        payload
    }
}

/// A live streaming response: [`Stream`] of [`StreamEvent`]s, decoded by `D`.
///
/// See the [cancellation](self#cancellation) notes on the module: dropping
/// this value aborts the connection.
pub struct SseChatStream<D: SseDecoder> {
    response: reqwest::Response,
    framer: SseFramer,
    decoder: D,
    pending: VecDeque<StreamEvent>,
    eof: bool,
    finished: bool,
    saw_terminal: bool,
    cancel: Option<CancellationToken>,
}

impl<D: SseDecoder + Unpin> SseChatStream<D> {
    /// Wrap an already-answered streaming response.
    pub(crate) fn new(response: reqwest::Response, decoder: D) -> Self {
        Self {
            response,
            framer: SseFramer::new(),
            decoder,
            pending: VecDeque::new(),
            eof: false,
            finished: false,
            saw_terminal: false,
            cancel: None,
        }
    }

    /// Stop consuming the stream as soon as `token` is cancelled. The
    /// connection is aborted when the stream is subsequently dropped.
    pub fn cancel_with(&mut self, token: CancellationToken) {
        self.cancel = Some(token);
    }

    /// Await the next event (a `StreamExt::next` stand-in that keeps
    /// `futures-util` out of the dependency tree).
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, ChatError>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

    /// Drain the whole stream into an [`crate::cardinal::stream::AssembledMessage`].
    /// The first error aborts; `Ok` with `done == false` only happens when the
    /// stream was cancelled or dropped early.
    pub async fn collect(mut self) -> Result<crate::cardinal::stream::AssembledMessage, ChatError> {
        let mut assembler = crate::cardinal::stream::Assembler::new();
        while let Some(event) = self.next_event().await {
            assembler.push(&event?);
        }
        Ok(assembler.finish())
    }
}

impl<D: SseDecoder + Unpin> Stream for SseChatStream<D> {
    type Item = Result<StreamEvent, ChatError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.pending.pop_front() {
                match event {
                    StreamEvent::Done => this.finished = true,
                    StreamEvent::Finish { .. } => this.saw_terminal = true,
                    _ => {}
                }
                return Poll::Ready(Some(Ok(event)));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            if this.eof {
                this.finished = true;
                if this.saw_terminal {
                    // A seen terminal event counts as an end even without the
                    // protocol's explicit terminator — end cleanly so the
                    // assembled content survives.
                    return Poll::Ready(Some(Ok(StreamEvent::Done)));
                }
                // Bytes ran out without any terminal signal: flush the verdict
                // as an error so the runner never treats a truncated stream as
                // a turn.
                return Poll::Ready(Some(Err(ChatError::EarlyClose)));
            }
            if let Some(token) = &this.cancel
                && token.is_cancelled()
            {
                this.finished = true;
                return Poll::Ready(None);
            }

            let mut payloads = Vec::new();
            let chunk = std::pin::pin!(this.response.chunk());
            let read = match chunk.poll(cx) {
                Poll::Ready(Ok(bytes)) => Ok(bytes),
                Poll::Ready(Err(error)) => Err(ChatError::Transport(error)),
                Poll::Pending => return Poll::Pending,
            };
            match read {
                Ok(Some(bytes)) => {
                    if let Err(error) = this.framer.push(&bytes, &mut payloads) {
                        this.finished = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                }
                Ok(None) => {
                    this.eof = true;
                    this.framer.finish(&mut payloads);
                }
                Err(error) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(error)));
                }
            }
            for payload in payloads {
                let mut events = Vec::new();
                if let Err(error) = this.decoder.decode(&payload, &mut events) {
                    this.finished = true;
                    return Poll::Ready(Some(Err(error)));
                }
                this.pending.extend(events);
            }
            // Loop: yield buffered events, or keep reading.
        }
    }
}

impl<D: SseDecoder + Unpin> std::fmt::Debug for SseChatStream<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SseChatStream")
            .field("finished", &self.finished)
            .field("eof", &self.eof)
            .field("pending", &self.pending.len())
            .field("cancelled", &self.cancel.as_ref().map(|c| c.is_cancelled()))
            .finish_non_exhaustive()
    }
}

/// An HTTP client for a streaming endpoint: connect quickly, never time out a
/// healthy long stream, and optionally bound silence.
pub(crate) fn streaming_http_client(
    stall_timeout: Option<Duration>,
) -> Result<reqwest::Client, ChatError> {
    // A dead endpoint should fail fast instead of hanging the runner.
    let mut builder = crate::http_client().connect_timeout(Duration::from_secs(10));
    // There is never a total deadline: long-lived streams are fine. The read
    // timeout only fires after that much silence (it resets on every
    // successful read) and only when configured.
    if let Some(stall) = stall_timeout {
        builder = builder.read_timeout(stall);
    }
    builder.build().map_err(ChatError::Transport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_framer_splits_on_blank_lines_and_joins_multi_line_data() {
        let mut framer = SseFramer::new();
        let mut out = Vec::new();
        framer
            .push(
                b"event: x\ndata: {\"a\":1}\n\n: keepalive\n\ndata: one\ndata: two\n\n",
                &mut out,
            )
            .expect("frame");
        assert_eq!(out, ["{\"a\":1}", "one\ntwo"]);
    }

    #[test]
    fn a_trailing_payload_flushes_at_eof() {
        let mut framer = SseFramer::new();
        let mut out = Vec::new();
        framer.push(b"data: tail", &mut out).expect("frame");
        assert!(out.is_empty());
        framer.finish(&mut out);
        assert_eq!(out, ["tail"]);
    }

    #[test]
    fn a_utf8_bom_and_crlf_are_tolerated() {
        let mut framer = SseFramer::new();
        let mut out = Vec::new();
        framer
            .push(b"\xEF\xBB\xBFdata: x\r\n\r\n", &mut out)
            .expect("frame");
        assert_eq!(out, ["x"]);
    }
}
