//! The one error type every chat transport returns.
//!
//! Two protocols share this enum ([`crate::cardinal`]'s chat completions and
//! [`crate::messages`]'s Messages): the [`crate::cardinal::chat::Chat`] seam
//! erases the transport, so the runner cannot (and must not) know which wire
//! protocol produced a failure. The messages here are therefore
//! provider-neutral — they name what happened, not which gateway said it.

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
