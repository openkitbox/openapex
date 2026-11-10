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

#[cfg(test)]
mod tests {
    use super::*;
    use ox_llm::model_catalog::ModelEntry;

    fn entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: None,
            model_type: None,
            context: None,
            input: None,
            output: None,
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
            temperature: None,
            reasoning: false,
            supported_reasoning_levels: Vec::new(),
        }
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog::from_entries(vec![entry("cheap", "slim"), entry("big", "session")])
    }

    fn reference(id: &str, provider: &str) -> ModelRef {
        ModelRef {
            id: id.to_owned(),
            provider_id: provider.to_owned(),
            variant: None,
        }
    }

    #[test]
    fn logical_resolves_an_entry_and_misses_unknown_ids() {
        let catalog = catalog();
        assert_eq!(logical(&catalog, "slim"), Some(reference("slim", "cheap")));
        assert_eq!(logical(&catalog, "missing"), None);
    }

    #[test]
    fn agent_model_prefers_config_then_the_built_in_table() {
        let explore = ox_tools::agent::get("explore").expect("explore is a built-in agent");
        let mut config = Config::default();
        assert_eq!(agent_model(&config, explore), Some(SLIM));
        config.agents.insert(
            "explore".to_owned(),
            ox_core::config::AgentConfig {
                model: Some("triage".to_owned()),
            },
        );
        assert_eq!(agent_model(&config, explore), Some("triage"));
    }

    #[test]
    fn agent_model_is_none_without_a_preference() {
        let general = ox_tools::agent::get("general").expect("general is a built-in agent");
        assert_eq!(agent_model(&Config::default(), general), None);
    }

    #[test]
    fn title_model_defaults_to_slim() {
        let model = title_model(&Config::default(), &catalog(), reference("session", "big"));
        assert_eq!(model, reference("slim", "cheap"));
    }

    #[test]
    fn title_model_uses_the_configured_role() {
        let mut config = Config::default();
        config.roles.title = Some("session".to_owned());
        let model = title_model(&config, &catalog(), reference("slim", "cheap"));
        assert_eq!(model, reference("session", "big"));
    }

    #[test]
    fn title_model_falls_back_to_the_session_model() {
        let mut config = Config::default();
        config.roles.title = Some("missing".to_owned());
        let model = title_model(&config, &catalog(), reference("session", "big"));
        assert_eq!(model, reference("session", "big"));
    }

    #[test]
    fn turn_model_without_an_override_keeps_the_session_model() {
        let model = turn_model(&catalog(), None, reference("session", "big"));
        assert_eq!(model, reference("session", "big"));
    }

    #[test]
    fn turn_model_resolves_an_override() {
        let model = turn_model(&catalog(), Some("slim"), reference("session", "big"));
        assert_eq!(model, reference("slim", "cheap"));
    }

    #[test]
    fn turn_model_falls_back_when_the_override_is_unknown() {
        let model = turn_model(&catalog(), Some("missing"), reference("session", "big"));
        assert_eq!(model, reference("session", "big"));
    }

    #[test]
    fn compaction_model_prefers_the_configured_role_over_the_turn() {
        let mut config = Config::default();
        config.roles.compaction = Some("slim".to_owned());
        let turn = reference("session", "big");
        let model = compaction_model(&config, &catalog(), Some(&turn), turn.clone());
        assert_eq!(model, reference("slim", "cheap"));
    }

    #[test]
    fn compaction_model_falls_through_to_the_turn() {
        let mut config = Config::default();
        config.roles.compaction = Some("missing".to_owned());
        let turn = reference("session", "big");
        let model = compaction_model(&config, &catalog(), Some(&turn), reference("slim", "cheap"));
        assert_eq!(model, reference("session", "big"));
    }

    #[test]
    fn compaction_model_falls_back_to_the_session_model() {
        let model = compaction_model(
            &Config::default(),
            &catalog(),
            None,
            reference("session", "big"),
        );
        assert_eq!(model, reference("session", "big"));
    }

    #[test]
    fn override_id_reads_a_present_value_only() {
        let mut metadata = Record::new();
        assert_eq!(override_id(&metadata), None);
        metadata.insert(
            ox_schema::inbox::TURN_MODEL_KEY.to_owned(),
            serde_json::json!("slim"),
        );
        assert_eq!(override_id(&metadata), Some("slim"));
    }

    #[test]
    fn override_id_ignores_blank_values() {
        let mut metadata = Record::new();
        metadata.insert(
            ox_schema::inbox::TURN_MODEL_KEY.to_owned(),
            serde_json::json!("   "),
        );
        assert_eq!(override_id(&metadata), None);
    }
}
