//! OpenAI Responses wire translation: `ChatRequest` → a `/v1/responses` body.

pub mod client;
pub mod stream;
pub mod wire;

pub use client::{ResponsesClient, ResponsesStream, responses_url};
pub use stream::ResponsesDecoder;
pub use wire::{
    InputContent, InputItem, Reasoning, ResponsesRequest, ResponsesTool, build_request,
};
