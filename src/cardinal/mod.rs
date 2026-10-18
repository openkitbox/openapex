//! cardinal LLM gateway client: a standard OpenAI Chat Completions *streaming*
//! client.
//!
//! Cardinal normalizes providers behind its `/v1/chat/completions` route and
//! forwards chat-protocol bytes verbatim, so this module contains zero
//! provider-specific logic — the wire shapes here were verified against
//! cardinal's own kernel (`cardinal/src/protocol/openai_chat.rs`) and its SSE
//! framer (`cardinal/src/protocol/sse.rs`):
//!
//! - **Request**: cardinal validates only `model` (string) and a non-empty
//!   `messages` array, then passes every other field through verbatim — so
//!   [`wire::ChatRequest`] serializes canonical OpenAI fields
//!   (`tool_choice`, `stream: true`, `stream_options.include_usage: true`).
//! - **Chunks**: `data: {...}` lines carry `choices[0].delta.{content,
//!   reasoning_content | reasoning, tool_calls[]}`, `finish_reason`, and a
//!   final `usage` (often in a trailing chunk whose `choices` array is empty —
//!   that chunk does *not* terminate the stream; only `data: [DONE]` does).
//! - **Usage**: `prompt_tokens` / `completion_tokens` plus optional cached /
//!   reasoning token details, extracted exactly like cardinal's decoder.
//!
//! Layout: [`wire`] (request types), [`stream`] (SSE framing, chunk decoding,
//! delta assembly), [`client`] (HTTP + the [`client::ChatStream`]).
//!
//! Errors: [`CardinalError`] — an HTTP error carries the status and a body
//! snippet; framing-level junk lines are skipped per the SSE spec, while a
//! `data:` payload that is not valid JSON is fatal (matching cardinal's own
//! verdict on a misbehaving upstream).

pub mod client;
pub mod stream;
pub mod wire;

pub use client::{CardinalClient, ChatStream};
pub use stream::{AssembledMessage, AssembledToolCall, Assembler, ChunkDecoder, StreamEvent};
pub use wire::{ChatMessage, ChatRequest, ToolCall, ToolDef, Usage};

use thiserror::Error;

/// Everything that can go wrong while talking to cardinal.
#[derive(Debug, Error)]
pub enum CardinalError {
    /// Non-2xx HTTP status from cardinal. `body` is a truncated snippet of the
    /// response body for diagnostics.
    #[error("cardinal returned HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Truncated response body (at most [`HTTP_BODY_SNIPPET_BYTES`]).
        body: String,
    },
    /// A chunk carried an `error` object (cardinal forwards upstream
    /// failures in-band once the stream has started).
    #[error("cardinal stream error: {0}")]
    Upstream(String),
    /// The connection closed before `data: [DONE]` arrived — the response is
    /// incomplete and must not be treated as a successful turn.
    #[error("cardinal stream closed before [DONE]")]
    EarlyClose,
    /// A `data:` payload was not valid JSON (fatal, like cardinal's
    /// `InvalidUpstreamResponse`), or an SSE framing limit was exceeded.
    #[error("malformed stream payload: {0}")]
    Sse(String),
    /// Transport-level failure (connect, read, body decode).
    #[error("cardinal transport error: {0}")]
    Transport(#[from] reqwest::Error),
}

/// Maximum bytes of a non-2xx response body kept in [`CardinalError::Http`].
pub const HTTP_BODY_SNIPPET_BYTES: usize = 512;

/// Maximum bytes in one SSE line (mirrors cardinal's `MAX_LINE_BYTES`).
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;
/// Maximum bytes accumulated for one SSE event (mirrors cardinal's
/// `MAX_EVENT_BYTES`).
pub(crate) const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
