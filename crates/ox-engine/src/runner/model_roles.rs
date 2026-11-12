//! Which model serves a request that is not the session's own turn.
//!
//! This module is the single owner of that decision. Everything here is a pure
//! function over a [`Config`], the cached model catalogue and an already
//! resolved session model: no IO, no cache, no lock, no database. Callers pass
//! logical model ids in and get a [`ModelRef`] out, which is what the turn
//! loop, the subagent seam and compaction persist or send.
//!
//! Precedence, highest first:
//!
//! | request      | order                                                              |
//! |--------------|--------------------------------------------------------------------|
//! | title        | `[roles].title` (default [`SLIM`]) → the session model on a miss    |
//! | turn         | the one-turn override of this prompt → the session model            |
//! | compaction   | `[roles].compaction` → this turn's snapshot → the session model     |
//! | child session| `[agents.<id>].model` → the built-in agent preference → inherit     |
//!
//! An explicit `[roles].compaction` outranks the turn snapshot because it is an
//! operator policy: compaction quality is load-bearing and its request size is
//! derived from the chosen model's window, so it must not follow a single
//! conversation's temporary downgrade.
//!
//! Invariants:
//! - Only logical ids travel through this module. Provider endpoints, candidate
//!   lists and failover state stay in the catalogue and router; nothing here
//!   copies or caches routing state.
//! - A miss on the catalogue is not fatal for title/turn/compaction: those fall
//!   back so the request still runs. A miss for a child session is fatal at the
//!   call site ([`logical`] returns `None` and the spawner refuses), because
//!   silently inheriting the parent's model would defeat a cost policy.
//! - Resolution reads `ModelCatalog::cached()` only, never `entries()`, so no
//!   request waits on the network to pick a model.

use ox_core::config::Config;
use ox_llm::model_catalog::ModelCatalog;
use ox_schema::support::{ModelRef, Record, SLIM};
use ox_tools::agent::Agent;

/// The [`ModelRef`] a logical model id resolves to, or `None` when the cached
/// catalogue does not know it.
///
/// With `[models.<id>]` routes configured the served catalogue projects one
/// entry per route key, so a logical id resolves here; in passthrough mode the
/// wire id is the logical id and resolves the same way. The variant stays unset:
/// a role picks a model, not a reasoning level.
pub(crate) fn logical(catalog: &ModelCatalog, id: &str) -> Option<ModelRef> {
    catalog
        .cached()
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| ModelRef {
            id: entry.id.clone(),
            provider_id: entry.provider.clone(),
            variant: None,
        })
}

/// The logical model a child session of `agent` prefers: `[agents.<id>].model`
/// outranks the built-in table, so an operator can retarget a role without a
/// rebuild. `None` means inherit the parent session's model.
pub(crate) fn agent_model<'a>(config: &'a Config, agent: &'a Agent) -> Option<&'a str> {
    config
        .agents
        .get(agent.id)
        .and_then(|configured| configured.model.as_deref())
        .or(agent.model)
}

/// The model for a session title: `[roles].title`, defaulting to [`SLIM`], and
/// the session's own model when that id is not in the catalogue.
pub(crate) fn title_model(
    config: &Config,
    catalog: &ModelCatalog,
    session_model: ModelRef,
) -> ModelRef {
    let id = config.roles.title.as_deref().unwrap_or(SLIM);
    match logical(catalog, id) {
        Some(model) => model,
        None => {
            tracing::warn!(
                role = "title",
                model = %id,
                session = %session_model.id,
                "logical model is not in the catalogue; using the session model",
            );
            session_model
        }
    }
}

/// The model for one turn: the prompt's one-turn override when it resolves,
/// otherwise the session's own model.
///
/// Takes no config on purpose: a turn override is a client decision, not an
/// operator policy like [`compaction_model`]'s role.
pub(crate) fn turn_model(
    catalog: &ModelCatalog,
    requested: Option<&str>,
    session_model: ModelRef,
) -> ModelRef {
    let Some(id) = requested else {
        return session_model;
    };
    match logical(catalog, id) {
        Some(model) => model,
        None => {
            tracing::warn!(
                role = "turn",
                model = %id,
                session = %session_model.id,
                "requested model is not in the catalogue; using the session model",
            );
            session_model
        }
    }
}

/// The model for a compaction request: `[roles].compaction`, then this turn's
/// snapshot, then the session's own model.
///
/// `turn` is the snapshot of the turn being compacted (`None` for a manual or
/// background compaction outside any turn). A configured role that does not
/// resolve is warned about and falls through rather than blocking the
/// compaction — the request still has to run.
pub(crate) fn compaction_model(
    config: &Config,
    catalog: &ModelCatalog,
    turn: Option<&ModelRef>,
    session_model: ModelRef,
) -> ModelRef {
    if let Some(id) = config.roles.compaction.as_deref() {
        if let Some(model) = logical(catalog, id) {
            return model;
        }
        tracing::warn!(
            role = "compaction",
            model = %id,
            "logical model is not in the catalogue; falling through to the turn model",
        );
    }
    turn.cloned().unwrap_or(session_model)
}

/// The one-turn model override a user prompt carries in its metadata, or `None`.
///
/// A present but empty value is `None`: the tool and TUI never mean "switch to
/// nothing", and an empty id can never resolve.
pub(crate) fn override_id(metadata: &Record) -> Option<&str> {
    let id = metadata
        .get(ox_schema::inbox::TURN_MODEL_KEY)
        .and_then(serde_json::Value::as_str)?
        .trim();
    (!id.is_empty()).then_some(id)
}
