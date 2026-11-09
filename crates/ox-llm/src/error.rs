//! The one error type every chat transport returns.
//!
//! Two protocols share this enum ([`crate::cardinal`]'s chat completions and
//! [`crate::messages`]'s Messages): the [`crate::cardinal::chat::Chat`] seam
//! erases the transport, so the runner cannot (and must not) know which wire
//! protocol produced a failure. The messages here are therefore
//! provider-neutral — they name what happened, not which gateway said it.

use std::time::Duration;

use thiserror::Error;

/// Everything that can go wrong while streaming a chat turn.
#[derive(Debug, Error)]
pub enum ChatError {
    /// Non-2xx HTTP status from the endpoint. `body` is a truncated snippet of
    /// the response body for diagnostics.
    #[error("provider returned HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Truncated response body (at most [`HTTP_BODY_SNIPPET_BYTES`]).
        body: String,
        /// Provider's minimum retry delay, normalized at the HTTP boundary.
        retry_after: Option<Duration>,
    },
    /// A chunk carried an `error` object (the endpoint forwards upstream
    /// failures in-band once the stream has started).
    #[error("provider stream error: {0}")]
    Upstream(String),
    /// The connection closed before any terminal signal — no terminal event —
    /// so the response is incomplete and must not be treated as a successful
    /// turn. A seen terminal event counts as an end even without the explicit
    /// terminator and ends the stream cleanly instead.
    #[error("provider stream closed before its terminal event")]
    EarlyClose,
    /// A `data:` payload was not valid JSON (fatal, like the gateways' own
    /// verdict on a misbehaving upstream), or an SSE framing limit was
    /// exceeded.
    #[error("malformed stream payload: {0}")]
    Sse(String),
    /// Transport-level failure (connect, read, body decode).
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The requested model has no candidates: no `[models]` route names it
    /// and `[providers]` is configured, so the implicit passthrough endpoint
    /// is disabled.
    #[error("no candidates configured for model `{model}`: add a [models.\"{model}\"] route")]
    NoRoute {
        /// The logical model id ox could not route.
        model: String,
    },
}

/// Maximum bytes of a non-2xx response body kept in [`ChatError::Http`].
pub const HTTP_BODY_SNIPPET_BYTES: usize = 512;

const MAX_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

impl ChatError {
    /// The endpoint's bounded minimum retry delay, when supplied.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub(crate) async fn from_response(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let retry_after = retry_after(response.headers(), jiff::Timestamp::now());
        let body = response.bytes().await.unwrap_or_default();
        let mut snippet = String::from_utf8_lossy(&body).into_owned();
        let mut end = snippet.len().min(HTTP_BODY_SNIPPET_BYTES);
        while !snippet.is_char_boundary(end) {
            end -= 1;
        }
        snippet.truncate(end);
        Self::Http {
            status,
            body: snippet,
            retry_after,
        }
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap, now: jiff::Timestamp) -> Option<Duration> {
    let milliseconds = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| decimal_delay(value, 1));
    let seconds_or_date = || {
        let value = headers
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?
            .trim();
        decimal_delay(value, 1000).or_else(|| {
            let timestamp = jiff::fmt::rfc2822::DateTimeParser::new()
                .parse_timestamp(value)
                .ok()?;
            let nanos = timestamp
                .as_nanosecond()
                .saturating_sub(now.as_nanosecond())
                .max(0);
            Some(Duration::from_nanos(
                u64::try_from(nanos).unwrap_or(u64::MAX),
            ))
        })
    };
    milliseconds
        .or_else(seconds_or_date)
        .map(|delay| delay.min(MAX_RETRY_AFTER))
}

fn decimal_delay(value: &str, milliseconds: u64) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = value.parse::<u64>().unwrap_or(u64::MAX);
    Some(Duration::from_millis(number.saturating_mul(milliseconds)))
}
