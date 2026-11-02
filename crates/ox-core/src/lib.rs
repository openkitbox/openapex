//! Shared core for ox: configuration, the event bus and its projector, and the
//! `§N§` tag stripper. The LLM endpoint config, model catalogue and outbound
//! HTTP identity live in `ox-llm` (re-exported here where callers need them).
//!
//! Depends only on the contract (`ox-schema`), storage (`ox-db`) and
//! LLM-endpoint (`ox-llm`) crates — everything engine-shaped (runner, tools,
//! mctx) sits above it.

pub mod config;
pub mod copied_tags;
pub mod credential;
pub mod event_bus;
pub mod home_paths;
pub mod projector;
pub mod reminder_tags;
pub mod which;

pub use ox_llm::{USER_AGENT, http_client};
