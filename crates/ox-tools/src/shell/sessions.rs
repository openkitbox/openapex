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
use std::sync::Arc;
use std::sync::Mutex;

use pi_shell::cancel::CancelToken;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::broadcast;

use super::engine;

/// Newest bytes retained per session. Upstream keeps the whole capture in a
/// file; the tail is what a client renders and what the model needs.
pub const OUTPUT_CAPACITY: usize = 8 * 1024 * 1024;
/// How long a settled session stays listed before the next spawn reaps it.
/// A running session never ages out: there is no completion timestamp to
/// measure from, and its output is still being written.
pub const SHELL_SESSION_TTL_MS: i64 = 60 * 60 * 1000;
/// Default page size for one `output` read.
pub const DEFAULT_PAGE_BYTES: usize = 64 * 1024;

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
}

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

    /// Terminate the command. pi-shell's abort kills the whole process group
    /// of the in-flight run, so `force` no longer changes the mechanism (the
    /// old single-process `start_kill`/`kill` distinction is gone).
    pub async fn stop(&self, force: bool) {
        let _ = force;
        {
            let mut exit = self
                .exit
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if exit.is_settled() {
                return;
            }
            *exit = Exit {
                status: Some(Status::Killed),
                code: None,
                signal: None,
                completed: Some(ox_schema::id::now_millis()),
            };
        }
        let shell = self.shell.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |slot| slot.clone(),
        );
        if let Some(shell) = shell {
            shell.abort().await;
        }
        self.changed.notify_waiters();
        let _ = self.events.send(ShellEvent {
            kind: "shell.exited",
            id: self.id.clone(),
            status: Status::Killed,
            exit: None,
        });
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
            metadata: HashMap::new(),
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
}

/// The process-wide session table, cloned as an `Arc` by the tool and the API.
#[derive(Default)]
pub struct ShellSessions {
    sessions: Mutex<HashMap<String, Arc<ShellSession>>>,
}

impl ShellSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn one session and start capturing its output. The caller decides
    /// how long to wait before returning control to the model.
    pub fn spawn(&self, request: SpawnRequest) -> std::io::Result<Arc<ShellSession>> {
        let (events, _) = broadcast::channel(32);
        let shell = Arc::new(engine::shell());
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
        });

        tokio::spawn(drive(Arc::clone(&session), shell, request.timeout_ms));

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
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<ShellSession>> {
        self.sessions
            .lock()
            .ok()
            .and_then(|table| table.get(id).cloned())
    }

    /// Every session, oldest first.
    pub fn list(&self) -> Vec<ShellInfo> {
        let sessions: Vec<Arc<ShellSession>> = self
            .sessions
            .lock()
            .map(|table| table.values().cloned().collect())
            .unwrap_or_default();
        let mut infos: Vec<ShellInfo> = sessions.iter().map(|session| session.info()).collect();
        infos.sort_by_key(|info| info.time.started);
        infos
    }

    /// Remove a settled session. A running one is left alone: the caller
    /// stops it first.
    pub fn remove(&self, id: &str) -> bool {
        let Ok(mut table) = self.sessions.lock() else {
            return false;
        };
        match table.get(id) {
            Some(session) if !session.settled() => false,
            Some(_) => table.remove(id).is_some(),
            None => false,
        }
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
        table.retain(|_, session| match session.exit().completed {
            Some(completed) => now_ms.saturating_sub(completed) <= SHELL_SESSION_TTL_MS,
            None => true,
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

/// Run the session's command to completion: stream raw chunks into the
/// capture buffer as they arrive, then record the terminal state once and
/// wake every waiter.
///
/// Requires a multi-threaded runtime: pi-shell pumps the interpreter's pipe
/// output and drives the interpreter on the same runtime, so under
/// `current_thread` a pipe buffer's worth of output deadlocks the runtime.
/// Production is `#[tokio::main]` (multi-threaded); tests use the
/// `multi_thread` flavor for the same reason (as `tests/acp.rs` does).
async fn drive(session: Arc<ShellSession>, shell: Arc<pi_shell::Shell>, timeout_ms: Option<u64>) {
    // pi-shell decodes each stream's bytes and forwards them on this channel;
    // the forwarder is the only writer of the capture buffer. The channel is
    // unbounded on purpose: pi-shell emits with `send_async` from inside a
    // nested runtime, so a full bounded channel would await a consumer that
    // cannot be polled until the run returns (deadlock at 256 chunks).
    let (chunk_tx, chunk_rx) = flume::unbounded::<String>();
    let forwarder = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            while let Ok(chunk) = chunk_rx.recv_async().await {
                if let Ok(mut buffer) = session.output.lock() {
                    buffer.append(chunk.as_bytes());
                }
                session.changed.notify_waiters();
            }
        })
    };
    let options = pi_shell::ShellRunOptions {
        command: session.command.clone(),
        cwd: Some(session.cwd.clone()),
        ..Default::default()
    };
    // The deadline lives in pi-shell's cancel token: it maps to the run
    // result's `timed_out` flag, while an external `stop` lands as
    // `cancelled`.
    let timeout = timeout_ms.map(|ms| u32::try_from(ms).unwrap_or(u32::MAX));
    let result = shell
        .run(options, Some(chunk_tx), CancelToken::new(timeout))
        .await;
    // The sender dropped with the run: drain whatever is still queued before
    // settling so no tail output is lost.
    let _ = forwarder.await;

    // Decide the interpreter's fate *before* the terminal state is published, so
    // a waiter that observes the settle also observes the final ownership: a
    // settled run no longer needs its interpreter (the brush session core is
    // per-call state, the table keeps the finished entry for an hour, and every
    // tool call spawns one). A live background child is the one exception —
    // dropping the interpreter SIGKILLs it (pi-shell's kill-on-drop), which is
    // what `live_background_job_count` exists to tell the host. That count
    // covers child processes only; a background *builtin* (`sleep 5 &` runs
    // in-process) has no pid and is therefore not protected.
    if shell.live_background_job_count().await == 0
        && let Ok(mut slot) = session.shell.lock()
    {
        *slot = None;
    }

    let now = ox_schema::id::now_millis();
    let exit = match result {
        Ok(result) => {
            Exit {
                status: Some(if result.timed_out {
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
    };
    // A `stop` that landed first already published `Killed`; keep that
    // earlier terminal state rather than flipping it back.
    let settled = {
        let mut slot = session
            .exit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_settled() {
            false
        } else {
            *slot = exit;
            true
        }
    };
    session.changed.notify_waiters();
    if settled {
        let _ = session.events.send(ShellEvent {
            kind: "shell.exited",
            id: session.id.clone(),
            status: exit.status.unwrap_or(Status::Exited),
            exit: exit.code,
        });
    }
    // The forwarder is gone, so no one appends from here on; give back the
    // slack the drain policy left in a buffer that once crossed the capacity.
    if let Ok(mut buffer) = session.output.lock() {
        buffer.shrink_to_retained();
    }
}
