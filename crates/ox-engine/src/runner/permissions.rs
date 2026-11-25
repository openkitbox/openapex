//! The tool permission gate adapter: one authorization call per tool execution.
//!
//! Pending requests, replies and saved approvals are owned by
//! [`crate::permission::PermissionService`], which is shared with the HTTP
//! layer. This adapter only carries the per-turn facts (config generation,
//! agent rules and the unattended marker) into that service.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use ox_core::config::Config;
use ox_schema::permission::Rule;
use ox_tools::contract::{ExecuteHost, NestedCallContext, ToolContext};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::permission::PermissionService;
use crate::permission::gate::AuthorizeParams;
pub use crate::permission::{Decision, SourceRef};

/// Everything one authorization needs, cloned out of the drain context so the
/// check runs inside the spawned tool task.
#[derive(Clone)]
pub(crate) struct Gate {
    pub service: Arc<PermissionService>,
    pub config: Arc<Config>,
    pub agent: &'static ox_tools::agent::Agent,
    /// `Some` only for an automation turn: no pending request is created.
    pub unattended: Option<Vec<Rule>>,
    pub source: Option<SourceRef>,
}

impl Gate {
    pub(crate) async fn authorize(
        &self,
        session_id: &str,
        directory: &Path,
        tool: &str,
        input: &Value,
        is_mcp: bool,
        cancellation: &CancellationToken,
    ) -> Decision {
        self.service
            .authorize(AuthorizeParams {
                config: &self.config,
                agent: self.agent,
                session_id,
                directory,
                tool,
                input,
                source: self.source.clone(),
                unattended: self.unattended.as_deref(),
                is_mcp,
                cancellation,
            })
            .await
    }
}

/// The engine half of the `execute` host. Every nested call takes the same
/// [`Gate::authorize`] path as a top-level call; the server composes progress
/// around this handle without owning any permission state.
pub(crate) struct NestedToolRunner {
    db: ox_db::Db,
    permissions: Arc<PermissionService>,
    live: Arc<super::Live>,
}

impl NestedToolRunner {
    pub(crate) fn new(
        db: ox_db::Db,
        permissions: Arc<PermissionService>,
        live: Arc<super::Live>,
    ) -> Self {
        Self {
            db,
            permissions,
            live,
        }
    }
}

impl ExecuteHost for NestedToolRunner {
    fn run_nested<'a>(
        &'a self,
        ctx: NestedCallContext<'a>,
        path: &'a str,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            if ctx.cancellation.is_cancelled() {
                return Err("Tool execution interrupted".to_owned());
            }
            let input: Value = if input.trim().is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(input)
                    .map_err(|error| format!("Nested tool input is not valid JSON: {error}"))?
            };
            let services = self.live.current();
            let agent = super::session_agent(&self.db, ctx.session_id).await;
            let registry = services.tools_for(agent);
            let Some(tool) = registry.get(path) else {
                return Err(format!("Tool is not available for this request: {path}"));
            };
            // The nested call is its own identity, so a permission ask and
            // any tool seeing `call_id` never confuse it with the parent.
            let call_id = loop {
                let candidate = ox_schema::id::id("call");
                if candidate != ctx.parent_call_id {
                    break candidate;
                }
            };
            let gate = Gate {
                service: Arc::clone(&self.permissions),
                config: Arc::clone(&services.config),
                agent,
                unattended: None,
                source: Some(SourceRef {
                    message_id: ctx.assistant_message_id.to_owned(),
                    call_id: call_id.clone(),
                }),
            };
            let is_mcp = services.mcp.has_tool(path);
            if let Decision::Deny(reason) = gate
                .authorize(
                    ctx.session_id,
                    ctx.directory,
                    path,
                    &input,
                    is_mcp,
                    &ctx.cancellation,
                )
                .await
            {
                return Err(reason);
            }
            if ctx.cancellation.is_cancelled() {
                return Err("Tool execution interrupted".to_owned());
            }
            let output = tool
                .execute(
                    ToolContext {
                        session_id: ctx.session_id.to_owned(),
                        directory: ctx.directory.to_path_buf(),
                        assistant_message_id: ctx.assistant_message_id.to_owned(),
                        call_id,
                        cancellation: ctx.cancellation,
                        registry,
                    },
                    input,
                )
                .await;
            match output.error {
                Some(error) => Err(error),
                None => Ok(tool_output_text(output.content)),
            }
        })
    }

    fn publish_progress(
        &self,
        _session_id: &str,
        _assistant_message_id: &str,
        _call_id: &str,
        _metadata: Value,
    ) {
        // The server's composing host owns progress; this adapter only
        // authorizes and executes.
    }
}

/// A tool output's model-facing text: strings verbatim, anything else as
/// compact JSON. Shared with the top-level tool task so nested and top-level
/// calls render content the same way.
pub(crate) fn tool_output_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use ox_core::config::{Config, Permission, ToolPermission};
    use ox_core::event_bus::EventBus;
    use ox_db::Db;
    use ox_llm::chat_completions::EndpointRouter;
    use ox_llm::model_catalog::ModelCatalog;
    use ox_mcp::Hub;
    use ox_schema::wire::ToolDef;
    use ox_tools::contract::{Tool, ToolOutput, ToolRegistry};
    use tokio::sync::oneshot;

    use super::*;

    struct ProbeTool {
        name: &'static str,
        calls: Arc<AtomicUsize>,
        started: Arc<Mutex<Option<oneshot::Sender<String>>>>,
        await_cancel: bool,
    }

    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn definition(&self) -> ToolDef {
            ToolDef::function(
                self.name,
                "permission probe",
                serde_json::json!({"type": "object"}),
            )
        }

        fn execute(
            &self,
            ctx: ToolContext,
            _input: Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            let calls = Arc::clone(&self.calls);
            let started = Arc::clone(&self.started);
            let await_cancel = self.await_cancel;
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(sender) = started
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    let _ = sender.send(ctx.call_id.clone());
                }
                if await_cancel {
                    ctx.cancellation.cancelled().await;
                    return ToolOutput {
                        content: Value::Null,
                        error: Some("Tool execution interrupted".to_owned()),
                        metadata: None,
                        attachments: None,
                    };
                }
                ToolOutput {
                    content: Value::String("ok".to_owned()),
                    error: None,
                    metadata: None,
                    attachments: None,
                }
            })
        }
    }

    async fn harness(config: Config, probe: ProbeTool) -> (NestedToolRunner, Arc<ProbeTool>) {
        let db = Db::open_in_memory().await.expect("in-memory database");
        let connection = db.acquire().await.expect("db connection");
        connection
            .execute(
                "INSERT INTO session_v2 \
                 (id, project_id, slug, directory, version, time_created, time_updated) \
                 VALUES ('ses_nested', 'proj', 'probe', '.', '1', 0, 0)",
                (),
            )
            .await
            .expect("session row");
        drop(connection);

        let catalog = ModelCatalog::new(&config.llm);
        let chat: Arc<dyn crate::runner::Chat> = Arc::new(
            EndpointRouter::from_config(&config.llm).expect("chat router without endpoints"),
        );
        let probe = Arc::new(probe);
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::clone(&probe) as Arc<dyn Tool>)
            .expect("register probe");
        let tools = Arc::new(tools);
        let live = Arc::new(super::super::Live::new(super::super::ServiceSet {
            config: Arc::new(config),
            chat,
            model_catalog: catalog,
            mcp: Arc::new(Hub::empty()),
            tools,
        }));
        let bus = EventBus::new(db.clone());
        let permissions = Arc::new(PermissionService::new(
            db.clone(),
            bus,
            CancellationToken::new(),
        ));
        (NestedToolRunner::new(db, permissions, live), probe)
    }

    #[tokio::test]
    async fn permissions_nested_call_is_denied_before_execution() {
        let mut config = Config::default();
        config
            .permissions
            .tools
            .insert("probe".to_owned(), ToolPermission::Policy(Permission::Deny));
        let (sender, _receiver) = oneshot::channel();
        let (host, probe) = harness(
            config,
            ProbeTool {
                name: "probe",
                calls: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Mutex::new(Some(sender))),
                await_cancel: false,
            },
        )
        .await;
        let ctx = NestedCallContext {
            session_id: "ses_nested",
            directory: Path::new("."),
            assistant_message_id: "msg_parent",
            parent_call_id: "call_parent",
            cancellation: CancellationToken::new(),
        };
        let Err(reason) = host.run_nested(ctx, "probe", "{}").await else {
            panic!("a denied nested call must not succeed");
        };
        assert!(
            reason.contains("denied"),
            "unexpected denial reason: {reason}"
        );
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            0,
            "the nested tool must not execute after denial"
        );
    }

    #[tokio::test]
    async fn permissions_nested_call_uses_fresh_id_and_parent_cancellation() {
        let (sender, receiver) = oneshot::channel();
        let (host, _probe) = harness(
            Config::default(),
            ProbeTool {
                // `question` is outside the permission extractor, so the
                // test isolates identity/cancellation from the deny path.
                name: "question",
                calls: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Mutex::new(Some(sender))),
                await_cancel: true,
            },
        )
        .await;
        let token = CancellationToken::new();
        let ctx = NestedCallContext {
            session_id: "ses_nested",
            directory: Path::new("."),
            assistant_message_id: "msg_parent",
            parent_call_id: "call_parent",
            cancellation: token.clone(),
        };
        let nested = tokio::spawn(async move { host.run_nested(ctx, "question", "{}").await });
        let call_id = tokio::time::timeout(Duration::from_secs(5), receiver)
            .await
            .expect("the nested tool started")
            .expect("the tool reported its call id");
        assert_ne!(call_id, "call_parent", "nested calls need their own id");
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), nested)
            .await
            .expect("the nested call settled")
            .expect("the nested task joined");
        let Err(reason) = result else {
            panic!("the parent's cancellation must stop the nested tool");
        };
        assert_eq!(reason, "Tool execution interrupted");
    }
}
