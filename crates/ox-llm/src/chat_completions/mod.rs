//! OpenAI Chat Completions *streaming* client.
//!
//! Endpoints expose the `/v1/chat/completions` route and forward the protocol
//! bytes, so this module contains no provider-specific logic — the wire shapes
//! here are the canonical Chat Completions request/chunk forms and SSE framing:
//!
//! - **Request**: the protocol requires only `model` (string) and a non-empty
//!   `messages` array; every other field passes through verbatim — so
//!   [`wire::ChatRequest`] serializes canonical OpenAI fields
//!   (`tool_choice`, `stream: true`, `stream_options.include_usage: true`).
//! - **Chunks**: `data: {...}` lines carry `choices[0].delta.{content,
//!   reasoning_content | reasoning, tool_calls[]}`, `finish_reason`, and a
//!   final `usage` (often in a trailing chunk whose `choices` array is empty —
//!   that chunk does *not* terminate the stream; only `data: [DONE]` does).
//! - **Usage**: `prompt_tokens` / `completion_tokens` plus optional cached /
//!   reasoning token details, extracted from the vendor shapes in use.
//!
//! Layout: [`wire`] (request types), [`stream`] (SSE framing, chunk decoding,
//! delta assembly), [`client`] (HTTP + the [`client::ChatStream`]), [`chat`]
//! (the transport seam), [`limit`] (per-endpoint concurrency gate), and
//! [`router`] (multi-endpoint candidate routing).
//!
//! Errors: [`ChatError`] — an HTTP error carries the status and a body
//! snippet; framing-level junk lines are skipped per the SSE spec, while a
//! `data:` payload that is not valid JSON is fatal (a misbehaving upstream
//! fails the turn).

pub mod chat;
pub mod client;
pub mod limit;
pub mod router;
pub mod stream;
pub mod wire;

pub use client::{ChatCompletionsClient, ChatStream};
pub use router::EndpointRouter;
pub use stream::{AssembledMessage, AssembledToolCall, Assembler, ChunkDecoder, StreamEvent};
pub use wire::{ChatMessage, ChatRequest, ToolCall, ToolDef, Usage};

/// The shared transport error lives at the crate root: both wire protocols
/// return it, so it cannot belong to either one.
pub use crate::error::{ChatError, HTTP_BODY_SNIPPET_BYTES};
