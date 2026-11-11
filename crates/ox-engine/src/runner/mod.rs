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
//!   actor publishes the terminal after owned tools settle. Scheduled periods
//!   already own their token; idle interruption is a no-op.
//! - **Parked handoff**: an idle session's mailbox receiver moves back into
//!   the registry entry; the next dispatch takes it out and spawns a fresh
//!   task. Send always happens before the take, and the parked task re-checks
//!   the channel under the slot mutex before parking, so no message can fall
//!   into the gap.
//!
//! The [`Tool`] trait + [`ToolRegistry`] here are the seam task 7 fills with
//! real tools; the registry ships empty until task 7 fills it.

pub use ox_llm::cardinal::chat::Chat;
pub use services::{Live, ServiceSet, Services};
pub use step::usage::SessionCache;

pub mod compaction;
pub mod drain;
mod fold_input;
pub(crate) mod generate;
mod historian_trigger;
pub(crate) mod instructions;
mod loop_guard;
pub(crate) mod model_roles;
mod output_budget;
pub(crate) mod permissions;
pub(crate) mod provider_error;
mod request_budget;
mod retry;
mod services;
pub mod step;
mod tasks;
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
use ox_schema::support::{ModelRef, SessionError};
use ox_tools::agent;
use ox_tools::agent::Agent;
use ox_tools::agent::Mode;
use step::usage::CacheLedger;

/// Messages the per-session actor mailbox accepts.
#[derive(Debug)]
pub enum ActorMsg {
    /// An inbox item was admitted; join (or open) the session's busy period.
    Enqueue {
        /// The `session_inbox` row id that was enqueued.
        inbox_id: String,
        generation: u64,
    },
    /// Wake after cancelling a period. Activity, not this hint, owns cancellation.
    Interrupt { generation: u64 },
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
    /// Canonical project working directory (system prompt + tool roots).
    directory: PathBuf,
    /// The ox home directory (`~/.ox`) the global AGENTS.md lives in.
    home: PathBuf,
    db: Db,
    bus: EventBus,
    /// Config-derived services (config, chat, tools, model catalogue, MCP
    /// hub). Read through [`Live::current`]; a reload swaps the generation
    /// and running turns pick it up at their next step boundary.
    live: Arc<Live>,
    /// Per-session keyed-instruction baselines (process memory only; a restart
    /// re-baselines from current sources and the wire fold corrects stale
    /// delta rows).
    instruction_baselines:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, instructions::Baseline>>>,
    /// Per-session provider ÷ local token ratio (the budget's unit correction).
    /// Process memory only: a restart re-measures in one turn.
    calibrations: crate::mctx::calibration::Store,
    /// Per-session prompt-cache accounting, reported per attempt and read only
    /// by diagnostics ([`Runner::diagnostic_cache_ledger`]).
    cache_ledger: Arc<CacheLedger>,
    /// Per-session "allow for this session" permission grants (process
    /// memory; a restart asks again).
    grants: Arc<permissions::Grants>,
    /// Serializes registry replacement with process-table cleanup. Never held
    /// across Activity, admission, DB access or await; shard guards are dropped
    /// before the attached tables are locked.
    registry_lifecycle: Mutex<()>,
    sessions: DashMap<String, SessionHandle>,
    tasks: tasks::Tasks,
    stop: CancellationToken,
}

/// Everything an actor task shares with the registry.
#[derive(Clone)]
struct SessionHandle {
    tx: mpsc::Sender<ActorMsg>,
    /// `Some` while no task owns the mailbox receiver (session parked).
    parked: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<ActorMsg>>>>,
    /// One owner from scheduling through terminal publication.
    activity: Arc<Mutex<Activity>>,
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
    /// Woken every time the session stops being active: a busy period
    /// settles, or the entry is forgotten. [`Runner::wait_idle`] parks on it
    /// instead of polling `is_active`.
    settled: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Scheduled,
    Running,
    Settling,
}

struct Period {
    generation: u64,
    token: CancellationToken,
    phase: Phase,
    pending: bool,
}

#[derive(Default)]
struct Activity {
    next: u64,
    current: Option<Period>,
    successor: Option<Period>,
}

impl Activity {
    fn schedule(&mut self, stop: &CancellationToken) -> u64 {
        let slot = if self
            .current
            .as_ref()
            .is_some_and(|p| p.phase == Phase::Settling)
        {
            &mut self.successor
        } else {
            &mut self.current
        };
        let period = slot.get_or_insert_with(|| {
            self.next += 1;
            Period {
                generation: self.next,
                token: stop.child_token(),
                phase: Phase::Scheduled,
                pending: false,
            }
        });
        period.pending = true;
        period.generation
    }

    fn claim(&mut self) -> Option<(u64, CancellationToken)> {
        let period = self.current.as_mut()?;
        if period.phase != Phase::Scheduled {
            return None;
        }
        period.phase = Phase::Running;
        period.pending = false;
        Some((period.generation, period.token.clone()))
    }

    /// The quiet check and terminal admission share dispatch's lock.
    fn settle(&mut self, generation: u64, force: bool) -> bool {
        let Some(period) = self.current.as_mut().filter(|p| p.generation == generation) else {
            return false;
        };
        if !force && period.pending && !period.token.is_cancelled() {
            period.pending = false;
            return false;
        }
        period.phase = Phase::Settling;
        true
    }

    fn finish(&mut self, generation: u64) {
        if self
            .current
            .as_ref()
            .is_some_and(|p| p.generation == generation)
        {
            self.current = self.successor.take();
        }
    }
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

/// Ownership remains active through cancellation and terminal publication.
fn handle_is_active(handle: &SessionHandle) -> bool {
    lock(&handle.activity).current.is_some()
}

/// A fresh registry entry: channels and flags only, idle-clock current.
fn new_session_handle() -> SessionHandle {
    let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
    SessionHandle {
        tx,
        parked: Arc::new(tokio::sync::Mutex::new(Some(rx))),
        activity: Arc::new(Mutex::new(Activity::default())),
        instruction_root: InstructionRootCache::default(),
        fold_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_used: Arc::new(std::sync::atomic::AtomicU64::new(
            ox_schema::id::now_millis() as u64,
        )),
        settled: Arc::new(tokio::sync::Notify::new()),
    }
}

/// How long a registry entry may sit silent (no dispatch, no park) before
/// the idle sweep reaps it. A pause between turns of one conversation is
/// minutes at most; half an hour never reaps a live session, and the cost
/// of a reap is only the next dispatch's rebuild + re-baseline.
const RUNNER_IDLE_TTL_MS: u64 = 30 * 60 * 1000;

impl Inner {
    /// Drop every runner-side table entry for one session: the registry entry
    /// (channels and flags), its keyed-instruction baseline, its token
    /// calibration and its prompt-cache ledger. The single owner of "what is
    /// kept per session", shared by the actor's shutdown, the idle sweep and a
    /// session delete — a new per-session table must be forgotten here or it
    /// outlives its session.
    ///
    /// Only process state goes: the session's rows, events and compartments are
    /// the database's, and a later dispatch of an evicted session rebuilds the
    /// entry and re-baselines exactly as the existing shutdown path does.
    fn forget(&self, session_id: &str, owner: Option<&SessionHandle>) {
        let _lifecycle = lock(&self.registry_lifecycle);
        let removed = self.sessions.remove_if(session_id, |_, handle| {
            owner.is_none_or(|owner| Arc::ptr_eq(&owner.activity, &handle.activity))
        });
        if let Some((_, handle)) = removed {
            handle.settled.notify_waiters();
            self.forget_tables(session_id);
        } else if owner.is_none() {
            self.forget_tables(session_id);
        }
    }

    /// Caller owns `registry_lifecycle`, with no registry shard guard held.
    fn forget_tables(&self, session_id: &str) {
        lock(&self.instruction_baselines).remove(session_id);
        crate::mctx::calibration::forget(&self.calibrations, session_id);
        self.cache_ledger.forget(session_id);
        self.grants.forget(session_id);
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
        model_catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
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
        model_catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
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
        model_catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
        mcp: Arc<ox_mcp::Hub>,
        home: PathBuf,
    ) -> Self {
        let live = Arc::new(Live::new(ServiceSet {
            config,
            chat,
            model_catalog,
            mcp,
            tools,
        }));
        let inner = Arc::new(Inner {
            directory,
            home,
            db,
            bus,
            live,
            instruction_baselines: Default::default(),
            calibrations: Default::default(),
            cache_ledger: Default::default(),
            grants: Default::default(),
            registry_lifecycle: Mutex::new(()),
            sessions: DashMap::new(),
            tasks: tasks::Tasks::default(),
            stop: CancellationToken::new(),
        });
        let tools = inner.live.current().tools.clone();
        if let Some(handle) = tools.subagent_handle.clone() {
            handle.attach(Arc::new(WeakRunner(Arc::downgrade(&inner))));
        }
        if let Some(completion) = tools.shell_completion.clone() {
            completion.attach(Arc::new(WeakRunner(Arc::downgrade(&inner))));
        }
        Self { inner }
    }

    /// The chat transport this runner issues completions through. Background
    /// work that must not open a second transport (the dreamer) borrows it.
    #[must_use]
    pub fn chat(&self) -> Arc<dyn Chat> {
        Arc::clone(&self.inner.live.current().chat)
    }

    /// The holder of the current config-derived services; a reload installs
    /// the next generation through it.
    #[must_use]
    pub fn live(&self) -> Arc<Live> {
        Arc::clone(&self.inner.live)
    }

    #[must_use]
    pub fn is_active(&self, session_id: &str) -> bool {
        self.inner
            .sessions
            .get(session_id)
            .map(|entry| entry.clone())
            .is_some_and(|handle| handle_is_active(&handle))
    }

    /// Resolve once the session has no work scheduled or in flight. Returns
    /// at once for an idle or unknown session; otherwise parks on the entry's
    /// settle notification and re-checks on every wake-up. The notification
    /// is armed *before* the activity check, so a busy period that settles
    /// between the check and the await is not missed.
    pub async fn wait_idle(&self, session_id: &str) {
        loop {
            // Clone out of the shard guard: parking while holding a DashMap
            // read guard would block the actor's own `entry` call.
            let Some(handle) = self
                .inner
                .sessions
                .get(session_id)
                .map(|entry| entry.clone())
            else {
                return;
            };
            let settled = handle.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if !handle_is_active(&handle) {
                return;
            }
            settled.await;
        }
    }

    /// Whether any session has work scheduled or in flight.
    #[must_use]
    pub fn has_active(&self) -> bool {
        // Snapshot only shared activity pointers: no ID copies or result Vec,
        // and no Activity lock is acquired under a registry shard guard.
        let activities: Vec<_> = self
            .inner
            .sessions
            .iter()
            .map(|entry| Arc::clone(&entry.activity))
            .collect();
        activities
            .into_iter()
            .any(|activity| lock(&activity).current.is_some())
    }

    /// Ids of every session with work scheduled or in flight (upstream
    /// `SessionExecution.active`).
    #[must_use]
    pub fn active_sessions(&self) -> Vec<String> {
        let handles: Vec<_> = self
            .inner
            .sessions
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        handles
            .into_iter()
            .filter(|(_, handle)| handle_is_active(handle))
            .map(|(id, _)| id)
            .collect()
    }

    /// Process-local per-session table sizes (registry entries, keyed-instruction
    /// baselines), for isolated allocator profiling and for tests that need to
    /// see eviction — no HTTP surface exposes what a session costs the process.
    #[must_use]
    pub fn diagnostic_table_sizes(&self) -> (usize, usize) {
        let baselines = lock(&self.inner.instruction_baselines).len();
        (self.inner.sessions.len(), baselines)
    }

    /// Per-session prompt-cache accounting for the sessions this runner still
    /// holds, sorted by session id: `read` is the prompt prefix the provider
    /// served from its cache, `uncached` the part it had to prefill. A
    /// diagnostic log line reads this; nothing in a request depends on it. An
    /// entry evicted from the registry is left out even if its ledger row has
    /// not been reaped yet.
    #[must_use]
    pub fn diagnostic_cache_ledger(&self) -> Vec<(String, SessionCache)> {
        self.inner
            .cache_ledger
            .snapshot(|session| self.inner.sessions.contains_key(session))
    }

    /// Wake the session's actor for a drain: spawn one if parked, otherwise
    /// join the live task's mailbox. Safe to call repeatedly; the actor
    /// coalesces enqueues into one busy period.
    pub async fn dispatch(&self, session_id: &str, inbox_id: &str) -> Result<()> {
        let _admission = self.try_admit()?;
        // Opportunistic idle sweep: dispatch is the one regular entry, so the
        // per-session tables stay bounded by live work (the sweep's own O(n)
        // is dozens of entries at most).
        self.sweep_idle(ox_schema::id::now_millis() as u64);
        // Clone under the shard guard; its ownership prevents idle reaping.
        // Activity is always acquired after releasing the registry guard.
        let handle = {
            let _lifecycle = lock(&self.inner.registry_lifecycle);
            let entry = self.inner.sessions.entry(session_id.to_owned());
            let slot = entry.or_insert_with(new_session_handle);
            slot.last_used.store(
                ox_schema::id::now_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            slot.clone()
        };
        // Reserve before admission: failure has no generation to roll back and
        // cannot clear a concurrent dispatch's work. Sending never holds Activity.
        let permit = tokio::select! {
            biased;
            () = self.inner.stop.cancelled() => return Err(ox_tools::contract::Closing.into()),
            permit = handle.tx.reserve() => permit
                .map_err(|_| anyhow::anyhow!("session `{session_id}` actor mailbox closed"))?,
        };
        let generation = lock(&handle.activity).schedule(&self.inner.stop);
        let scheduled = self.inner.tasks.gate.register(|| {
            permit.send(ActorMsg::Enqueue {
                inbox_id: inbox_id.to_owned(),
                generation,
            });
        });
        if let Err(error) = scheduled {
            let mut activity = lock(&handle.activity);
            if activity.current.as_ref().is_some_and(|period| {
                period.generation == generation && period.phase == Phase::Scheduled
            }) {
                activity.finish(generation);
            }
            drop(activity);
            handle.settled.notify_waiters();
            return Err(error.into());
        }
        if let Err(error) = self.spawn_if_parked(session_id, handle.clone()).await {
            let mut activity = lock(&handle.activity);
            if activity
                .current
                .as_ref()
                .is_some_and(|period| period.phase == Phase::Scheduled)
            {
                activity.current = None;
                activity.successor = None;
            }
            drop(activity);
            handle.settled.notify_waiters();
            return Err(error);
        }
        Ok(())
    }

    /// Answer `prompt` from the session's current context without mutating it
    /// (upstream `session.generate`, the TUI's `/btw` dependency). Nothing is
    /// persisted, no event is published and no tool loop runs.
    pub async fn generate(&self, session_id: &str, prompt: &str) -> Result<String> {
        let admission = self.try_admit()?;
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        let prompt = prompt.to_owned();
        let (reply, result) = tokio::sync::oneshot::channel();
        self.inner.tasks.gate.register(|| {
            self.inner.tasks.auxiliary.spawn(async move {
                let _admission = admission;
                let answer = generate::run(&inner, &session_id, &prompt).await;
                let _ = reply.send(answer);
            })
        })??;
        result
            .await
            .map_err(|_| anyhow::anyhow!("generate task ended without an answer"))?
    }

    /// Cancel the session's in-flight busy period. Returns whether one was
    /// running (idle interruption is a no-op, matching upstream).
    #[must_use]
    pub fn interrupt(&self, session_id: &str) -> bool {
        let Some(handle) = self
            .inner
            .sessions
            .get(session_id)
            .map(|entry| entry.clone())
        else {
            return false;
        };
        let generation = {
            let activity = lock(&handle.activity);
            let Some(period) = &activity.current else {
                return false;
            };
            if period.phase != Phase::Settling {
                period.token.cancel();
            }
            if let Some(successor) = &activity.successor {
                successor.token.cancel();
            }
            period.generation
        };
        // The token is authoritative; a full mailbox already wakes the actor.
        let _ = handle.tx.try_send(ActorMsg::Interrupt { generation });
        true
    }

    /// Start manual title work through the same auxiliary owner as automatic titles.
    pub fn spawn_title(&self, session_id: &str) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        self.inner.tasks.spawn_auxiliary(async move {
            let svc = inner.live.current();
            title::generate(
                inner.db.clone(),
                inner.bus.clone(),
                Arc::clone(&svc.chat),
                Arc::clone(&svc.config),
                Arc::clone(&svc.model_catalog),
                session_id,
            )
            .await;
        })
    }

    pub fn try_admit(
        &self,
    ) -> Result<ox_tools::contract::ExecutionAdmission, ox_tools::contract::Closing> {
        self.inner.tasks.gate.try_admit()
    }

    pub async fn wait_admissions(&self) {
        self.inner.tasks.gate.wait().await;
    }

    /// Idempotently stop execution admission, then cooperatively cancel actors.
    pub fn close(&self) {
        self.inner.tasks.gate.close();
        self.inner.stop.cancel();
        let handles: Vec<_> = self
            .inner
            .sessions
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        for handle in handles {
            let activity = lock(&handle.activity);
            if let Some(period) = &activity.current {
                period.token.cancel();
            }
            if let Some(period) = &activity.successor {
                period.token.cancel();
            }
        }
    }

    pub async fn join(&self) -> Result<()> {
        self.close();
        self.wait_admissions().await;
        let actors = self.inner.tasks.actors.seal_and_join().await;
        let auxiliary = self.inner.tasks.auxiliary.seal_and_join().await;
        actors.and(auxiliary)
    }

    pub fn shutdown(&self) {
        self.close();
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
        self.inner.forget(session_id, None);
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
        let _lifecycle = lock(&self.inner.registry_lifecycle);
        let mut evicted: Vec<String> = Vec::new();
        self.inner.sessions.retain(|id, handle| {
            let idle =
                now_ms.saturating_sub(handle.last_used.load(std::sync::atomic::Ordering::Relaxed));
            // Dispatch and actors hold a handle outside the shard lock. Only
            // an unshared parked entry can be reaped, without nesting locks.
            if idle > RUNNER_IDLE_TTL_MS && Arc::strong_count(&handle.activity) == 1 {
                evicted.push(id.clone());
                return false;
            }
            true
        });
        if evicted.is_empty() {
            return;
        }
        for id in &evicted {
            // Replacement is excluded until the old owner's tables are gone.
            self.inner.forget_tables(id);
        }
    }

    /// Take the parked receiver and hand it to a freshly spawned actor task.
    /// The parked task stores the receiver under the same mutex, so take and
    /// park are mutually exclusive: at most one live task per session.
    async fn spawn_if_parked(&self, session_id: &str, handle: SessionHandle) -> Result<()> {
        let mut parked = handle.parked.lock().await;
        self.inner.tasks.gate.register(|| {
            let Some(rx) = parked.take() else {
                return Ok(());
            };
            let task = ActorTask {
                inner: self.inner.clone(),
                session_id: session_id.to_owned(),
                handle: handle.clone(),
                rx: Some(rx),
                parked: false,
            };
            self.inner.tasks.actors.spawn(task.run())
        })?
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
    parked: bool,
}

impl Drop for ActorTask {
    fn drop(&mut self) {
        // A parked receiver can already belong to a successor actor. Only a
        // terminal owner may clear activity; this also runs during unwinding.
        if self.parked {
            return;
        }
        let mut activity = lock(&self.handle.activity);
        activity.current = None;
        activity.successor = None;
        drop(activity);
        self.handle.settled.notify_waiters();
        self.handle_shutdown();
        // The task supervisor still observes the original panic as JoinError.
    }
}

impl ActorTask {
    async fn run(mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        loop {
            if self.inner.stop.is_cancelled() {
                let mut activity = lock(&self.handle.activity);
                activity.current = None;
                activity.successor = None;
                drop(activity);
                self.handle.settled.notify_waiters();
                self.handle_shutdown();
                return;
            }
            // Doorbells are hints, never ownership. Old generations cannot
            // create a new period, even when they arrive after cancellation.
            loop {
                match rx.try_recv() {
                    Ok(ActorMsg::Shutdown) => {
                        self.handle_shutdown();
                        return;
                    }
                    Ok(
                        ActorMsg::Enqueue { generation, .. } | ActorMsg::Interrupt { generation },
                    ) => {
                        let _ = generation;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            let claimed = lock(&self.handle.activity).claim();
            let Some((generation, period)) = claimed else {
                if !self.park(&mut rx).await {
                    return;
                }
                continue;
            };
            if let Err(error) = self
                .inner
                .bus
                .publish(SessionEventData::ExecutionStarted {
                    session_id: self.session_id.clone(),
                })
                .await
            {
                tracing::error!(session = %self.session_id, %error, "execution.started failed");
                lock(&self.handle.activity).finish(generation);
                self.handle.settled.notify_waiters();
                continue;
            }
            if period.is_cancelled() {
                lock(&self.handle.activity).settle(generation, true);
                self.terminal_interrupted().await;
                lock(&self.handle.activity).finish(generation);
                self.handle.settled.notify_waiters();
                continue;
            }

            // Busy period: consecutive drains until the inbox is quiet. Every
            // coalesced drain shares one terminal publication. The project
            // directory is the SESSION's directory (the client's project),
            // not the server process cwd — tools and the system prompt must
            // operate where the session lives.
            // Re-pointed at the newest generation before each drain and
            // before the background fold; `run_turn` also does so at each
            // step boundary.
            // Resolved per busy period, not per actor: the session's agent
            // can be switched (`SessionAgentSelected`), which changes its
            // toolset and system layer from its next busy period. Each
            // generation precomputes the registries (see
            // [`Services::tools_for`]).
            let agent = self.session_agent().await;
            let svc = self.inner.live.current();
            let mut ctx = drain::DrainCtx {
                session_id: self.session_id.clone(),
                agent,
                directory: self.session_directory().await,
                home: self.inner.home.clone(),
                config: Arc::clone(&svc.config),
                db: self.inner.db.clone(),
                bus: self.inner.bus.clone(),
                chat: Arc::clone(&svc.chat),
                tools: svc.tools_for(agent),
                model_catalog: Arc::clone(&svc.model_catalog),
                mcp: Arc::clone(&svc.mcp),
                live: Arc::clone(&self.inner.live),
                generation: svc.generation,
                instruction_baselines: self.inner.instruction_baselines.clone(),
                calibrations: self.inner.calibrations.clone(),
                cache_ledger: Arc::clone(&self.inner.cache_ledger),
                grants: Arc::clone(&self.inner.grants),
                instruction_root: self.handle.instruction_root.clone(),
                fold_in_flight: Arc::clone(&self.handle.fold_in_flight),
                tasks: self.inner.tasks.clone(),
            };
            let mut scope = drain::Promotable::Input;
            loop {
                if period.is_cancelled() {
                    lock(&self.handle.activity).settle(generation, true);
                    self.terminal_interrupted().await;
                    break;
                }
                ctx.refresh();
                match drain::drain(&ctx, &period, scope).await {
                    drain::DrainOutcome::Interrupted => {
                        lock(&self.handle.activity).settle(generation, true);
                        tracing::debug!(session = %self.session_id, "drain interrupted");
                        self.terminal_interrupted().await;
                        break;
                    }
                    drain::DrainOutcome::Failed(error) => {
                        lock(&self.handle.activity).settle(generation, true);
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
                        loop {
                            match rx.try_recv() {
                                Ok(ActorMsg::Enqueue { .. } | ActorMsg::Interrupt { .. }) => {}
                                Ok(ActorMsg::Shutdown) => {
                                    self.handle_shutdown();
                                    return;
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => return,
                            }
                        }
                        if period.is_cancelled() {
                            lock(&self.handle.activity).settle(generation, true);
                            self.terminal_interrupted().await;
                            break;
                        }
                        if lock(&self.handle.activity).settle(generation, false) {
                            if period.is_cancelled() {
                                self.terminal_interrupted().await;
                                break;
                            }
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
                            // The fold resolves its own selection once, so the
                            // model and its reasoning policy cannot diverge.
                            ctx.refresh();
                            compaction::spawn_background_fold(&ctx);
                            break;
                        }
                        // Fresh work: the next pass starts at an input boundary.
                        tracing::debug!(session = %self.session_id, "doorbell saw new work; draining again");
                        scope = drain::Promotable::Input;
                    }
                }
            }
            lock(&self.handle.activity).finish(generation);
            // The terminal is published and the flags are down: every
            // `wait_idle` re-checks `is_active` on this wake-up.
            self.handle.settled.notify_waiters();
            // Loop: absorb anything that arrived while the terminal settled.
        }
    }

    fn handle_shutdown(&self) {
        // The actor is done for good; drop the registry entry so a later
        // dispatch builds a fresh handle instead of messaging a dead mailbox.
        // The baseline and the calibration go with it: a later dispatch simply
        // re-baselines from the sources and re-measures in one turn.
        self.inner.forget(&self.session_id, Some(&self.handle));
    }

    /// The session's agent record, from its `agent` and `parent_id` (see
    /// [`agent_of`]). A missing row or a failed lookup answers the read-only
    /// agent, the narrowest toolset: the row might be a read-only child, and a
    /// missing tool for one busy period is cheaper than a writer the parent
    /// asked to withhold.
    async fn session_agent(&self) -> &'static Agent {
        session_agent(&self.inner.db, &self.session_id).await
    }

    /// The session's project directory: the `directory` recorded on the
    /// session row (the client's project), canonicalized; falls back to the
    /// server's directory when the row predates the column or the path is
    /// gone.
    async fn session_directory(&self) -> std::path::PathBuf {
        session_directory(&self.inner.db, &self.session_id, &self.inner.directory).await
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
    async fn park(&mut self, rx: &mut mpsc::Receiver<ActorMsg>) -> bool {
        // The turn truly ended here: the idle clock restarts from this point,
        // not from the dispatch that started the (possibly long) busy period.
        self.handle.last_used.store(
            ox_schema::id::now_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let mut slot = self.handle.parked.lock().await;
        if !rx.is_empty() || handle_is_active(&self.handle) {
            return true;
        }
        // Any dispatch past this point finds the slot occupied and spawns a
        // fresh task, so the handoff loses nothing.
        let parked = std::mem::replace(rx, mpsc::channel(1).1);
        *slot = Some(parked);
        self.parked = true;
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

/// Shared directory lookup, with the actor's canonicalization/fallback policy.
async fn session_directory(
    db: &Db,
    session_id: &str,
    fallback: &std::path::Path,
) -> std::path::PathBuf {
    let looked_up: anyhow::Result<Option<String>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT directory FROM session_v2 WHERE id = ?1")
            .await?;
        let row = ox_db::optional(&mut stmt, [session_id]).await?;
        match row {
            Some(row) => Ok(row.get::<Option<String>>(0)?),
            None => Ok(None),
        }
    }
    .await;
    let raw = looked_up.inspect_err(|error| {
        tracing::warn!(session = %session_id, error = %error, "session directory lookup failed; falling back to server directory")
    }).ok().flatten();
    match raw.map(std::path::PathBuf::from) {
        Some(path) if path.is_dir() => path.canonicalize().unwrap_or(path),
        other => {
            tracing::warn!(session = %session_id, directory = ?other, "session directory missing or gone; falling back to server directory");
            fallback.to_path_buf()
        }
    }
}

/// Shared session agent lookup. Missing rows and failed reads stay read-only.
async fn session_agent(db: &Db, session_id: &str) -> &'static Agent {
    type Row = (Option<String>, Option<String>);
    let looked_up: anyhow::Result<Option<Row>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT parent_id, agent FROM session_v2 WHERE id = ?1")
            .await?;
        let row = ox_db::optional(&mut stmt, [session_id]).await?;
        match row {
            Some(row) => Ok(Some((
                row.get::<Option<String>>(0)?,
                row.get::<Option<String>>(1)?,
            ))),
            None => Ok(None),
        }
    }
    .await;
    match looked_up {
        Ok(Some((parent_id, agent))) => agent_of(parent_id.as_deref(), agent.as_deref()),
        Ok(None) => {
            tracing::warn!(
                session = %session_id,
                "session row missing; running with the read-only toolset"
            );
            agent::read_only()
        }
        Err(error) => {
            tracing::warn!(
                session = %session_id,
                error = %error,
                "session agent lookup failed; running with the read-only toolset"
            );
            agent::read_only()
        }
    }
}

/// Resolve a session row's agent record. An unnamed or unknown agent id runs
/// the default agent. A child (a row with a parent) never runs a primary
/// agent: it is downgraded to the subagent default, because a primary agent's
/// full toolset includes `question`, whose form would hang the parent's
/// `subagent` call.
#[must_use]
fn agent_of(parent_id: Option<&str>, agent: Option<&str>) -> &'static Agent {
    let resolved = agent
        .and_then(agent::get)
        .unwrap_or_else(agent::default_agent);
    if parent_id.is_some() && resolved.mode == Mode::Primary {
        return agent::subagent_default();
    }
    resolved
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
    model_catalog: &ox_llm::model_catalog::ModelCatalog,
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
        None => match model_catalog.default_entry(&config.llm) {
            Some(configured) => ox_schema::support::ModelRef {
                id: configured.id.clone(),
                provider_id: configured.provider.clone(),
                variant: None,
            },
            None => ox_schema::support::ModelRef {
                id: "default".to_owned(),
                provider_id: ox_llm::config::DEFAULT_PROVIDER_NAME.to_owned(),
                variant: None,
            },
        },
    };
    Ok((
        agent.unwrap_or_else(|| agent::DEFAULT_AGENT.to_owned()),
        model,
    ))
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

struct WeakRunner(std::sync::Weak<Inner>);

impl WeakRunner {
    fn runner(&self) -> Result<Runner> {
        self.0
            .upgrade()
            .map(|inner| Runner { inner })
            .ok_or_else(|| ox_tools::contract::Closing.into())
    }
}

impl ox_tools::shell::completion::ShellWaker for WeakRunner {
    fn try_admit(
        &self,
    ) -> Result<ox_tools::contract::ExecutionAdmission, ox_tools::contract::Closing> {
        let inner = self.0.upgrade().ok_or(ox_tools::contract::Closing)?;
        inner.tasks.gate.try_admit()
    }
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { self.runner()?.dispatch(session_id, inbox_id).await })
    }
}

impl ox_tools::subagent::SubagentSpawner for WeakRunner {
    fn try_admit(
        &self,
    ) -> Result<ox_tools::contract::ExecutionAdmission, ox_tools::contract::Closing> {
        let inner = self.0.upgrade().ok_or(ox_tools::contract::Closing)?;
        inner.tasks.gate.try_admit()
    }
    fn is_closing(&self) -> bool {
        self.0
            .upgrade()
            .is_none_or(|inner| inner.tasks.gate.is_closed())
    }
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { self.runner()?.dispatch(session_id, inbox_id).await })
    }
    fn is_active(&self, session_id: &str) -> bool {
        self.runner()
            .is_ok_and(|runner| runner.is_active(session_id))
    }
    fn interrupt(&self, session_id: &str) -> bool {
        self.runner()
            .is_ok_and(|runner| runner.interrupt(session_id))
    }
    /// Block until the child is no longer active. Capture its handle before
    /// returning the future so a later forget cannot turn an active wait idle.
    fn wait_idle<'a>(&'a self, session_id: &'a str) -> ox_tools::subagent::WaitIdle<'a> {
        let handle = self
            .0
            .upgrade()
            .and_then(|inner| inner.sessions.get(session_id).map(|entry| entry.clone()));
        Box::pin(async move {
            let Some(handle) = handle else {
                return;
            };
            loop {
                let notified = handle.settled.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !handle_is_active(&handle) {
                    return;
                }
                notified.await;
            }
        })
    }
    fn settled_tail<'a>(&'a self, session_id: &'a str) -> ox_tools::subagent::SettledTail<'a> {
        Box::pin(async move {
            crate::runner::step::settled_tail(&self.runner()?.inner.db, session_id).await
        })
    }
    /// Forward to the live runner. A dropped engine refuses instead of
    /// resolving against a catalogue it no longer has.
    fn child_model(&self, agent: &str) -> std::result::Result<Option<ModelRef>, String> {
        let Some(inner) = self.0.upgrade() else {
            return Err("Engine is shutting down; no child model was resolved".to_owned());
        };
        <Runner as ox_tools::subagent::SubagentSpawner>::child_model(&Runner { inner }, agent)
    }
}

/// Wakes a session for a background `shell` completion.
impl ox_tools::shell::completion::ShellWaker for Runner {
    fn try_admit(
        &self,
    ) -> Result<ox_tools::contract::ExecutionAdmission, ox_tools::contract::Closing> {
        Runner::try_admit(self)
    }
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Runner::dispatch(self, session_id, inbox_id).await })
    }
}

/// The engine capabilities the `subagent` tool drives a child session
/// through, implemented over the registry: dispatch, activity, interruption
/// and the projected transcript.
impl ox_tools::subagent::SubagentSpawner for Runner {
    fn try_admit(
        &self,
    ) -> Result<ox_tools::contract::ExecutionAdmission, ox_tools::contract::Closing> {
        Runner::try_admit(self)
    }
    fn is_closing(&self) -> bool {
        self.inner.tasks.gate.is_closed()
    }
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

    fn wait_idle<'a>(&'a self, session_id: &'a str) -> ox_tools::subagent::WaitIdle<'a> {
        Box::pin(Runner::wait_idle(self, session_id))
    }

    fn interrupt(&self, session_id: &str) -> bool {
        Runner::interrupt(self, session_id)
    }

    fn settled_tail<'a>(&'a self, session_id: &'a str) -> ox_tools::subagent::SettledTail<'a> {
        Box::pin(crate::runner::step::settled_tail(
            &self.inner.db,
            session_id,
        ))
    }

    /// The child's model comes from the agent's logical preference resolved
    /// against the current services generation. An agent the table does not
    /// define, or one without a preference, inherits the parent's model; a
    /// preference the catalogue does not know refuses the spawn rather than
    /// quietly running the child on the parent's model.
    fn child_model(&self, agent_id: &str) -> std::result::Result<Option<ModelRef>, String> {
        let services = self.inner.live.current();
        let Some(profile) = agent::get(agent_id) else {
            return Ok(None);
        };
        let Some(id) = model_roles::agent_model(&services.config, profile) else {
            return Ok(None);
        };
        model_roles::logical(&services.model_catalog, id)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "Agent `{agent_id}` prefers model `{id}`, which the model catalogue does not know. Configure it under `[models.{id}]` or remove the preference; no child was started."
                )
            })
    }
}
