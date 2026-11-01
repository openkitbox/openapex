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
//! their teardown, and signals only ever go to the group this session spawned.

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
/// to fit [`SETTLED_OUTPUT_BUDGET`], oldest first.
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
fn plan_with(
    settled: &[SettledCapture],
    now_ms: i64,
    ttl_ms: i64,
    limit: usize,
    budget: usize,
) -> EvictionPlan {
    let mut plan = EvictionPlan::default();
    let mut survivors: Vec<usize> = Vec::with_capacity(settled.len());
    for (index, capture) in settled.iter().enumerate() {
        if now_ms.saturating_sub(capture.completed) > ttl_ms {
            plan.remove.push(index);
        } else {
            survivors.push(index);
        }
    }
    // Oldest first; equal completion times fall back to slice order.
    survivors.sort_by_key(|&index| (settled[index].completed, index));
    let excess = survivors.len().saturating_sub(limit);
    plan.remove.extend(survivors.drain(..excess));

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
    /// then may a settled `stop` signal `pgid`; `stop` clears it the first
    /// time it sees the group gone, so a reused pid is never signalled.
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
    gate: Mutex<Gate>,
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
///   delivers the completion (`Sent`).
/// - `Held`: a foreground run still inside its yield window. Settling here
///   means the tool returns the result inline, so nothing is delivered
///   (`Quiet`); being handed back first makes it `Armed`.
/// - `Quiet`: settled inline, never notifies. `Sent`: delivered, or there is
///   no sink to deliver to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Armed,
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
    /// The run settled first and is being returned inline; no completion
    /// will ever be delivered for it.
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
pub type CompletionCallback = Arc<dyn Fn(Completion) + Send + Sync + 'static>;

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
                    *gate = Gate::Sent;
                    true
                }
                Gate::Held => {
                    *gate = Gate::Quiet;
                    false
                }
                Gate::Quiet | Gate::Sent => false,
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
            Gate::Armed | Gate::Sent => HandBack::Background,
        }
    }

    /// Whether this session reports its completion through a sink. Such a
    /// session needs no polling: the model is notified when it settles.
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
        // A panicking sink must not unwind through `drive`: the session is
        // already settled, and the rest of finalization (waking waiters) still
        // has to run.
        if std::panic::catch_unwind(AssertUnwindSafe(move || callback(completion))).is_err() {
            tracing::error!(id = %self.id, "shell completion callback panicked");
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
    ///   is still alive.
    ///
    /// Residual risk: the group id is a pid, and the kernel may reuse it once
    /// every process of the group has exited. While any member lives the id
    /// cannot be reused, so a settled session signals its group only if
    /// `drive` saw the group alive at settle (`group_may_live`) and it is
    /// still alive now. The first settled `stop` that finds the group gone
    /// clears the flag and the session never signals again. This relies on
    /// someone calling `stop` while the group is gone but its id is not yet
    /// reused: a group that dies and is reassigned before any `stop` runs is
    /// indistinguishable from ours, so that narrow window remains.
    pub async fn stop(&self, force: bool) {
        if self.settled() {
            if self.group_may_live.load(Ordering::Acquire) {
                if process::group_alive(self.pgid) {
                    self.signal_group(Signal::Kill);
                } else {
                    self.group_may_live.store(false, Ordering::Release);
                }
            }
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

    /// Spawn one session and start capturing its output. The caller decides
    /// how long to wait before returning control to the model.
    pub fn spawn(self: &Arc<Self>, request: SpawnRequest) -> std::io::Result<Arc<ShellSession>> {
        let (events, _) = broadcast::channel(32);
        let spawned = process::spawn(&self.shell, &request.command, &request.cwd)?;
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
            gate: Mutex::new(match (request.completion.is_some(), request.foreground) {
                (false, _) => Gate::Sent,
                (true, true) => Gate::Held,
                (true, false) => Gate::Armed,
            }),
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
            let handle = tokio::spawn(deadline_timer(
                Arc::downgrade(&session),
                std::time::Duration::from_millis(timeout),
            ))
            .abort_handle();
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
        tokio::spawn(drive(Arc::clone(&session), spawned));
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<ShellSession>> {
        self.sessions
            .lock()
            .ok()
            .and_then(|table| table.get(id).cloned())
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
            let Ok(mut table) = self.sessions.lock() else {
                return false;
            };
            match table.get(id) {
                Some(session) if !session.settled() => return false,
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

    /// Bound the settled history: drop sessions settled longer than
    /// [`SHELL_SESSION_TTL_MS`] ago or beyond the newest [`EXITED_LIMIT`], then
    /// release the output of the oldest survivors until all settled captures
    /// fit [`SETTLED_OUTPUT_BUDGET`]. Running sessions are never touched:
    /// `Exit.completed` only exists once a session settles. Removal is silent
    /// (no `deleted` notice, plan D2). Runs after every settle and on spawn.
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
                })
                .collect();
            let plan = eviction_plan(&captures, now_ms);
            // Released under the table lock, so a concurrent sweep never plans
            // from a snapshot that predates these releases.
            for &index in &plan.release {
                settled[index].release_output();
            }
            plan.remove
                .iter()
                .filter_map(|&index| table.remove(&settled[index].id))
                .collect()
        };
        // A removed session may drop its last reference here, off the lock.
        drop(removed);
    }

    /// Subscribe to lifecycle changes instead of polling.
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<ShellEvent>> {
        self.get(id).map(|session| session.events.subscribe())
    }
}

/// The process-wide session table. The tool and the `/api/shell` handlers
/// share one instance, so a session the model started is the session a client
/// can list and page.
pub fn shared() -> Arc<ShellSessions> {
    static TABLE: std::sync::OnceLock<Arc<ShellSessions>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| Arc::new(ShellSessions::new())).clone()
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
    let mut pump = tokio::spawn({
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
            result
        }
    });

    let waited = child.wait().await;
    // The shell is gone: a deadline firing now would kill background
    // children that are meant to outlive the call.
    session.clear_deadline();
    // Whatever is already in the pipe arrives within the grace; a background
    // child that keeps the pipe open is left to the detached pump.
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

    fn capture(completed: i64, retained: usize) -> SettledCapture {
        SettledCapture {
            completed,
            retained,
        }
    }

    #[test]
    fn release_keeps_size_and_reports_truncation() {
        let mut buffer = OutputBuffer::with_capacity(16);
        buffer.append(b"hello world");
        buffer.release();
        assert_eq!(
            buffer.size(),
            11,
            "release must keep total, got {}",
            buffer.size()
        );
        assert_eq!(
            buffer.retained(),
            0,
            "release must free bytes, got {}",
            buffer.retained()
        );
        let old = buffer.page(0, 8);
        assert!(
            old.output.is_empty() && old.truncated && old.cursor == 11,
            "old cursor must read empty and truncated, got {old:?}"
        );
        let eof = buffer.page(11, 8);
        assert!(
            eof.output.is_empty() && !eof.truncated && eof.cursor == 11,
            "tail cursor must read EOF, got {eof:?}"
        );
    }

    #[test]
    fn ttl_removes_only_expired() {
        let now = SHELL_SESSION_TTL_MS + 1_000;
        let settled = [capture(999, 1), capture(1_000, 1), capture(now, 1)];
        let plan = eviction_plan(&settled, now);
        assert_eq!(
            plan.remove,
            vec![0],
            "only entries past the TTL go, got {plan:?}"
        );
        assert!(plan.release.is_empty(), "nothing over budget, got {plan:?}");
    }

    #[test]
    fn count_cap_keeps_newest() {
        // Newest first in the slice, so index order and age order differ.
        let settled: Vec<SettledCapture> = (0..EXITED_LIMIT + 3)
            .map(|index| capture(100 - index as i64, 0))
            .collect();
        let plan = eviction_plan(&settled, 100);
        let oldest: Vec<usize> = (EXITED_LIMIT..EXITED_LIMIT + 3).collect();
        let mut removed = plan.remove.clone();
        removed.sort_unstable();
        assert_eq!(removed, oldest, "the three oldest must go, got {plan:?}");
        assert!(plan.release.is_empty(), "no bytes retained, got {plan:?}");
    }

    #[test]
    fn budget_releases_oldest_survivors_first() {
        // Index 0 expires, index 1 is the oldest beyond the count limit of 3;
        // neither may be released. Survivors 2..=4 hold 30 bytes against 15.
        let settled = [
            capture(0, 100),
            capture(10, 100),
            capture(20, 10),
            capture(40, 10),
            capture(30, 10),
        ];
        let plan = plan_with(&settled, 50, 45, 3, 15);
        assert_eq!(
            plan.remove,
            vec![0, 1],
            "TTL then count removal, got {plan:?}"
        );
        assert_eq!(
            plan.release,
            vec![2, 4],
            "oldest survivors released, got {plan:?}"
        );
    }

    #[test]
    fn under_limits_is_noop() {
        let settled = [capture(10, 1024), capture(20, 2048)];
        let plan = eviction_plan(&settled, 30);
        assert!(
            plan.remove.is_empty() && plan.release.is_empty(),
            "nothing over limit, got {plan:?}"
        );
    }

    fn spawn_quick(table: &Arc<ShellSessions>, command: &str) -> Arc<ShellSession> {
        table
            .spawn(SpawnRequest {
                command: command.to_owned(),
                cwd: std::env::temp_dir(),
                timeout_ms: None,
                completion: None,
                foreground: false,
                owner: None,
                metadata: Record::new(),
            })
            .expect("spawn shell")
    }

    async fn settle(session: &ShellSession) {
        assert!(
            session
                .wait_settled(std::time::Duration::from_secs(5))
                .await,
            "session {} did not settle",
            session.id
        );
    }

    fn settled_count(table: &ShellSessions) -> usize {
        table
            .sessions
            .lock()
            .map(|table| table.values().filter(|session| session.settled()).count())
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn settle_bounds_the_table_by_count() {
        let table = Arc::new(ShellSessions::new());
        let mut ids = Vec::new();
        for _ in 0..EXITED_LIMIT + 2 {
            let session = spawn_quick(&table, "true");
            settle(&session).await;
            ids.push(session.id.clone());
        }
        // The sweep runs right after the settle publication, so poll for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while settled_count(&table) > EXITED_LIMIT && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        let count = settled_count(&table);
        assert!(
            count <= EXITED_LIMIT,
            "settled sessions over the limit: {count}"
        );
        for id in &ids[..2] {
            assert!(
                table.get(id).is_none(),
                "oldest session {id} must be evicted"
            );
        }
        let newest = &ids[ids.len() - 1];
        assert!(
            table.get(newest).is_some(),
            "newest session {newest} must stay"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn released_output_reads_as_truncated() {
        let table = Arc::new(ShellSessions::new());
        let session = spawn_quick(&table, "printf hello");
        settle(&session).await;
        session.release_output();
        let (page, _) = session.poll(0, 64);
        assert!(
            page.output.is_empty() && page.truncated && page.size == 5,
            "released capture must read empty and truncated, got {page:?}"
        );
        assert!(
            table.get(&session.id).is_some(),
            "a released session stays readable"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn running_session_is_never_evicted() {
        let table = Arc::new(ShellSessions::new());
        let running = spawn_quick(&table, "sleep 5");
        for _ in 0..EXITED_LIMIT + 1 {
            let session = spawn_quick(&table, "true");
            settle(&session).await;
        }
        table.evict_idle(ox_schema::id::now_millis());
        assert!(
            table.get(&running.id).is_some(),
            "running session {} was evicted",
            running.id
        );
        running.stop(true).await;
    }
}
