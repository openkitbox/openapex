//! Serde models for the opencode v2 wire protocol (contract layer).
//!
//! Extracted verbatim from ox's `src/schema/` so the server and any client
//! crate share one definition of the protocol.

pub mod event;
pub mod id;
pub mod inbox;
pub mod message;
pub mod paths;
pub mod slug;
pub mod support;
pub mod wire;
