//! Live shell sessions: the process-wide table behind the `shell` tool's
//! `run` / `interact` / `stop` actions and the `/api/shell` endpoints.
//!
//! One [`ShellSession`] owns a system shell child (see [`super::process`]),
//! the merged stdout/stderr tail in [`OutputBuffer`], and the run's terminal
//! state. The table is shared by `Arc` between the tool (which spawns and
//! stops) and the API (which lists sessions and pages their output), so
//! neither side owns the other.
//!
//! Capture is bounded: the buffer keeps the newest [`OUTPUT_CAPACITY`] bytes
//! and remembers how many it dropped, so a long-running server cannot grow the
//! process without limit and a reader can still tell that it missed output.
//! Output pages are built from one slice, never by copying the whole buffer.
//!
//! Process model: each command is `shell -c` in its own process group
//! (`pgid` = the shell's pid). The session settles when the shell exits.
//! Background children (`cmd &`) are ordinary processes of that group: they
//! outlive the call, stay visible to `ps`, and keep writing into the bounded
//! capture until they close the pipe. `stop` is the one explicit owner of
//! their teardown (session deletion and shutdown go through it), and a
//! settled session whose group still lives is never evicted, so that handle
//! is not lost. Signals only ever go to the group this session spawned.

use std::collections::HashMap;
use std::mem;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use super::process::{self, Signal, Spawned};

/// Newest bytes retained per session. Upstream keeps the whole capture in a
/// file; the tail is what a client renders and what the model needs.
pub const OUTPUT_CAPACITY: usize = 8 * 1024 * 1024;
/// How long a settled session stays readable before a sweep (after a settle
/// or on spawn) reaps it.
/// A running session never ages out: there is no completion timestamp to
/// measure from, and its output is still being written.
pub const SHELL_SESSION_TTL_MS: i64 = 60 * 60 * 1000;
/// Settled sessions kept readable through `get`/`output`, newest first
/// (upstream `packages/core/src/shell.ts` `EXITED_LIMIT`).
pub const EXITED_LIMIT: usize = 25;
/// Bytes all settled captures may retain together. ox keeps output in memory
/// rather than in a file, so finished captures also share a byte budget
/// (plan deviation D1); the oldest give up their bytes first.
const SETTLED_OUTPUT_BUDGET: usize = 32 * 1024 * 1024;
/// Default page size for one `output` read.
pub const DEFAULT_PAGE_BYTES: usize = 64 * 1024;
/// How long the output pipe may keep delivering after the shell exited before
/// the session settles. Output already in the pipe is drained within this
/// window; a background child that still holds the pipe keeps appending
/// afterwards.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);
/// How long a non-forced `stop` waits for SIGTERM to take effect before it
/// escalates to SIGKILL.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// How often a settled `stop` re-probes its process group while waiting for
/// SIGTERM to take effect.
const GROUP_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Upstream `Shell.Status`, re-exported from the shared wire schema.
pub use ox_schema::support::ShellStatus as Status;
pub use ox_schema::support::{ShellInfo, ShellInfoTime as Time};

use ox_schema::support::Record;

/// The bounded tail of a session's merged stdout/stderr.
#[derive(Debug)]
pub struct OutputBuffer {
    capacity: usize,
    bytes: Vec<u8>,
    /// Bytes ever appended, including the ones dropped from the front.
    total: u64,
}

/// One page of captured output (upstream `Shell.Output`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPage {
    pub output: String,
    /// Byte cursor after this page; equals `size` when caught up.
    pub cursor: u64,
    /// Total bytes captured so far.
    pub size: u64,
    /// The requested cursor predates the retained tail.
    pub truncated: bool,
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self::with_capacity(OUTPUT_CAPACITY)
    }
}

impl OutputBuffer {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            // A bound, not a preallocation: a session that prints one line
            // must not reserve the whole capacity.
            bytes: Vec::new(),
            total: 0,
        }
    }

    /// Bytes appended so far, including dropped ones.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.total
    }

    /// The first retained byte's absolute offset.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.total - self.bytes.len() as u64
    }

    /// Append one chunk, dropping from the front when over capacity.
    pub fn append(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if chunk.len() >= self.capacity {
            // The chunk alone fills the buffer: keep only its tail.
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&chunk[chunk.len() - self.capacity..]);
            return;
        }
        self.bytes.extend_from_slice(chunk);
        let overflow = self.bytes.len().saturating_sub(self.capacity);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
    }

    /// Read `limit` bytes starting at absolute `cursor`. The returned text is
    /// the only allocation; the buffer is only read.
    #[must_use]
    pub fn page(&self, cursor: u64, limit: usize) -> OutputPage {
        let dropped = self.dropped();
        let truncated = cursor < dropped;
        // Clamp both ends: a cursor past the capture's end is EOF (an empty
        // page), not a panic — `bytes[offset..]` rejects offset > len even
        // for an empty range.
        let start = cursor.max(dropped).min(self.total);
        let offset = (start - dropped) as usize;
        let available = self.bytes.len().saturating_sub(offset);
        let take = available.min(limit.max(1));
        let slice = &self.bytes[offset..offset + take];
        OutputPage {
            output: String::from_utf8_lossy(slice).into_owned(),
            cursor: start + take as u64,
            size: self.total,
            truncated,
        }
    }

    /// The newest `limit` retained bytes as text (a completion's bounded tail).
    #[must_use]
    pub fn recent_text(&self, limit: usize) -> String {
        let start = self.bytes.len().saturating_sub(limit);
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }

    /// The whole retained tail, for the model-facing tool result.
    #[must_use]
    pub fn tail_text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Bytes currently retained.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.bytes.len()
    }

    /// Release the slack the drain policy leaves behind. Called once at
    /// settle: afterwards the buffer is read-mostly (paged output), so the
    /// one realloc costs nothing and a session whose output once crossed the
    /// capacity stops pinning 8–16MiB for its remaining lifetime.
    pub fn shrink_to_retained(&mut self) {
        self.bytes.shrink_to_fit();
    }

    /// Free the retained bytes while keeping `size()`, so an old cursor reads
    /// as truncated instead of as a capture that never had output.
    pub fn release(&mut self) {
        drop(mem::take(&mut self.bytes));
    }
}

/// What the retention policy needs to know about one settled session.
#[derive(Debug, Clone, Copy)]
struct SettledCapture {
    /// `Exit.completed` in epoch milliseconds.
    completed: i64,
    /// [`OutputBuffer::retained`] at snapshot time.
    retained: usize,
    /// The session's process group was alive at snapshot time. A pinned
    /// session is never removed (it holds the only handle to its `cmd &`
    /// children), but its output still counts toward the budget.
    pinned: bool,
}

/// Indices into the [`SettledCapture`] slice the plan was made from.
#[derive(Debug, Default)]
struct EvictionPlan {
    /// Sessions to drop from the table.
    remove: Vec<usize>,
    /// Surviving sessions whose output is released.
    release: Vec<usize>,
}

/// Decide which settled sessions to drop (past the TTL, then beyond
/// [`EXITED_LIMIT`], oldest first) and which survivors release their output
/// to fit [`SETTLED_OUTPUT_BUDGET`], oldest first. Pinned sessions are never
/// dropped and do not occupy an [`EXITED_LIMIT`] slot, but their output is
/// budgeted and released like any other.
fn eviction_plan(settled: &[SettledCapture], now_ms: i64) -> EvictionPlan {
    plan_with(
        settled,
        now_ms,
        SHELL_SESSION_TTL_MS,
        EXITED_LIMIT,
        SETTLED_OUTPUT_BUDGET,
    )
}

/// [`eviction_plan`] with explicit limits, so tests can use small numbers.
/// Pinned captures never enter `remove` and are exempt from `ttl_ms` and
/// `limit`; they only take part in the `budget` pass.
fn plan_with(
    settled: &[SettledCapture],
    now_ms: i64,
    ttl_ms: i64,
    limit: usize,
    budget: usize,
) -> EvictionPlan {
    let mut plan = EvictionPlan::default();
    let mut pinned: Vec<usize> = Vec::new();
    let mut survivors: Vec<usize> = Vec::with_capacity(settled.len());
    for (index, capture) in settled.iter().enumerate() {
        if capture.pinned {
            pinned.push(index);
        } else if now_ms.saturating_sub(capture.completed) > ttl_ms {
            plan.remove.push(index);
        } else {
            survivors.push(index);
        }
    }
    // Oldest first; equal completion times fall back to slice order.
    survivors.sort_by_key(|&index| (settled[index].completed, index));
    let excess = survivors.len().saturating_sub(limit);
    plan.remove.extend(survivors.drain(..excess));

    // Pinned sessions stay, so their output joins the budget pass.
    survivors.extend(pinned);
    survivors.sort_by_key(|&index| (settled[index].completed, index));
    let mut total: usize = survivors.iter().map(|&index| settled[index].retained).sum();
    for &index in &survivors {
        if total <= budget {
            break;
        }
        let retained = settled[index].retained;
        if retained > 0 {
            plan.release.push(index);
            total -= retained;
        }
    }
    plan
}

/// Terminal state of a session.
#[derive(Debug, Clone, Copy, Default)]
pub struct Exit {
    pub status: Option<Status>,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub completed: Option<i64>,
}

impl Exit {
    fn is_settled(self) -> bool {
        self.status.is_some_and(|status| status != Status::Running)
    }
}

/// One running or finished shell command.
pub struct ShellSession {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub started: i64,
    output: Mutex<OutputBuffer>,
    exit: Mutex<Exit>,
    /// Process group id of the command: the shell's pid, since it leads a
    /// fresh group. Every signal this session sends targets this group only.
    pgid: u32,
    /// Whether the group still had members when the session settled. Only
    /// then may a settled `stop` signal `pgid`. Every liveness probe through
    /// [`ShellSession::group_alive`] (`stop`, eviction, shutdown) clears it
    /// the first time it sees the group gone, so a reused pid is never
    /// signalled.
    group_may_live: AtomicBool,
    /// Basename of the shell the command runs under (`ShellInfo.shell`).
    shell_name: Arc<str>,
    /// Fires when output is appended or the session settles, so a waiter
    /// needs no polling interval of its own.
    changed: Notify,
    events: broadcast::Sender<ShellEvent>,
    completion: Option<CompletionCallback>,
    /// Decides, together with the terminal state, whether the completion is
    /// delivered. Always locked BEFORE `exit` (see [`ShellSession::publish_terminal`]).
    gate: Arc<Mutex<Gate>>,
    stop_requested: AtomicBool,
    /// Set once `publish_terminal` has delivered the exited notice (and the
    /// completion). `settled` flips a moment earlier; `stop` waits for this
    /// so a caller that deletes the session afterwards can never announce
    /// `deleted` ahead of `exited`.
    announced: AtomicBool,
    /// Set by the deadline timer right before it kills the group, so `drive`
    /// can tell a timeout from a `stop`.
    timed_out: AtomicBool,
    /// The session deadline timer. ox owns it so a foreground run handed back
    /// to the background can drop it, as upstream does
    /// (`shell.timeout(id, 0)`).
    deadline: Mutex<Option<AbortHandle>>,
    /// The calling session, when the run belongs to one: its id becomes
    /// `ShellInfo.metadata.sessionID` and its directory stamps lifecycle
    /// notices. A run without an owner has no location, so it is never
    /// announced.
    owner: Option<Owner>,
    /// Extra tags merged into `ShellInfo.metadata` (upstream
    /// `CreateInput.metadata`), besides the owner's `sessionID`.
    metadata: Record,
    /// The table's observer as of spawn. Cloned once per session, so a
    /// session reports through the sink that was installed when it started.
    observer: Option<Observer>,
    /// The table this session lives in, so `drive` can run the retention
    /// sweep right after the settle. `Weak`: the table owns its sessions.
    table: Weak<ShellSessions>,
}

/// Whether a settling session reports a completion. The gate and the terminal
/// state change under one lock, so "settled" and "notified or not" can never
/// be observed apart:
///
/// - `Armed`: the model was told this run is in the background; the settle
///   hands off to `Delivering`, retained until the sink returns after durable
///   commit or explicit failure. Only then does the gate become `Sent`.
/// - `Held`: a foreground run still inside its yield window. Settling here
///   means the tool returns the result inline, so nothing is delivered
///   (`Quiet`); being handed back first makes it `Armed`.
/// - `Quiet`: settled inline, never notifies. `Sent`: delivered, or there is
///   no sink to deliver to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Armed,
    Delivering,
    Held,
    Quiet,
    Sent,
}

/// The outcome of [`ShellSession::hand_back`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandBack {
    /// The run is now a background job: exactly one completion follows its
    /// settle (or already went out, if it settled an instant ago).
    Background,
    /// The run settled first and is being returned inline, or it was
    /// silenced (its owner is being deleted or ox is exiting, and the turn is
    /// cancelled); no completion will ever be delivered for it.
    Settled,
}

/// A terminal shell result delivered exactly once, without reading transcript
/// parts or requiring an output poller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub id: String,
    pub command: String,
    pub status: Status,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    /// The newest bytes of the capture, at most one model-facing `MAX_BYTES`
    /// tail: a completion never carries the whole 8 MiB buffer.
    pub output: String,
    /// The capture held more than `output` carries.
    pub truncated: bool,
}

/// Receives one completion for each session that reaches a terminal state.
pub type CompletionFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub type CompletionCallback = Arc<dyn Fn(Completion) -> CompletionFuture + Send + Sync + 'static>;

/// The session a run belongs to (upstream stores `{ sessionID }` in
/// `Shell.Info.metadata`; the directory is the location its events carry).
#[derive(Debug, Clone)]
pub struct Owner {
    pub session_id: String,
    pub directory: String,
}

/// A table-level lifecycle notice (upstream `Shell.Event.Created` /
/// `Exited` / `Deleted`), delivered synchronously to the installed
/// [`Observer`] in the order it happens: created, then exited (after the
/// capture is final), then deleted.
#[derive(Debug, Clone)]
pub enum Notice {
    Created(Box<ShellInfo>),
    Exited {
        id: String,
        exit: Option<i32>,
        status: Status,
    },
    Deleted {
        id: String,
    },
}

/// Receives `(owner directory, notice)`. Runs inline on the thread that
/// causes the transition, so it must not block.
pub type Observer = Arc<dyn Fn(&str, Notice) + Send + Sync + 'static>;

/// A lifecycle notification (upstream `Shell.Event`).
#[derive(Debug, Clone)]
pub struct ShellEvent {
    pub kind: &'static str,
    pub id: String,
    pub status: Status,
    pub exit: Option<i32>,
}

impl ShellSession {
    /// Captured output since `cursor`, plus terminal state.
    pub fn poll(&self, cursor: u64, limit: usize) -> (OutputPage, Exit) {
        let page = self.output.lock().map_or_else(
            |_| OutputPage {
                output: String::new(),
                cursor,
                size: 0,
                truncated: false,
            },
            |buffer| buffer.page(cursor, limit),
        );
        (page, self.exit())
    }

    /// The newest `limit` retained bytes as text.
    pub fn recent(&self, limit: usize) -> String {
        self.output
            .lock()
            .map(|buffer| buffer.recent_text(limit))
            .unwrap_or_default()
    }

    /// The retained tail as model text.
    pub fn tail(&self) -> String {
        self.output
            .lock()
            .map(|buffer| buffer.tail_text())
            .unwrap_or_default()
    }

    /// Bytes appended so far.
    pub fn size(&self) -> u64 {
        self.output.lock().map(|buffer| buffer.size()).unwrap_or(0)
    }

    /// Bytes the capture currently retains (what the settled budget counts).
    fn retained(&self) -> usize {
        self.output
            .lock()
            .map(|buffer| buffer.retained())
            .unwrap_or(0)
    }

    /// Free the retained capture, keeping its size (see [`OutputBuffer::release`]).
    fn release_output(&self) {
        if let Ok(mut buffer) = self.output.lock() {
            buffer.release();
        }
    }

    /// Terminal state, or the default (still running) state.
    pub fn exit(&self) -> Exit {
        self.exit
            .lock()
            .map(|exit| *exit)
            .unwrap_or_else(|poisoned| *poisoned.into_inner())
    }

    /// Whether the command has settled (exited, timed out, or been killed).
    pub fn settled(&self) -> bool {
        self.exit().is_settled()
    }

    /// Wait until output arrives or `timeout` passes, so an `interact` call
    /// returns as soon as the command reacts instead of after a fixed delay.
    pub async fn wait_change(&self, timeout: std::time::Duration) {
        let _ = tokio::time::timeout(timeout, self.changed.notified()).await;
    }

    /// Wait until the session settles or `timeout` passes.
    pub async fn wait_settled(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.settled() {
                return true;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return self.settled();
            }
            if tokio::time::timeout(left, self.changed.notified())
                .await
                .is_err()
            {
                return self.settled();
            }
        }
    }

    /// Record the terminal state once. Only the call that settles the session
    /// emits the lifecycle event and decides the completion, so concurrent
    /// stops and a finishing run cannot publish twice. The gate is locked
    /// before `exit` and both change together: a reader holding the gate sees
    /// a settled session and its delivery decision as one fact.
    fn publish_terminal(&self, exit: Exit) {
        let fire = {
            let mut gate = self
                .gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            {
                let mut slot = self
                    .exit
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if slot.is_settled() {
                    return;
                }
                *slot = exit;
            }
            match *gate {
                Gate::Armed => {
                    *gate = Gate::Delivering;
                    true
                }
                Gate::Held => {
                    *gate = Gate::Quiet;
                    false
                }
                Gate::Quiet | Gate::Sent | Gate::Delivering => false,
            }
        };
        self.clear_deadline();
        let _ = self.events.send(ShellEvent {
            kind: "shell.exited",
            id: self.id.clone(),
            status: exit.status.unwrap_or(Status::Exited),
            exit: exit.code,
        });
        self.notify(Notice::Exited {
            id: self.id.clone(),
            exit: exit.code,
            status: exit.status.unwrap_or(Status::Exited),
        });
        if fire {
            self.notify_completion(exit);
        }
        self.announced.store(true, Ordering::Release);
    }

    /// Hand a foreground run to the background after its yield window. If it
    /// is still unsettled, its settle will deliver exactly one completion; if
    /// it settled first, the caller returns the result inline and no
    /// completion is ever delivered.
    pub fn hand_back(&self) -> HandBack {
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *gate {
            Gate::Held => {
                *gate = Gate::Armed;
                drop(gate);
                self.clear_deadline();
                HandBack::Background
            }
            Gate::Quiet => HandBack::Settled,
            Gate::Armed | Gate::Sent | Gate::Delivering => HandBack::Background,
        }
    }

    /// Never deliver a completion for this run: an `Armed` or `Held` gate
    /// becomes `Quiet`. Used before stopping the shells of a session that is
    /// being deleted, or every shell when ox exits, whose completion would
    /// otherwise wake a turn. A later `hand_back` reports `Settled`; only a
    /// cancelled turn can reach it, and that turn stops the run itself.
    fn silence(&self) {
        let mut gate = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(*gate, Gate::Armed | Gate::Held) {
            *gate = Gate::Quiet;
        }
    }

    /// Whether the completion gate still owes durable notification.
    fn owes_wakeup(&self) -> bool {
        matches!(
            *self.gate.lock().unwrap_or_else(|p| p.into_inner()),
            Gate::Armed | Gate::Delivering
        )
    }

    /// Whether this session reports its completion through a sink.
    pub fn notifies_on_completion(&self) -> bool {
        self.completion.is_some()
    }

    /// Drop the session deadline: the run now lasts until it exits or is
    /// stopped. A deadline that already fired is not undone.
    pub fn clear_deadline(&self) {
        let handle = self
            .deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            handle.abort();
        }
    }

    /// Report a lifecycle notice to the observer. Needs both an observer and
    /// an owner: the notice is stamped with the owner's directory.
    fn notify(&self, notice: Notice) {
        if let (Some(observer), Some(owner)) = (&self.observer, &self.owner) {
            // A panicking observer must not unwind through `drive` or a
            // `remove`: the completion and every `stop` waiter still follow.
            if std::panic::catch_unwind(AssertUnwindSafe(|| observer(&owner.directory, notice)))
                .is_err()
            {
                tracing::error!(id = %self.id, "shell lifecycle observer panicked");
            }
        }
    }

    fn notify_completion(&self, exit: Exit) {
        let Some(callback) = self.completion.as_ref() else {
            return;
        };
        let completion = Completion {
            id: self.id.clone(),
            command: self.command.clone(),
            status: exit.status.unwrap_or(Status::Exited),
            exit: exit.code,
            signal: exit.signal,
            output: self.recent(super::MAX_BYTES),
            truncated: self.size() > super::MAX_BYTES as u64,
        };
        let delivery = callback(completion);
        let gate = Arc::clone(&self.gate);
        let Some(table) = self.table.upgrade() else {
            tracing::error!(id = %self.id, "shell delivery lost its owner");
            return;
        };
        if let Err(error) = table.deliveries.spawn(async move {
            // Drop also releases the obligation on explicit failure/cancellation.
            struct DeliveryGuard(Arc<Mutex<Gate>>);
            impl Drop for DeliveryGuard {
                fn drop(&mut self) {
                    *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Gate::Sent;
                }
            }
            let _guard = DeliveryGuard(gate);
            delivery.await;
        }) {
            tracing::error!(%error, "shell delivery registration failed");
        }
    }

    /// Send `signal` to this session's process group. Failures other than a
    /// vanished group are logged, never propagated: a stop has no recovery.
    fn signal_group(&self, signal: Signal) {
        if let Err(error) = process::kill_group(self.pgid, signal) {
            tracing::warn!(id = %self.id, pgid = self.pgid, ?signal, %error, "shell signal failed");
        }
    }

    /// Terminate the command's whole process group.
    ///
    /// - `force`: SIGKILL at once. Otherwise SIGTERM, escalating to SIGKILL
    ///   when the shell outlives [`STOP_GRACE`] or the group still has members
    ///   once the shell is gone.
    /// - On an already settled session the leader is gone but background
    ///   children may live on (`npm run dev &`); they are killed if the group
    ///   is still alive: SIGTERM first, SIGKILL if the group outlives
    ///   [`STOP_GRACE`] (`force` skips the grace).
    ///
    /// Residual risk: the group id is a pid, and the kernel may reuse it once
    /// every process of the group has exited. While any member lives the id
    /// cannot be reused, so a settled session signals its group only if
    /// `drive` saw the group alive at settle (`group_may_live`) and it is
    /// still alive now. The first liveness probe that finds the group gone
    /// (from `stop`, eviction or shutdown) clears the flag and the session
    /// never signals again. This relies on some probe running while the
    /// group is gone but its id is not yet reused: a group that dies and is
    /// reassigned before any probe runs is indistinguishable from ours, so
    /// that narrow window remains.
    pub async fn stop(self: &Arc<Self>, force: bool) {
        let Some(table) = self.table.upgrade() else {
            tracing::error!(id = %self.id, "shell stop lost its domain owner");
            return;
        };
        let session = Arc::clone(self);
        let (done, wait) = tokio::sync::oneshot::channel();
        if let Err(error) = table.stops.spawn(async move {
            session.stop_owned(force).await;
            let _ = done.send(());
        }) {
            tracing::error!(%error, "shell stop registration failed");
            return;
        }
        if let Err(error) = wait.await {
            tracing::error!(%error, "shell stop task failed");
        }
    }

    async fn stop_owned(&self, force: bool) {
        if self.settled() {
            self.stop_settled_group(force).await;
            self.wait_announced().await;
            return;
        }
        self.stop_requested.store(true, Ordering::Release);
        if force {
            self.signal_group(Signal::Kill);
        } else {
            self.signal_group(Signal::Term);
            if !self.wait_settled(STOP_GRACE).await || process::group_alive(self.pgid) {
                self.signal_group(Signal::Kill);
            }
        }
        // `drive` owns terminal publication. Waiting here is intentional: the
        // exit must be published (and its output drained) before stop
        // returns, otherwise callers can observe a completion without the
        // final output and race a second stop against the first one.
        self.wait_announced().await;
    }

    /// Tear down a settled session's surviving group: SIGTERM, then SIGKILL
    /// once [`STOP_GRACE`] passes with the group still alive. Every signal is
    /// preceded by [`Self::group_alive`], so a group that exited (and whose id
    /// may be reused) is never signalled.
    async fn stop_settled_group(&self, force: bool) {
        if !self.group_alive() {
            return;
        }
        if force {
            self.signal_group(Signal::Kill);
            return;
        }
        self.signal_group(Signal::Term);
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(left.min(GROUP_POLL)).await;
            if !self.group_alive() {
                return;
            }
        }
        if self.group_alive() {
            self.signal_group(Signal::Kill);
        }
    }

    /// Wait until the terminal notice went out (see [`Self::announced`]).
    async fn wait_announced(&self) {
        loop {
            let notified = self.changed.notified();
            if self.announced.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Whether a settled session's process group may still be signalled:
    /// `drive` saw it alive at settle and it is alive now. The first call
    /// that finds it gone clears `group_may_live`, so a reused pid is never
    /// treated as ours (see [`Self::stop`]).
    fn group_alive(&self) -> bool {
        if !self.group_may_live.load(Ordering::Acquire) {
            return false;
        }
        if process::group_alive(self.pgid) {
            return true;
        }
        self.group_may_live.store(false, Ordering::Release);
        false
    }

    /// The `Shell.Info` a client reads.
    pub fn info(&self) -> ShellInfo {
        let exit = self.exit();
        ShellInfo {
            id: self.id.clone(),
            status: exit.status.unwrap_or(Status::Running),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            shell: self.shell_name.to_string(),
            // Capture lives in memory; the URL names the paging endpoint.
            file: format!("ox-shell://{}", self.id),
            pid: Some(self.pgid),
            exit: exit.code,
            signal: exit.signal.map(|signal| signal.to_string()),
            metadata: {
                let mut metadata = self.metadata.clone();
                if let Some(owner) = &self.owner {
                    metadata.insert("sessionID".to_owned(), serde_json::json!(owner.session_id));
                }
                metadata
            },
            time: Time {
                started: self.started,
                completed: exit.completed,
            },
        }
    }
}

/// What `spawn` needs to start one session.
pub struct SpawnRequest {
    pub command: String,
    pub cwd: std::path::PathBuf,
    /// Deadline for the whole session; `None` or `Some(0)` disables it.
    pub timeout_ms: Option<u64>,
    /// Optional one-shot terminal notification.
    pub completion: Option<CompletionCallback>,
    /// The run is a foreground call still inside its yield window: the
    /// completion stays gated until [`ShellSession::hand_back`]. `false`
    /// means the run is a background job from the start.
    pub foreground: bool,
    /// The calling session; `None` for a run that belongs to no session.
    pub owner: Option<Owner>,
    /// Extra opaque tags merged into `ShellInfo.metadata` (upstream
    /// `CreateInput.metadata`); `info()` adds `sessionID` on top.
    pub metadata: Record,
}

/// The process-wide session table, cloned as an `Arc` by the tool and the API.
pub struct ShellSessions {
    sessions: Mutex<HashMap<String, Arc<ShellSession>>>,
    environments: Mutex<HashMap<String, HashMap<String, String>>>,
    closed: Mutex<bool>,
    tasks: crate::contract::TaskOwner,
    deliveries: crate::contract::TaskOwner,
    pumps: crate::contract::TaskOwner,
    stops: crate::contract::TaskOwner,
    shutdown_started: Mutex<bool>,
    shutdown_task: crate::contract::TaskOwner,
    shutdown_error: Mutex<Option<String>>,
    /// Where lifecycle notices go. The composition root installs the event
    /// bridge; the table itself knows nothing about the bus.
    observer: Mutex<Option<Observer>>,
    /// The system shell every command runs under, selected once.
    shell: PathBuf,
    /// Basename of [`Self::shell`], what the model is told to write.
    shell_name: Arc<str>,
}

impl Default for ShellSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellSessions {
    #[must_use]
    pub fn new() -> Self {
        let shell = process::select_shell();
        let shell_name = shell
            .file_name()
            .map_or_else(|| "sh".into(), |name| name.to_string_lossy());
        Self {
            sessions: Mutex::default(),
            environments: Mutex::default(),
            closed: Mutex::new(false),
            tasks: crate::contract::TaskOwner::default(),
            deliveries: crate::contract::TaskOwner::default(),
            pumps: crate::contract::TaskOwner::default(),
            stops: crate::contract::TaskOwner::default(),
            shutdown_started: Mutex::new(false),
            shutdown_task: crate::contract::TaskOwner::default(),
            shutdown_error: Mutex::new(None),
            observer: Mutex::default(),
            shell_name: Arc::from(shell_name.as_ref()),
            shell,
        }
    }

    /// The shell's basename (`zsh`, `bash`, `sh`): the dialect to write.
    pub fn shell_name(&self) -> &str {
        &self.shell_name
    }

    /// Install (or replace) the lifecycle observer. Sessions spawned from now
    /// on report through it.
    pub fn set_observer(&self, observer: Observer) {
        *self
            .observer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(observer);
    }

    /// Replace the ephemeral environment used by future shells owned by
    /// `session_id`.
    pub fn set_environment(&self, session_id: &str, environment: HashMap<String, String>) {
        self.environments
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_owned(), environment);
    }

    /// Forget a session's ephemeral environment. Clearing an unknown session
    /// is intentionally idempotent.
    pub fn clear_environment(&self, session_id: &str) {
        self.environments
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    /// Spawn one session and start capturing its output. The caller decides
    /// how long to wait before returning control to the model.
    pub fn spawn(self: &Arc<Self>, request: SpawnRequest) -> std::io::Result<Arc<ShellSession>> {
        let closed = self.closed.lock().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(std::io::Error::other("shell admission is closed"));
        }
        let (events, _) = broadcast::channel(32);
        let environment = request.owner.as_ref().and_then(|owner| {
            self.environments
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&owner.session_id)
                .cloned()
        });
        let spawned = process::spawn(
            &self.shell,
            &request.command,
            &request.cwd,
            environment.as_ref(),
            request.owner.is_some(),
        )?;
        let session = Arc::new(ShellSession {
            id: ox_schema::id::id("sh"),
            command: request.command,
            cwd: request.cwd.to_string_lossy().into_owned(),
            started: ox_schema::id::now_millis(),
            output: Mutex::new(OutputBuffer::default()),
            exit: Mutex::new(Exit::default()),
            pgid: spawned.pgid,
            group_may_live: AtomicBool::new(false),
            shell_name: Arc::clone(&self.shell_name),
            changed: Notify::new(),
            events: events.clone(),
            gate: Arc::new(Mutex::new(
                match (request.completion.is_some(), request.foreground) {
                    (false, _) => Gate::Sent,
                    (true, true) => Gate::Held,
                    (true, false) => Gate::Armed,
                },
            )),
            completion: request.completion,
            stop_requested: AtomicBool::new(false),
            announced: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            deadline: Mutex::new(None),
            owner: request.owner,
            metadata: request.metadata,
            observer: self
                .observer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            table: Arc::downgrade(self),
        });
        if let Some(timeout) = request.timeout_ms.filter(|timeout| *timeout > 0) {
            let handle = self
                .tasks
                .spawn_with_handle(deadline_timer(
                    Arc::downgrade(&session),
                    std::time::Duration::from_millis(timeout),
                ))
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            *session
                .deadline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
        }

        if let Ok(mut table) = self.sessions.lock() {
            table.insert(session.id.clone(), Arc::clone(&session));
        }
        self.evict_idle(ox_schema::id::now_millis());
        let _ = events.send(ShellEvent {
            kind: "shell.created",
            id: session.id.clone(),
            status: Status::Running,
            exit: None,
        });
        // Announce before the driver starts: a command that exits at once must
        // not publish `exited` ahead of `created`.
        session.notify(Notice::Created(Box::new(session.info())));
        self.tasks
            .spawn(drive(Arc::clone(&session), spawned))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        drop(closed);
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<ShellSession>> {
        self.sessions
            .lock()
            .ok()
            .and_then(|table| table.get(id).cloned())
    }

    /// Includes settled shells whose sink has not finished durable admission.
    pub fn owes_wakeup(&self, owner: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .any(|session| {
                session
                    .owner
                    .as_ref()
                    .is_some_and(|entry| entry.session_id == owner)
                    && session.owes_wakeup()
            })
    }

    /// Every running session, oldest first (upstream `Shell.list`). Settled
    /// sessions stay readable through [`Self::get`].
    pub fn list(&self) -> Vec<ShellInfo> {
        let sessions: Vec<Arc<ShellSession>> = self
            .sessions
            .lock()
            .map(|table| table.values().cloned().collect())
            .unwrap_or_default();
        let mut infos: Vec<ShellInfo> = sessions
            .iter()
            .filter(|session| !session.settled())
            .map(|session| session.info())
            .collect();
        infos.sort_by_key(|info| info.time.started);
        infos
    }

    /// Remove a settled session. A running one is left alone: the caller
    /// stops it first.
    pub fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut table = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match table.get(id) {
                Some(session) if !session.settled() || session.owes_wakeup() => return false,
                Some(_) => table.remove(id),
                None => return false,
            }
        };
        // Announced after the table lock is released, and with the location
        // the session was created under: the client keys its shell store by it.
        removed.is_some_and(|session| {
            session.notify(Notice::Deleted {
                id: session.id.clone(),
            });
            true
        })
    }

    /// Stop every session owned by one of `session_ids`, then remove them
    /// (each removal announces `deleted`). Used when a session tree is
    /// deleted, as upstream `Session.remove` → `jobs.cancelSession` →
    /// `shell.remove`.
    /// Their completions are silenced first: the owner still exists while
    /// they settle, and a delivered completion would wake a turn on a session
    /// that is being deleted.
    /// The stops run concurrently, so the whole call is bounded by one
    /// [`STOP_GRACE`] rather than growing with the number of shells.
    pub async fn stop_owned(&self, session_ids: &[String]) {
        let owned: Vec<Arc<ShellSession>> = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|session| {
                session
                    .owner
                    .as_ref()
                    .is_some_and(|owner| session_ids.contains(&owner.session_id))
            })
            .cloned()
            .collect();
        for session in &owned {
            session.silence();
        }
        stop_concurrently(&owned).await;
        for session in &owned {
            self.remove(&session.id);
        }
    }

    /// Stop every session that may still have processes: the running ones
    /// and the settled ones whose process group is still alive (`cmd &`
    /// children). Called once by the server when serving ends, so no process
    /// group outlives ox (upstream finalizer, `core/src/shell.ts:149-159`).
    /// Their completions are silenced first: a turn woken while ox exits is
    /// paid for and cut off. Sessions stay in the table. The stops run
    /// concurrently, so the whole call is bounded by one [`STOP_GRACE`].
    /// Close process admission and silence only notifications not yet started.
    pub fn close(&self) {
        let mut closed = self.closed.lock().unwrap_or_else(|p| p.into_inner());
        *closed = true;
        let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        for session in sessions.values() {
            session.silence();
        }
    }

    pub async fn shutdown(self: &Arc<Self>) {
        if let Err(error) = self.stop_all_and_join().await {
            tracing::error!(%error, "shell shutdown failed");
        }
    }

    /// Wait for a previously closed shell domain; cancellation only drops a waiter.
    pub async fn stop_all_and_join(self: &Arc<Self>) -> anyhow::Result<()> {
        self.close();
        {
            let mut started = self
                .shutdown_started
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !*started {
                let table = Arc::clone(self);
                self.shutdown_task.spawn(async move {
                    table.finish_shutdown().await;
                })?;
                *started = true;
            }
        }
        self.shutdown_task.seal_and_join().await?;
        match self
            .shutdown_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            Some(error) => Err(anyhow::anyhow!("{error}")),
            None => Ok(()),
        }
    }

    async fn finish_shutdown(&self) {
        let live: Vec<Arc<ShellSession>> = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|session| !session.settled() || session.group_alive())
            .cloned()
            .collect();
        for session in &live {
            session.silence();
        }
        stop_concurrently(&live).await;
        let stops = self.stops.seal_and_join().await;
        let tasks = self.tasks.seal_and_join().await;
        let pumps = self.pumps.seal_and_join().await;
        let deliveries = self.deliveries.seal_and_join().await;
        if let Err(error) = stops.and(tasks).and(pumps).and(deliveries) {
            *self
                .shutdown_error
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
        }
    }

    /// Bound the settled history: drop sessions settled longer than
    /// [`SHELL_SESSION_TTL_MS`] ago or beyond the newest [`EXITED_LIMIT`], then
    /// release the output of the oldest survivors until all settled captures
    /// fit [`SETTLED_OUTPUT_BUDGET`]. Running sessions are never touched:
    /// `Exit.completed` only exists once a session settles. Each removal
    /// announces `deleted`, as upstream eviction goes through
    /// `removeCommand`. Runs after every settle and on spawn.
    ///
    /// A settled session whose process group is still alive is pinned: it is
    /// never removed and takes no [`EXITED_LIMIT`] slot, so it keeps the one
    /// handle that lets `DELETE`, session deletion and shutdown kill its
    /// `cmd &` children (plan D1). Its output still counts toward the budget
    /// and is released like any other.
    ///
    /// Lock order: table, then each session's `exit`/`output`; the plan is a
    /// pure decision over a snapshot of completion times and retained bytes.
    pub fn evict_idle(&self, now_ms: i64) {
        let removed: Vec<Arc<ShellSession>> = {
            let Ok(mut table) = self.sessions.lock() else {
                return;
            };
            let mut settled: Vec<Arc<ShellSession>> = Vec::new();
            for session in table.values() {
                if session.exit().completed.is_some() {
                    settled.push(Arc::clone(session));
                }
            }
            // Ids ascend with creation, so equal completion times break ties
            // by spawn order instead of by map iteration order.
            settled.sort_unstable_by(|left, right| left.id.cmp(&right.id));
            let captures: Vec<SettledCapture> = settled
                .iter()
                .map(|session| SettledCapture {
                    completed: session.exit().completed.unwrap_or_default(),
                    retained: session.retained(),
                    pinned: session.group_alive() || session.owes_wakeup(),
                })
                .collect();
            let plan = eviction_plan(&captures, now_ms);
            // Released under the table lock, so a concurrent sweep never plans
            // from a snapshot that predates these releases.
            for &index in &plan.release {
                settled[index].release_output();
            }
            // Pinned (live-group) sessions are never in `remove` (plan D1).
            plan.remove
                .iter()
                .filter_map(|&index| table.remove(&settled[index].id))
                .collect()
        };
        for session in removed {
            session.notify(Notice::Deleted {
                id: session.id.clone(),
            });
        }
    }

    /// Subscribe to lifecycle changes instead of polling.
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<ShellEvent>> {
        self.get(id).map(|session| session.events.subscribe())
    }
}

/// Run `stop(false)` on every session at once and wait for all of them, so
/// the total wait is one [`STOP_GRACE`] however many there are. Each stop is
/// a detached task: if the caller is dropped mid-grace (a client hangs up on
/// `DELETE`), the stops still escalate to SIGKILL. Each task owns one `Arc`
/// clone; no session data is copied.
async fn stop_concurrently(sessions: &[Arc<ShellSession>]) {
    let mut stops = Vec::new();
    for session in sessions {
        let Some(table) = session.table.upgrade() else {
            continue;
        };
        let session = Arc::clone(session);
        let (reply, result) = tokio::sync::oneshot::channel();
        match table.stops.spawn(async move {
            session.stop_owned(false).await;
            let _ = reply.send(());
        }) {
            Ok(()) => stops.push(result),
            Err(error) => tracing::error!(%error, "shell stop registration failed"),
        }
    }
    for stop in stops {
        if let Err(error) = stop.await {
            tracing::error!(%error, "shell stop task failed");
        }
    }
}

/// Kill the session's process group when its deadline passes. Holds only a
/// `Weak` handle: a dropped session needs no timer.
async fn deadline_timer(session: std::sync::Weak<ShellSession>, after: std::time::Duration) {
    tokio::time::sleep(after).await;
    let Some(session) = session.upgrade() else {
        return;
    };
    session.timed_out.store(true, Ordering::Release);
    session.signal_group(Signal::Kill);
}

/// Owns the terminal publication for one `drive` run. The normal path sets
/// `exit` once the output is drained. If `drive` unwinds before that, `Drop`
/// settles the session with a fallback state, so waiters and `stop` cannot
/// hang on a driver that is gone.
struct TerminalGuard {
    session: Arc<ShellSession>,
    exit: Option<Exit>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let exit = self.exit.take().unwrap_or_else(|| {
            if let Ok(mut buffer) = self.session.output.lock() {
                buffer.append(b"shell driver stopped before the command settled\n");
            }
            Exit {
                status: Some(Status::Exited),
                code: None,
                signal: None,
                completed: Some(ox_schema::id::now_millis()),
            }
        });
        self.session.publish_terminal(exit);
        self.session.changed.notify_waiters();
    }
}

/// Run the session's command to completion: stream the merged output into the
/// capture buffer as it arrives, wait for the shell to exit, let the pipe
/// drain for [`DRAIN_GRACE`], then record the terminal state once and wake
/// every waiter.
///
/// The reader task outlives the settle when a background child still holds
/// the pipe: it keeps appending (bounded by the buffer) until EOF, then gives
/// back the slack the drain policy left behind.
async fn drive(session: Arc<ShellSession>, spawned: Spawned) {
    // Created first so any unwind below still settles the session.
    let mut guard = TerminalGuard {
        session: Arc::clone(&session),
        exit: None,
    };
    let Spawned {
        mut child,
        mut reader,
        ..
    } = spawned;
    let Some(table) = session.table.upgrade() else {
        return;
    };
    let (pump_done, mut pump) = tokio::sync::oneshot::channel();
    let registration = table.pumps.spawn({
        let session = Arc::clone(&session);
        async move {
            let result = process::pump(&mut reader, |chunk| {
                if let Ok(mut buffer) = session.output.lock() {
                    buffer.append(chunk);
                }
                session.changed.notify_waiters();
            })
            .await;
            if let Ok(mut buffer) = session.output.lock() {
                buffer.shrink_to_retained();
            }
            let _ = pump_done.send(result);
        }
    });
    drop(table);
    if let Err(error) = registration {
        tracing::error!(%error, "shell pump registration failed");
        return;
    }

    let waited = child.wait().await;
    // The shell is gone: a deadline firing now would kill background
    // children that are meant to outlive the call.
    session.clear_deadline();
    // Whatever is already in the pipe arrives within the grace; a background
    // child that keeps the pipe open remains owned by the domain pump supervisor.
    let _ = tokio::time::timeout(DRAIN_GRACE, &mut pump).await;

    // Recorded before the terminal state is published: from then on `stop`
    // treats the session as settled and consults this flag.
    session
        .group_may_live
        .store(process::group_alive(session.pgid), Ordering::Release);

    let now = ox_schema::id::now_millis();
    guard.exit = Some(match waited {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            let signal = status.signal();
            Exit {
                status: Some(if session.stop_requested.load(Ordering::Acquire) {
                    Status::Killed
                } else if session.timed_out.load(Ordering::Acquire) && signal.is_some() {
                    Status::Timeout
                } else {
                    Status::Exited
                }),
                code: status.code(),
                signal,
                completed: Some(now),
            }
        }
        Err(error) => {
            // Waiting failed: surface it in the capture so the model sees
            // something actionable.
            if let Ok(mut buffer) = session.output.lock() {
                buffer.append(format!("{error}\n").as_bytes());
            }
            Exit {
                status: Some(Status::Exited),
                code: None,
                signal: None,
                completed: Some(now),
            }
        }
    });
    // Publishes the terminal state, runs the completion callback, and wakes
    // every waiter, after the drained output is final.
    drop(guard);
    // Only now is this session part of the settled history; an unwind skips
    // this and the next spawn sweeps instead.
    if let Some(table) = session.table.upgrade() {
        table.evict_idle(ox_schema::id::now_millis());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn request(owner: Option<Owner>, command: &str) -> SpawnRequest {
        SpawnRequest {
            command: command.to_owned(),
            cwd: PathBuf::from("."),
            timeout_ms: None,
            completion: None,
            foreground: false,
            owner,
            metadata: Record::new(),
        }
    }

    #[tokio::test]
    async fn session_environment_is_applied_to_owned_shells() {
        let sessions = Arc::new(ShellSessions::new());
        sessions.set_environment(
            "session-1",
            HashMap::from([(String::from("OX_SESSION_ENV"), String::from("from-session"))]),
        );

        let shell = sessions
            .spawn(request(
                Some(Owner {
                    session_id: String::from("session-1"),
                    directory: String::from("."),
                }),
                "printf '%s' \"${OX_SESSION_ENV-absent}\"",
            ))
            .expect("shell should spawn");
        shell.wait_settled(Duration::from_secs(2)).await;

        assert_eq!(shell.poll(0, 1024).0.output, "from-session");
    }

    #[tokio::test]
    async fn owned_shells_receive_terminal_environment_variables() {
        let sessions = Arc::new(ShellSessions::new());
        let shell = sessions
            .spawn(request(
                Some(Owner {
                    session_id: String::from("session-1"),
                    directory: String::from("."),
                }),
                "printf '%s|%s' \"${TERM-absent}\" \"${OPENCODE_TERMINAL-absent}\"",
            ))
            .expect("owned shell should spawn");
        shell.wait_settled(Duration::from_secs(2)).await;

        assert_eq!(
            shell.poll(0, 1024).0.output,
            "xterm-256color|1",
            "owned shells must receive the upstream terminal environment"
        );
    }

    #[tokio::test]
    async fn session_environment_replaces_and_does_not_affect_unowned_shells() {
        let sessions = Arc::new(ShellSessions::new());
        sessions.set_environment(
            "session-1",
            HashMap::from([(String::from("OX_SESSION_ENV"), String::from("old"))]),
        );
        sessions.set_environment(
            "session-1",
            HashMap::from([(String::from("OX_SESSION_ENV"), String::from("new"))]),
        );

        let owned = sessions
            .spawn(request(
                Some(Owner {
                    session_id: String::from("session-1"),
                    directory: String::from("."),
                }),
                "printf '%s' \"${OX_SESSION_ENV-absent}\"",
            ))
            .expect("owned shell should spawn");
        let unowned = sessions
            .spawn(request(None, "printf '%s' \"${OX_SESSION_ENV-absent}\""))
            .expect("shell should spawn");
        owned.wait_settled(Duration::from_secs(2)).await;
        unowned.wait_settled(Duration::from_secs(2)).await;

        assert_eq!(owned.poll(0, 1024).0.output, "new");
        assert_eq!(unowned.poll(0, 1024).0.output, "absent");
    }

    #[tokio::test]
    async fn session_environment_cannot_restore_herdr_variables() {
        let sessions = Arc::new(ShellSessions::new());
        sessions.set_environment(
            "session-1",
            HashMap::from([(String::from("HERDR_FAKE"), String::from("must-remove"))]),
        );

        let shell = sessions
            .spawn(request(
                Some(Owner {
                    session_id: String::from("session-1"),
                    directory: String::from("."),
                }),
                "printf '%s' \"${HERDR_FAKE-absent}\"",
            ))
            .expect("shell should spawn");
        shell.wait_settled(Duration::from_secs(2)).await;

        assert_eq!(shell.poll(0, 1024).0.output, "absent");
    }

    #[tokio::test]
    async fn session_environment_replaces_inherited_process_environment() {
        let sessions = Arc::new(ShellSessions::new());
        sessions.set_environment(
            "session-1",
            HashMap::from([(String::from("OX_SESSION_ENV"), String::from("set"))]),
        );

        let shell = sessions
            .spawn(request(
                Some(Owner {
                    session_id: String::from("session-1"),
                    directory: String::from("."),
                }),
                "printf '%s' \"${CARGO_MANIFEST_DIR-absent}\"",
            ))
            .expect("shell should spawn");
        shell.wait_settled(Duration::from_secs(2)).await;

        assert_eq!(shell.poll(0, 1024).0.output, "absent");
    }

    #[tokio::test]
    async fn shell_without_session_environment_inherits_process_environment() {
        let sessions = Arc::new(ShellSessions::new());
        let expected = std::env::var("CARGO_MANIFEST_DIR")
            .expect("test process should have CARGO_MANIFEST_DIR");

        let shell = sessions
            .spawn(request(
                None,
                "printf '%s' \"${CARGO_MANIFEST_DIR-absent}\"",
            ))
            .expect("shell should spawn");
        shell.wait_settled(Duration::from_secs(2)).await;

        assert_eq!(shell.poll(0, 1024).0.output, expected);
    }
}
