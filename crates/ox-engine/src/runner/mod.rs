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
mod output_budget;
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

/// Tools a subagent session must not have. `question` opens a form only the
/// user can answer; no UI watches a child session and a foreground tool call
/// has no timeout, so the child would wait forever — and the parent's
/// `subagent` call with it. A fail-closed exclusion, not a permission model.
const SUBAGENT_EXCLUDED_TOOLS: &[&str] = &["question"];

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
    /// Per-session provider ÷ local token ratio (the budget's unit correction).
    /// Process memory only: a restart re-measures in one turn.
    calibrations: crate::mctx::calibration::Store,
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
    /// A background fold is running for this session (plan D3): at most one fold
    /// at a time, and a running compaction row that belongs to it is not
    /// stale (drain entry must not settle it out from under the live task).
    fold_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// The last time dispatch or park touched this session (millis). The idle
    /// sweep's clock: an entry with no work in flight or scheduled that has
    /// been silent past [`RUNNER_IDLE_TTL_MS`] is reaped.
    last_used: Arc<std::sync::atomic::AtomicU64>,
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

/// Whether the entry has work scheduled or in flight (`scheduled` covers
/// the enqueue-to-busy gap, `cancel` the busy period itself). The idle
/// sweep and the public `Runner::is_active` must answer this identically.
fn handle_is_active(handle: &SessionHandle) -> bool {
    *lock(&handle.scheduled) || lock(&handle.cancel).is_some()
}

/// A fresh registry entry: channels and flags only, idle-clock current.
fn new_session_handle() -> SessionHandle {
    let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
    SessionHandle {
        tx,
        parked: Arc::new(tokio::sync::Mutex::new(Some(rx))),
        cancel: Arc::new(Mutex::new(None)),
        scheduled: Arc::new(Mutex::new(false)),
        instruction_root: InstructionRootCache::default(),
        fold_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_used: Arc::new(std::sync::atomic::AtomicU64::new(
            ox_schema::id::now_millis() as u64,
        )),
    }
}

/// How long a registry entry may sit silent (no dispatch, no park) before
/// the idle sweep reaps it. A pause between turns of one conversation is
/// minutes at most; half an hour never reaps a live session, and the cost
/// of a reap is only the next dispatch's rebuild + re-baseline.
const RUNNER_IDLE_TTL_MS: u64 = 30 * 60 * 1000;

impl Inner {
    /// Drop every runner-side table entry for one session: the registry entry
    /// (channels and flags), its keyed-instruction baseline and its token
    /// calibration. The single owner of "what is kept per session", shared by
    /// the actor's shutdown, the idle sweep and a session delete — a new
    /// per-session table must be forgotten here or it outlives its session.
    ///
    /// Only process state goes: the session's rows, events and compartments are
    /// the database's, and a later dispatch of an evicted session rebuilds the
    /// entry and re-baselines exactly as the existing shutdown path does.
    fn forget(&self, session_id: &str) {
        self.sessions.remove(session_id);
        lock(&self.instruction_baselines).remove(session_id);
        crate::mctx::calibration::forget(&self.calibrations, session_id);
    }
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
            calibrations: Default::default(),
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
            .is_some_and(|handle| handle_is_active(&handle))
    }

    /// Process-local per-session table sizes (registry entries, keyed-instruction
    /// baselines), for isolated allocator profiling and for tests that need to
    /// see eviction — no HTTP surface exposes what a session costs the process.
    pub fn diagnostic_table_sizes(&self) -> (usize, usize) {
        let baselines = lock(&self.inner.instruction_baselines).len();
        (self.inner.sessions.len(), baselines)
    }

    /// Wake the session's actor for a drain: spawn one if parked, otherwise
    /// join the live task's mailbox. Safe to call repeatedly; the actor
    /// coalesces enqueues into one busy period.
    pub async fn dispatch(&self, session_id: &str, inbox_id: &str) -> Result<()> {
        // Opportunistic idle sweep: dispatch is the one regular entry, so the
        // per-session tables stay bounded by live work (the sweep's own O(n)
        // is dozens of entries at most).
        self.sweep_idle(ox_schema::id::now_millis() as u64);
        // Stamp and mark while holding the entry's shard lock: a concurrent
        // sweep's retain serializes behind this guard, so it can never evict
        // an entry this dispatch is about to use. Marking scheduled after the
        // guard used to leave exactly that window open.
        let handle = {
            let entry = self.inner.sessions.entry(session_id.to_owned());
            let slot = entry.or_insert_with(new_session_handle);
            slot.last_used.store(
                ox_schema::id::now_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            // Mark scheduled BEFORE the send: once the message is in the
            // mailbox the actor may install the busy period at any point, and
            // `is_active` must hold continuously from here until the terminal
            // settles.
            *lock(&slot.scheduled) = true;
            slot.clone()
        };
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
            tracing::debug!(session = %session_id, "dispatch enqueue failed; mailbox closed");
            *lock(&handle.scheduled) = false;
        } else {
            tracing::debug!(session = %session_id, inbox = %inbox_id, "dispatch enqueued");
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

    /// Forget one session's process-local state: the API's delete path calls
    /// this once the session and its descendants are gone.
    ///
    /// Without it the entry waits for [`Runner::sweep_idle`], which only runs
    /// inside a *dispatch* and only past [`RUNNER_IDLE_TTL_MS`] — a deleted
    /// session's channels, baseline render and calibration would then outlive
    /// the session by up to half an hour, and indefinitely in a process that
    /// dispatches nothing else. A busy entry is forgotten too: the actor holds
    /// its own handle clone, so its turn keeps running and settles into a task
    /// that exits, while nothing can dispatch the deleted session again
    /// (`resolve_selection` rejects a row that no longer exists).
    pub fn forget_session(&self, session_id: &str) {
        tracing::debug!(session = %session_id, "forgetting runner state for a deleted session");
        self.inner.forget(session_id);
    }

    /// Reap registry entries idle past [`RUNNER_IDLE_TTL_MS`]: the runner-side
    /// only-grows tables (`sessions`, one entry per session ever dispatched —
    /// subagent children included — and `instruction_baselines`, one frozen
    /// render per such session). A parked entry has no live task (the actor
    /// handed the receiver back and exited), so removal drops only channels
    /// and flags; the next dispatch rebuilds and re-baselines exactly as the
    /// existing shutdown path does. A busy entry is never touched, whatever
    /// its timestamp says.
    fn sweep_idle(&self, now_ms: u64) {
        let mut evicted: Vec<String> = Vec::new();
        self.inner.sessions.retain(|id, handle| {
            let idle =
                now_ms.saturating_sub(handle.last_used.load(std::sync::atomic::Ordering::Relaxed));
            if idle > RUNNER_IDLE_TTL_MS && !handle_is_active(handle) {
                evicted.push(id.clone());
                return false;
            }
            true
        });
        if evicted.is_empty() {
            return;
        }
        for id in &evicted {
            // Already removed from the registry by `retain`; the baseline and
            // the calibration are what still has to go.
            self.inner.forget(id);
        }
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
        // Resolve the toolset once per actor task: a session's parent link
        // never changes, so the child registry (no `question`) is built at
        // most once per spawn, not per step.
        let tools = self.session_tools().await;
        loop {
            // Absorb the backlog; the first Enqueue opens a busy period.
            let mut token: Option<CancellationToken> = None;
            loop {
                match rx.try_recv() {
                    Ok(ActorMsg::Enqueue { .. }) if token.is_none() => {
                        tracing::debug!(session = %self.session_id, "actor opened busy period");
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
                tools: Arc::clone(&tools),
                model_catalog: Arc::clone(&self.inner.model_catalog),
                mcp: self.inner.mcp.clone(),
                instruction_baselines: self.inner.instruction_baselines.clone(),
                calibrations: self.inner.calibrations.clone(),
                instruction_root: self.handle.instruction_root.clone(),
                fold_in_flight: Arc::clone(&self.handle.fold_in_flight),
            };
            let mut scope = drain::Promotable::Input;
            loop {
                if period.is_cancelled() {
                    self.terminal_interrupted().await;
                    break;
                }
                match drain::drain(&ctx, &period, scope).await {
                    drain::DrainOutcome::Interrupted => {
                        tracing::debug!(session = %self.session_id, "drain interrupted");
                        self.terminal_interrupted().await;
                        break;
                    }
                    drain::DrainOutcome::Failed(error) => {
                        tracing::debug!(session = %self.session_id, error = ?error, "drain failed");
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
                        tracing::debug!(session = %self.session_id, "drain complete; doorbell check");
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
                            tracing::debug!(session = %self.session_id, "publishing execution.succeeded");
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
                            tracing::debug!(session = %self.session_id, "execution.succeeded published");
                            // Upstream fires the fold on this same event
                            // (`session.execution.succeeded` → idle()): the
                            // busy period is over, the tail may fold in the
                            // background.
                            compaction::spawn_background_fold(&ctx);
                            break;
                        }
                        // Fresh work: the next pass starts at an input boundary.
                        tracing::debug!(session = %self.session_id, "doorbell saw new work; draining again");
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
        // The baseline and the calibration go with it: a later dispatch simply
        // re-baselines from the sources and re-measures in one turn.
        self.inner.forget(&self.session_id);
    }

    /// The registry this session's turns advertise and dispatch against: the
    /// shared one, minus [`SUBAGENT_EXCLUDED_TOOLS`] for a child session.
    /// A form opened by a child has no UI behind it, and ox has no foreground
    /// tool timeout, so `question` there would wait forever and hang the
    /// parent's `subagent` call with it. Resolved once per actor task.
    async fn session_tools(&self) -> Arc<ToolRegistry> {
        if self.session_is_subagent().await {
            Arc::new(self.inner.tools.without(SUBAGENT_EXCLUDED_TOOLS))
        } else {
            Arc::clone(&self.inner.tools)
        }
    }

    /// Whether this session is a subagent (its `parent_id` is set). A missing
    /// row or a failed lookup answers `true`: fail closed, an unanswered form
    /// is worse than a missing tool for one turn.
    async fn session_is_subagent(&self) -> bool {
        let looked_up: anyhow::Result<Option<Option<String>>> = async {
            let conn = self.inner.db.acquire().await?;
            let mut stmt = conn
                .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
                .await?;
            let row = ox_db::optional(&mut stmt, [self.session_id.as_str()]).await?;
            match row {
                Some(row) => Ok(Some(row.get::<Option<String>>(0)?)),
                None => Ok(None),
            }
        }
        .await;
        match looked_up {
            Ok(Some(parent_id)) => parent_id.is_some(),
            Ok(None) => {
                tracing::warn!(
                    session = %self.session_id,
                    "session row missing; running without the question tool"
                );
                true
            }
            Err(error) => {
                tracing::warn!(
                    session = %self.session_id,
                    error = %error,
                    "session parent lookup failed; running without the question tool"
                );
                true
            }
        }
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
        // The turn truly ended here: the idle clock restarts from this point,
        // not from the dispatch that started the (possibly long) busy period.
        self.handle.last_used.store(
            ox_schema::id::now_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
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
