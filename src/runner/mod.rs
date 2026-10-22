//! Per-session execution actors (Task 6a).
//!
//! One actor task per active session, spawned lazily by [`Runner::dispatch`]
//! and parked (dropped) when the session goes idle — the same process-local
//! ownership model as upstream `packages/core/src/session/execution.ts`
//! (`SessionRunCoordinator`), collapsed to a plain tokio task:
//!
//! - **Busy period**: consecutive [`drain::drain`] passes covering every
//!   inbox item admitted while work was in flight. Exactly one
//!   `session.execution.started` opens the period and one terminal
//!   (`succeeded` / `failed` / `interrupted`) closes it, mirroring upstream's
//!   one-terminal-per-busy-period rule.
//! - **Interruption**: the endpoint cancels the busy period's
//!   [`CancellationToken`]; the drain aborts at the next await point and the
//!   actor publishes the terminal. Idle interruption is a no-op.
//! - **Parked handoff**: an idle session's mailbox receiver moves back into
//!   the registry entry; the next dispatch takes it out and spawns a fresh
//!   task. Send always happens before the take, and the parked task re-checks
//!   the channel under the slot mutex before parking, so no message can fall
//!   into the gap.
//!
//! The [`Tool`] trait + [`ToolRegistry`] here are the seam task 7 fills with
//! real tools; the registry ships empty until task 7 fills it.

pub use crate::cardinal::chat::Chat;

pub(crate) mod compaction;
pub mod drain;
mod historian_trigger;
pub(crate) mod instructions;
pub(crate) mod provider_error;
pub mod step;
pub(crate) mod title;
pub(crate) mod todo_reminder;

/// Reset the todo reminder budget after the todo tool mutates a list. Kept as a
/// thin wrapper here so the tool does not depend on the reminder module's
/// internals; failures never surface (a stalled budget is a lost reminder, not
/// a failed tool call).
pub(crate) async fn reset_todo_reminder_budget(db: &crate::db::Db, session_id: &str) {
    todo_reminder::reset_budget(db, session_id).await;
}

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, bail};
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cardinal::wire::ToolDef;
use crate::config::Config;
use crate::db::Db;
use crate::event_bus::EventBus;
use crate::schema::event::{InterruptReason, SessionEventData};
use crate::schema::support::SessionError;

/// Messages the per-session actor mailbox accepts.
#[derive(Debug)]
pub enum ActorMsg {
    /// An inbox item was admitted; join (or open) the session's busy period.
    Enqueue {
        /// The `session_inbox` row id that was enqueued.
        inbox_id: String,
    },
    /// Abort the current busy period. The endpoint also cancels the period's
    /// token directly; this message covers the mailbox-absorb bookkeeping so
    /// a racing interrupt is never lost between absorb loops.
    Interrupt,
    /// Stop serving this session and drop the actor entirely.
    Shutdown,
}

/// Actor mailbox capacity. Prompt dispatch is one message per turn, so a
/// small bounded queue is ample; a full mailbox means a runaway client.
const MAILBOX_CAPACITY: usize = 64;

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
    pub(crate) fn register_all(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
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
}

// ---------------------------------------------------------------------------
// Runner registry
// ---------------------------------------------------------------------------

/// Registry of per-session actors. `dispatch` lazily spawns; interrupt and
/// shutdown reach the live actor through its registry entry.
#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

/// The handle the `subagent` tool uses to drive child sessions. `Runner::new`
/// attaches itself; the tool reads it through this process-wide slot so the
/// registry can be built before the runner exists.
pub static SUBAGENT_HANDLE: std::sync::OnceLock<
    std::sync::Arc<crate::tool::subagent::SubagentHandle>,
> = std::sync::OnceLock::new();

struct Inner {
    /// The loaded config, shared by reference: nothing reads it mutably, and
    /// every drain/step boundary clones the handle rather than the struct.
    config: std::sync::Arc<Config>,
    /// Canonical project working directory (system prompt + tool roots).
    directory: PathBuf,
    db: Db,
    bus: EventBus,
    chat: Arc<dyn Chat>,
    tools: Arc<ToolRegistry>,
    /// The model catalogue (default-model resolution); shared with the API.
    model_catalog: Arc<crate::model_catalog::ModelCatalog>,
    /// Connected MCP servers: tools (registered before the runner was built)
    /// plus the instruction block the system prompt carries.
    mcp: Arc<crate::mcp::Hub>,
    /// Per-session keyed-instruction baselines (process memory only; a restart
    /// re-baselines from current sources and the wire fold corrects stale
    /// delta rows).
    instruction_baselines:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, instructions::Baseline>>>,
    sessions: DashMap<String, SessionHandle>,
}

/// Everything an actor task shares with the registry.
#[derive(Clone)]
struct SessionHandle {
    tx: mpsc::Sender<ActorMsg>,
    /// `Some` while no task owns the mailbox receiver (session parked).
    parked: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<ActorMsg>>>>,
    /// Cancellation of the busy period in flight, if any.
    cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// `true` from `dispatch` until the actor installs the busy period's
    /// cancellation token — covers the gap between enqueue and the actor's
    /// first poll so `is_active` never reports idle for scheduled work.
    scheduled: Arc<Mutex<bool>>,
}

/// Lock a std mutex, recovering from poisoning (a panic in one actor must
/// not wedge the registry; the guard data is always consistent enough).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Runner {
    /// A runner over the given bus/db/config rooted at `directory`, with the
    /// given chat transport and (initially empty or test-populated) tools.
    /// No MCP servers: direct construction in tests keeps the pre-MCP prompt.
    pub fn new(
        config: std::sync::Arc<Config>,
        directory: PathBuf,
        db: Db,
        bus: EventBus,
        chat: Arc<dyn Chat>,
        tools: Arc<ToolRegistry>,
        model_catalog: Arc<crate::model_catalog::ModelCatalog>,
    ) -> Self {
        Self::new_with_mcp(
            config,
            directory,
            db,
            bus,
            chat,
            tools,
            model_catalog,
            Arc::new(crate::mcp::Hub::empty()),
        )
    }

    /// [`Runner::new`] over the process-wide MCP hub the composition root
    /// connected, so the system prompt reads the same server snapshot the
    /// HTTP layer serves.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_mcp(
        config: std::sync::Arc<Config>,
        directory: PathBuf,
        db: Db,
        bus: EventBus,
        chat: Arc<dyn Chat>,
        tools: Arc<ToolRegistry>,
        model_catalog: Arc<crate::model_catalog::ModelCatalog>,
        mcp: Arc<crate::mcp::Hub>,
    ) -> Self {
        let inner = Arc::new(Inner {
            config,
            directory,
            db,
            bus,
            chat,
            tools,
            model_catalog,
            mcp,
            instruction_baselines: Default::default(),
            sessions: DashMap::new(),
        });
        if let Some(handle) = SUBAGENT_HANDLE.get() {
            handle.attach(Self {
                inner: Arc::clone(&inner),
            });
        }
        Self { inner }
    }

    /// Whether work is scheduled or a busy period is currently in flight
    /// (process-local view). `scheduled` bridges the gap between dispatch
    /// and the actor's first poll installing the cancellation token.
    /// The chat transport this runner issues completions through. Background
    /// work that must not open a second transport (the dreamer) borrows it.
    pub fn chat(&self) -> Arc<dyn Chat> {
        Arc::clone(&self.inner.chat)
    }

    pub fn is_active(&self, session_id: &str) -> bool {
        self.inner
            .sessions
            .get(session_id)
            .is_some_and(|handle| *lock(&handle.scheduled) || lock(&handle.cancel).is_some())
    }

    /// Wake the session's actor for a drain: spawn one if parked, otherwise
    /// join the live task's mailbox. Safe to call repeatedly; the actor
    /// coalesces enqueues into one busy period.
    pub async fn dispatch(&self, session_id: &str, inbox_id: &str) -> Result<()> {
        let handle = self.handle(session_id);
        // Mark scheduled BEFORE the send: once the message is in the mailbox
        // the actor may install the busy period at any point, and `is_active`
        // must hold continuously from here until the terminal settles.
        *lock(&handle.scheduled) = true;
        // Send BEFORE taking the parked receiver: a task parking concurrently
        // re-checks the channel under the slot lock, so a message that beat
        // the send is picked up by exactly one side.
        let sent = handle
            .tx
            .send(ActorMsg::Enqueue {
                inbox_id: inbox_id.to_owned(),
            })
            .await;
        if sent.is_err() {
            *lock(&handle.scheduled) = false;
        }
        self.spawn_if_parked(session_id, handle).await;
        Ok(())
    }

    /// Cancel the session's in-flight busy period. Returns whether one was
    /// running (idle interruption is a no-op, matching upstream).
    pub fn interrupt(&self, session_id: &str) -> bool {
        let Some(handle) = self.inner.sessions.get(session_id) else {
            return false;
        };
        let Some(token) = lock(&handle.cancel).take() else {
            return false;
        };
        token.cancel();
        // Wake the actor's absorb loop so the terminal settles promptly even
        // if the drain is parked between awaits.
        let _ = handle.tx.try_send(ActorMsg::Interrupt);
        true
    }

    /// Ask every live actor to stop. Dispatching afterwards starts a fresh
    /// actor (used by process-shutdown paths; task 9 wires signals).
    pub fn shutdown(&self) {
        for entry in self.inner.sessions.iter() {
            let _ = entry.value().tx.try_send(ActorMsg::Shutdown);
        }
    }

    /// Registry entry for `session_id`, creating (parked) when absent.
    fn handle(&self, session_id: &str) -> SessionHandle {
        self.inner
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
                SessionHandle {
                    tx,
                    parked: Arc::new(tokio::sync::Mutex::new(Some(rx))),
                    cancel: Arc::new(Mutex::new(None)),
                    scheduled: Arc::new(Mutex::new(false)),
                }
            })
            .clone()
    }

    /// Take the parked receiver and hand it to a freshly spawned actor task.
    /// The parked task stores the receiver under the same mutex, so take and
    /// park are mutually exclusive: at most one live task per session.
    async fn spawn_if_parked(&self, session_id: &str, handle: SessionHandle) {
        let Some(rx) = handle.parked.lock().await.take() else {
            return;
        };
        let task = ActorTask {
            inner: self.inner.clone(),
            session_id: session_id.to_owned(),
            handle,
            rx: Some(rx),
        };
        tokio::spawn(task.run());
    }
}

// ---------------------------------------------------------------------------
// Actor task
// ---------------------------------------------------------------------------

/// One live actor. Owns the mailbox receiver while running; parks it back on
/// idle and exits (zero tasks for an idle session).
struct ActorTask {
    inner: Arc<Inner>,
    session_id: String,
    handle: SessionHandle,
    /// `Some` until parked; taken at task start.
    rx: Option<mpsc::Receiver<ActorMsg>>,
}

impl ActorTask {
    async fn run(mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        loop {
            // Absorb the backlog; the first Enqueue opens a busy period.
            let mut token: Option<CancellationToken> = None;
            loop {
                match rx.try_recv() {
                    Ok(ActorMsg::Enqueue { .. }) if token.is_none() => {
                        let period = CancellationToken::new();
                        *lock(&self.handle.cancel) = Some(period.clone());
                        // Upstream: the started hook publishes exactly once per
                        // busy period, before the first drain.
                        if let Err(error) = self
                            .inner
                            .bus
                            .publish(SessionEventData::ExecutionStarted {
                                session_id: self.session_id.clone(),
                            })
                            .await
                        {
                            tracing::error!(
                                session = %self.session_id,
                                error = %error,
                                "execution.started failed; releasing busy period"
                            );
                            *lock(&self.handle.cancel) = None;
                            break;
                        }
                        token = Some(period);
                    }
                    Ok(ActorMsg::Interrupt) => {
                        // The endpoint cancelled the token; this covers the
                        // absorb-window race where the slot was already taken.
                        if let Some(period) = &token {
                            period.cancel();
                        }
                    }
                    Ok(ActorMsg::Shutdown) => {
                        self.handle_shutdown();
                        return;
                    }
                    Ok(_) => {}
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            let Some(period) = token else {
                if !self.park(&mut rx).await {
                    return;
                }
                continue;
            };

            // Busy period: consecutive drains until the inbox is quiet. Every
            // coalesced drain shares one terminal publication. The project
            // directory is the SESSION's directory (the client's project),
            // not the server process cwd — tools and the system prompt must
            // operate where the session lives.
            let ctx = drain::DrainCtx {
                session_id: self.session_id.clone(),
                directory: self.session_directory().await,
                config: self.inner.config.clone(),
                db: self.inner.db.clone(),
                bus: self.inner.bus.clone(),
                chat: self.inner.chat.clone(),
                tools: self.inner.tools.clone(),
                model_catalog: Arc::clone(&self.inner.model_catalog),
                mcp: self.inner.mcp.clone(),
                instruction_baselines: self.inner.instruction_baselines.clone(),
            };
            let mut scope = drain::Promotable::Input;
            loop {
                if period.is_cancelled() {
                    self.terminal_interrupted().await;
                    break;
                }
                match drain::drain(&ctx, &period, scope).await {
                    drain::DrainOutcome::Interrupted => {
                        self.terminal_interrupted().await;
                        break;
                    }
                    drain::DrainOutcome::Failed(error) => {
                        if let Err(publish_error) = self
                            .inner
                            .bus
                            .publish(SessionEventData::ExecutionFailed {
                                session_id: self.session_id.clone(),
                                error,
                            })
                            .await
                        {
                            tracing::error!(
                                session = %self.session_id,
                                error = %publish_error,
                                "execution.failed failed to publish"
                            );
                        }
                        break;
                    }
                    drain::DrainOutcome::Complete => {
                        // Doorbell: work admitted during the drain keeps the
                        // busy period open instead of publishing a terminal.
                        let mut more = false;
                        loop {
                            match rx.try_recv() {
                                Ok(ActorMsg::Enqueue { .. }) => more = true,
                                Ok(ActorMsg::Interrupt) => period.cancel(),
                                Ok(ActorMsg::Shutdown) => {
                                    self.handle_shutdown();
                                    return;
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => return,
                            }
                        }
                        if period.is_cancelled() {
                            self.terminal_interrupted().await;
                            break;
                        }
                        if !more {
                            if let Err(error) = self
                                .inner
                                .bus
                                .publish(SessionEventData::ExecutionSucceeded {
                                    session_id: self.session_id.clone(),
                                })
                                .await
                            {
                                tracing::error!(
                                    session = %self.session_id,
                                    error = %error,
                                    "execution.succeeded failed to publish"
                                );
                            }
                            break;
                        }
                        // Fresh work: the next pass starts at an input boundary.
                        scope = drain::Promotable::Input;
                    }
                }
            }
            *lock(&self.handle.cancel) = None;
            *lock(&self.handle.scheduled) = false;
            // Loop: absorb anything that arrived while the terminal settled.
        }
    }

    fn handle_shutdown(&self) {
        // The actor is done for good; drop the registry entry so a later
        // dispatch builds a fresh handle instead of messaging a dead mailbox.
        self.inner.sessions.remove(&self.session_id);
    }

    /// The session's project directory: the `directory` recorded on the
    /// session row (the client's project), canonicalized; falls back to the
    /// server's directory when the row predates the column or the path is
    /// gone.
    async fn session_directory(&self) -> std::path::PathBuf {
        let looked_up: anyhow::Result<Option<String>> = async {
            let conn = self.inner.db.acquire().await?;
            let mut stmt = conn
                .prepare("SELECT directory FROM session_v2 WHERE id = ?1")
                .await?;
            let row = crate::db::optional(&mut stmt, [self.session_id.as_str()]).await?;
            match row {
                Some(row) => Ok(row.get::<Option<String>>(0)?),
                None => Ok(None),
            }
        }
        .await;
        let raw = looked_up
            .inspect_err(|error| {
                tracing::warn!(
                    session = %self.session_id,
                    error = %error,
                    "session directory lookup failed; falling back to server directory"
                )
            })
            .ok()
            .flatten();
        match raw.map(std::path::PathBuf::from) {
            Some(path) if path.is_dir() => path.canonicalize().unwrap_or(path),
            other => {
                tracing::warn!(
                    session = %self.session_id,
                    directory = ?other,
                    "session directory missing or gone; falling back to server directory"
                );
                self.inner.directory.clone()
            }
        }
    }

    async fn terminal_interrupted(&self) {
        if let Err(error) = self
            .inner
            .bus
            .publish(SessionEventData::ExecutionInterrupted {
                session_id: self.session_id.clone(),
                reason: InterruptReason::User,
            })
            .await
        {
            tracing::error!(
                session = %self.session_id,
                error = %error,
                "execution.interrupted failed to publish"
            );
        }
    }

    /// Hand the receiver back to the registry and exit (the session is idle:
    /// zero tasks). Returns true to re-enter the absorb loop when a message
    /// was already waiting.
    async fn park(&self, rx: &mut mpsc::Receiver<ActorMsg>) -> bool {
        let mut slot = self.handle.parked.lock().await;
        if !rx.is_empty() {
            return true;
        }
        // Any dispatch past this point finds the slot occupied and spawns a
        // fresh task, so the handoff loses nothing.
        let parked = std::mem::replace(rx, mpsc::channel(1).1);
        *slot = Some(parked);
        false
    }
}

/// Session errors surfaced by the drain loop carry the `unknown` type (the
/// upstream `toSessionError` fallback for non-schema errors).
pub(crate) fn unknown_error(message: impl Into<String>) -> SessionError {
    SessionError {
        kind: "unknown".to_owned(),
        message: message.into(),
        status: None,
        response: None,
    }
}

/// Resolve the session's agent + model (`context.resolveModel`): the projected
/// selection when set, else the config's default model (`default_model`,
/// falling back to the first `[[models]]` entry), else the cardinal
/// placeholder id.
///
/// Shared state, shared reader: the turn loop, compaction and title
/// generation all ask the same question, so the resolver lives here rather
/// than in any one caller.
pub(crate) async fn resolve_selection(
    db: &Db,
    session_id: &str,
    config: &Config,
    model_catalog: &crate::model_catalog::ModelCatalog,
) -> Result<(String, crate::schema::support::ModelRef)> {
    let row: Option<(Option<String>, Option<String>)> = {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT agent, model FROM session_v2 WHERE id = ?1")
            .await?;
        crate::db::optional(&mut stmt, [session_id])
            .await?
            .map(|row| -> Result<_> {
                Ok((row.get::<Option<String>>(0)?, row.get::<Option<String>>(1)?))
            })
            .transpose()?
    };
    let Some((agent, model)) = row else {
        bail!("session `{session_id}` is not projected");
    };
    let model = match model.and_then(|raw| serde_json::from_str(&raw).ok()) {
        Some(model) => model,
        None => match model_catalog.default_entry(config) {
            Some(configured) => crate::schema::support::ModelRef {
                id: configured.id.clone(),
                provider_id: configured.provider.clone(),
                variant: None,
            },
            None => crate::schema::support::ModelRef {
                id: "default".to_owned(),
                provider_id: "opencode".to_owned(),
                variant: None,
            },
        },
    };
    Ok((agent.unwrap_or_else(|| "build".to_owned()), model))
}
