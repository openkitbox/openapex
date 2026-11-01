//! Reloadable config-derived services.
//!
//! [`Services`] is one immutable generation of everything a config reload
//! rebuilds (config, chat transport, model catalog, MCP hub, tool registry).
//! [`Live`] owns the current generation and swaps it atomically: readers take
//! a snapshot with [`Live::current`] and keep using it for as long as they
//! hold it, so a reload never changes services under an in-flight step.
//! Dropping the last snapshot of an old generation frees it; nothing
//! accumulates across reloads.
//!
//! Invariants: `generation` increases by exactly one per successful
//! [`Live::replace`]; a failed build leaves the current snapshot untouched;
//! `child_tools` and `read_only_tools` are derived from `tools` once, at
//! construction.

use std::future::Future;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use ox_core::config::Config;
use ox_llm::model_catalog::ModelCatalog;
use ox_mcp::Hub;
use ox_tools::contract::READ_ONLY_AGENT;
use ox_tools::contract::READ_ONLY_TOOLS;
use tokio::sync::Mutex as AsyncMutex;

use super::{Chat, SUBAGENT_EXCLUDED_TOOLS, ToolRegistry};

/// Inputs for one generation: everything a reload rebuilds.
pub struct ServiceSet {
    pub config: Arc<Config>,
    pub chat: Arc<dyn Chat>,
    pub model_catalog: Arc<ModelCatalog>,
    pub mcp: Arc<Hub>,
    pub tools: Arc<ToolRegistry>,
}

/// One immutable generation of config-derived services.
pub struct Services {
    pub generation: u64,
    pub config: Arc<Config>,
    pub chat: Arc<dyn Chat>,
    pub model_catalog: Arc<ModelCatalog>,
    pub mcp: Arc<Hub>,
    /// Toolset for main sessions.
    pub tools: Arc<ToolRegistry>,
    /// Toolset for subagent children: `tools` minus [`SUBAGENT_EXCLUDED_TOOLS`].
    pub child_tools: Arc<ToolRegistry>,
    /// Toolset for read-only children: only [`READ_ONLY_TOOLS`].
    pub read_only_tools: Arc<ToolRegistry>,
}

/// Which toolset and prompt profile a session runs with. Resolved from the
/// session row at the start of each busy period: the parent link is fixed,
/// but the agent can be switched between periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A root session the user drives.
    Main,
    /// A child spawned by `subagent`.
    Subagent,
    /// A child spawned by `subagent` with `read_only: true`.
    ReadOnlySubagent,
}

impl SessionKind {
    /// Classify a session row. Only a child gets the read-only tier: a root
    /// session whose agent happens to be named `explore` stays [`Main`].
    ///
    /// [`Main`]: SessionKind::Main
    #[must_use]
    pub fn of(parent_id: Option<&str>, agent: Option<&str>) -> Self {
        match (parent_id, agent) {
            (None, _) => Self::Main,
            (Some(_), Some(agent)) if agent == READ_ONLY_AGENT => Self::ReadOnlySubagent,
            (Some(_), _) => Self::Subagent,
        }
    }

    /// Whether the session was spawned by another session.
    #[must_use]
    pub fn is_subagent(self) -> bool {
        !matches!(self, Self::Main)
    }
}

impl Services {
    /// Move `set` into a generation and derive the child registries once.
    #[must_use]
    pub fn new(generation: u64, set: ServiceSet) -> Arc<Self> {
        let child_tools = Arc::new(set.tools.without(SUBAGENT_EXCLUDED_TOOLS));
        let read_only_tools = Arc::new(set.tools.only(READ_ONLY_TOOLS));
        Arc::new(Self {
            generation,
            config: set.config,
            chat: set.chat,
            model_catalog: set.model_catalog,
            mcp: set.mcp,
            tools: set.tools,
            child_tools,
            read_only_tools,
        })
    }

    /// The registry a session's turns advertise and dispatch against: the
    /// shared one, `child_tools` for a subagent, or `read_only_tools` for a
    /// read-only child. A form opened by a child has no UI behind it and ox
    /// has no foreground tool timeout, so `question` there would hang the
    /// parent's `subagent` call. A read-only child calling a tool outside its
    /// registry gets the registry's unknown-tool error.
    #[must_use]
    pub fn tools_for(&self, kind: SessionKind) -> Arc<ToolRegistry> {
        Arc::clone(match kind {
            SessionKind::Main => &self.tools,
            SessionKind::Subagent => &self.child_tools,
            SessionKind::ReadOnlySubagent => &self.read_only_tools,
        })
    }
}

/// Holder of the current [`Services`] generation.
pub struct Live {
    current: RwLock<Arc<Services>>,
    /// Serializes [`Live::replace`] so each build sees the previous result.
    gate: AsyncMutex<()>,
}

impl Live {
    /// Start at generation 0.
    #[must_use]
    pub fn new(set: ServiceSet) -> Self {
        Self {
            current: RwLock::new(Services::new(0, set)),
            gate: AsyncMutex::new(()),
        }
    }

    /// The current snapshot. Poisoning is recovered: the guarded value is a
    /// single `Arc`, always consistent.
    pub fn current(&self) -> Arc<Services> {
        Arc::clone(&self.current.read().unwrap_or_else(|p| p.into_inner()))
    }

    /// The current generation number, without cloning the snapshot.
    pub fn generation(&self) -> u64 {
        self.current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .generation
    }

    /// Build and install the next generation. Calls are serialized; `build`
    /// receives the current snapshot. On `Err` nothing changes and the error
    /// is returned.
    pub async fn replace<F, Fut>(&self, build: F) -> Result<Arc<Services>>
    where
        F: FnOnce(Arc<Services>) -> Fut,
        Fut: Future<Output = Result<ServiceSet>>,
    {
        let _gate = self.gate.lock().await;
        let old = self.current();
        let set = build(Arc::clone(&old)).await?;
        let next = Services::new(old.generation + 1, set);
        *self.current.write().unwrap_or_else(|p| p.into_inner()) = Arc::clone(&next);
        Ok(next)
    }
}
