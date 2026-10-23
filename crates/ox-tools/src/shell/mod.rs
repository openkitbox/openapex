//! The `shell` tool: run one command in an embedded brush session (pi-shell),
//! with combined stdout/stderr capture, timeout kill, and cancellation kill.
//!
//! The surface mirrors upstream `packages/core/src/tool/plugin/shell.ts`:
//! both streams interleave into one buffer, the tail is truncated by the
//! tool-output limits (last 2 000 lines / 50 KiB), and a command that is
//! still running when its yield window closes becomes an owned session,
//! addressed by `session_id` from `interact` (observe) and `stop`. The table
//! lives in [`sessions`], shared with the `/api/shell` endpoints.
//!
//! The engine underneath is pi-shell (pinned oh-my-pi rev): the model writes
//! `bash`, not the host login shell, and the minimizer compacts the settled
//! output the model reads while the paged capture stays raw. Commands run
//! with stdin at `/dev/null` — pi-shell has no writer for a running command,
//! so the old `chars` parameter is gone and interactive commands fail fast
//! on EOF instead of hanging.

pub mod sessions;

mod engine;

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{
    fail, object_param, ok_text, optional_bool, optional_nonneg_int, optional_str, param,
    required_str, resolve_path,
};

/// Upstream `ShellTool.DEFAULT_TIMEOUT_MS`.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;

/// How long `run` observes before handing back an owned session.
const DEFAULT_YIELD_MS: u64 = 30_000;
/// How long `interact` observes when the caller does not say.
const DEFAULT_INTERACT_YIELD_MS: u64 = 5_000;

/// Upstream `ToolOutput.MAX_LINES` / `MAX_BYTES`.
const MAX_LINES: usize = 2_000;
const MAX_BYTES: usize = 50 * 1024;

/// The dialect the model should write: brush implements `bash`.
const SHELL_NAME: &str = "bash";

pub struct ShellTool {
    /// Live sessions started by `run`, shared with the `/api/shell` handlers.
    sessions: Arc<sessions::ShellSessions>,
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            sessions: sessions::shared(),
        }
    }

    /// A shell tool over a caller-provided session table, so a test can
    /// inspect the sessions it started.
    pub fn with_sessions(sessions: Arc<sessions::ShellSessions>) -> Self {
        Self { sessions }
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn definition(&self) -> ToolDef {
        let mut lines = vec!["Execute a shell command and return its output.".to_owned()];
        lines.push(format!("Commands run on {} using {SHELL_NAME}.", os_name()));
        lines.push("Quote file paths containing spaces or special characters.".to_owned());
        lines.push("Prefer dedicated tools over shell commands when possible.".to_owned());
        lines.push(
            "When output is large, the full result is saved to a file and a truncated preview is returned."
                .to_owned(),
        );
        lines.push(
            "Rely on automatic truncation unless filtering the output is more useful.".to_owned(),
        );
        lines.push(
            "Commands accept an optional timeout, background commands have no timeout by default."
                .to_owned(),
        );
        lines.push(
            "Background commands return immediately, and you will be notified when they complete."
                .to_owned(),
        );
        lines.push(
            "When to use: run builds, tests, formatters, and version control, or any command no dedicated tool covers."
                .to_owned(),
        );
        lines.push(
            "When NOT to use: read, write, edit, glob, or grep a file (each has a dedicated tool), print a file with cat, or search text with grep or rg. Never detach with &, nohup, or setsid; use the background parameter instead."
                .to_owned(),
        );
        ToolDef::function(
            "shell",
            lines.join(" "),
            object_param(
                vec![
                    (
                        "action",
                        json!({
                            "type": "string",
                            "enum": ["run", "interact", "stop"],
                            "description": "run starts a command; interact observes an owned session; stop terminates one. Defaults to run.",
                        }),
                    ),
                    (
                        "command",
                        param(
                            "string",
                            "Shell command string to execute. Required for action=run.",
                        ),
                    ),
                    (
                        "workdir",
                        param(
                            "string",
                            "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead.",
                        ),
                    ),
                    (
                        "timeout",
                        super::int_param(
                            0,
                            "Timeout in milliseconds for the whole session. Set to 0 to disable the timeout. Defaults to 120000. A command that outlives its yield window keeps running under this deadline.",
                        ),
                    ),
                    (
                        "yield_time_ms",
                        super::int_param(
                            0,
                            "How long to observe before returning. For run, defaults to 30000; 0 returns an owned session at once (background). For interact, defaults to 5000.",
                        ),
                    ),
                    (
                        "session_id",
                        param(
                            "string",
                            "Owned session handle returned by an earlier run or interact. Required for action=interact and action=stop.",
                        ),
                    ),
                    (
                        "cursor",
                        super::int_param(
                            0,
                            "Byte offset to read output from for action=interact. Defaults to 0, which returns the whole retained tail.",
                        ),
                    ),
                    (
                        "force",
                        param(
                            "boolean",
                            "For action=stop, terminate immediately instead of asking the process to exit. Defaults to false.",
                        ),
                    ),
                ],
                &["action"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(self, &ctx, &input).await })
    }
}

fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "the host OS"
    }
}

/// The `shell` action (upstream splits these across `shell` and
/// `write_stdin`; one tool with a discriminator keeps the schema flat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Run,
    Interact,
    Stop,
}

fn action_of(input: &Value) -> Result<Action, ToolOutput> {
    match input.get("action") {
        None | Some(Value::Null) => Ok(Action::Run),
        Some(Value::String(action)) => match action.as_str() {
            "run" => Ok(Action::Run),
            "interact" => Ok(Action::Interact),
            "stop" => Ok(Action::Stop),
            other => Err(super::invalid_arguments(
                "shell",
                &[(
                    "action",
                    format!("Expected one of \"run\" | \"interact\" | \"stop\", got \"{other}\""),
                )],
                input,
            )),
        },
        Some(other) => Err(super::invalid_arguments(
            "shell",
            &[(
                "action",
                format!("Expected one of \"run\" | \"interact\" | \"stop\", got {other}"),
            )],
            input,
        )),
    }
}

async fn run(tool: &ShellTool, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let action = match action_of(input) {
        Ok(action) => action,
        Err(output) => return output,
    };
    match action {
        Action::Run => run_command(tool, ctx, input).await,
        Action::Interact => interact(tool, input).await,
        Action::Stop => stop(tool, input).await,
    }
}

/// Start one command. Fast commands settle inside the yield window and return
/// their whole output; a command still running when the window closes returns
/// its `session_id` and keeps running for `interact`.
async fn run_command(tool: &ShellTool, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let command = match required_str("shell", input, "command") {
        Ok(command) => command,
        Err(output) => return output,
    };
    let workdir = match optional_str("shell", input, "workdir") {
        Ok(workdir) => workdir,
        Err(output) => return output,
    };
    let timeout_ms = match optional_nonneg_int("shell", input, "timeout") {
        Ok(value) => value.unwrap_or(DEFAULT_TIMEOUT_MS),
        Err(output) => return output,
    };
    let yield_time_ms = match optional_nonneg_int("shell", input, "yield_time_ms") {
        Ok(value) => value.unwrap_or(DEFAULT_YIELD_MS),
        Err(output) => return output,
    };
    // `background` from the pre-session schema: keep it as "return at once",
    // which the session model expresses as a zero yield window.
    let background = match optional_bool("shell", input, "background") {
        Ok(value) => value.unwrap_or(false),
        Err(output) => return output,
    };
    // Upstream prepare() failures and spawn failures are both wrapped by the
    // execute mapError, so the model only sees the generic message.
    let wrap = |_cause: String| fail(format!("Unable to execute command: {command}"));
    let workdir: PathBuf = match workdir {
        Some(workdir) => {
            let absolute = resolve_path(&ctx.directory, workdir);
            match std::fs::metadata(&absolute) {
                Err(_) => {
                    return wrap(format!(
                        "Working directory does not exist: {}",
                        absolute.display()
                    ));
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return wrap(format!(
                        "Working directory is not a directory: {}",
                        absolute.display()
                    ));
                }
                Ok(_) => absolute,
            }
        }
        None => ctx.directory.clone(),
    };
    // A background request starts a session and returns at once; a foreground
    // request waits out its yield window.
    let yield_ms = if background { 0 } else { yield_time_ms };
    let session = match tool.sessions.spawn(sessions::SpawnRequest {
        command: command.to_owned(),
        cwd: workdir,
        timeout_ms: Some(timeout_ms).filter(|timeout| *timeout > 0),
    }) {
        Ok(session) => session,
        Err(_) => return wrap("spawn failed".to_owned()),
    };
    // Wait for the yield window, an interruption, or a fast exit.
    if yield_ms > 0 {
        tokio::select! {
            _ = session.wait_settled(Duration::from_millis(yield_ms)) => {}
            () = ctx.cancellation.cancelled() => {}
        }
    }
    if ctx.cancellation.is_cancelled() {
        session.stop(true).await;
        return wrap("command cancelled".to_owned());
    }
    let (page, exit) = session.poll(0, usize::MAX);
    if exit
        .status
        .is_some_and(|status| status != sessions::Status::Running)
    {
        // Settled: this is the ordinary one-shot result. The model reads the
        // minimizer's rendering when the engine produced one; the paged
        // capture stays raw either way.
        let text = session.minimized_text().unwrap_or(page.output);
        return settled_output(&session.id, timeout_ms, &text, exit).with_metadata(json!({
            "status": "completed",
            "shellID": session.id,
            "truncated": false,
            "exit": exit.code,
        }));
    }
    // Still running: hand the model an owned handle.
    let tail = truncate_for_model(&page.output);
    let mut text = if tail.is_empty() {
        "(no output yet)".to_owned()
    } else {
        tail
    };
    text.push_str(&format!(
        "\n\nCommand is still running as shell session {}. Call shell with \
         action=\"interact\" and session_id={} to observe it, or action=\"stop\" to \
         terminate it. Do not start the same command again.",
        session.id, session.id
    ));
    ok_text(text).with_metadata(json!({
        "status": "running",
        "shellID": session.id,
    }))
}

/// The model-facing text for a settled session.
fn settled_output(
    session_id: &str,
    timeout_ms: u64,
    text: &str,
    exit: sessions::Exit,
) -> ToolOutput {
    let (text, truncated, total_lines, shown_lines) = truncate_output(text);
    let notice = match exit.status {
        Some(sessions::Status::Timeout) => Some("Timed out before completion".to_owned()),
        _ => exit_notice_parts(exit.code, exit.signal),
    };
    let mut output = if text.is_empty() {
        "(no output)".to_owned()
    } else {
        text
    };
    if truncated {
        output.push_str(&format!(
            "\n\n[showing lines {}-{} of {total_lines}; full output is paged through \
             GET /api/shell/{session_id}/output]",
            total_lines - shown_lines + 1,
            total_lines,
        ));
    }
    if notice.as_deref() == Some("Timed out before completion") {
        output.push_str(&format!(
            "\n\nCommand exceeded timeout of {timeout_ms} ms. Retry with a larger timeout if the \
             command is expected to take longer."
        ));
    }
    let mut messages: Vec<String> = vec![output];
    if let Some(notice) = notice {
        messages.push(notice);
    }
    ok_text(messages.join("\n\n"))
}

/// Observe an owned session.
async fn interact(tool: &ShellTool, input: &Value) -> ToolOutput {
    let Some(session_id) = input.get("session_id").and_then(Value::as_str) else {
        return super::invalid_arguments(
            "shell",
            &[(
                "session_id",
                "Missing required field for action=interact".to_owned(),
            )],
            input,
        );
    };
    let Some(session) = tool.sessions.get(session_id) else {
        return fail(format!("Unknown shell session: {session_id}"));
    };
    let yield_time_ms = match optional_nonneg_int("shell", input, "yield_time_ms") {
        Ok(value) => value.unwrap_or(DEFAULT_INTERACT_YIELD_MS),
        Err(output) => return output,
    };
    let cursor = match optional_nonneg_int("shell", input, "cursor") {
        Ok(value) => value.unwrap_or(0),
        Err(output) => return output,
    };
    // Wait for output to change (or for the process to settle) rather than
    // sleeping a fixed poll interval.
    let deadline = Duration::from_millis(yield_time_ms);
    if session.settled() {
        // Nothing to wait for; report what is already buffered.
    } else {
        tokio::select! {
            () = session.wait_change(deadline) => {}
            _ = session.wait_settled(deadline) => {}
        }
    }
    let (page, exit) = session.poll(cursor, usize::MAX);
    let settled = exit
        .status
        .is_some_and(|status| status != sessions::Status::Running);
    let tail = truncate_for_model(&page.output);
    let mut text = if tail.is_empty() {
        "(no new output)".to_owned()
    } else {
        tail
    };
    if settled {
        if let Some(notice) = exit_notice_parts(exit.code, exit.signal) {
            text.push_str(&format!("\n\n{notice}"));
        }
    } else {
        text.push_str(&format!(
            "\n\nCommand is still running as shell session {session_id}. Interact again to \
             observe, or stop it with action=\"stop\"."
        ));
    }
    ok_text(text).with_metadata(json!({
        "status": if settled { "completed" } else { "running" },
        "shellID": session.id,
        "cursor": page.cursor,
        "truncated": page.truncated,
    }))
}

/// Terminate an owned session.
async fn stop(tool: &ShellTool, input: &Value) -> ToolOutput {
    let Some(session_id) = input.get("session_id").and_then(Value::as_str) else {
        return super::invalid_arguments(
            "shell",
            &[(
                "session_id",
                "Missing required field for action=stop".to_owned(),
            )],
            input,
        );
    };
    let Some(session) = tool.sessions.get(session_id) else {
        return fail(format!("Unknown shell session: {session_id}"));
    };
    let force = match optional_bool("shell", input, "force") {
        Ok(value) => value.unwrap_or(false),
        Err(output) => return output,
    };
    session.stop(force).await;
    ok_text(format!("Stopped shell session {session_id}.")).with_metadata(json!({
        "status": "completed",
        "shellID": session.id,
    }))
}

/// Upstream `notice` (shell/result.ts), from the recorded exit instead of a
/// live `ExitStatus` (the watcher owns the child).
fn exit_notice_parts(code: Option<i32>, signal: Option<i32>) -> Option<String> {
    if let Some(signal) = signal.and_then(signal_name_of) {
        return Some(format!("Killed by {signal}"));
    }
    match code {
        Some(0) | None => None,
        Some(code) => Some(format!("Exited with code {code}")),
    }
}

fn signal_name_of(signal: i32) -> Option<&'static str> {
    Some(match signal {
        2 => "SIGINT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// The tail the model reads from a session's capture: the same byte and line
/// caps as a one-shot result, so a chatty server cannot flood the context.
fn truncate_for_model(raw: &str) -> String {
    truncate_output(raw).0
}

/// Upstream capture truncation (shell.ts `result`): the last `MAX_BYTES` of
/// the capture, then the last `MAX_LINES` lines. Returns the visible text, the
/// truncation flag, the total line count (newlines across the WHOLE capture,
/// +1 when the page doesn't end with a newline), and the number of shown
/// lines (page-relative, capped at `MAX_LINES`).
fn truncate_output(raw: &str) -> (String, bool, usize, usize) {
    let bytes = raw.as_bytes();
    let page = if bytes.len() > MAX_BYTES {
        let mut start = bytes.len() - MAX_BYTES;
        while start > 0 && !raw.is_char_boundary(start) {
            start -= 1;
        }
        &raw[start..]
    } else {
        raw
    };
    let truncated_bytes = bytes.len() > MAX_BYTES;
    let mut lines: Vec<&str> = page.split('\n').collect();
    if page.ends_with('\n') {
        lines.pop();
    }
    let truncated = truncated_bytes || lines.len() > MAX_LINES;
    let total = raw.matches('\n').count() + usize::from(!page.ends_with('\n'));
    let shown = lines.len().min(MAX_LINES);
    let text = if lines.len() > MAX_LINES {
        lines[lines.len() - MAX_LINES..].join("\n")
    } else {
        page.to_owned()
    };
    (text, truncated, total, shown)
}
