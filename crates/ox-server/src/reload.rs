//! `POST /api/location/reload`: re-read `ox.toml` and hot-swap the services
//! derived from it (upstream `location.reload`).
//!
//! **Hot keys** (rebuilt on every reload): the whole LLM section
//! (`cardinal_url`, `cardinal_key`, `cardinal_limits`, `[providers]`,
//! `[models]`, `context_caps`, `default_model`) becomes a new
//! `EndpointRouter` plus a pre-warmed `ModelCatalog`; `[mcp]` becomes a new
//! `Hub` plus the MCP tools in a rebuilt `ToolRegistry`.
//!
//! **Pinned keys** (kept at the running value, with a `warn!` naming each):
//! `listen`, `db_path`, `event_heartbeat_ms`, `[events]`, `[sessions]`, `[turn]`,
//! `[subagent]`, `[magic_context]` (see `Config::pin_restart_only`).
//!
//! Everything is built inside `Live::replace`, so concurrent reloads
//! serialize and each starts from the previous generation. Any failure (bad
//! TOML, an unknown `[agents.<id>]`, every model endpoint down) leaves the
//! running services untouched.
//! A new registry is the old one minus the MCP tools the old hub registered
//! (`Hub::tool_names`, recorded by `register_mcp_tools` from what it really
//! registered, never from what merely collided with a builtin) plus the new
//! hub's tools, so builtins are never removed.
//!
//! Known deviations from upstream: a failed reload keeps the old services
//! (upstream has already invalidated them); pending forms are not cancelled;
//! refresh events are stamped with the requesting directory only; the
//! Dreamer keeps the chat/catalog it was spawned with until restart.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use ox_core::config::Config;
use ox_engine::runner::ServiceSet;
use ox_llm::model_catalog::ModelCatalog;
use serde_json::{Value, json};

use crate::api::AppState;
use crate::{chat_transport, register_mcp_tools};

/// Reload the configuration and swap the services; on success announce the
/// change to clients of `directory`.
pub(crate) async fn run(state: &AppState, directory: &str) -> Result<()> {
    // The file is read inside the gate, so a reload that reads later always
    // installs later and pins against the generation it replaces.
    let mut was: BTreeSet<String> = BTreeSet::new();
    let was_slot = &mut was;
    let after = state
        .live
        .replace(|old| async move {
            let mut next = Config::load_from(state.config_path.clone())
                .with_context(|| format!("reload {}", state.config_path.display()))?;
            // A misspelled `[agents.<id>]` fails the reload, like any other bad
            // edit: `Live::replace` leaves the old services running.
            ox_tools::agent::validate_ids(next.agents.keys().map(String::as_str))
                .with_context(|| format!("reload {}", state.config_path.display()))?;
            // Keys `/connect` stored live in the database, not in `ox.toml`, so
            // every reload re-reads them before the transport is rebuilt —
            // otherwise a hot reload would forget a provider connected in this
            // process' lifetime.
            let credentials = ox_core::credential::load(&state.db)
                .await
                .context("loading stored credentials")?;
            next.llm.apply_credentials(&credentials);
            for name in next.pin_restart_only(&old.config) {
                tracing::warn!(
                    key = name,
                    "config key requires a restart; keeping the running value"
                );
            }
            if old.config.magic_context.dreamer_enabled {
                tracing::warn!(
                    "the dreamer keeps its previous chat transport and catalog until restart"
                );
            }
            let chat = chat_transport(&next)?;
            let model_catalog = ModelCatalog::new(&next.llm);
            model_catalog.refresh().await?;
            let mcp = state.runtime.new_hub();
            let stale = old.mcp.tool_names();
            let stale: Vec<&str> = stale.iter().map(String::as_str).collect();
            let mut tools = old.tools.without(&stale);
            register_mcp_tools(&mcp, &next.mcp, &state.directory, &mut tools).await;
            *was_slot = old.mcp.servers().into_iter().map(|s| s.name).collect();
            Ok(ServiceSet {
                config: std::sync::Arc::new(next),
                chat,
                model_catalog,
                mcp,
                tools: std::sync::Arc::new(tools),
            })
        })
        .await?;

    announce(state, directory, &was, &after);
    Ok(())
}

/// Ephemeral refresh frames (never persisted), stamped with the requesting
/// directory. `config.updated`, `command.updated`, `skill.updated` and
/// `agent.updated` are not sent: that data does not derive from `ox.toml`.
fn announce(
    state: &AppState,
    directory: &str,
    was: &BTreeSet<String>,
    after: &ox_engine::runner::Services,
) {
    let bus = &state.bus;
    let emit = |kind: &str, data: Value| bus.broadcast_ephemeral(kind, data, Some(directory));
    emit("location.shutdown", json!({}));
    emit("provider.updated", json!({}));
    emit("model.updated", json!({}));
    let now: BTreeSet<String> = after.mcp.servers().into_iter().map(|s| s.name).collect();
    for name in now.union(was) {
        emit("mcp.status.changed", json!({ "server": name }));
    }
    for name in &now {
        emit("mcp.resources.changed", json!({ "server": name }));
    }
}
