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
//! `child_tools` is derived from `tools` once, at construction.

use std::future::Future;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use ox_core::config::Config;
use ox_llm::model_catalog::ModelCatalog;
use ox_mcp::Hub;
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
}

impl Services {
    /// Move `set` into a generation and derive `child_tools` once.
    pub fn new(generation: u64, set: ServiceSet) -> Arc<Self> {
        let child_tools = Arc::new(set.tools.without(SUBAGENT_EXCLUDED_TOOLS));
        Arc::new(Self {
            generation,
            config: set.config,
            chat: set.chat,
            model_catalog: set.model_catalog,
            mcp: set.mcp,
            tools: set.tools,
            child_tools,
        })
    }

    /// The registry a session's turns advertise and dispatch against: the
    /// shared one, or `child_tools` for a subagent. A form opened by a child
    /// has no UI behind it and ox has no foreground tool timeout, so
    /// `question` there would hang the parent's `subagent` call.
    pub fn tools_for(&self, is_subagent: bool) -> Arc<ToolRegistry> {
        Arc::clone(if is_subagent {
            &self.child_tools
        } else {
            &self.tools
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

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ox_llm::cardinal::CardinalError;
    use ox_llm::cardinal::chat::ChatFuture;
    use ox_llm::cardinal::wire::ChatRequest;
    use ox_schema::wire::ToolDef;
    use serde_json::Value;
    use tokio::sync::{Notify, oneshot};

    use super::super::{Tool, ToolContext, ToolOutput};
    use super::*;

    struct StubChat;

    impl Chat for StubChat {
        fn stream_chat<'a>(&'a self, _request: &'a ChatRequest) -> ChatFuture<'a> {
            Box::pin(async { Err(CardinalError::EarlyClose) })
        }
    }

    struct NamedTool(&'static str);

    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn definition(&self) -> ToolDef {
            ToolDef::function(self.0, "test tool", serde_json::json!({"type": "object"}))
        }

        fn execute(
            &self,
            _ctx: ToolContext,
            _input: Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            Box::pin(async { unreachable!("not executed in these tests") })
        }
    }

    fn set() -> ServiceSet {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(NamedTool("question"))).unwrap();
        tools.register(Arc::new(NamedTool("read"))).unwrap();
        ServiceSet {
            config: Arc::new(Config::default()),
            chat: Arc::new(StubChat),
            model_catalog: Arc::new(ModelCatalog::from_entries(vec![])),
            mcp: Arc::new(Hub::empty()),
            tools: Arc::new(tools),
        }
    }

    #[tokio::test]
    async fn replace_success_advances_generation() {
        let live = Live::new(set());
        assert_eq!(live.generation(), 0);
        let next_set = set();
        let config = Arc::clone(&next_set.config);
        let next = live.replace(|_| async move { Ok(next_set) }).await.unwrap();
        assert_eq!(next.generation, 1);
        assert_eq!(live.generation(), 1);
        assert!(Arc::ptr_eq(&live.current().config, &config));
        assert!(Arc::ptr_eq(&live.current(), &next));
    }

    #[tokio::test]
    async fn replace_failure_keeps_current() {
        let live = Live::new(set());
        let before = live.current();
        let err = live
            .replace(|_| async { Err(anyhow::anyhow!("boom")) })
            .await
            .err()
            .expect("build error is returned");
        assert_eq!(err.to_string(), "boom");
        assert!(Arc::ptr_eq(&live.current(), &before));
        assert_eq!(live.generation(), 0);
    }

    #[tokio::test]
    async fn concurrent_replaces_are_serialized() {
        let live = Arc::new(Live::new(set()));
        let started_first = Arc::new(Notify::new());
        let (release_tx, release_rx) = oneshot::channel::<()>();

        let first = {
            let live = Arc::clone(&live);
            let started = Arc::clone(&started_first);
            tokio::spawn(async move {
                live.replace(|_| async move {
                    started.notify_one();
                    release_rx.await.unwrap();
                    Ok(set())
                })
                .await
                .unwrap()
                .generation
            })
        };
        started_first.notified().await;

        let second_started = Arc::new(AtomicBool::new(false));
        let second = {
            let live = Arc::clone(&live);
            let flag = Arc::clone(&second_started);
            tokio::spawn(async move {
                live.replace(|seen| async move {
                    flag.store(true, Ordering::SeqCst);
                    assert_eq!(seen.generation, 1, "second build sees the first result");
                    Ok(set())
                })
                .await
                .unwrap()
                .generation
            })
        };
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            !second_started.load(Ordering::SeqCst),
            "second build must wait for the first"
        );

        release_tx.send(()).unwrap();
        assert_eq!(first.await.unwrap(), 1);
        assert_eq!(second.await.unwrap(), 2);
        assert!(second_started.load(Ordering::SeqCst));
        assert_eq!(live.generation(), 2);
    }

    #[test]
    fn child_tools_exclude_question() {
        let services = Services::new(0, set());
        assert!(services.tools.get("question").is_some());
        assert!(services.tools.get("read").is_some());
        assert!(services.child_tools.get("question").is_none());
        assert!(services.child_tools.get("read").is_some());
    }

    #[tokio::test]
    async fn old_generation_is_freed_after_replace() {
        let live = Live::new(set());
        let old = live.current();
        let weak = Arc::downgrade(&old);
        drop(old);
        assert!(weak.upgrade().is_some(), "still current");
        live.replace(|_| async { Ok(set()) }).await.unwrap();
        assert!(weak.upgrade().is_none(), "no accumulation across reloads");
    }
}
