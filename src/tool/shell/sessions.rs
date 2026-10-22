//! Live shell sessions: the process-wide table behind the `shell` tool's
//! `run` / `interact` / `stop` actions and the `/api/shell` endpoints.
//!
//! One [`ShellSession`] owns a spawned child, the merged stdout/stderr tail in
//! [`OutputBuffer`], and the child's stdin. The table is shared by `Arc`
//! between the tool (which spawns, writes, and kills) and the API (which lists
//! sessions and pages their output), so neither side owns the other.
//!
//! Capture is bounded: the buffer keeps the newest [`OUTPUT_CAPACITY`] bytes
//! and remembers how many it dropped, so a long-running server cannot grow the
//! process without limit and a reader can still tell that it missed output.
//! Output pages are built from one slice, never by copying the whole buffer.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;

/// Newest bytes retained per session. Upstream keeps the whole capture in a
/// file; the tail is what a client renders and what the model needs.
pub const OUTPUT_CAPACITY: usize = 8 * 1024 * 1024;
/// Default page size for one `output` read.
pub const DEFAULT_PAGE_BYTES: usize = 64 * 1024;
/// How often the watcher polls the child for exit. Capture is signalled, not
/// polled; only process exit needs a timer.
const EXIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

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

struct Live {
    child: Child,
    exit: Exit,
    timeout_ms: Option<u64>,
}

/// One running or finished shell command.
pub struct ShellSession {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub started: i64,
    output: Mutex<OutputBuffer>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    live: AsyncMutex<Option<Live>>,
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
        // `try_lock` keeps this usable from a sync context; the watcher only
        // holds the lock across an assignment.
        match self.live.try_lock() {
            Ok(guard) => guard.as_ref().map_or_else(Exit::default, |live| live.exit),
            Err(_) => Exit::default(),
        }
    }

    /// Whether the child has settled (exited, timed out, or been killed).
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

    /// Write to the child's stdin. An empty write with `append_newline`
    /// becomes a bare newline, which is what a terminal sends on Enter.
    pub async fn write(&self, bytes: &[u8], append_newline: bool) -> std::io::Result<()> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "shell stdin is closed",
            ));
        };
        stdin.write_all(bytes).await?;
        if append_newline && !bytes.ends_with(b"\n") {
            stdin.write_all(b"\n").await?;
        }
        stdin.flush().await
    }

    /// Terminate the child; `force` skips the grace period.
    pub async fn stop(&self, force: bool) {
        let mut guard = self.live.lock().await;
        let Some(live) = guard.as_mut() else {
            return;
        };
        if live.exit.is_settled() {
            return;
        }
        if force {
            let _ = live.child.kill().await;
        } else {
            let _ = live.child.start_kill();
        }
        let exit = Exit {
            status: Some(Status::Killed),
            code: live.exit.code,
            signal: live.exit.signal,
            completed: live
                .exit
                .completed
                .or_else(|| Some(crate::id::now_millis())),
        };
        live.exit = exit;
        drop(guard);
        self.changed.notify_waiters();
        let _ = self.events.send(ShellEvent {
            kind: "shell.exited",
            id: self.id.clone(),
            status: Status::Killed,
            exit: exit.code,
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
            shell: self.shell.clone(),
            // Capture lives in memory; the URL names the paging endpoint.
            file: format!("ox-shell://{}", self.id),
            pid: None,
            exit: exit.code,
            signal: exit.signal.map(|signal| signal.to_string()),
            metadata: HashMap::new(),
            time: Time {
                started: self.started,
                completed: exit.completed,
            },
        }
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
    pub shell: std::path::PathBuf,
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
        let mut command = tokio::process::Command::new(&request.shell);
        // The command runs through the user's shell so pipelines, globs and
        // quoting behave the way the model wrote them.
        command
            .arg("-c")
            .arg(&request.command)
            .current_dir(&request.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (events, _) = broadcast::channel(32);
        let session = Arc::new(ShellSession {
            id: crate::id::id("sh"),
            command: request.command,
            cwd: request.cwd.to_string_lossy().into_owned(),
            shell: request.shell.to_string_lossy().into_owned(),
            started: crate::id::now_millis(),
            output: Mutex::new(OutputBuffer::default()),
            stdin: AsyncMutex::new(stdin),
            live: AsyncMutex::new(Some(Live {
                child,
                exit: Exit::default(),
                timeout_ms: request.timeout_ms,
            })),
            changed: Notify::new(),
            events: events.clone(),
        });

        // Capture both streams into the one buffer the client pages through.
        if let Some(stdout) = stdout {
            let session = Arc::clone(&session);
            tokio::spawn(async move { pump(session, Box::new(stdout)).await });
        }
        if let Some(stderr) = stderr {
            let session = Arc::clone(&session);
            tokio::spawn(async move { pump(session, Box::new(stderr)).await });
        }
        // Watch for exit and for the wall-clock deadline.
        tokio::spawn(watch(Arc::clone(&session)));

        if let Ok(mut table) = self.sessions.lock() {
            table.insert(session.id.clone(), Arc::clone(&session));
        }
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

/// Copy one pipe into the session buffer until EOF.
async fn pump(
    session: Arc<ShellSession>,
    mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
) {
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Ok(mut buffer) = session.output.lock() {
                    buffer.append(&chunk[..read]);
                }
                session.changed.notify_waiters();
            }
        }
    }
    session.changed.notify_waiters();
}

/// Poll the child until it exits or its deadline passes, record the terminal
/// state once, and wake every waiter.
async fn watch(session: Arc<ShellSession>) {
    loop {
        let mut settled = None;
        {
            let mut guard = session.live.lock().await;
            let Some(live) = guard.as_mut() else {
                return;
            };
            if live.exit.is_settled() {
                return;
            }
            let now = crate::id::now_millis();
            match live.child.try_wait() {
                Ok(Some(status)) => {
                    live.exit = Exit {
                        status: Some(Status::Exited),
                        code: status.code(),
                        signal: exit_signal(&status),
                        completed: Some(now),
                    };
                    settled = Some(Status::Exited);
                }
                Ok(None) => {
                    let expired = live.timeout_ms.is_some_and(|timeout| {
                        timeout > 0 && now - session.started >= timeout as i64
                    });
                    if expired {
                        let _ = live.child.start_kill();
                        live.exit = Exit {
                            status: Some(Status::Timeout),
                            code: None,
                            signal: None,
                            completed: Some(now),
                        };
                        settled = Some(Status::Timeout);
                    }
                }
                Err(_) => {
                    live.exit = Exit {
                        status: Some(Status::Exited),
                        code: None,
                        signal: None,
                        completed: Some(now),
                    };
                    settled = Some(Status::Exited);
                }
            }
        }
        if let Some(status) = settled {
            // The child handle stays: `try_wait` already reaped it, and the
            // session keeps serving its captured output until the client
            // deletes it.
            session.changed.notify_waiters();
            let _ = session.events.send(ShellEvent {
                kind: "shell.exited",
                id: session.id.clone(),
                status,
                exit: session.exit().code,
            });
            return;
        }
        tokio::time::sleep(EXIT_POLL_INTERVAL).await;
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_drops_the_front_when_it_overflows() {
        let mut buffer = OutputBuffer::with_capacity(8);
        buffer.append(b"abcdefgh");
        assert_eq!(buffer.size(), 8);
        assert_eq!(buffer.dropped(), 0);
        buffer.append(b"ij");
        assert_eq!(buffer.size(), 10);
        assert_eq!(buffer.dropped(), 2);
        assert_eq!(buffer.tail_text(), "cdefghij");
    }

    #[test]
    fn page_reports_truncation_for_a_stale_cursor() {
        let mut buffer = OutputBuffer::with_capacity(4);
        buffer.append(b"abcdef");
        let page = buffer.page(0, 4);
        assert!(page.truncated);
        assert_eq!(page.cursor, 6);
        assert_eq!(page.size, 6);
        assert_eq!(page.output, "cdef");
    }

    #[test]
    fn page_reads_incrementally() {
        let mut buffer = OutputBuffer::with_capacity(64);
        buffer.append(b"hello world");
        let first = buffer.page(0, 5);
        assert_eq!(first.output, "hello");
        assert_eq!(first.cursor, 5);
        assert!(!first.truncated);
        let second = buffer.page(first.cursor, 5);
        assert_eq!(second.output, " worl");
        assert_eq!(second.cursor, 10);
        assert_eq!(second.size, 11);
    }

    #[test]
    fn a_chunk_larger_than_capacity_keeps_its_tail() {
        let mut buffer = OutputBuffer::with_capacity(4);
        buffer.append(b"0123456789");
        assert_eq!(buffer.size(), 10);
        assert_eq!(buffer.dropped(), 6);
        assert_eq!(buffer.tail_text(), "6789");
    }

    #[test]
    fn an_empty_buffer_reads_as_caught_up() {
        let buffer = OutputBuffer::with_capacity(8);
        let page = buffer.page(0, 16);
        assert_eq!(page.output, "");
        assert_eq!(page.cursor, 0);
        assert_eq!(page.size, 0);
        assert!(!page.truncated);
    }

    #[test]
    fn a_cursor_beyond_the_end_reads_as_caught_up() {
        // Production panic 2026-10-22: a model-invented cursor of 2000 on a
        // 1292-byte capture panicked slicing `bytes[2000..2000]`. Past-the-end
        // is a valid read (EOF semantics), never a panic.
        let mut buffer = OutputBuffer::with_capacity(2048);
        buffer.append(&[b'x'; 1292]);
        let page = buffer.page(2000, 64);
        assert_eq!(page.output, "");
        assert_eq!(page.cursor, 1292);
        assert_eq!(page.size, 1292);
        assert!(!page.truncated);
    }

    #[test]
    fn a_cursor_beyond_the_end_with_a_dropped_front_reads_as_caught_up() {
        let mut buffer = OutputBuffer::with_capacity(8);
        buffer.append(b"0123456789");
        assert_eq!(buffer.dropped(), 2);
        let page = buffer.page(99, 4);
        assert_eq!(page.output, "");
        assert_eq!(page.cursor, 10);
        assert_eq!(page.size, 10);
        assert!(!page.truncated);
    }
}
