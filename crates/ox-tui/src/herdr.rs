//! Reports the TUI's agent state to herdr so its agents panel and
//! `herdr agent list` show an `ox` row for the pane, and so a herdr restart can
//! resume the pane with `ox --session <id>`.
//!
//! Only the TUI reports: the `ox serve` process is shared by several TUIs and
//! cannot stand for a single pane. Outside a herdr pane (`HERDR_ENV`,
//! `HERDR_PANE_ID`, `HERDR_SOCKET_PATH` incomplete) nothing is created and the
//! TUI pays nothing.
//!
//! Protocol: one JSON request per line on herdr's unix socket,
//! `pane.report_agent` for each state change and `pane.release_agent` on exit.
//! herdr ignores reports whose `seq` is not above the last one from the same
//! source, so `seq` starts at the current Unix time in microseconds and only
//! grows. Socket failures are logged at debug level and never reach the user.

use std::env;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use serde_json::json;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time;

/// Name used for both herdr's `source` and `agent` fields.
const AGENT: &str = "ox";

/// Upper bound for one socket exchange.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);

/// Upper bound for [`Reporter::finish`]: a report already in flight plus the
/// release. Release is best-effort beyond that; herdr also clears the agent
/// once the pane is back at a shell prompt.
const FINISH_TIMEOUT: Duration = Duration::from_millis(1000);

/// herdr's total size limit for `resume_argv`, minus room for `ox`,
/// `--session` and the session id.
const RESUME_FLAGS_MAX_BYTES: usize = 7 * 1024;

/// Agent state as herdr's agents panel understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    /// herdr's wire name for the state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
        }
    }
}

/// Maps the focused session's activity to a herdr state. A pending question
/// form wins over a running turn because herdr should show "waiting for the
/// user" even while the turn that asked is still open.
#[must_use]
pub fn state_of(running: bool, has_form: bool) -> State {
    if has_form {
        State::Blocked
    } else if running {
        State::Working
    } else {
        State::Idle
    }
}

/// The herdr pane this process runs in, as advertised by herdr's environment.
struct Pane {
    id: String,
    socket: PathBuf,
}

impl Pane {
    fn from_env() -> Option<Pane> {
        if env::var("HERDR_ENV").ok()? != "1" {
            return None;
        }
        let id = env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty())?;
        let socket = env::var_os("HERDR_SOCKET_PATH").filter(|path| !path.is_empty())?;
        Some(Pane {
            id,
            socket: PathBuf::from(socket),
        })
    }
}

/// The latest value to report.
struct Report {
    state: State,
    session: Option<String>,
}

/// Handle to the background task that reports state to herdr.
///
/// The channel holds `None` until the first [`Reporter::update`], so that first
/// call always counts as a change and herdr learns the agent exists even when
/// it starts idle without a session. Later calls only wake the task when the
/// state or session differs; intermediate values that pile up while a send is
/// in flight are overwritten, and herdr only ever sees the latest one.
pub struct Reporter {
    tx: watch::Sender<Option<Report>>,
    task: JoinHandle<()>,
}

impl Reporter {
    /// Starts reporting when running inside a herdr pane; `None` otherwise.
    ///
    /// `resume_flags` are the TUI options replayed on resume, placed between
    /// `ox` and `--session <id>` in `resume_argv`. Must be called inside a
    /// tokio runtime. herdr rejects a whole report whose `resume_argv` breaks
    /// its rules, and resuming without the flags could reach the wrong
    /// server, so such flags turn resume off and only state is reported.
    #[must_use]
    pub fn from_env(resume_flags: Vec<String>) -> Option<Reporter> {
        let pane = Pane::from_env()?;
        let resume_flags = if resume_flags_valid(&resume_flags) {
            Some(resume_flags)
        } else {
            tracing::debug!("herdr: resume flags break herdr's rules, resume disabled");
            None
        };
        let (tx, rx) = watch::channel(None);
        let task = tokio::spawn(report_loop(pane, resume_flags, rx));
        Some(Reporter { tx, task })
    }

    /// Publishes the current state. Called once per frame, so it only borrows
    /// and allocates nothing unless the session id itself changed.
    pub fn update(&self, state: State, session: Option<&str>) {
        self.tx.send_if_modified(|slot| {
            if let Some(report) = slot
                && report.session.as_deref() == session
            {
                if report.state == state {
                    return false;
                }
                report.state = state;
                return true;
            }
            *slot = Some(Report {
                state,
                session: session.map(str::to_owned),
            });
            true
        });
    }

    /// Closes the channel and waits, at most [`FINISH_TIMEOUT`], for the task
    /// to send the release.
    pub async fn finish(self) {
        let Reporter { tx, task } = self;
        drop(tx);
        match time::timeout(FINISH_TIMEOUT, task).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::debug!(%err, "herdr: reporter task failed"),
            Err(_) => tracing::debug!("herdr: release did not finish in time"),
        }
    }
}

/// herdr's `resume_argv` rules applied to the flags: no apostrophe or control
/// character, and a total size well under its limit.
fn resume_flags_valid(flags: &[String]) -> bool {
    let bytes: usize = flags.iter().map(String::len).sum();
    bytes <= RESUME_FLAGS_MAX_BYTES
        && flags
            .iter()
            .all(|flag| !flag.chars().any(|c| c == '\'' || c.is_control()))
}

/// Sends every change as `pane.report_agent`, then `pane.release_agent` once
/// the sender is dropped, so the release always follows the last state. A
/// value still pending when the sender closes is skipped: the release clears
/// the state anyway, and skipping keeps exit within one exchange plus release.
async fn report_loop(
    pane: Pane,
    resume_flags: Option<Vec<String>>,
    mut rx: watch::Receiver<Option<Report>>,
) {
    let mut seq = initial_seq();
    while rx.changed().await.is_ok() {
        if rx.has_changed().is_err() {
            break;
        }
        let line = {
            let current = rx.borrow_and_update();
            // `None` is only the initial value, which `changed` never yields.
            let Some(report) = current.as_ref() else {
                continue;
            };
            report_line(&pane, resume_flags.as_deref(), report, seq)
        };
        send_line(&pane.socket, &line).await;
        seq += 1;
    }
    let release = request_line(
        "pane.release_agent",
        seq,
        json!({ "pane_id": pane.id, "source": AGENT, "agent": AGENT, "seq": seq }),
    );
    send_line(&pane.socket, &release).await;
}

/// Current Unix time in microseconds, so `seq` also grows across restarts.
fn initial_seq() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_micros()).unwrap_or(0)
}

/// `resume_flags` is `None` when resume is disabled: the session id is still
/// reported, but no `resume_argv`.
fn report_line(pane: &Pane, resume_flags: Option<&[String]>, report: &Report, seq: u64) -> Vec<u8> {
    let mut params = json!({
        "pane_id": pane.id,
        "source": AGENT,
        "agent": AGENT,
        "state": report.state.as_str(),
        "seq": seq,
    });
    if let Some(session) = &report.session {
        params["agent_session_id"] = json!(session);
        if let Some(flags) = resume_flags {
            let mut resume_argv: Vec<&str> = Vec::with_capacity(flags.len() + 3);
            resume_argv.push(AGENT);
            resume_argv.extend(flags.iter().map(String::as_str));
            resume_argv.push("--session");
            resume_argv.push(session);
            params["resume_argv"] = json!(resume_argv);
        }
    }
    request_line("pane.report_agent", seq, params)
}

/// One newline-terminated request.
fn request_line(method: &str, seq: u64, params: Value) -> Vec<u8> {
    let request = json!({ "id": format!("{AGENT}:{seq}"), "method": method, "params": params });
    let mut line = request.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// Writes one request and waits for the first reply line, all within
/// [`SOCKET_TIMEOUT`]. Failures are logged and dropped: herdr being absent or
/// slow must never affect the TUI.
async fn send_line(socket: &Path, line: &[u8]) {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;

    let exchange = async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(line).await?;
        let mut reply = Vec::new();
        BufReader::new(stream).read_until(b'\n', &mut reply).await?;
        io::Result::Ok(reply)
    };
    match time::timeout(SOCKET_TIMEOUT, exchange).await {
        Ok(Ok(reply)) => {
            tracing::debug!(reply = %String::from_utf8_lossy(&reply).trim_end(), "herdr: reply");
        }
        Ok(Err(err)) => tracing::debug!(%err, "herdr: socket request failed"),
        Err(_) => tracing::debug!("herdr: socket request timed out"),
    }
}
