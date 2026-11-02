//! The `shell` tool: run one command through the system shell
//! (`$SHELL` unless fish/nu, else `/bin/zsh` on macOS, `/bin/bash`,
//! `/bin/sh`) as its own process group, with combined stdout/stderr capture,
//! timeout kill, and cancellation kill.
//!
//! The surface mirrors upstream `packages/core/src/tool/plugin/shell.ts`:
//! both streams interleave into one buffer, the tail is truncated by the
//! tool-output limits (last 2 000 lines / 50 KiB), and a command that is
//! still running when its yield window closes becomes an owned session,
//! addressed by `session_id` from `interact` (observe) and `stop`. The table
//! lives in [`sessions`], shared with the `/api/shell` endpoints; the OS
//! layer (shell choice, spawn, signals) lives in [`process`].
//!
//! The model writes the dialect of the selected shell; the tool description
//! names it ([`sessions::ShellSessions::shell_name`]). Background children
//! (`cmd &`) are real processes that outlive the call and are torn down by
//! `stop`. Output compression is RTK's job, done BEFORE execution:
//! [`RtkRewriter`] rewrites the command itself (`git status` →
//! `rtk git status`), so the capture, the paged output and the model-facing
//! text are all the same bytes — there is no second, minimized copy. RTK
//! buffers a wrapped command's output until it exits, so a command that returns
//! an owned session at once (`yield_time_ms=0`) is never rewritten; a rewritten
//! foreground command that outlives its yield window
//! says so in its "still running" notice. Commands run with stdin at
//! `/dev/null`, so interactive commands fail fast on EOF instead of hanging.
//!
//! A foreground run with a completion sink also registers as a job in the
//! registry's [`crate::jobs::Jobs`], so `session.background` can end its
//! yield window early and hand it back exactly as an overflowing run is
//! (upstream `jobs.block` in `shell.ts`).

pub mod completion;
pub mod events;
pub mod sessions;
pub mod user;

mod process;

use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::jobs::Jobs;
use crate::rtk::RtkRewriter;
use ox_schema::support::Record;
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
pub const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;

/// The timeout a call gets when it passes none. Upstream keyed this on
/// `background === true ? (timeout ?? 0) : (timeout ?? DEFAULT_TIMEOUT_MS)`,
/// which this surface expresses as `yield_time_ms=0`. An `owned` run is handed
/// back at once and has no deadline unless the caller asks for one; a
/// foreground run keeps the default.
#[must_use]
pub fn default_timeout_ms(owned: bool) -> u64 {
    if owned { 0 } else { DEFAULT_TIMEOUT_MS }
}

/// How long `run` observes before handing back an owned session.
const DEFAULT_YIELD_MS: u64 = 30_000;
/// How long `interact` observes when the caller does not say.
const DEFAULT_INTERACT_YIELD_MS: u64 = 5_000;

/// Upstream `ToolOutput.MAX_LINES` / `MAX_BYTES`.
const MAX_LINES: usize = 2_000;
pub(crate) const MAX_BYTES: usize = 50 * 1024;

/// Upstream `BACKGROUND_INSTRUCTION` (tool/plugin/shell.ts).
const BACKGROUND_INSTRUCTION: &str = "You will be notified automatically when the command finishes. The notification will include the command's output. Unless the user explicitly asks otherwise, DO NOT poll for completion, even if you need the final result to continue. Repeatedly sleeping and reading or searching the output file is polling, not useful work. You may read the current output if it lets you do useful work now, but do not repeatedly check it while waiting for the command to finish. Keep working on anything that does not depend on the result. If you have nothing else to do, end your response; you will be resumed automatically when the command finishes.";

pub struct ShellTool {
    /// Live sessions started by `run`, shared with the `/api/shell` handlers.
    sessions: Arc<sessions::ShellSessions>,
    /// Optional completion delivery; background runs fail closed without it.
    completion: Option<completion::SessionSink>,
    /// Optional pre-execution RTK rewrite; a disabled rewriter is a no-op.
    rtk: Arc<RtkRewriter>,
    /// Optional job registry a blocking foreground run registers in.
    jobs: Option<Arc<Jobs>>,
}

impl ShellTool {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: sessions::shared(),
            completion: None,
            rtk: Arc::new(RtkRewriter::new()),
            jobs: None,
        }
    }

    /// A shell tool over a caller-provided session table, so a test can
    /// inspect the sessions it started.
    pub fn with_sessions(sessions: Arc<sessions::ShellSessions>) -> Self {
        Self {
            sessions,
            completion: None,
            rtk: Arc::new(RtkRewriter::new()),
            jobs: None,
        }
    }

    /// Swap the RTK rewriter. Tests inject [`RtkRewriter::disabled`] (host
    /// independence) or [`RtkRewriter::with_binary`] (a mock rtk).
    pub fn with_rewriter(mut self, rtk: RtkRewriter) -> Self {
        self.rtk = Arc::new(rtk);
        self
    }

    /// Install the sink background runs report their terminal state through.
    pub fn with_completion(mut self, completion: completion::SessionSink) -> Self {
        self.completion = Some(completion);
        self
    }

    /// Install the job registry `session.background` uses to hand a blocking
    /// foreground run to the background. Only runs that also have a
    /// completion sink register, so a backgrounded run is always notified.
    pub fn with_jobs(mut self, jobs: Arc<Jobs>) -> Self {
        self.jobs = Some(jobs);
        self
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
        lines.push(format!(
            "Commands run on {} using {}.",
            os_name(),
            self.sessions.shell_name()
        ));
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
        if self.completion.is_some() {
            lines.push(
                "Background commands return immediately, and you will be notified when they complete."
                    .to_owned(),
            );
        } else {
            lines.push(
                "Background completion notifications are unavailable in this runtime; use a foreground command or an existing session."
                    .to_owned(),
            );
        }
        lines.push(
            "When to use: run builds, tests, formatters, and version control, or any command no dedicated tool covers."
                .to_owned(),
        );
        lines.push(
            "When NOT to use: read, write, edit, glob, or grep a file (each has a dedicated tool), print a file with cat, or search text with grep or rg. Never detach with &, nohup, or setsid; pass yield_time_ms=0 instead."
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
                            "Timeout in milliseconds for the whole session. Set to 0 to disable the timeout. Defaults to 120000 for foreground commands and to 0 (no timeout) for a command that returns at once (yield_time_ms=0). A foreground command that outlives its yield window and moves to the background loses its timeout and runs until it exits or is stopped.",
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
                &[],
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
/// their whole output; a command still running when the window closes (or
/// when `session.background` releases its job) returns its `session_id` and
/// keeps running in the background.
async fn run_command(tool: &ShellTool, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let command = match required_str("shell", input, "command") {
        Ok(command) => command,
        Err(output) => return output,
    };
    let workdir = match optional_str("shell", input, "workdir") {
        Ok(workdir) => workdir,
        Err(output) => return output,
    };
    let timeout = match optional_nonneg_int("shell", input, "timeout") {
        Ok(value) => value,
        Err(output) => return output,
    };
    // `background` was a parameter before the session model replaced it with a
    // zero yield window. The schema no longer offers it, so a call that still
    // sends it is answered with what to use instead of being run in the
    // foreground for its whole yield window.
    if let Some(sent) = input.get("background").filter(|value| !value.is_null()) {
        return super::invalid_arguments(
            "shell",
            &[(
                "background",
                format!("Not a parameter; pass yield_time_ms=0 instead (got {sent})"),
            )],
            input,
        );
    }
    // A zero yield window hands the run back as an owned session at once; any
    // other value waits out the window.
    let yield_ms = match optional_nonneg_int("shell", input, "yield_time_ms") {
        Ok(value) => value.unwrap_or(DEFAULT_YIELD_MS),
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
    let timeout_ms = timeout.unwrap_or_else(|| default_timeout_ms(yield_ms == 0));
    if yield_ms == 0 && tool.completion.is_none() {
        return fail(
            "Background shell completion notification is unavailable; run in the foreground or configure a completion sink.".to_owned(),
        );
    }
    // RTK rewrites the command itself before execution; any failure leaves it
    // unchanged. A command that returns an owned session at once
    // (`yield_time_ms=0`) is exempt: its value is streaming output, which RTK
    // would buffer until the command exits.
    let mut rewritten = false;
    let to_run = if yield_ms == 0 {
        command.to_owned()
    } else {
        match tool.rtk.rewrite(command).await {
            Some(text) => {
                rewritten = true;
                text
            }
            None => command.to_owned(),
        }
    };
    // Every owned run carries the sink. A run handed back at once is armed
    // from the start; a foreground run is gated until `hand_back`, so a
    // command that settles inside its window returns inline and never
    // notifies, while one that overflows is notified exactly once.
    let completion = tool.completion.as_ref().map(|sink| {
        let sink = Arc::clone(sink);
        let session_id = ctx.session_id.clone();
        Arc::new(move |completion| sink(&session_id, completion)) as sessions::CompletionCallback
    });
    let session = match tool.sessions.spawn(sessions::SpawnRequest {
        command: to_run,
        cwd: workdir,
        timeout_ms: Some(timeout_ms).filter(|timeout| *timeout > 0),
        completion,
        foreground: yield_ms > 0,
        owner: Some(sessions::Owner {
            session_id: ctx.session_id.clone(),
            directory: ctx.directory.to_string_lossy().into_owned(),
        }),
        metadata: Record::new(),
    }) {
        Ok(session) => session,
        Err(_) => return wrap("spawn failed".to_owned()),
    };
    // A foreground run that can be notified blocks its session as a job, so
    // `session.background` can end the window early (upstream `jobs.block`,
    // titled with the command the model sent, before any RTK rewrite).
    let guard = match &tool.jobs {
        Some(jobs) if yield_ms > 0 && tool.completion.is_some() => {
            Some(jobs.block(&session.id, "shell", command, &ctx.session_id))
        }
        _ => None,
    };
    // Wait for the yield window, an interruption, a fast exit, or a
    // user-requested background; the last one takes the overflow path below.
    if yield_ms > 0 {
        let backgrounded = async {
            match &guard {
                Some(guard) => guard.backgrounded().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = session.wait_settled(Duration::from_millis(yield_ms)) => {}
            () = ctx.cancellation.cancelled() => {}
            () = backgrounded => {}
        }
    }
    if ctx.cancellation.is_cancelled() {
        session.stop(true).await;
        return wrap("command cancelled".to_owned());
    }
    if yield_ms == 0 {
        // Handed back at once, whatever the process has done since: the
        // completion notice carries the outcome, so this response must not
        // also carry it (upstream `backgroundResult`).
        return background_result(&session.id, false);
    }
    // Past the window: settle the hand-back race under the session's gate. A
    // run that settled first is returned inline (and never notifies); one
    // still running becomes a background job whose settle notifies once.
    // Without a sink there is nothing to notify through, so the run stays an
    // `interact`-addressed session.
    if tool.completion.is_some() && session.hand_back() == sessions::HandBack::Background {
        return background_result(&session.id, rewritten);
    }
    let (page, exit) = session.poll(0, usize::MAX);
    if exit
        .status
        .is_some_and(|status| status != sessions::Status::Running)
    {
        // Settled: this is the ordinary one-shot result. Compression already
        // happened upstream (RTK rewrote the command), so the model reads the
        // same capture the paged output serves.
        return settled_output(&session.id, timeout_ms, &page.output, exit).with_metadata(json!({
            "status": "completed",
            "shellID": session.id,
            "truncated": false,
            "exit": exit.code,
        }));
    }
    // Still running and no sink to notify through: hand the model an owned
    // handle to observe with `interact`; its timeout still applies.
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
    if rewritten {
        text.push_str(
            "\n\nThis command runs through rtk, which buffers its output until it exits; \
             interact returns that output once the command completes.",
        );
    }
    ok_text(text).with_metadata(json!({
        "status": "running",
        "shellID": session.id,
    }))
}

/// The `running` response of a run handed back to the background (upstream
/// `backgroundResult`). `rewritten` adds the RTK buffering caveat: RTK emits a
/// wrapped command's output only when it exits.
fn background_result(session_id: &str, rewritten: bool) -> ToolOutput {
    let mut text = format!(
        "Command moved to the background (shell ID: {session_id}).\n{BACKGROUND_INSTRUCTION}"
    );
    if rewritten {
        text.push_str(
            "\n\nThis command runs through rtk, which buffers its output until it exits; \
             the completion notification carries that output.",
        );
    }
    ok_text(text).with_metadata(json!({
        "status": "running",
        "shellID": session_id,
    }))
}

/// The `interact` reply for a running session whose completion is delivered
/// automatically: the same no-poll guidance as a background hand-back.
fn still_running_notice(session_id: &str) -> String {
    format!(
        "\n\nstatus: running (shell ID: {session_id}).\n{BACKGROUND_INSTRUCTION} \
         To terminate it, call shell with action=\"stop\" and session_id={session_id}."
    )
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
    } else if session.notifies_on_completion() {
        // The completion is delivered automatically, so this reply must not
        // invite another observe; the output above stays available.
        text.push_str(&still_running_notice(session_id));
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
pub(crate) fn exit_notice_parts(code: Option<i32>, signal: Option<i32>) -> Option<String> {
    if let Some(signal) = signal.and_then(signal_name_of) {
        return Some(format!("Killed by {signal}"));
    }
    match code {
        Some(0) | None => None,
        Some(code) => Some(format!("Exited with code {code}")),
    }
}

pub(crate) fn signal_name_of(signal: i32) -> Option<&'static str> {
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
pub(crate) fn truncate_output(raw: &str) -> (String, bool, usize, usize) {
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::contract::ToolRegistry;
    use crate::jobs::JobInfo;

    #[tokio::test(flavor = "multi_thread")]
    async fn session_background_hands_a_blocking_run_back() {
        let sessions = Arc::new(sessions::ShellSessions::new());
        let jobs = Arc::new(Jobs::default());
        let sink: completion::SessionSink = Arc::new(|_: &str, _| {});
        let tool = ShellTool::with_sessions(Arc::clone(&sessions))
            .with_rewriter(RtkRewriter::disabled())
            .with_completion(sink)
            .with_jobs(Arc::clone(&jobs));
        let ctx = ToolContext {
            session_id: "ses_test".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_test".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        // Outlives the 5 s bound below, so only the backgrounding can return it.
        let command = "sleep 10";
        let input = json!({ "command": command, "yield_time_ms": 30_000 });

        let background = async {
            let started = Instant::now();
            loop {
                let moved = jobs.background_all("ses_test");
                if !moved.is_empty() {
                    return moved;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "the foreground run never registered as a job",
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let run = tokio::time::timeout(Duration::from_secs(5), run(&tool, &ctx, &input));
        let (output, moved) = tokio::join!(run, background);
        let output = output.expect("a backgrounded run must return before its yield window");

        let metadata = output
            .metadata
            .expect("a background result carries metadata");
        assert_eq!(metadata["status"], "running", "metadata: {metadata}");
        let text = output.content.as_str().expect("text content");
        assert!(
            text.starts_with("Command moved to the background (shell ID:"),
            "unexpected text: {text}",
        );
        let shell_id = metadata["shellID"].as_str().expect("shellID").to_owned();
        assert_eq!(
            moved,
            vec![JobInfo {
                id: shell_id.clone(),
                kind: "shell",
                title: command.to_owned(),
            }],
        );

        if let Some(session) = sessions.get(&shell_id) {
            session.stop(true).await;
        }
    }

    /// `background` was replaced by `yield_time_ms=0`: the definition must stop
    /// offering it (in prose as well as in the schema), and a leftover call
    /// must be answered with the replacement instead of a foreground run.
    #[tokio::test]
    async fn background_is_not_a_parameter() {
        let tool = ShellTool::new();
        let definition = tool.definition().function;
        assert!(
            !definition.description.contains("background parameter"),
            "the description must not name a parameter the schema does not declare",
        );
        let properties = definition.parameters["properties"]
            .as_object()
            .expect("shell parameters declare properties");
        assert!(
            !properties.contains_key("background"),
            "the schema must not offer a parameter the parser rejects",
        );
        assert!(
            properties.contains_key("yield_time_ms"),
            "the yield window is the parameter that replaced background",
        );
        assert!(
            definition.parameters["required"].is_null(),
            "action defaults to run, so the schema must not require it",
        );

        let ctx = ToolContext {
            session_id: "ses_test".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_test".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        let output = run(
            &tool,
            &ctx,
            &json!({"command": "true", "background": "true"}),
        )
        .await;
        let message = output
            .error
            .expect("a leftover background call is rejected");
        assert!(
            message.contains("pass yield_time_ms=0 instead"),
            "the error must name the replacement parameter: {message}",
        );
    }
}
