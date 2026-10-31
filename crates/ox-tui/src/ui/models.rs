//! Model and agent switching: the pure decisions behind `/models`, `/agents`
//! and the `Tab` agent cycle.
//!
//! No I/O, no app state. The catalogue comes in as data and goes out as a
//! [`Select`] (or a chosen agent id); `app::switching` owns the wiring.
//!
//! Reference: upstream `component/dialog-model.tsx` (grouped list, current
//! model marked) and `sortModelOptions`.
//!
//! Deviations from upstream, all deliberate:
//! - No Favorites/Recent/Free groups, no price/context detail row, no variant
//!   sub-dialog: ox has no preference store and `/api/model` carries no
//!   pricing. The list is the plain provider-grouped one.
//! - Provider groups are ordered by display name, models inside a group keep
//!   server order (upstream's `sortModelOptions` also leaves server order as
//!   the base and only stabilises the provider grouping).

use ox_client::rest::{AgentRef, ModelInfo, ProviderRef};

use super::dialog::{Item, Select};
use crate::bootstrap::display_agent;

/// What an open `Select` dialog was opened for, so `DialogOutcome::Selected`
/// knows what to do with the picked value. The App keeps one next to its
/// `dialog`; it is `None` for dialogs whose outcome nobody consumes. Each
/// variant owns the catalogue the dialog was built from, which is where the
/// display labels for the chrome come from.
#[derive(Debug)]
pub enum DialogPurpose {
    /// `/models`: values are [`encode`]d `provider/model`.
    Model(Vec<ProviderRef>),
    /// `/agents`: values are agent ids.
    Agent(Vec<AgentRef>),
}

/// `provider/model` — the value a model row carries through the dialog.
/// Provider ids never contain `/`, so the first one splits the pair.
pub fn encode(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}/{model_id}")
}

/// Inverse of [`encode`].
pub fn decode(value: &str) -> Option<(&str, &str)> {
    value.split_once('/')
}

/// The provider's display name, falling back to its id when the catalogue has
/// no entry (or an empty name) for it.
pub fn provider_label<'a>(providers: &'a [ProviderRef], provider_id: &'a str) -> &'a str {
    providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .map(|provider| provider.name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(provider_id)
}

/// Which model row is the current one, matched the way the prompt chrome
/// stores it: the model id plus the provider's display name.
pub fn current_model_value(
    models: &[ModelInfo],
    providers: &[ProviderRef],
    model: Option<&str>,
    provider: Option<&str>,
) -> Option<String> {
    let model = model?;
    models
        .iter()
        .filter(|candidate| candidate.enabled && candidate.model_id == model)
        .find(|candidate| {
            provider.is_none_or(|label| provider_label(providers, &candidate.provider_id) == label)
        })
        .map(|candidate| encode(&candidate.provider_id, &candidate.model_id))
}

/// Selectable models as dialog rows: disabled models dropped, grouped under
/// the provider's display name (groups sorted by name, server order inside).
pub fn model_items(models: &[ModelInfo], providers: &[ProviderRef]) -> Vec<Item> {
    let mut rows: Vec<(&str, &ModelInfo)> = models
        .iter()
        .filter(|model| model.enabled)
        .map(|model| (provider_label(providers, &model.provider_id), model))
        .collect();
    // `sort_by_key` is stable, so models keep server order within a provider.
    rows.sort_by_key(|(label, _)| label.to_lowercase());
    rows.into_iter()
        .map(|(label, model)| {
            let title = if model.name.is_empty() {
                &model.model_id
            } else {
                &model.name
            };
            Item::new(encode(&model.provider_id, &model.model_id), title).category(label)
        })
        .collect()
}

/// The `/models` dialog; `current` is the value to mark and start on.
pub fn model_select(
    models: &[ModelInfo],
    providers: &[ProviderRef],
    current: Option<String>,
) -> Select {
    let select = Select::new("Select model", model_items(models, providers));
    match current {
        Some(value) => select.current(value),
        None => select,
    }
}

/// Agents the user may switch between: visible primary ones, server order.
pub fn primary_agents(all: Vec<AgentRef>) -> Vec<AgentRef> {
    all.into_iter()
        .filter(|agent| !agent.hidden && agent.mode.as_deref() != Some("subagent"))
        .collect()
}

fn current_index(agents: &[AgentRef], current: Option<&str>) -> Option<usize> {
    let current = current?;
    agents
        .iter()
        .position(|agent| display_agent(&agent.name) == current)
}

/// The agent after (`step > 0`) or before (`step < 0`) the current one,
/// wrapping. `None` when there is nothing to switch to (fewer than two
/// agents). An unknown current agent counts as the first, the server default.
pub fn next_agent<'a>(
    agents: &'a [AgentRef],
    current: Option<&str>,
    step: isize,
) -> Option<&'a AgentRef> {
    if agents.len() < 2 {
        return None;
    }
    let len = agents.len() as isize;
    let from = current_index(agents, current).unwrap_or(0) as isize;
    agents.get((from + step).rem_euclid(len) as usize)
}

/// The `/agents` dialog; `current` is the chrome's agent display name.
pub fn agent_select(agents: &[AgentRef], current: Option<&str>) -> Select {
    let items = agents
        .iter()
        .map(|agent| Item::new(&agent.id, display_agent(&agent.name)))
        .collect();
    let select = Select::new("Select agent", items);
    match current_index(agents, current) {
        Some(index) => select.current(&agents[index].id),
        None => select,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn model(provider: &str, id: &str, name: &str, enabled: bool) -> ModelInfo {
        ModelInfo {
            model_id: id.into(),
            provider_id: provider.into(),
            name: name.into(),
            enabled,
            limit: Default::default(),
        }
    }

    fn provider(id: &str, name: &str) -> ProviderRef {
        ProviderRef {
            id: id.into(),
            name: name.into(),
        }
    }

    fn agent(id: &str, mode: &str, hidden: bool) -> AgentRef {
        AgentRef {
            id: id.into(),
            name: id.into(),
            mode: Some(mode.into()),
            hidden,
        }
    }

    fn catalogue() -> (Vec<ModelInfo>, Vec<ProviderRef>) {
        (
            vec![
                model("zeta", "z1", "Zeta One", true),
                model("cardinal", "k3-256k", "K3 256k", true),
                model("zeta", "z2", "", true),
                model("cardinal", "off", "Disabled", false),
                model("ghost", "g1", "Ghost", true),
            ],
            vec![provider("zeta", "Zeta"), provider("cardinal", "Cardinal")],
        )
    }

    #[test]
    fn values_round_trip_and_split_on_the_first_slash() {
        assert_eq!(encode("p", "m"), "p/m");
        assert_eq!(decode("p/m"), Some(("p", "m")));
        assert_eq!(decode("p/org/m"), Some(("p", "org/m")));
        assert_eq!(decode("nope"), None);
    }

    #[test]
    fn items_group_by_provider_name_and_keep_server_order_inside() {
        let (models, providers) = catalogue();
        let rows: Vec<(String, String, String)> = model_items(&models, &providers)
            .into_iter()
            .map(|item| (item.category.unwrap(), item.title, item.value))
            .collect();
        // "Cardinal" < "ghost" (id fallback) < "Zeta", case-insensitively.
        assert_eq!(
            rows,
            vec![
                (
                    "Cardinal".into(),
                    "K3 256k".into(),
                    "cardinal/k3-256k".into()
                ),
                ("ghost".into(), "Ghost".into(), "ghost/g1".into()),
                ("Zeta".into(), "Zeta One".into(), "zeta/z1".into()),
                // An empty name falls back to the model id.
                ("Zeta".into(), "z2".into(), "zeta/z2".into()),
            ]
        );
    }

    #[test]
    fn disabled_models_are_not_listed() {
        let (models, providers) = catalogue();
        assert!(
            model_items(&models, &providers)
                .iter()
                .all(|item| item.value != "cardinal/off")
        );
    }

    #[test]
    fn typing_filters_by_title_and_by_provider_group() {
        let (models, providers) = catalogue();
        let mut select = model_select(&models, &providers, None);
        for ch in "k3".chars() {
            select.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(select.selected_value(), Some("cardinal/k3-256k"));

        // The provider name is part of the match (`searchInTitle` keys).
        let mut select = model_select(&models, &providers, None);
        for ch in "zeta".chars() {
            select.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(select.selected_value(), Some("zeta/z1"));
    }

    #[test]
    fn the_current_model_is_found_by_id_and_provider_label() {
        let models = vec![
            model("a", "shared", "A", true),
            model("b", "shared", "B", true),
        ];
        let providers = vec![provider("a", "Alpha"), provider("b", "Beta")];
        let found = |model, provider| current_model_value(&models, &providers, model, provider);
        assert_eq!(
            found(Some("shared"), Some("Beta")).as_deref(),
            Some("b/shared")
        );
        assert_eq!(found(Some("shared"), None).as_deref(), Some("a/shared"));
        assert_eq!(found(Some("missing"), Some("Beta")), None);
        assert_eq!(found(None, Some("Beta")), None);
    }

    #[test]
    fn the_dialog_starts_on_the_current_model() {
        let (models, providers) = catalogue();
        let select = model_select(&models, &providers, Some("zeta/z2".into()));
        assert_eq!(select.selected_value(), Some("zeta/z2"));
    }

    #[test]
    fn only_visible_primary_agents_are_switchable() {
        let all = vec![
            agent("build", "primary", false),
            agent("explore", "subagent", false),
            agent("secret", "primary", true),
            agent("plan", "primary", false),
        ];
        let ids: Vec<String> = primary_agents(all).into_iter().map(|a| a.id).collect();
        assert_eq!(ids, vec!["build", "plan"]);
    }

    #[test]
    fn cycling_wraps_both_ways_and_needs_two_agents() {
        let agents = vec![
            agent("build", "primary", false),
            agent("plan", "primary", false),
            agent("code", "primary", false),
        ];
        let id = |current, step| next_agent(&agents, current, step).map(|a| a.id.as_str());
        assert_eq!(id(Some("Build"), 1), Some("plan"));
        assert_eq!(id(Some("Code"), 1), Some("build"));
        assert_eq!(id(Some("Build"), -1), Some("code"));
        // Unknown or absent current counts as the first agent.
        assert_eq!(id(None, 1), Some("plan"));
        assert_eq!(id(Some("Gone"), -1), Some("code"));

        assert!(next_agent(&agents[..1], Some("Build"), 1).is_none());
        assert!(next_agent(&[], None, 1).is_none());
    }

    #[test]
    fn the_agent_dialog_lists_display_names_and_marks_the_current() {
        let agents = vec![
            agent("build", "primary", false),
            agent("plan", "primary", false),
        ];
        let select = agent_select(&agents, Some("Plan"));
        assert_eq!(select.selected_value(), Some("plan"));
        let select = agent_select(&agents, None);
        assert_eq!(select.selected_value(), Some("build"));
    }
}
