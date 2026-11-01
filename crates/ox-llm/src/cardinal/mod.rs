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
//! delta assembly), [`client`] (HTTP + the [`client::ChatStream`]), [`chat`]
//! (the transport seam), [`limit`] (per-endpoint concurrency gate), and
//! [`router`] (multi-endpoint candidate routing).
//!
//! Errors: [`ChatError`] — an HTTP error carries the status and a body
//! snippet; framing-level junk lines are skipped per the SSE spec, while a
//! `data:` payload that is not valid JSON is fatal (matching cardinal's own
//! verdict on a misbehaving upstream).

pub mod chat;
pub mod client;
pub mod limit;
pub mod router;
pub mod stream;
pub mod wire;

pub use client::{CardinalClient, ChatStream};
pub use router::EndpointRouter;
pub use stream::{AssembledMessage, AssembledToolCall, Assembler, ChunkDecoder, StreamEvent};
pub use wire::{ChatMessage, ChatRequest, ToolCall, ToolDef, Usage};

/// The shared transport error lives at the crate root: both wire protocols
/// return it, so it cannot belong to either one.
pub use crate::error::{ChatError, HTTP_BODY_SNIPPET_BYTES};
