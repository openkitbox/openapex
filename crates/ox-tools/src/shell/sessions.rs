//! Live shell sessions: the process-wide table behind the `shell` tool's
//! `run` / `interact` / `stop` actions and the `/api/shell` endpoints.
//!
//! One [`ShellSession`] owns an embedded brush session (pi-shell), the merged
//! stdout/stderr tail in [`OutputBuffer`], and the run's terminal state. The
//! table is shared by `Arc` between the tool (which spawns and stops) and the
//! API (which lists sessions and pages their output), so neither side owns
//! the other.
//!
//! Capture is bounded: the buffer keeps the newest [`OUTPUT_CAPACITY`] bytes
//! and remembers how many it dropped, so a long-running server cannot grow the
//! process without limit and a reader can still tell that it missed output.
//! Output pages are built from one slice, never by copying the whole buffer.
//!
//! A settled session releases its interpreter (`shell` becomes `None`), so a
//! finished entry costs its capture instead of a brush session; a live
//! background child keeps it, because dropping the interpreter would kill the
//! child.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;

use std::sync::atomic::{AtomicBool, Ordering};

use pi_shell::cancel::{AbortReason, AbortToken, CancelToken};
use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use super::engine;

/// Newest bytes retained per session. Upstream keeps the whole capture in a
/// file; the tail is what a client renders and what the model needs.
pub const OUTPUT_CAPACITY: usize = 8 * 1024 * 1024;
/// Largest piece appended to the capture in one step. pi-shell's decoder is
/// assumed to emit at most one 64KiB pipe read as a decoded chunk; malformed
/// UTF-8 can expand each byte to the three-byte replacement character, so a
/// decoded chunk is at most three times the read size. Each chunk is split
/// into pieces of this size before it reaches the capture buffer. This is an
/// assumed producer contract, not an enforced limit on pi-shell's emit size:
/// if pi-shell changes its read buffer or decoder, revisit this constant.
const OUTPUT_QUEUE_CHUNK_BYTES: usize = 3 * 64 * 1024;
/// Chunks buffered between pi-shell and the capture writer (count, not bytes).
const OUTPUT_QUEUE_CAPACITY: usize = 1;
/// How long a settled session stays listed before the next spawn reaps it.
/// A running session never ages out: there is no completion timestamp to
/// measure from, and its output is still being written.
pub const SHELL_SESSION_TTL_MS: i64 = 60 * 60 * 1000;
/// Default page size for one `output` read.
pub const DEFAULT_PAGE_BYTES: usize = 64 * 1024;
/// How often a release watcher asks the job table whether a background child
/// is still alive. pi-shell reports child exit only through that poll.
const IDLE_RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Upstream `Shell.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Exited,
    Timeout,
    Killed,
}

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
    pub fn size(&self) -> u64 {
        self.total
    }

    /// The first retained byte's absolute offset.
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
    pub fn recent_text(&self, limit: usize) -> String {
        let start = self.bytes.len().saturating_sub(limit);
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }

    /// The whole retained tail, for the model-facing tool result.
    pub fn tail_text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Bytes currently retained.
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
    /// The embedded brush session this command runs in. `None` once the run
    /// settled and nothing is left running in it: an interpreter is shell
    /// state (variables, functions, job table) and the table keeps a finished
    /// session for an hour, so pinning one per tool call is the table's
    /// largest resident cost. Dropping it SIGKILLs its background children
    /// (pi-shell's kill-on-drop), which is why a live `&` child keeps it.
    shell: Mutex<Option<Arc<pi_shell::Shell>>>,
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
    abort: AbortToken,
    /// The session deadline: a timer that aborts the run with
    /// [`AbortReason::Timeout`]. ox owns it (instead of pi-shell's
    /// `CancelToken` deadline, which is fixed at construction) so a foreground
    /// run handed back to the background can drop it, as upstream does
    /// (`shell.timeout(id, 0)`).
    deadline: Mutex<Option<AbortHandle>>,
    /// The calling session, when the run belongs to one: its id becomes
    /// `ShellInfo.metadata.sessionID` and its directory stamps lifecycle
    /// notices. A run without an owner has no location, so it is never
    /// announced.
    owner: Option<Owner>,
    /// The table's observer as of spawn. Cloned once per session, so a
    /// session reports through the sink that was installed when it started.
    observer: Option<Observer>,
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

    /// Whether this session still owns the brush interpreter it ran in.
    /// `false` once the run settled and no background child is left running.
    pub fn retains_interpreter(&self) -> bool {
        self.shell.lock().map_or_else(
            |poisoned| poisoned.into_inner().is_some(),
            |slot| slot.is_some(),
        )
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
        // already settled, and the rest of finalization (waiters, interpreter
        // release) still has to run.
        if std::panic::catch_unwind(AssertUnwindSafe(move || callback(completion))).is_err() {
            tracing::error!(id = %self.id, "shell completion callback panicked");
        }
    }

    /// Drop the interpreter. Dropping it SIGKILLs any background child, so
    /// callers must only reach here when none is live.
    fn release_interpreter(&self) {
        if let Ok(mut slot) = self.shell.lock() {
            *slot = None;
        }
        self.changed.notify_waiters();
    }

    /// Terminate the command. pi-shell's abort kills the whole process group
    /// of the in-flight run, so `force` no longer changes the mechanism (the
    /// old single-process `start_kill`/`kill` distinction is gone).
    pub async fn stop(&self, force: bool) {
        let _ = force;
        if self.settled() {
            // A command may have returned while its shell retained a live `&`
            // child. There is no in-flight run left for `abort()` to cancel;
            // dropping the interpreter is pi-shell's kill-on-drop boundary.
            self.release_interpreter();
            self.wait_announced().await;
            return;
        }
        self.stop_requested.store(true, Ordering::Release);
        self.abort.abort(AbortReason::Signal);
        let shell = self.shell.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |slot| slot.clone(),
        );
        if let Some(shell) = shell {
            shell.abort().await;
        }
        // `drive` owns terminal publication. Waiting here is intentional: the
        // abort must finish the run and drain the chunk forwarder before stop
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
            shell: "bash".to_owned(),
            // Capture lives in memory; the URL names the paging endpoint.
            file: format!("ox-shell://{}", self.id),
            pid: self.interpreter_pid(),
            exit: exit.code,
            signal: exit.signal.map(|signal| signal.to_string()),
            metadata: self
                .owner
                .iter()
                .map(|owner| ("sessionID".to_owned(), serde_json::json!(owner.session_id)))
                .collect(),
            time: Time {
                started: self.started,
                completed: exit.completed,
            },
        }
    }

    /// The live pid of the run, when this session still owns an interpreter.
    /// `None` after release: the process is already gone.
    fn interpreter_pid(&self) -> Option<u32> {
        let slot = self.shell.lock().ok()?;
        let shell = slot.as_ref()?;
        shell
            .pids()
            .first()
            .and_then(|pid| u32::try_from(*pid).ok())
    }
}

/// Upstream `Shell.Info`.
#[derive(Debug, Clone, Serialize)]
pub struct ShellInfo {
    pub id: String,
    pub status: Status,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub time: Time,
}

#[derive(Debug, Clone, Serialize)]
pub struct Time {
    pub started: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
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
}

/// The process-wide session table, cloned as an `Arc` by the tool and the API.
#[derive(Default)]
pub struct ShellSessions {
    sessions: Mutex<HashMap<String, Arc<ShellSession>>>,
    /// Where lifecycle notices go. The composition root installs the event
    /// bridge; the table itself knows nothing about the bus.
    observer: Mutex<Option<Observer>>,
}

impl ShellSessions {
    pub fn new() -> Self {
        Self::default()
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
    pub fn spawn(&self, request: SpawnRequest) -> std::io::Result<Arc<ShellSession>> {
        let (events, _) = broadcast::channel(32);
        let shell = Arc::new(engine::shell());
        let timeout_ms = request.timeout_ms;
        // No deadline inside the token: the session owns it (see
        // `ShellSession::deadline`).
        let mut cancel = CancelToken::new(None);
        let abort = cancel.emplace_abort_token();
        let deadline = timeout_ms.filter(|timeout| *timeout > 0).map(|timeout| {
            let abort = abort.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(timeout)).await;
                abort.abort(AbortReason::Timeout);
            })
            .abort_handle()
        });
        let session = Arc::new(ShellSession {
            id: ox_schema::id::id("sh"),
            command: request.command,
            cwd: request.cwd.to_string_lossy().into_owned(),
            started: ox_schema::id::now_millis(),
            output: Mutex::new(OutputBuffer::default()),
            exit: Mutex::new(Exit::default()),
            shell: Mutex::new(Some(Arc::clone(&shell))),
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
            abort,
            deadline: Mutex::new(deadline),
            owner: request.owner,
            observer: self
                .observer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        });

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
        tokio::spawn(drive(Arc::clone(&session), shell, cancel));
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

    /// Reap settled sessions whose completion is older than
    /// [`SHELL_SESSION_TTL_MS`]. Running sessions never age out: `Exit.completed`
    /// only exists once a session settles. Called from [`Self::spawn`] so the
    /// table is bounded by live work plus an hour of recent history instead of
    /// the process lifetime.
    pub fn evict_idle(&self, now_ms: i64) {
        let Ok(mut table) = self.sessions.lock() else {
            return;
        };
        table.retain(|_, session| {
            // `Exit` can be terminal before a background child is done: the
            // foreground command has settled, but dropping the interpreter
            // would still kill that child. Eviction must not be another
            // kill-on-drop path; `stop` is the explicit owner of that choice.
            match session.exit().completed {
                None => true,
                Some(completed) => {
                    // A retained interpreter owns a live background child;
                    // keep it regardless of age so eviction cannot trigger
                    // pi-shell's kill-on-drop behavior.
                    session.retains_interpreter()
                        || now_ms.saturating_sub(completed) <= SHELL_SESSION_TTL_MS
                }
            }
        });
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

/// Append one decoded chunk to the capture. The chunk is split into pieces of
/// at most [`OUTPUT_QUEUE_CHUNK_BYTES`] bytes and each piece is appended on
/// its own; the buffer still drops its oldest bytes once over capacity.
fn append_output_chunk(session: &ShellSession, chunk: String) {
    if let Ok(mut buffer) = session.output.lock() {
        for bytes in chunk.as_bytes().chunks(OUTPUT_QUEUE_CHUNK_BYTES) {
            buffer.append(bytes);
        }
    }
}

/// Owns the terminal publication for one `drive` run. The normal path sets
/// `exit` once the interpreter's ownership is decided. If `drive` unwinds
/// before that (a panic in `shell.run`), `Drop` settles the session with a
/// fallback state, so waiters and `stop` cannot hang on a driver that is gone.
struct TerminalGuard {
    session: Arc<ShellSession>,
    exit: Option<Exit>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let exit = if let Some(exit) = self.exit.take() {
            exit
        } else {
            if let Ok(mut buffer) = self.session.output.lock() {
                buffer.append(b"shell driver stopped before the command settled\n");
            }
            // The run is gone, so nothing is left to keep the interpreter for.
            self.session.release_interpreter();
            Exit {
                status: Some(Status::Exited),
                code: None,
                signal: None,
                completed: Some(ox_schema::id::now_millis()),
            }
        };
        self.session.publish_terminal(exit);
        self.session.changed.notify_waiters();
    }
}

/// Release the interpreter once its last background child exits. pi-shell
/// reports child exit only through its job-table poll, so this polls the
/// count at a short interval. It holds only `Weak` handles: `stop` and the
/// table still own teardown, and the watcher ends as soon as either side is
/// gone.
fn watch_background_release(session: &Arc<ShellSession>, shell: std::sync::Weak<pi_shell::Shell>) {
    let session = Arc::downgrade(session);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(IDLE_RELEASE_POLL).await;
            let (Some(session), Some(shell)) = (session.upgrade(), shell.upgrade()) else {
                return;
            };
            if shell.live_background_job_count().await == 0 {
                session.release_interpreter();
                return;
            }
        }
    });
}

/// Run the session's command to completion: stream raw chunks into the
/// capture buffer as they arrive, then record the terminal state once and
/// wake every waiter.
///
/// Requires a multi-threaded runtime: pi-shell pumps the interpreter's pipe
/// output and drives the interpreter on the same runtime, so under
/// `current_thread` a pipe buffer's worth of output deadlocks the runtime.
/// Production is `#[tokio::main]` (multi-threaded); tests use the
/// `multi_thread` flavor for the same reason (as `tests/acp.rs` does).
async fn drive(session: Arc<ShellSession>, shell: Arc<pi_shell::Shell>, cancel: CancelToken) {
    // Created first so any unwind below still settles the session.
    let mut guard = TerminalGuard {
        session: Arc::clone(&session),
        exit: None,
    };
    // Keep the queue bounded, but consume it on a blocking thread: pi-shell's
    // producer can apply backpressure without waiting for a Tokio task that is
    // itself needed to drive the nested shell runtime.
    let (chunk_tx, chunk_rx) = flume::bounded::<String>(OUTPUT_QUEUE_CAPACITY);
    let forwarding_done = Arc::new(AtomicBool::new(false));
    let forwarder = std::thread::spawn({
        let session = Arc::clone(&session);
        let forwarding_done = Arc::clone(&forwarding_done);
        move || loop {
            match chunk_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(chunk) => {
                    append_output_chunk(&session, chunk);
                    session.changed.notify_waiters();
                }
                Err(flume::RecvTimeoutError::Timeout)
                    if forwarding_done.load(Ordering::Acquire) =>
                {
                    while let Ok(chunk) = chunk_rx.try_recv() {
                        append_output_chunk(&session, chunk);
                    }
                    break;
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    let options = pi_shell::ShellRunOptions {
        command: session.command.clone(),
        cwd: Some(session.cwd.clone()),
        ..Default::default()
    };
    // The deadline is an `AbortReason::Timeout` abort fired by the session's
    // timer: it maps to the run result's `timed_out` flag, while an external
    // `stop` lands as `cancelled`.
    let result = shell.run(options, Some(chunk_tx), cancel).await;
    forwarding_done.store(true, Ordering::Release);
    // The sender dropped with the run: drain whatever is still queued before
    // settling so no tail output is lost.
    tokio::task::spawn_blocking(move || {
        forwarder
            .join()
            .expect("shell output forwarder thread must not panic");
    })
    .await
    .expect("shell output forwarder task must not panic");

    // Decide the interpreter's fate *before* the terminal state is published, so
    // a waiter that observes the settle also observes the final ownership: a
    // settled run no longer needs its interpreter (the brush session core is
    // per-call state, the table keeps the finished entry for an hour, and every
    // tool call spawns one). A live background child is the one exception —
    // dropping the interpreter SIGKILLs it (pi-shell's kill-on-drop), which is
    // what `live_background_job_count` exists to tell the host. That count
    // covers child processes only; a background *builtin* (`sleep 5 &` runs
    // in-process) has no pid and is therefore not protected. A retained
    // interpreter is released by a watcher once its child exits.
    if shell.live_background_job_count().await == 0 {
        session.release_interpreter();
    } else {
        watch_background_release(&session, Arc::downgrade(&shell));
    }

    let now = ox_schema::id::now_millis();
    guard.exit = Some(match result {
        Ok(result) => {
            Exit {
                status: Some(if session.stop_requested.load(Ordering::Acquire) {
                    Status::Killed
                } else if result.timed_out {
                    Status::Timeout
                } else if result.cancelled {
                    Status::Killed
                } else {
                    Status::Exited
                }),
                code: result.exit_code,
                // pi-shell reports neither the signal nor the stop reason as
                // a signal; the session status carries it instead.
                signal: None,
                completed: Some(now),
            }
        }
        Err(error) => {
            // Engine-level failure (spawn, poisoned session): surface it in
            // the capture so the model sees something actionable.
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
    // The forwarder is gone, so no one appends from here on; give back the
    // slack the drain policy left in a buffer that once crossed the capacity.
    if let Ok(mut buffer) = session.output.lock() {
        buffer.shrink_to_retained();
    }
    // `guard` drops here: it publishes the terminal state, runs the completion
    // callback, and wakes every waiter, after output and interpreter ownership
    // are final.
}
