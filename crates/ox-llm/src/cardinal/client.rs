//! HTTP client for cardinal's `/v1/chat/completions` streaming endpoint.
//!
//! [`CardinalClient::stream_chat`] POSTs a [`ChatRequest`] and returns a
//! [`ChatStream`] — a [`Stream`] of [`StreamEvent`]s that:
//!
//! - yields text / reasoning / tool-call deltas, then `Finish`, then `Usage`
//!   (cardinal sends usage in a trailing chunk whose `choices` array is
//!   empty), then `Done` for `data: [DONE]`;
//! - maps a non-2xx response to [`CardinalError::Http`] with a body snippet;
//! - maps an in-band chunk `error` object to [`CardinalError::Upstream`];
//! - reports a connection closed before any terminal signal as
//!   [`CardinalError::EarlyClose`] (the turn is incomplete). A seen
//!   `finish_reason` *is* a terminal for cardinal's verdict even when
//!   `[DONE]` never arrives, so an EOF after `Finish` ends the stream
//!   cleanly with a synthetic `Done` — assembled content is kept.
//!
//! # Cancellation
//!
//! Dropping the [`ChatStream`] drops the underlying `reqwest::Response`,
//! which aborts the HTTP connection (the socket is closed and any buffered
//! body bytes are discarded) — no extra shutdown step is needed. For
//! cooperative cancellation from another task, pass a
//! [`CancellationToken`] to [`ChatStream::cancel_with`]: the next poll stops
//! consuming and ends the stream, which has the same abort-on-drop effect.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use super::stream::{AssembledMessage, Assembler, ChunkDecoder, SseFramer, StreamEvent};
use super::wire::ChatRequest;
use super::{CardinalError, HTTP_BODY_SNIPPET_BYTES};
use crate::config::LlmConfig;

/// Cardinal's chat completions route, appended to the configured base URL.
pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// A cardinal gateway client.
#[derive(Debug, Clone)]
pub struct CardinalClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl CardinalClient {
    /// A client for the gateway at `base_url`, authenticated with
    /// `api_key` when present (cardinal expects `Authorization: Bearer`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self, CardinalError> {
        Self::with_stall_timeout(base_url, api_key, None)
    }

    /// Like [`Self::new`], with an optional silence bound: when `Some`, a
    /// request fails with [`CardinalError::Transport`] after that long
    /// without receiving a byte (response headers or any body read).
    pub fn with_stall_timeout(
        base_url: impl Into<String>,
        api_key: Option<String>,
        stall_timeout: Option<Duration>,
    ) -> Result<Self, CardinalError> {
        // A dead gateway should fail fast instead of hanging the runner.
        let mut builder = crate::http_client().connect_timeout(Duration::from_secs(10));
        // There is never a total deadline: long-lived streams are fine. The
        // read timeout only fires after that much silence (it resets on
        // every successful read) and only when configured.
        if let Some(stall) = stall_timeout {
            builder = builder.read_timeout(stall);
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
        })
    }

    /// A client configured from ox's config (`cardinal_url` + `cardinal_key`).
    pub fn from_config(config: &LlmConfig) -> Result<Self, CardinalError> {
        Self::with_stall_timeout(
            config.cardinal_url.clone(),
            config.cardinal_key.clone(),
            config.cardinal_limits.stall_timeout(),
        )
    }

    /// Start a streaming chat completion. The request is sent wire-exact as
    /// serialized by [`ChatRequest`].
    pub async fn stream_chat(&self, request: &ChatRequest) -> Result<ChatStream, CardinalError> {
        let url = format!("{}{}", self.base_url, CHAT_COMPLETIONS_PATH);
        let mut builder = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(request);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.bytes().await.unwrap_or_default();
            let mut snippet = String::from_utf8_lossy(&body).into_owned();
            snippet.truncate(HTTP_BODY_SNIPPET_BYTES);
            return Err(CardinalError::Http {
                status: status.as_u16(),
                body: snippet,
            });
        }
        Ok(ChatStream {
            response,
            framer: SseFramer::new(),
            decoder: ChunkDecoder::new(),
            pending: VecDeque::new(),
            eof: false,
            finished: false,
            saw_finish: false,
            cancel: None,
        })
    }
}

/// A live streaming chat response: [`Stream`] of [`StreamEvent`]s.
///
/// See the [cancellation](self#cancellation) notes on the module: dropping
/// this value aborts the connection.
pub struct ChatStream {
    response: reqwest::Response,
    framer: SseFramer,
    decoder: ChunkDecoder,
    pending: VecDeque<StreamEvent>,
    eof: bool,
    finished: bool,
    saw_finish: bool,
    cancel: Option<CancellationToken>,
}

impl ChatStream {
    /// Stop consuming the stream as soon as `token` is cancelled. The
    /// connection is aborted when the stream is subsequently dropped.
    pub fn cancel_with(&mut self, token: CancellationToken) {
        self.cancel = Some(token);
    }

    /// Await the next event (a `StreamExt::next` stand-in that keeps
    /// `futures-util` out of the dependency tree).
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, CardinalError>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

    /// Drain the whole stream into an [`AssembledMessage`]. The first error
    /// aborts; `Ok` with `done == false` only happens when the stream was
    /// cancelled or dropped early.
    pub async fn collect(mut self) -> Result<AssembledMessage, CardinalError> {
        let mut assembler = Assembler::new();
        while let Some(event) = self.next_event().await {
            assembler.push(&event?);
        }
        Ok(assembler.finish())
    }
}

impl Stream for ChatStream {
    type Item = Result<StreamEvent, CardinalError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.pending.pop_front() {
                match event {
                    StreamEvent::Done => this.finished = true,
                    StreamEvent::Finish { .. } => this.saw_finish = true,
                    _ => {}
                }
                return Poll::Ready(Some(Ok(event)));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            if this.eof {
                this.finished = true;
                if this.saw_finish {
                    // Cardinal's verdict counts any seen finish_reason as a
                    // terminal even without `[DONE]` — end cleanly so the
                    // assembled content survives.
                    return Poll::Ready(Some(Ok(StreamEvent::Done)));
                }
                // Bytes ran out without any terminal signal: flush the
                // verdict as an error so the runner never treats a truncated
                // stream as a turn.
                return Poll::Ready(Some(Err(CardinalError::EarlyClose)));
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
                Poll::Ready(Err(error)) => Err(CardinalError::Transport(error)),
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

impl std::fmt::Debug for ChatStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatStream")
            .field("finished", &self.finished)
            .field("eof", &self.eof)
            .field("pending", &self.pending.len())
            .field("cancelled", &self.cancel.as_ref().map(|c| c.is_cancelled()))
            .finish_non_exhaustive()
    }
}
