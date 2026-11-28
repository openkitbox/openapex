//! HTTP client for the OpenAI Responses streaming endpoint.
//!
//! Three execution shapes share one [`Chat`] seam: an ordinary SSE turn, a
//! `background + stream` turn that reconnects once after a disconnect, and a
//! long `2h`/`8h` background turn that is polled through `retrieve` and
//! converted into the same [`StreamEvent`] vocabulary. The wire builder and
//! decoder stay in [`super::wire`] and [`super::stream`]; this module only
//! drives HTTP and the lifecycle state machine.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use serde_json::{Value, json};

use super::control::{ResponseResource, ResponsesControl};
use super::options::{RequestOptions, ResponsesOptions, merge_options};
use super::stream::ResponsesDecoder;
use super::wire::build_request;
use crate::chat_completions::chat::{Chat, ChatFuture};
use crate::chat_completions::stream::StreamEvent;
use crate::chat_completions::wire::ChatRequest;
use crate::error::ChatError;
use crate::sse::{SseChatStream, SseDecoder, streaming_http_client};

/// A plain Responses stream (kept for callers that drive SSE directly).
pub type ResponsesStream = SseChatStream<ResponsesDecoder>;

/// The Responses path appended to a provider base URL.
const RESPONSES_PATH: &str = "/v1/responses";

/// The Responses request path for `base_url`.
#[must_use]
pub fn responses_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/responses")
    } else {
        format!("{base}{RESPONSES_PATH}")
    }
}

fn is_long_tier(tier: Option<&str>) -> bool {
    matches!(tier, Some("2h" | "8h"))
}

/// One endpoint identity, shared by the first stream and its single resume.
#[derive(Debug)]
struct ResumeConfig {
    stream_http: reqwest::Client,
    control_http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    provider: String,
    model: String,
    fingerprint: Option<(String, usize)>,
}

impl ResumeConfig {
    fn control(&self) -> ResponsesControl<'_> {
        ResponsesControl::new(&self.control_http, &self.base_url, self.api_key.as_deref())
    }

    /// Reconnecting returns an SSE body, so it must keep the streaming client's
    /// long silence window instead of the short control-call deadline.
    fn resume_control(&self) -> ResponsesControl<'_> {
        ResponsesControl::new(&self.stream_http, &self.base_url, self.api_key.as_deref())
    }

    fn decoder(&self) -> ResponsesDecoder {
        let decoder = ResponsesDecoder::new().with_replay(
            self.provider.clone(),
            responses_url(&self.base_url),
            self.model.clone(),
        );
        match &self.fingerprint {
            Some((hash, len)) => decoder.with_fingerprint(hash, *len),
            None => decoder,
        }
    }

    async fn open_resume(
        self: Arc<Self>,
        anchor: ResumeAnchor,
    ) -> Result<SseChatStream<ResponsesDecoder>, ChatError> {
        let (id, sequence) = anchor.into_parts().ok_or(ChatError::EarlyClose)?;
        let response = self.resume_control().resume_stream(&id, sequence).await?;
        Ok(SseChatStream::new(response, self.decoder()))
    }
}

/// Highest sequence number and response id seen on the first connection.
#[derive(Debug, Default, Clone)]
struct ResumeAnchor {
    id: Option<String>,
    sequence: Option<u64>,
}

impl ResumeAnchor {
    fn into_parts(self) -> Option<(String, u64)> {
        Some((self.id?, self.sequence?))
    }
}

/// A decoder adapter that records the resume anchor before the SSE stream
/// consumes the decoder. It forwards decoding unchanged; no event is copied.
#[derive(Debug)]
struct TrackingDecoder {
    inner: ResponsesDecoder,
    anchor: Arc<Mutex<ResumeAnchor>>,
}

impl SseDecoder for TrackingDecoder {
    fn decode(&mut self, payload: &str, out: &mut Vec<StreamEvent>) -> Result<(), ChatError> {
        let result = self.inner.decode(payload, out);
        let mut anchor = self
            .anchor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(id) = self.inner.response_id() {
            anchor.id = Some(id.to_owned());
        }
        if let Some(sequence) = self.inner.last_sequence_number() {
            anchor.sequence = Some(sequence);
        }
        result
    }
}

/// Pending result of a long background poll.
type PollFuture = Pin<Box<dyn Future<Output = Result<Vec<StreamEvent>, ChatError>> + Send>>;

struct PollingState {
    future: PollFuture,
    queue: VecDeque<StreamEvent>,
    error: Option<ChatError>,
}

/// The event stream returned by every Responses execution shape.
pub struct ResponsesTurnStream {
    state: TurnState,
    anchor: Arc<Mutex<ResumeAnchor>>,
    config: Arc<ResumeConfig>,
    resume_attempted: bool,
    provider_state_seen: bool,
    done_seen: bool,
}

enum TurnState {
    First(SseChatStream<TrackingDecoder>),
    Resuming(
        Pin<Box<dyn Future<Output = Result<SseChatStream<ResponsesDecoder>, ChatError>> + Send>>,
    ),
    Continued(SseChatStream<ResponsesDecoder>),
    Polling(PollingState),
    Done,
}

impl ResponsesTurnStream {
    /// Await the next event without pulling in `futures-util`.
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, ChatError>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl Stream for ResponsesTurnStream {
    type Item = Result<StreamEvent, ChatError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                TurnState::First(stream) => match Pin::new(stream).poll_next(cx) {
                    Poll::Ready(Some(Ok(event))) => {
                        if matches!(event, StreamEvent::ProviderState(_)) {
                            this.provider_state_seen = true;
                        }
                        if matches!(event, StreamEvent::Done) {
                            this.done_seen = true;
                            this.state = TurnState::Done;
                        }
                        return Poll::Ready(Some(Ok(event)));
                    }
                    Poll::Ready(Some(Err(ChatError::EarlyClose))) => {
                        this.state = TurnState::Done;
                        if this.resume_attempted {
                            return Poll::Ready(Some(Err(ChatError::EarlyClose)));
                        }
                        this.resume_attempted = true;
                        let anchor = this
                            .anchor
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .clone();
                        let Some((id, sequence)) = anchor.into_parts() else {
                            return Poll::Ready(Some(Err(ChatError::EarlyClose)));
                        };
                        let config = Arc::clone(&this.config);
                        let future = Box::pin(async move {
                            config
                                .open_resume(ResumeAnchor {
                                    id: Some(id),
                                    sequence: Some(sequence),
                                })
                                .await
                        });
                        this.state = TurnState::Resuming(future);
                    }
                    Poll::Ready(Some(Err(error))) => {
                        this.state = TurnState::Done;
                        return Poll::Ready(Some(Err(error)));
                    }
                    Poll::Ready(None) => this.state = TurnState::Done,
                    Poll::Pending => return Poll::Pending,
                },
                TurnState::Resuming(future) => match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(stream)) => this.state = TurnState::Continued(stream),
                    Poll::Ready(Err(error)) => {
                        this.state = TurnState::Done;
                        return Poll::Ready(Some(Err(error)));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                TurnState::Continued(stream) => match Pin::new(stream).poll_next(cx) {
                    // The first connection already published the replay record;
                    // a resumed stream would otherwise replace it with a partial one.
                    Poll::Ready(Some(Ok(StreamEvent::ProviderState(_))))
                        if this.provider_state_seen => {}
                    // A terminal event followed by `[DONE]` is one completion,
                    // not two. The engine must see a single Done per turn.
                    Poll::Ready(Some(Ok(StreamEvent::Done))) if this.done_seen => {}
                    Poll::Ready(Some(Ok(event))) => return Poll::Ready(Some(Ok(event))),
                    Poll::Ready(Some(Err(error))) => {
                        this.state = TurnState::Done;
                        return Poll::Ready(Some(Err(error)));
                    }
                    Poll::Ready(None) => this.state = TurnState::Done,
                    Poll::Pending => return Poll::Pending,
                },
                TurnState::Polling(polling) => {
                    if let Some(event) = polling.queue.pop_front() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                    if let Some(error) = polling.error.take() {
                        this.state = TurnState::Done;
                        return Poll::Ready(Some(Err(error)));
                    }
                    match polling.future.as_mut().poll(cx) {
                        Poll::Ready(Ok(events)) => polling.queue.extend(events),
                        Poll::Ready(Err(error)) => polling.error = Some(error),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                TurnState::Done => return Poll::Ready(None),
            }
        }
    }
}

/// A client for one Responses endpoint.
#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    control_http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    provider: String,
    options: ResponsesOptions,
    poll_interval: Duration,
    max_polls: usize,
}

impl ResponsesClient {
    /// A client for `base_url`, authenticated with `api_key` when present.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        stall_timeout: Option<Duration>,
    ) -> Result<Self, ChatError> {
        Self::with_options(
            base_url,
            api_key,
            stall_timeout,
            "responses",
            ResponsesOptions::default(),
        )
    }

    /// Bind an endpoint id and its configured option defaults. The id is used
    /// as the origin of replay items, so items cannot cross endpoints.
    pub fn with_options(
        base_url: impl Into<String>,
        api_key: Option<String>,
        stall_timeout: Option<Duration>,
        provider: impl Into<String>,
        options: ResponsesOptions,
    ) -> Result<Self, ChatError> {
        // Control calls are short and must never wait forever; streaming calls
        // keep their optionally unbounded silence window on `http`.
        let control_http = crate::http_client()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(stall_timeout.unwrap_or(Duration::from_secs(30)))
            .build()
            .map_err(ChatError::Transport)?;
        Ok(Self {
            http: streaming_http_client(stall_timeout)?,
            control_http,
            base_url: base_url.into(),
            api_key: api_key.filter(|key| !key.is_empty()),
            provider: provider.into(),
            options,
            poll_interval: Duration::from_secs(5),
            max_polls: 5760,
        })
    }

    /// Override the long-tier polling cadence and bound (mainly for tests).
    #[must_use]
    pub fn with_polling(mut self, interval: Duration, max_polls: usize) -> Self {
        self.poll_interval = interval;
        self.max_polls = max_polls;
        self
    }

    fn config(
        &self,
        request: &ChatRequest,
        fingerprint: Option<(String, usize)>,
    ) -> Arc<ResumeConfig> {
        Arc::new(ResumeConfig {
            stream_http: self.http.clone(),
            control_http: self.control_http.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            provider: self.provider.clone(),
            model: request.model.clone(),
            fingerprint,
        })
    }

    /// Start a Responses turn: SSE, resumable SSE, or long-tier polling.
    pub async fn stream_chat(
        &self,
        request: &ChatRequest,
    ) -> Result<ResponsesTurnStream, ChatError> {
        let overrides = request
            .responses_options
            .as_ref()
            .map(|value| RequestOptions {
                overrides: Some(value),
                stream: true,
            });
        let overridden = overrides.and_then(|value| value.overrides);
        let tier = overridden
            .and_then(|value| value.get("service_tier"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.options.service_tier.clone());
        let background = overridden
            .and_then(|value| value.get("background"))
            .and_then(Value::as_bool)
            .or(self.options.background);
        let polling = background == Some(true) && is_long_tier(tier.as_deref());
        // Long tiers are never streamed: `merge_options` rejects that
        // combination, so the validation sees the same decision the client does.
        let merged = merge_options(
            Some(&self.options),
            None,
            overrides.map(|value| RequestOptions {
                stream: !polling,
                ..value
            }),
        )
        .map_err(|error| ChatError::Upstream(format!("invalid Responses options: {error}")))?;
        let body = build_request(request, &merged, request.responses_replay_state.as_deref())?;
        let config = self.config(
            request,
            body.logical_fingerprint()
                .map(|(hash, len)| (hash.to_owned(), len)),
        );

        if polling {
            let mut value = serde_json::to_value(&body)
                .map_err(|error| ChatError::Upstream(format!("invalid Responses body: {error}")))?;
            value["stream"] = json!(false);
            let future = poll_turn(
                Arc::clone(&config),
                value,
                self.poll_interval,
                self.max_polls,
            );
            return Ok(ResponsesTurnStream {
                state: TurnState::Polling(PollingState {
                    future,
                    queue: VecDeque::new(),
                    error: None,
                }),
                anchor: Arc::new(Mutex::new(ResumeAnchor::default())),
                config,
                resume_attempted: true,
                provider_state_seen: false,
                done_seen: false,
            });
        }

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
        let background = merged.background == Some(true);
        let anchor = Arc::new(Mutex::new(ResumeAnchor::default()));
        let state = if background {
            let decoder = TrackingDecoder {
                inner: config.decoder(),
                anchor: Arc::clone(&anchor),
            };
            TurnState::First(SseChatStream::new(response, decoder))
        } else {
            TurnState::Continued(SseChatStream::new(response, config.decoder()))
        };
        Ok(ResponsesTurnStream {
            state,
            anchor,
            config,
            resume_attempted: false,
            provider_state_seen: false,
            done_seen: false,
        })
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

/// Poll a background response until it reaches a terminal status, then emit
/// the same event vocabulary as the SSE path. An unfinished response after the
/// bounded poll budget is an `EarlyClose`, never a fabricated success.
fn poll_turn(
    config: Arc<ResumeConfig>,
    body: Value,
    interval: Duration,
    max_polls: usize,
) -> PollFuture {
    Box::pin(async move {
        let control = config.control();
        let created = control.create(&body).await?;
        for _ in 0..max_polls {
            let resource = control.retrieve(&created.id).await?;
            match resource.status.as_str() {
                "completed" | "incomplete" => return terminal_events(&resource, &config),
                "queued" | "in_progress" => {}
                other => {
                    return Err(ChatError::Upstream(format!(
                        "Responses background status `{other}` cannot be completed"
                    )));
                }
            }
            if !interval.is_zero() {
                tokio::time::sleep(interval).await;
            }
        }
        Err(ChatError::EarlyClose)
    })
}

/// Convert a final response object into deltas plus the decoder's terminal
/// events (replay record, usage, finish, done).
fn terminal_events(
    resource: &ResponseResource,
    config: &ResumeConfig,
) -> Result<Vec<StreamEvent>, ChatError> {
    let status = resource.status.as_str();
    if matches!(status, "failed" | "cancelled") {
        let message = resource
            .error
            .as_ref()
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or(status);
        return Err(ChatError::Upstream(format!(
            "Responses background {status}: {message}"
        )));
    }
    let mut events = Vec::new();
    append_output_events(&resource.output, &mut events);
    let terminal = json!({
        "type": if status == "completed" { "response.completed" } else { "response.incomplete" },
        "response": resource,
    });
    config
        .decoder()
        .decode(&terminal.to_string(), &mut events)?;
    Ok(events)
}

/// Emit readable output items as deltas before the terminal event. Provider
/// items ox does not interpret remain in the replay record, not in text.
fn append_output_events(items: &[Value], out: &mut Vec<StreamEvent>) {
    let mut tool_index = 0u32;
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(text) = part.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(StreamEvent::TextDelta(text.to_owned()));
                        }
                    }
                }
            }
            Some("reasoning") => {
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    for part in summary {
                        if let Some(text) = part.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(StreamEvent::ReasoningDelta(text.to_owned()));
                        }
                    }
                }
            }
            Some("function_call") => {
                out.push(StreamEvent::ToolCallDelta {
                    index: tool_index,
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
                tool_index += 1;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResponsesClient, is_long_tier, responses_url};
    use crate::chat_completions::chat::Chat;
    use crate::chat_completions::stream::StreamEvent;
    use crate::chat_completions::wire::{ChatMessage, ChatRequest, UserContent};
    use crate::responses::replay::from_provider_state;
    use crate::sse::SseDecoder;

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
    fn only_long_tiers_use_polling() {
        assert!(is_long_tier(Some("2h")));
        assert!(is_long_tier(Some("8h")));
        assert!(!is_long_tier(Some("auto")));
        assert!(!is_long_tier(None));
    }

    #[test]
    fn client_implements_the_chat_seam() {
        fn assert_chat<T: Chat>() {}
        assert_chat::<ResponsesClient>();
    }

    #[test]
    fn request_fingerprint_reaches_the_persisted_replay_state() {
        let client =
            ResponsesClient::new("https://vendor.example", None, None).expect("client builds");
        let request = ChatRequest::new(
            "gpt-5",
            vec![ChatMessage::User {
                content: UserContent::Text("hello".to_owned()),
            }],
        );
        let config = client.config(&request, Some(("fnv:test".to_owned(), 42)));
        let mut events = Vec::new();
        config
            .decoder()
            .decode(
                r#"{"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[{"type":"message"}]}}"#,
                &mut events,
            )
            .expect("terminal event decodes");
        let record = events.iter().find_map(|event| match event {
            StreamEvent::ProviderState(record) => Some(record),
            _ => None,
        });
        let state = from_provider_state(record).expect("replay state is persisted");
        assert!(
            state.fingerprint_matches("fnv:test", 42),
            "chained turns must store the prefix they generated from"
        );
    }
}
