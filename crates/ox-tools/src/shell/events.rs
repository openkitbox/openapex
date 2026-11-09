//! The bridge from the shell table's lifecycle notices to the event bus.
//!
//! Upstream's TUI treats a shell's presence in its `data.shell` store as
//! "running"; the store is filled by `GET /api/shell` and by the ephemeral
//! `shell.created` / `shell.exited` / `shell.deleted` frames (upstream
//! `packages/schema/src/shell.ts`). [`frame`] is the pure mapping from a
//! [`Notice`] to its wire frame; [`init`] is the edge that installs the
//! publisher on the process-wide table. There is no task: the table calls the
//! observer inline, so ordering is the table's and nothing can outlive a shell.

use ox_core::event_bus::EventBus;
use serde_json::{Value, json};

use super::sessions::{Notice, ShellSessions};

/// `(event type, data)` for one notice, in the upstream shapes:
/// `shell.created { info }`, `shell.exited { id, exit?, status }`,
/// `shell.deleted { id }`.
#[must_use]
pub fn frame(notice: Notice) -> (&'static str, Value) {
    match notice {
        Notice::Created(info) => ("shell.created", json!({ "info": info })),
        Notice::Exited { id, exit, status } => {
            let mut data = json!({ "id": id, "status": status });
            if let Some(code) = exit {
                data["exit"] = json!(code);
            }
            ("shell.exited", data)
        }
        Notice::Deleted { id } => ("shell.deleted", json!({ "id": id })),
    }
}

/// Publish the shared table's notices on `bus`, stamped with the owning
/// session's directory (the TUI drops `shell.*` frames without a location).
/// The composition root calls this once per boot; a later call replaces the
/// bus, as [`crate::form::init`] does.
pub fn init(sessions: &ShellSessions, bus: EventBus) {
    sessions.set_observer(std::sync::Arc::new(move |directory, notice| {
        let (kind, data) = frame(notice);
        bus.broadcast_ephemeral(kind, data, Some(directory));
    }));
}
