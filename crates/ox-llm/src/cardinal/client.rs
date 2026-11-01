//! HTTP client for cardinal's `/v1/chat/completions` streaming endpoint.
//!
//! [`CardinalClient::stream_chat`] POSTs a [`ChatRequest`] and returns a
//! [`ChatStream`] — a [`Stream`] of [`StreamEvent`]s that:
//!
//! - yields text / reasoning / tool-call deltas, then `Finish`, then `Usage`
//!   (cardinal sends usage in a trailing chunk whose `choices` array is
//!   empty), then `Done` for `data: [DONE]`;
//! - maps a non-2xx response to [`ChatError::Http`] with a body snippet;
//! - maps an in-band chunk `error` object to [`ChatError::Upstream`];
//! - reports a connection closed before any terminal signal as
//!   [`ChatError::EarlyClose`] (the turn is incomplete). A seen
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

use std::time::Duration;

use super::stream::ChunkDecoder;
use super::wire::ChatRequest;
use super::{ChatError, HTTP_BODY_SNIPPET_BYTES};
use crate::config::LlmConfig;
use crate::sse::{SseChatStream, streaming_http_client};

/// A live streaming chat response, decoded as chat-completions chunks.
pub type ChatStream = SseChatStream<ChunkDecoder>;

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
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Result<Self, ChatError> {
        Self::with_stall_timeout(base_url, api_key, None)
    }

    /// Like [`Self::new`], with an optional silence bound: when `Some`, a
    /// request fails with [`ChatError::Transport`] after that long
    /// without receiving a byte (response headers or any body read).
    pub fn with_stall_timeout(
        base_url: impl Into<String>,
        api_key: Option<String>,
        stall_timeout: Option<Duration>,
    ) -> Result<Self, ChatError> {
        let http = streaming_http_client(stall_timeout)?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
        })
    }

    /// A client configured from ox's config (`cardinal_url` + `cardinal_key`).
    pub fn from_config(config: &LlmConfig) -> Result<Self, ChatError> {
        Self::with_stall_timeout(
            config.cardinal_url.clone(),
            config.cardinal_key.clone(),
            config.cardinal_limits.stall_timeout(),
        )
    }

    /// Start a streaming chat completion. The request is sent wire-exact as
    /// serialized by [`ChatRequest`].
    pub async fn stream_chat(&self, request: &ChatRequest) -> Result<ChatStream, ChatError> {
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
            return Err(ChatError::Http {
                status: status.as_u16(),
                body: snippet,
            });
        }
        Ok(SseChatStream::new(response, ChunkDecoder::new()))
    }
}
