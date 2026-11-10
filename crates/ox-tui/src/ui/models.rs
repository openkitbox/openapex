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
#[must_use]
pub fn encode(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}/{model_id}")
}

/// The server uses `default` for the absence of a named reasoning level.
#[must_use]
pub fn normalize_variant(variant: Option<&str>) -> Option<&str> {
    variant.filter(|value| !value.is_empty() && *value != "default")
}

/// Cycle through named reasoning levels, returning `None` for the default.
/// The catalogue's `default` entry is never shown as a named level.
#[must_use]
pub fn cycle_variant<'a>(
    current: Option<&str>,
    variants: &'a [ox_client::rest::ModelVariant],
) -> Option<&'a str> {
    let named: Vec<&str> = variants
        .iter()
        .map(|variant| variant.id.as_str())
        .filter(|id| *id != "default" && !id.is_empty())
        .collect();
    if named.is_empty() {
        return None;
    }
    let current = normalize_variant(current);
    let Some(index) = current.and_then(|value| named.iter().position(|id| *id == value)) else {
        return Some(named[0]);
    };
    named.get(index + 1).copied()
}

/// Rows for the variant picker: default first, followed by supported levels.
#[must_use]
pub fn variant_items(variants: &[ox_client::rest::ModelVariant]) -> Vec<Item> {
    let mut items = vec![Item::new("default", "Default")];
    items.extend(
        variants
            .iter()
            .filter(|variant| variant.id != "default" && !variant.id.is_empty())
            .map(|variant| Item::new(&variant.id, &variant.id)),
    );
    items
}

/// Inverse of [`encode`].
#[must_use]
pub fn decode(value: &str) -> Option<(&str, &str)> {
    value.split_once('/')
}

/// The provider's display name, falling back to its id when the catalogue has
/// no entry (or an empty name) for it.
#[must_use]
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
#[must_use]
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
#[must_use]
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
#[must_use]
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
#[must_use]
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
#[must_use]
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
#[must_use]
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
