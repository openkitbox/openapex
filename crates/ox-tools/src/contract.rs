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
    /// The assistant message that holds the tool call; a tool that reports
    /// `session.tool.progress` addresses its part with it and `call_id`.
    pub assistant_message_id: String,
    /// The assistant tool-call id this execution answers.
    pub call_id: String,
    /// Fires when the turn is interrupted; long tools should poll it.
    pub cancellation: CancellationToken,
    /// The registry the call runs in — the session's live toolset (a
    /// `subagent` child's excludes what the child drops). Only cross-tool
    /// seams read it: `capability_search` indexes the catalog from here, so
    /// MCP and CBM tools are covered exactly as advertised. Most tools never
    /// touch it.
    pub registry: std::sync::Arc<ToolRegistry>,
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
    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// A successful output carrying model-visible attachments.
    #[must_use]
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

/// The boxed future [`Tool::execute`] returns.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

/// Run a synchronous tool body on tokio's blocking pool.
///
/// The file tools (`read`, `write`, `edit`, `patch`, `glob`, `grep`) are plain
/// filesystem work: a `grep` walks the whole tree, a `read` can pull in a
/// large file. Executed inline in the tool task they would hold an async
/// worker thread for that long, stalling every other actor sharing it;
/// here they occupy a blocking thread instead and the executor stays
/// responsive. A body that panics fails the call with the panic message
/// rather than taking the step down with it.
pub fn blocking<F>(ctx: ToolContext, input: Value, run: F) -> ToolFuture<'static>
where
    F: FnOnce(&ToolContext, &Value) -> ToolOutput + Send + 'static,
{
    Box::pin(async move {
        match tokio::task::spawn_blocking(move || run(&ctx, &input)).await {
            Ok(output) => output,
            Err(error) => ToolOutput {
                content: Value::Null,
                error: Some(format!("tool execution aborted: {error}")),
                metadata: None,
                attachments: None,
            },
        }
    })
}

/// Name-keyed tool registry. Definitions iterate in name order so the wire
/// `tools` array is stable across steps (keeps the provider's cached prefix).
#[derive(Default)]
pub struct ToolRegistry {
    tools: std::collections::BTreeMap<String, Arc<dyn Tool>>,
    /// The advertised definitions, built on first use and invalidated by every
    /// registration change. Definitions are pure functions of the registered
    /// tools, and a request is assembled on every model step, so rebuilding
    /// them per step only re-copies identical schemas.
    definitions_cache: std::sync::OnceLock<Arc<Vec<ToolDef>>>,
    /// The serialized size of the cached array, measured once with it.
    definitions_bytes_cache: std::sync::OnceLock<usize>,
    /// The handle the registered `subagent` tool shares with the runner, so
    /// the runner can attach itself the moment it exists.
    pub subagent_handle: SubagentSlot,
    /// The completion sink the registered `shell` tool reports through, so the
    /// runner can attach its waker the moment it exists.
    pub shell_completion: Option<std::sync::Arc<crate::shell::completion::ShellCompletion>>,
    /// Shared blocking-job registry; the server's `session.background`
    /// endpoint reaches the foreground tools' registrations through here.
    pub jobs: Arc<crate::jobs::Jobs>,
    /// Per-session file stamps: `read` records what it served, and the writing
    /// tools (`edit`, `write`, `patch`) refuse a file that changed since. One
    /// store per process, keyed by session, so a derived registry
    /// ([`ToolRegistry::without`] / [`ToolRegistry::only`]) shares it.
    pub files: Arc<crate::file_state::FileStates>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; duplicate names are a programming error.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.name().to_owned();
        self.definitions_cache = std::sync::OnceLock::new();
        self.definitions_bytes_cache = std::sync::OnceLock::new();
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
        self.definitions_cache = std::sync::OnceLock::new();
        self.definitions_bytes_cache = std::sync::OnceLock::new();
        self.tools.remove(name).is_some()
    }

    /// A copy of this registry without the named tools, sharing the remaining
    /// tool `Arc`s. Spawn-time only (a child session drops tools it cannot
    /// use); the map is rebuilt once per actor, never on a hot path.
    pub fn without(&self, names: &[&str]) -> Self {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| !names.contains(&name.as_str()))
            .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
            .collect();
        Self {
            tools,
            definitions_cache: std::sync::OnceLock::new(),
            definitions_bytes_cache: std::sync::OnceLock::new(),
            subagent_handle: self.subagent_handle.clone(),
            shell_completion: self.shell_completion.clone(),
            jobs: Arc::clone(&self.jobs),
            files: Arc::clone(&self.files),
        }
    }

    /// A copy of this registry holding only the named tools (names that are
    /// not registered are skipped), sharing their `Arc`s. The counterpart of
    /// [`ToolRegistry::without`] for allow-lists; built once per services
    /// generation, never on a hot path.
    pub fn only(&self, names: &[&str]) -> Self {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| names.contains(&name.as_str()))
            .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
            .collect();
        Self {
            tools,
            definitions_cache: std::sync::OnceLock::new(),
            definitions_bytes_cache: std::sync::OnceLock::new(),
            subagent_handle: self.subagent_handle.clone(),
            shell_completion: self.shell_completion.clone(),
            jobs: Arc::clone(&self.jobs),
            files: Arc::clone(&self.files),
        }
    }

    /// Bulk-register pre-built tools. Names must be unique; a duplicate is a
    /// programming error caught by debug builds (built-ins have fixed names).
    pub fn register_all(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        self.definitions_cache = std::sync::OnceLock::new();
        self.definitions_bytes_cache = std::sync::OnceLock::new();
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

    /// The same array, shared rather than rebuilt. A step hands this to the
    /// chat request by `Arc` clone, so advertising the tools costs no schema
    /// copies once the first request has been built.
    pub fn definitions_shared(&self) -> Arc<Vec<ToolDef>> {
        Arc::clone(
            self.definitions_cache
                .get_or_init(|| Arc::new(self.definitions())),
        )
    }

    /// The serialized size of [`Self::definitions_shared`] in bytes: what the
    /// tools array costs on every request, measured once. The size signal the
    /// context budget's baseline is built from.
    pub fn definitions_bytes(&self) -> usize {
        *self.definitions_bytes_cache.get_or_init(|| {
            serde_json::to_vec(&*self.definitions_shared())
                .map(|body| body.len())
                .unwrap_or(0)
        })
    }

    /// Each registered tool's name with the guidance it contributes, in name
    /// order. Borrowed throughout: the static system layer is assembled from
    /// this without cloning tool names.
    pub fn guidances(&self) -> impl Iterator<Item = (&str, Option<&'static str>)> + '_ {
        self.tools
            .iter()
            .map(|(name, tool)| (name.as_str(), tool.guidance()))
    }

    /// Every registered tool's `(name, description)`, in name order —
    /// `capability_search`'s live catalog view. Definitions are rebuilt per
    /// call: the catalog is dozens of entries and the tool is model-invoked,
    /// never a hot path.
    pub fn catalog(&self) -> Vec<(String, String)> {
        self.definitions()
            .into_iter()
            .map(|def| (def.function.name, def.function.description))
            .collect()
    }
}
