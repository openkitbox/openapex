//! The tool contract: [`Tool`], its execution context and output, and the
//! name-keyed [`ToolRegistry`]. They live in `ox-tools` (not the engine) so a
//! tool and the engine that calls it compile against one definition without
//! depending on each other.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use ox_schema::wire::ToolDef;

/// Look a registered `subagent` tool's runner slot up; `None` until
/// [`crate::register_subagent`] ran (see [`crate::subagent::SubagentHandle`]).
pub type SubagentSlot = Option<std::sync::Arc<crate::subagent::SubagentHandle>>;

// ---------------------------------------------------------------------------
// Tools (registry filled by task 7; exercised by tests)
// ---------------------------------------------------------------------------

/// Execution-scoped context handed to [`Tool::execute`].
#[derive(Clone)]
pub struct ToolContext {
    /// Owning session.
    pub session_id: String,
    /// Canonical project directory the server runs in.
    pub directory: PathBuf,
    /// The assistant tool-call id this execution answers.
    pub call_id: String,
    /// Fires when the turn is interrupted; long tools should poll it.
    pub cancellation: CancellationToken,
}

/// One attachment a tool result carries to the model (upstream
/// `Content.File`): a `data:` URI plus the MIME type the client renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub uri: String,
    pub mime: String,
    pub name: Option<String>,
}

/// A tool's terminal output.
#[derive(Debug)]
pub struct ToolOutput {
    /// Content for the model (JSON text or structured value).
    pub content: Value,
    /// `Some` marks the execution as failed with this message.
    pub error: Option<String>,
    /// Client-facing detail the model never reads: the `metadata` field of the
    /// `session.tool.success` / `session.tool.failed` events, which the
    /// projector stores on the tool part. Upstream uses it for diffs, match
    /// counts and shell status; ox fills it where it has something to say and
    /// leaves it `None` otherwise.
    pub metadata: Option<Value>,
    /// Model-visible attachments (images, PDFs) emitted alongside the text.
    /// Upstream hands these to the provider as `file` content parts.
    pub attachments: Option<Vec<Attachment>>,
}

impl ToolOutput {
    /// A successful output carrying client-facing `metadata`.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// A successful output carrying model-visible attachments.
    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = Some(attachments);
        self
    }
}

/// A model-invocable unit of work. Task 7 registers real implementations;
/// the runner only needs the name/definition seam plus one async entry.
pub trait Tool: Send + Sync {
    /// Registry key; must equal `definition().function.name`.
    fn name(&self) -> &str;
    /// The OpenAI `tools` array entry advertised to the model.
    fn definition(&self) -> ToolDef;
    /// One line of cross-tool policy appended to the static system layer while
    /// this tool is registered (upstream pi's tool-contributed `guideline`).
    /// Distilled from this tool's own `definition` description — the single
    /// source of truth — and kept to one short sentence: prompt budget, not
    /// documentation. `None` (the default) contributes nothing.
    fn guidance(&self) -> Option<&'static str> {
        None
    }
    /// Run the tool with the parsed `arguments` object.
    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>>;
}

/// Name-keyed tool registry. Definitions iterate in name order so the wire
/// `tools` array is stable across steps (keeps the provider's cached prefix).
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::collections::BTreeMap<String, Arc<dyn Tool>>,
    /// The handle the registered `subagent` tool shares with the runner, so
    /// the runner can attach itself the moment it exists.
    pub subagent_handle: SubagentSlot,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; duplicate names are a programming error.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.name().to_owned();
        if self.tools.insert(name.clone(), tool).is_some() {
            bail!("tool `{name}` is already registered");
        }
        Ok(())
    }

    /// Look a tool up by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Drop a tool by name (`true` when it was there). Exists so tests can
    /// pin a toolset after `build_state_with` attaches the wiring-dependent
    /// ones (`subagent`).
    pub fn remove(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    /// Bulk-register pre-built tools. Names must be unique; a duplicate is a
    /// programming error caught by debug builds (built-ins have fixed names).
    pub fn register_all(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for tool in tools {
            let name = tool.name().to_owned();
            if self.tools.insert(name, tool.clone()).is_some() {
                debug_assert!(false, "duplicate tool name `{}`", tool.name());
            }
        }
    }

    /// Whether no tools are registered (ox ships empty until task 7).
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The OpenAI `tools` array in name order.
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// Each registered tool's name with the guidance it contributes, in name
    /// order. Borrowed throughout: the static system layer is assembled from
    /// this without cloning tool names.
    pub fn guidances(&self) -> impl Iterator<Item = (&str, Option<&'static str>)> + '_ {
        self.tools
            .iter()
            .map(|(name, tool)| (name.as_str(), tool.guidance()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool that only answers the registry seam (name, definition, the
    /// optional guidance); execution is never reached.
    struct Stub {
        name: &'static str,
        guidance: Option<&'static str>,
    }

    impl Tool for Stub {
        fn name(&self) -> &str {
            self.name
        }

        fn definition(&self) -> ToolDef {
            ToolDef::function(self.name, "stub", serde_json::json!({ "type": "object" }))
        }

        fn guidance(&self) -> Option<&'static str> {
            self.guidance
        }

        fn execute(
            &self,
            _ctx: ToolContext,
            _input: Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            unimplemented!("stub tool is never executed")
        }
    }

    #[test]
    fn guidance_defaults_to_none() {
        struct Silent;
        impl Tool for Silent {
            fn name(&self) -> &str {
                "silent"
            }
            fn definition(&self) -> ToolDef {
                ToolDef::function("silent", "stub", serde_json::json!({ "type": "object" }))
            }
            fn execute(
                &self,
                _ctx: ToolContext,
                _input: Value,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
                unimplemented!("stub tool is never executed")
            }
        }
        assert_eq!(Silent.guidance(), None);
    }

    #[test]
    fn guidances_iterate_in_name_order_and_carry_each_tools_line() {
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(Stub {
                name: "zeta",
                guidance: Some("zeta line"),
            }))
            .unwrap();
        registry
            .register(Arc::new(Stub {
                name: "alpha",
                guidance: None,
            }))
            .unwrap();
        let collected: Vec<(&str, Option<&'static str>)> = registry.guidances().collect();
        assert_eq!(
            collected,
            vec![("alpha", None), ("zeta", Some("zeta line"))]
        );
    }
}
