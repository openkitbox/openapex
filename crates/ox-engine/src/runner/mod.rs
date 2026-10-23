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
pub(crate) mod generate;
mod historian_trigger;
pub(crate) mod instructions;
pub(crate) mod provider_error;
pub mod step;
pub mod title;
pub(crate) mod todo_reminder;

/// Reset the todo reminder budget after the todo tool mutates a list. Kept as a
/// thin wrapper here so the tool does not depend on the reminder module's
/// internals; failures never surface (a stalled budget is a lost reminder, not
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, bail};
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use ox_tools::contract::{Attachment, Tool, ToolContext, ToolOutput, ToolRegistry};

use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_schema::event::{InterruptReason, SessionEventData};
use ox_schema::support::SessionError;

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
// Runner registry
// ---------------------------------------------------------------------------

/// Registry of per-session actors. `dispatch` lazily spawns; interrupt and
/// shutdown reach the live actor through its registry entry.
#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

struct Inner {
    /// The loaded config, shared by reference: nothing reads it mutably, and
    /// every drain/step boundary clones the handle rather than the struct.
    config: std::sync::Arc<Config>,
    /// Canonical project working directory (system prompt + tool roots).
    directory: PathBuf,
    /// The ox home directory (`~/.ox`) the global AGENTS.md lives in.
    home: PathBuf,
    db: Db,
    bus: EventBus,
    chat: Arc<dyn Chat>,
    tools: Arc<ToolRegistry>,
    /// The model catalogue (default-model resolution); shared with the API.
    model_catalog: Arc<ox_core::model_catalog::ModelCatalog>,
    /// Connected MCP servers: tools (registered before the runner was built)
    /// plus the instruction block the system prompt carries.
    mcp: Arc<ox_mcp::Hub>,
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
    /// The session's instruction anchor: the git working-tree root of the
    /// session's directory, computed at most once per distinct directory
    /// (a `/cd` invalidates it by keying on the directory itself).
    instruction_root: InstructionRootCache,
}

/// The working-tree root of the repository containing `directory`, or `None`
/// outside a repository (or for a bare repository). Synchronous, in-process
/// (libgit2): deliberately ignores `GIT_DIR`/`GIT_CEILING_DIRECTORIES` — the
/// anchor is the physical tree of the session's directory, never the
/// environment. This mirrors `git rev-parse --show-toplevel`, whose CLI form
/// is intentionally not used here: the engine must not inherit the git-CLI
/// contract (flags, PATH, quoting) that the server's porcelain layer needs.
fn repo_toplevel(directory: &Path) -> Option<PathBuf> {
    let repo = git2::Repository::discover(directory).ok()?;
    repo.workdir().map(Path::to_path_buf)
}

/// One cache slot: the directory the anchor was computed for and the root
/// found there (`None` = not a repository — cached so non-repo sessions
/// don't rediscover on every step).
struct RootCacheEntry {
    directory: PathBuf,
    root: Option<PathBuf>,
}

/// Per-session instruction anchor cache. A single slot keyed on the
/// directory, so a `/cd` to a new directory recomputes and going back
/// recomputes again; clones (session handle ↔ busy period's `DrainCtx`)
/// share the one slot. Locks are held only around reads and writes —
/// never across discovery.
#[derive(Clone, Default)]
pub(crate) struct InstructionRootCache(Arc<Mutex<Option<RootCacheEntry>>>);

impl InstructionRootCache {
    /// The git working-tree root for `directory` together with the
    /// canonicalized directory the anchor was computed for. Callers must use
    /// the returned directory as the gather directory: directory and root
    /// share one namespace only in their canonical spelling.
    pub(crate) fn get_or_compute(&self, directory: &Path) -> (PathBuf, Option<PathBuf>) {
        self.get_or_compute_with(directory, repo_toplevel)
    }

    /// [`Self::get_or_compute`] with the discovery injected, so tests run
    /// without a repository. The returned directory (the cache key) and the
    /// root are both canonicalized (falling back to the raw path when
    /// canonicalization fails), so the pair stays consistent even when the
    /// input path carries symlink components.
    fn get_or_compute_with(
        &self,
        directory: &Path,
        compute: impl FnOnce(&Path) -> Option<PathBuf>,
    ) -> (PathBuf, Option<PathBuf>) {
        let key = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
        if let Some(entry) = lock(&self.0)
            .as_ref()
            .filter(|entry| entry.directory == key)
        {
            return (entry.directory.clone(), entry.root.clone());
        }
        let root = compute(directory).map(|root| std::fs::canonicalize(&root).unwrap_or(root));
        *lock(&self.0) = Some(RootCacheEntry {
            directory: key.clone(),
            root: root.clone(),
        });
        (key, root)
    }
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
        model_catalog: Arc<ox_core::model_catalog::ModelCatalog>,
    ) -> Self {
        Self::new_with_mcp(
            config,
            directory,
            db,
            bus,
            chat,
            tools,
            model_catalog,
            Arc::new(ox_mcp::Hub::empty()),
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
        model_catalog: Arc<ox_core::model_catalog::ModelCatalog>,
        mcp: Arc<ox_mcp::Hub>,
    ) -> Self {
        Self::new_with_mcp_and_home(
            config,
            directory,
            db,
            bus,
            chat,
            tools,
            model_catalog,
            mcp,
            ox_core::config::ox_home(),
        )
    }

    /// Same as [`Runner::new_with_mcp`], with an explicit global home for the
    /// global instruction file (tests embed a temp home so a developer's
    /// `~/.ox/AGENTS.md` never leaks in). The home must be set at
    /// construction: the inner state attaches to the subagent registry here.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_mcp_and_home(
        config: std::sync::Arc<Config>,
        directory: PathBuf,
        db: Db,
        bus: EventBus,
        chat: Arc<dyn Chat>,
        tools: Arc<ToolRegistry>,
        model_catalog: Arc<ox_core::model_catalog::ModelCatalog>,
        mcp: Arc<ox_mcp::Hub>,
        home: PathBuf,
    ) -> Self {
        let inner = Arc::new(Inner {
            config,
            directory,
            home,
            db,
            bus,
            chat,
            tools,
            model_catalog,
            mcp,
            instruction_baselines: Default::default(),
            sessions: DashMap::new(),
        });
        if let Some(handle) = inner.tools.subagent_handle.clone() {
            handle.attach(Arc::new(Self {
                inner: Arc::clone(&inner),
            }));
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

    /// Answer `prompt` from the session's current context without mutating it
    /// (upstream `session.generate`, the TUI's `/btw` dependency). Nothing is
    /// persisted, no event is published and no tool loop runs.
    pub async fn generate(&self, session_id: &str, prompt: &str) -> Result<String> {
        generate::run(&self.inner, session_id, prompt).await
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
                    instruction_root: InstructionRootCache::default(),
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
                home: self.inner.home.clone(),
                config: self.inner.config.clone(),
                db: self.inner.db.clone(),
                bus: self.inner.bus.clone(),
                chat: self.inner.chat.clone(),
                tools: self.inner.tools.clone(),
                model_catalog: Arc::clone(&self.inner.model_catalog),
                mcp: self.inner.mcp.clone(),
                instruction_baselines: self.inner.instruction_baselines.clone(),
                instruction_root: self.handle.instruction_root.clone(),
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
        // Same for the keyed-instruction baseline: its frozen initial render
        // would otherwise live (and grow) with every session the process
        // ever saw. A later dispatch simply re-baselines from the sources.
        lock(&self.inner.instruction_baselines).remove(&self.session_id);
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
            let row = ox_db::optional(&mut stmt, [self.session_id.as_str()]).await?;
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
    model_catalog: &ox_core::model_catalog::ModelCatalog,
) -> Result<(String, ox_schema::support::ModelRef)> {
    let row: Option<(Option<String>, Option<String>)> = {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT agent, model FROM session_v2 WHERE id = ?1")
            .await?;
        ox_db::optional(&mut stmt, [session_id])
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
            Some(configured) => ox_schema::support::ModelRef {
                id: configured.id.clone(),
                provider_id: configured.provider.clone(),
                variant: None,
            },
            None => ox_schema::support::ModelRef {
                id: "default".to_owned(),
                provider_id: "opencode".to_owned(),
                variant: None,
            },
        },
    };
    Ok((agent.unwrap_or_else(|| "build".to_owned()), model))
}

// ---------------------------------------------------------------------------
// Registration seams that need engine types (mctx tools, CBM tools)
// ---------------------------------------------------------------------------

/// Registry registrations whose tool types live in the engine. An extension
/// trait rather than methods on `ToolRegistry`: the contract crate owns the
/// registry, the engine owns these tools.
pub trait ToolRegistryExt {
    /// Register the magic-context tools (`ctx_reduce`, `ctx_expand`,
    /// `ctx_memory`, `ctx_note`, `ctx_search`). Deliberately separate from
    /// [`ToolRegistry::with_builtins`]:
    ///
    /// 1. They hold a [`Db`] handle (the `mc_tag`/`mc_drop` tables), which
    ///    only exists after the DB is opened — `with_builtins` runs before
    ///    that.
    /// 2. They must only be advertised when `magic_context.enabled` — a
    ///    default-off config keeps the advertised toolset byte-identical
    ///    (Task 1's zero-behavior-change guarantee), and the tool list is
    ///    part of the provider request.
    ///
    /// Called exactly once from `build_state_with`. Session scoping needs no
    /// dispatch change: `ToolContext` already carries `session_id`.
    fn register_mctx(&mut self, db: Db, embedder: Option<Arc<crate::mctx::embed::Embedder>>);

    /// Register the CBM code-graph tools (`index_repository`, `search_graph`,
    /// `trace_path`, `get_code_snippet`, `get_architecture`). Deliberately
    /// separate from [`ToolRegistry::with_builtins`]:
    ///
    /// 1. The `codebase-memory-mcp` binary is optional, so the tools are
    ///    registered only when it is discovered — at the composition root —
    ///    keeping the advertised toolset unchanged on machines without it.
    /// 2. The names are BARE names borrowed from the binary's own CLI
    ///    contract so its error hints match verbatim; the borrow enters the
    ///    registry here, in the module that owns the registration.
    ///
    /// Called exactly once from `run` when `ox_cbm::Cbm::discover`
    /// finds the binary.
    fn register_cbm(&mut self, cbm: ox_cbm::Cbm);
}

impl ToolRegistryExt for ToolRegistry {
    fn register_mctx(&mut self, db: Db, embedder: Option<Arc<crate::mctx::embed::Embedder>>) {
        self.register_all(vec![
            Arc::new(crate::mctx::tools::CtxReduceTool::new(db.clone())) as Arc<dyn Tool>,
            Arc::new(crate::mctx::tools::CtxExpandTool::new(db.clone())),
            Arc::new(crate::mctx::memory::CtxMemoryTool::new(
                db.clone(),
                embedder.clone(),
            )),
            Arc::new(crate::mctx::memory::CtxNoteTool::new(db.clone())),
            Arc::new(crate::mctx::search::CtxSearchTool::new(db, embedder)),
        ]);
    }

    fn register_cbm(&mut self, cbm: ox_cbm::Cbm) {
        self.register_all(ox_cbm::tools::all(Arc::new(cbm)));
    }
}

/// The engine capabilities the `subagent` tool drives a child session
/// through, implemented over the registry: dispatch, activity, interruption
/// and the projected transcript.
impl ox_tools::subagent::SubagentSpawner for Runner {
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Runner::dispatch(self, session_id, inbox_id).await })
    }

    fn is_active(&self, session_id: &str) -> bool {
        Runner::is_active(self, session_id)
    }

    fn interrupt(&self, session_id: &str) -> bool {
        Runner::interrupt(self, session_id)
    }

    fn load_history<'a>(&'a self, session_id: &'a str) -> ox_tools::subagent::LoadHistory<'a> {
        Box::pin(crate::runner::step::load_history(
            &self.inner.db,
            session_id,
        ))
    }
}

#[cfg(test)]
mod registration_tests {
    use super::{ToolRegistry, ToolRegistryExt};

    #[test]
    fn register_cbm_adds_exactly_the_five_cbm_tools() {
        // No SHELL lock needed here: the tests that mutate $SHELL live in the
        // ox-tools test binary, and this one only reads the tool list.
        let mut registry = ToolRegistry::with_builtins();
        let before: std::collections::BTreeSet<String> = registry
            .definitions()
            .iter()
            .map(|def| def.function.name.clone())
            .collect();
        registry.register_cbm(ox_cbm::Cbm::new(std::path::PathBuf::from(
            "/nonexistent/codebase-memory-mcp",
        )));
        let after = registry.definitions();
        let names: std::collections::BTreeSet<String> =
            after.iter().map(|def| def.function.name.clone()).collect();
        let added: Vec<String> = names.difference(&before).cloned().collect();
        assert_eq!(
            added,
            vec![
                "get_architecture".to_owned(),
                "get_code_snippet".to_owned(),
                "index_repository".to_owned(),
                "search_graph".to_owned(),
                "trace_path".to_owned(),
            ]
        );
        assert_eq!(
            names.len(),
            before.len() + 5,
            "CBM registration displaced an existing tool"
        );
        for definition in after
            .iter()
            .filter(|def| !before.contains(&def.function.name))
        {
            let name = &definition.function.name;
            assert!(
                !definition.function.description.is_empty(),
                "{name} has no description"
            );
            let parameters = &definition.function.parameters;
            assert_eq!(parameters["type"], serde_json::json!("object"), "{name}");
            assert!(parameters["properties"].is_object(), "{name}");
            let expected_required = match name.as_str() {
                "trace_path" => Some(serde_json::json!(["function_name"])),
                "get_code_snippet" => Some(serde_json::json!(["qualified_name"])),
                _ => None,
            };
            match expected_required {
                Some(expected) => {
                    assert_eq!(parameters["required"], expected, "{name}");
                    // A declared `required` key is expected to be an array.
                    for field in parameters["required"]
                        .as_array()
                        .expect("required must be an array")
                    {
                        assert!(
                            parameters["properties"]
                                .get(field.as_str().expect("required field name"))
                                .is_some(),
                            "required field {field} of {name} missing from properties"
                        );
                    }
                }
                None => assert!(
                    parameters.get("required").is_none(),
                    "{name} must not declare required fields"
                ),
            }
        }
    }
}

#[cfg(test)]
mod instruction_root_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{InstructionRootCache, repo_toplevel};

    /// Isolated `git init` (no user/system config bleeds into the repo).
    fn git_init(directory: &Path, bare: bool) {
        let mut command = std::process::Command::new("git");
        command.arg("init");
        if bare {
            command.arg("--bare");
        }
        let status = command
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git must exist");
        assert!(status.success(), "git init failed");
    }

    /// T11: inside a fresh repository the anchor is the canonicalized tempdir
    /// (git resolves symlinks — macOS `/var` is `/private/var`).
    #[test]
    fn repo_toplevel_resolves_the_repository_root() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path(), false);

        let root = repo_toplevel(dir.path());
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(root.as_deref(), Some(expected.as_path()));
    }

    /// T11: a bare repository has no working tree, matching
    /// `git rev-parse --show-toplevel` failing there.
    #[test]
    fn repo_toplevel_is_none_for_a_bare_repository() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path(), true);

        assert_eq!(repo_toplevel(dir.path()), None);
    }

    /// T11: a directory outside any repository has no anchor.
    #[test]
    fn repo_toplevel_is_none_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(repo_toplevel(dir.path()), None);
    }

    /// T12: the same directory computes at most once — the second call is a
    /// cache hit (exactly one injected compute).
    #[test]
    fn instruction_root_cache_computes_once_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cache = InstructionRootCache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let compute = {
            let calls = Arc::clone(&calls);
            let root = root.clone();
            move |_: &Path| {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(root.clone())
            }
        };

        let (first_dir, first_root) = cache.get_or_compute_with(dir.path(), compute.clone());
        let (second_dir, second_root) = cache.get_or_compute_with(dir.path(), compute.clone());

        assert_eq!(first_dir, root);
        assert_eq!(first_root.as_deref(), Some(root.as_path()));
        assert_eq!((second_dir, second_root), (first_dir, first_root));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A missing anchor (not a repository) is cached too: `/tmp` sessions must
    /// not re-run discovery on every step.
    #[test]
    fn instruction_root_cache_caches_a_missing_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let cache = InstructionRootCache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let compute = {
            let calls = Arc::clone(&calls);
            move |_: &Path| {
                calls.fetch_add(1, Ordering::SeqCst);
                None::<PathBuf>
            }
        };

        let (first_dir, first_root) = cache.get_or_compute_with(dir.path(), compute.clone());
        let (_second_dir, second_root) = cache.get_or_compute_with(dir.path(), compute.clone());

        assert_eq!(first_root, None);
        assert_eq!(second_root, None);
        // The canonical directory is handed back even without an anchor.
        assert_eq!(first_dir, std::fs::canonicalize(dir.path()).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// T13: a different directory is a miss (the `/cd` invalidation) — the
    /// single-slot cache recomputes for the new key.
    #[test]
    fn instruction_root_cache_recomputes_when_the_directory_changes() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let cache = InstructionRootCache::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let compute = {
            let calls = Arc::clone(&calls);
            move |directory: &Path| {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(directory.to_path_buf())
            }
        };

        let (first_dir_anchor, first_root) =
            cache.get_or_compute_with(first_dir.path(), compute.clone());
        let (second_dir_anchor, second_root) =
            cache.get_or_compute_with(second_dir.path(), compute.clone());

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_ne!(
            (first_dir_anchor, first_root),
            (second_dir_anchor, second_root),
            "a different directory is a miss"
        );
    }

    /// T14: the returned pair shares one namespace. A session reached through
    /// a symlink gets the canonical directory handed back beside the canonical
    /// root, so the chain's component-wise prefix check holds and gather walks
    /// the full ancestor chain — the raw symlinked path used to degenerate to
    /// leaf-only and miss the root's AGENTS.md despite a found anchor.
    #[cfg(unix)]
    #[test]
    fn instruction_root_pair_shares_one_namespace_with_the_chain() {
        let repo = tempfile::tempdir().unwrap();
        git_init(repo.path(), false);
        let sub = repo.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "sub rule\n").unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "repo rule\n").unwrap();

        let outside = tempfile::tempdir().unwrap();
        let link = outside.path().join("link");
        if std::os::unix::fs::symlink(&sub, &link).is_err() {
            return; // environment denies symlinks: nothing to pin here
        }

        let cache = InstructionRootCache::default();
        let (directory, root) = cache.get_or_compute(&link);

        let canonical_sub = std::fs::canonicalize(&sub).unwrap();
        let canonical_repo = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(directory, canonical_sub);
        assert_eq!(root.as_deref(), Some(canonical_repo.as_path()));
        // The component-wise prefix check the chain relies on now holds for
        // the handed-out directory…
        assert!(directory.starts_with(root.as_deref().unwrap()));
        // …so the gather chain is [sub, repo]: the repo AGENTS.md is
        // injected after the leaf's, nearest first.
        let home = tempfile::tempdir().unwrap();
        let observed = crate::prompt::gather(&directory, home.path(), root.as_deref(), None);
        let Some(crate::prompt::ObservedValue::Present(crate::prompt::SourceValue::Files {
            files,
        })) = observed
            .iter()
            .find(|source| source.key == crate::prompt::KEY_INSTRUCTIONS)
            .map(|source| &source.value)
        else {
            panic!("the instructions source must be present");
        };
        let rules: Vec<&str> = files.iter().map(|file| file.content.as_str()).collect();
        assert_eq!(rules, vec!["sub rule", "repo rule"]);
    }
}
