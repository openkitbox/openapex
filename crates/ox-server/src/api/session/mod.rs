//! Session lifecycle endpoints (Task 4).
//!
//! Ports `packages/server/src/handlers/session.ts` (`session.*`) and
//! `packages/server/src/handlers/message.ts` (`session.messages`).
//!
//! Every mutation is published through [`ox_core::event_bus::EventBus`]; the
//! projector — never these handlers — writes `session_v2` / `session_message`.
//! Reads go straight to SQLite. Response shapes follow
//! `packages/protocol/openapi.json` field-for-field (the TUI deserializes
//! strictly).
//!
//! Deliberate scope limits (documented inline at each site):
//! - `session.update` with an empty title triggers upstream title generation
//!   (a whole model-backed subsystem) — not implemented here.
//! - `session.fork` does not snapshot instruction state.
//! - `session.compact` skips upstream's `SessionRevert.commit` (no revert).

mod compaction;
mod control;
mod cursor;
mod inbox;
mod lifecycle;
mod listing;
mod message;
mod prompting;
mod response;
mod session_row;
mod switch;
mod transfer;
mod user_shell;

pub(crate) use compaction::compact;
pub(crate) use control::{
    background, command, generate, interrupt, move_session, permission_list, synthetic, wait_idle,
};
pub(crate) use inbox::{inbox_cancel, inbox_list, inbox_update};
pub(crate) use lifecycle::{create, delete_session_tree, fork, get, remove, update};
pub(crate) use listing::list;
pub(crate) use message::{load_message_row, message_get, message_list};
pub(crate) use prompting::prompt;
pub(crate) use response::{invalid_request, message_not_found, session_not_found, unknown_error};
pub(crate) use session_row::resolve_project;
pub(crate) use switch::{active, switch_agent, switch_model, view};
pub(crate) use transfer::{export, import};
pub(crate) use user_shell::shell;
