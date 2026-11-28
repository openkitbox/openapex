//! OpenAI Responses wire translation: `ChatRequest` → a `/v1/responses` body,
//! plus the option layering and opaque replay state that translation consumes.

pub mod client;
pub mod control;
pub mod options;
pub mod replay;
pub mod stream;
pub mod webhook;
pub mod wire;

pub use client::{ResponsesClient, ResponsesStream, responses_url};
pub use options::{
    RequestOptions, ResponsesOptions, ResponsesOptionsError, ResponsesStateMode, merge_options,
};
pub use replay::{
    REPLAY_KEY, ResponsesOrigin, ResponsesReplayInput, ResponsesReplayState, from_provider_state,
};
pub use stream::ResponsesDecoder;
pub use wire::{
    InputContent, InputItem, Reasoning, ResponsesRequest, ResponsesTool, build_request,
};
