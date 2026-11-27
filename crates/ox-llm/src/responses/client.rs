//! HTTP client for the OpenAI Responses streaming endpoint.
//!
//! [`ResponsesClient::stream_chat`] translates ox's internal [`ChatRequest`]
//! (see [`super::wire`]), POSTs it to `…/v1/responses`, and returns a
//! [`ResponsesStream`] of the engine's [`StreamEvent`]s. It is a third
//! implementation of [`Chat`], so the runner, router and failover rules are
//! shared with the other transports: the wire protocol belongs to the
//! endpoint, not to the engine.

use std::time::Duration;

use super::stream::ResponsesDecoder;
use super::wire::build_request;
use crate::chat_completions::chat::{Chat, ChatFuture};
use crate::chat_completions::wire::ChatRequest;
use crate::error::ChatError;
use crate::sse::{SseChatStream, streaming_http_client};

/// A live Responses stream.
pub type ResponsesStream = SseChatStream<ResponsesDecoder>;

/// The Responses path appended to a provider base URL.
const RESPONSES_PATH: &str = "/v1/responses";

/// The Responses request path for `base_url`.
///
/// A base that already carries `/v1` gets `/responses`, everything else gets
/// `/v1/responses`; trailing slashes never double the separator.
#[must_use]
pub fn responses_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/responses")
    } else {
        format!("{base}{RESPONSES_PATH}")
    }
}

/// A client for one Responses endpoint.
#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl ResponsesClient {
    /// A client for `base_url`, authenticated with `api_key` when present.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        stall_timeout: Option<Duration>,
    ) -> Result<Self, ChatError> {
        Ok(Self {
            http: streaming_http_client(stall_timeout)?,
            base_url: base_url.into(),
            api_key: api_key.filter(|key| !key.is_empty()),
        })
    }

    /// Start a streaming Responses turn.
    pub async fn stream_chat(&self, request: &ChatRequest) -> Result<ResponsesStream, ChatError> {
        let body = build_request(request);
        let mut builder = self
            .http
            .post(responses_url(&self.base_url))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await?;
        if !response.status().is_success() {
            return Err(ChatError::from_response(response).await);
        }
        Ok(SseChatStream::new(response, ResponsesDecoder::new()))
    }
}

impl Chat for ResponsesClient {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            ResponsesClient::stream_chat(self, request)
                .await
                .map(|stream| Box::pin(stream) as crate::chat_completions::chat::BoxChatStream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ResponsesClient, responses_url};
    use crate::chat_completions::chat::Chat;

    #[test]
    fn responses_url_appends_the_versioned_path() {
        assert_eq!(
            responses_url("https://api.openai.com"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            responses_url("https://api.openai.com/"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            responses_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn client_implements_the_chat_seam() {
        fn assert_chat<T: Chat>() {}
        assert_chat::<ResponsesClient>();
    }
}
