//! Anthropic Messages protocol support: the wire types, the translation from
//! ox's internal [`crate::cardinal::wire::ChatRequest`], the SSE decoder and the
//! streaming client.
//!
//! Why this exists next to [`crate::cardinal`]: DeepSeek's own agent harness
//! talks to the official endpoint over Messages (`…/anthropic`), and Anthropic
//! endpoints only speak Messages. ox keeps one internal request/event language
//! (see [`crate::cardinal::chat::Chat`]) and translates at the transport
//! boundary, so the engine never learns which protocol an endpoint uses.
//!
//! Layout: [`wire`] (request types + translation), [`stream`] (SSE payloads →
//! [`crate::cardinal::stream::StreamEvent`]), [`client`] (HTTP + the
//! [`client::MessagesClient`]).

pub mod client;
pub mod stream;
pub mod wire;

pub use client::{MessagesClient, MessagesStream, messages_url};
pub use stream::{ANTHROPIC_VERSION, MessagesDecoder};
pub use wire::{
    ImageSource, MessagesPolicy, MessagesRequest, OutputConfig, Thinking, WireBlock, WireMessage,
    WireTool, WireToolChoice, build_request,
};
