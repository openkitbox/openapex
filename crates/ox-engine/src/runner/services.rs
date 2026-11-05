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
//! `agent_tools` is derived from `tools` once, at construction.

use std::future::Future;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use ox_core::config::Config;
use ox_llm::model_catalog::ModelCatalog;
use ox_mcp::Hub;
use ox_tools::agent;
use ox_tools::agent::Agent;
use ox_tools::agent::Tools;
use tokio::sync::Mutex as AsyncMutex;

use super::{Chat, ToolRegistry};

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
    /// The registry every agent toolset is derived from.
    pub tools: Arc<ToolRegistry>,
    /// One toolset per built-in agent, in agent-table order. Derived once here
    /// so [`Services::tools_for`] is a lookup, never a rebuild.
    agent_tools: Vec<(&'static Agent, Arc<ToolRegistry>)>,
}

impl Services {
    /// Move `set` into a generation and derive one toolset per agent.
    #[must_use]
    pub fn new(generation: u64, set: ServiceSet) -> Arc<Self> {
        let agent_tools = derive_agent_tools(&set.tools);
        Arc::new(Self {
            generation,
            config: set.config,
            chat: set.chat,
            model_catalog: set.model_catalog,
            mcp: set.mcp,
            tools: set.tools,
            agent_tools,
        })
    }

    /// The registry a session's turns advertise and dispatch against: the
    /// toolset of `agent`'s record.
    ///
    /// Membership is the only gate — a call to a tool outside the registry
    /// fails as an unknown tool. A child session must not see `question`: no UI
    /// watches it and ox has no foreground tool timeout, so the form would hang
    /// the parent's `subagent` call.
    ///
    /// # Panics
    ///
    /// If `agent` is not a built-in record. Every agent a session can resolve to
    /// comes from [`agent::all`], so a caller passing anything else is a wiring
    /// bug, not a runtime condition: falling back to a wider toolset here would
    /// hand a session tools its agent withholds.
    #[must_use]
    pub fn tools_for(&self, agent: &'static Agent) -> Arc<ToolRegistry> {
        self.derived_tools(agent.id).expect(
            "every agent a session resolves to is a built-in record derived at construction",
        )
    }

    /// The derived toolset for one agent id, if this generation derived it.
    fn derived_tools(&self, id: &str) -> Option<Arc<ToolRegistry>> {
        self.agent_tools
            .iter()
            .find(|(known, _)| known.id == id)
            .map(|(_, tools)| Arc::clone(tools))
    }
}

/// One toolset per built-in agent, applying each record's [`Tools`] rule to
/// `tools`. `Tools::All` shares the source registry rather than copying it;
/// the filtered sets rebuild the name map once per generation (spawn-time work,
/// never on a hot path) while sharing the tool `Arc`s.
#[must_use]
fn derive_agent_tools(tools: &Arc<ToolRegistry>) -> Vec<(&'static Agent, Arc<ToolRegistry>)> {
    agent::all()
        .iter()
        .map(|agent| {
            let set = match agent.tools {
                Tools::All => Arc::clone(tools),
                Tools::Without(names) => Arc::new(tools.without(names)),
                Tools::Only(names) => Arc::new(tools.only(names)),
            };
            (agent, set)
        })
        .collect()
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
