//! Serde models for the opencode v2 wire protocol (schema layer).
//!
//! Ports the upstream Effect-Schema tagged unions from
//! `packages/schema/src/*` to serde enums, byte-compatible on both encode
//! and decode:
//!
//! - [`message`]: `SessionMessage.Info` union + assistant content/tool
//!   state/compaction sub-unions.
//! - [`event`]: event envelope (`packages/schema/src/event.ts`) + typed
//!   `SessionEventData` union for the chat-path events; unknown event types
//!   fall back losslessly.
//! - [`inbox`]: `SessionInbox.Item` / `Info` unions and payloads.
//! - [`support`]: supporting structs (errors, token usage, model refs,
//!   prompt attachments, permission rules, ...).

pub mod event;
pub mod inbox;
pub mod message;
pub mod support;
