//! HTTP client for an Anthropic-Messages streaming endpoint.
//!
//! [`MessagesClient::stream_chat`] translates ox's internal [`ChatRequest`]
//! (see [`super::wire`]), POSTs it to `…/v1/messages`, and returns a
//! [`MessagesStream`] of the engine's [`StreamEvent`]s. It is a second
//! implementation of [`Chat`], so the runner, the router and the failover rules
//! are shared with the chat-completions transport — the protocol is a property
//! of the endpoint, not of the engine.
//!
//! The endpoint base URL is joined the way the reference harnesses join it: a
//! base already ending in `/v1` gets `/messages`, anything else gets
//! `/v1/messages`. DeepSeek publishes its compat endpoint as
//! `https://api.deepseek.com/anthropic`, which lands on
//! `https://api.deepseek.com/anthropic/v1/messages`.

use std::time::Duration;

use super::stream::{ANTHROPIC_VERSION, MessagesDecoder};
use super::wire::{MessagesPolicy, build_request};
use crate::cardinal::chat::{Chat, ChatFuture};
use crate::cardinal::wire::ChatRequest;
use crate::error::{ChatError, HTTP_BODY_SNIPPET_BYTES};
use crate::sse::{SseChatStream, streaming_http_client};

/// A live Messages stream.
pub type MessagesStream = SseChatStream<MessagesDecoder>;

/// The Messages path appended to a provider base URL.
const MESSAGES_PATH: &str = "/v1/messages";

/// The Messages request path for `base_url`.
///
/// A base that already carries the version (`https://host/v1`) takes
/// `/messages`, everything else takes `/v1/messages` — the rule both the
/// reference harnesses and DeepSeek's compat layer document.
#[must_use]
pub fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}{MESSAGES_PATH}")
    }
}

/// A client for one Messages endpoint.
#[derive(Debug, Clone)]
pub struct MessagesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    policy: MessagesPolicy,
}

impl MessagesClient {
    /// A client for `base_url`, authenticated with `api_key` when present.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        policy: MessagesPolicy,
        stall_timeout: Option<Duration>,
    ) -> Result<Self, ChatError> {
        Ok(Self {
            http: streaming_http_client(stall_timeout)?,
            base_url: base_url.into(),
            api_key: api_key.filter(|key| !key.is_empty()),
            policy,
        })
    }

    /// The Messages dialect this client speaks.
    #[must_use]
    pub fn policy(&self) -> &MessagesPolicy {
        &self.policy
    }

    /// Start a streaming Messages turn.
    pub async fn stream_chat(&self, request: &ChatRequest) -> Result<MessagesStream, ChatError> {
        let body = build_request(request, &self.policy)?;
        let mut builder = self
            .http
            .post(messages_url(&self.base_url))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body);
        if let Some(key) = &self.api_key {
            // Anthropic-compatible endpoints disagree on the header: the
            // native API uses `x-api-key`, most compat layers (and Claude
            // Code, which DeepSeek documents) use a bearer token. Sending both
            // is accepted by every one of them.
            builder = builder.header("x-api-key", key).bearer_auth(key);
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
        Ok(SseChatStream::new(response, MessagesDecoder::new()))
    }
}

impl Chat for MessagesClient {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            MessagesClient::stream_chat(self, request)
                .await
                .map(|stream| Box::pin(stream) as crate::cardinal::chat::BoxChatStream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_base_gets_the_version_segment() {
        assert_eq!(
            messages_url("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn a_base_already_carrying_v1_does_not_repeat_it() {
        assert_eq!(
            messages_url("https://gateway.test/v1"),
            "https://gateway.test/v1/messages"
        );
    }
}
