//! Port of upstream `packages/core/src/tool/plugin/shell.ts` (+ its
//! `shell.ts` service): run one shell command with combined stdout/stderr
//! capture, timeout kill, and cancellation kill.
//!
//! Capture mirrors upstream's model: both streams write to one file whose
//! shared file description interleaves the output, then the tail is
//! truncated by the tool-output limits (last 2 000 lines / 50 KiB).
//!
//! MVP deviations: no background execution (the synthetic-notification
//! plumbing is out of scope), no permission scanning, and the "full output
//! saved to …" path is a per-call temp file instead of upstream's retained
//! `tool-output` store.

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::rtk::RtkRewriter;

use super::{
    fail, object_param, ok_text, optional_bool, optional_nonneg_int, optional_str, param,
    required_str, resolve_path,
};

/// Upstream `ShellTool.DEFAULT_TIMEOUT_MS`.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;

/// Upstream `ToolOutput.MAX_LINES` / `MAX_BYTES`.
const MAX_LINES: usize = 2_000;
const MAX_BYTES: usize = 50 * 1024;

pub struct ShellTool {
    shell: PathBuf,
    shell_name: String,
    /// Optional RTK rewrite seam: when an `rtk` binary is available, each
    /// command is passed through it before execution (path A integration).
    rtk: Arc<RtkRewriter>,
}

impl ShellTool {
    /// Resolve the login shell like upstream `ShellSelect`: `$SHELL` when it
    /// exists and is not a denied shell (fish, nushell), else the platform
    /// fallback (`/bin/zsh` on macOS, `/bin/bash`, `/bin/sh`). Denied shells
    /// never enter the candidate list (upstream `compatible`).
    pub fn new() -> Self {
        let shell = shell_candidates()
            .into_iter()
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        Self::with_shell(shell)
    }

    /// A shell tool pinned to a specific shell binary (the test seam).
    pub fn with_shell(shell: PathBuf) -> Self {
        let name = shell
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| "sh".to_owned());
        Self {
            shell,
            shell_name: name,
            rtk: Arc::new(RtkRewriter::new()),
        }
    }
}

/// Upstream `ShellSelect.META` deny list.
const DENIED_SHELLS: [&str; 2] = ["fish", "nu"];

/// Upstream `ShellSelect.compatible`: denied by basename.
fn denied(shell: &Path) -> bool {
    shell
        .file_name()
        .map(|name| DENIED_SHELLS.contains(&name.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(true)
}

/// Serializes tests that mutate `SHELL` against tests that resolve it.
#[cfg(test)]
pub(crate) static SHELL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

fn shell_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(shell) = std::env::var_os("SHELL") {
        candidates.push(PathBuf::from(shell));
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/bin/zsh"));
    }
    candidates.push(PathBuf::from("/bin/bash"));
    candidates.push(PathBuf::from("/bin/sh"));
    candidates
        .into_iter()
        .filter(|candidate| !denied(candidate))
        .collect()
}

/// How the process ended.
enum Outcome {
    Completed(std::process::ExitStatus),
    /// The timeout fired; the kill happens after the select.
    TimedOut,
    Cancelled,
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
            self.shell_name
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
        lines.push(
            "Background commands return immediately, and you will be notified when they complete."
                .to_owned(),
        );
        ToolDef::function(
            "shell",
            lines.join(" "),
            object_param(
                vec![
                    (
                        "command",
                        param("string", "Shell command string to execute"),
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
                            "Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 120000 for foreground commands. Background commands have no timeout by default.",
                        ),
                    ),
                    (
                        "background",
                        param(
                            "boolean",
                            "Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion.",
                        ),
                    ),
                ],
                &["command"],
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

async fn run(tool: &ShellTool, ctx: &ToolContext, input: &Value) -> ToolOutput {
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
    let background = match optional_bool("shell", input, "background") {
        Ok(value) => value.unwrap_or(false),
        Err(output) => return output,
    };
    if background {
        // Upstream background shells need the synthetic-notification loop;
        // ox's runner does not deliver those yet. Rejected before the RTK
        // rewrite so the message names the command the model sent.
        return fail(format!("Unable to execute command: {command}"));
    }
    // RTK path A: rewrite the command before execution when an `rtk` binary is
    // available and capable. A missing/incapable/failing rtk returns the
    // command unchanged (fail-open), so this is transparent when RTK is absent.
    let command = tool
        .rtk
        .rewrite(command)
        .await
        .unwrap_or_else(|| command.to_owned());
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
    let output_file = match capture_file(&ctx.call_id) {
        Ok(file) => file,
        Err(_) => return wrap("could not create the capture file".to_owned()),
    };
    let spawn = || -> std::io::Result<tokio::process::Child> {
        use std::process::Stdio;
        let stdout = std::fs::File::create(&output_file)?;
        let stderr = stdout.try_clone()?;
        let mut spawn = tokio::process::Command::new(&tool.shell);
        spawn
            .arg("-c")
            .arg(&command)
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .env("AGENT", "1")
            .env("OPENCODE", "1")
            .env("OPENCODE_SESSION_ID", &ctx.session_id);
        if std::env::var_os("AI_AGENT").is_none() {
            spawn.env("AI_AGENT", "opencode");
        }
        spawn.spawn()
    };
    let mut child = match spawn() {
        Ok(child) => child,
        Err(_) => return wrap("spawn failed".to_owned()),
    };
    // `child.wait()` pins a mutable borrow of the child; the kill + rewait
    // for the timeout/cancel outcomes happens after the select releases it.
    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        let sleep = async {
            if timeout_ms == 0 {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        };
        tokio::select! {
            status = &mut wait => match status {
                Ok(status) => Outcome::Completed(status),
                Err(_) => return wrap("wait failed".to_owned()),
            },
            () = sleep => Outcome::TimedOut,
            () = ctx.cancellation.cancelled() => Outcome::Cancelled,
        }
    };
    if let Outcome::TimedOut | Outcome::Cancelled = outcome {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    if let Outcome::Cancelled = outcome {
        return wrap("command cancelled".to_owned());
    }
    let raw = match std::fs::read(&output_file) {
        Ok(raw) => raw,
        Err(_) => return wrap("capture failed".to_owned()),
    };
    let raw_text = String::from_utf8_lossy(&raw);
    let (text, truncated, total_lines, shown_lines) = truncate_output(&raw_text);
    let notice = match outcome {
        Outcome::TimedOut => Some("Timed out before completion".to_owned()),
        Outcome::Completed(status) => exit_notice(&status),
        Outcome::Cancelled => None,
    };
    assemble_output(
        text,
        truncated,
        total_lines,
        shown_lines,
        timeout_ms,
        notice,
        &output_file,
    )
}

/// Upstream `notice` (shell/result.ts).
fn exit_notice(status: &std::process::ExitStatus) -> Option<String> {
    if let Some(signal) = signal_name(status) {
        return Some(format!("Killed by {signal}"));
    }
    match status.code() {
        Some(0) | None => None,
        Some(code) => Some(format!("Exited with code {code}")),
    }
}

fn signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    let signal = status.signal()?;
    Some(match signal {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        3 => "SIGQUIT".to_owned(),
        6 => "SIGABRT".to_owned(),
        9 => "SIGKILL".to_owned(),
        11 => "SIGSEGV".to_owned(),
        13 => "SIGPIPE".to_owned(),
        15 => "SIGTERM".to_owned(),
        other => format!("signal {other}"),
    })
}

/// The capture file: a per-call temp file (upstream keeps a retained
/// `tool-output` store; ox MVP uses the OS temp directory).
fn capture_file(call_id: &str) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join("ox-shell-output");
    std::fs::create_dir_all(&directory)?;
    let name: String = call_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(directory.join(format!("tool_{name}_{:x}_{unique}", std::process::id())))
}

/// Upstream capture truncation (shell.ts `result`): the last `MAX_BYTES` of
/// the file, then the last `MAX_LINES` lines. Returns the visible text, the
/// truncation flag, the total line count (newlines across the WHOLE file,
/// +1 when the captured page doesn't end with a newline), and the number of
/// shown lines (page-relative, capped at `MAX_LINES`).
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
    // Upstream: the store tracks cumulative newlines; `total` is file-wide,
    // not page-relative.
    let total = raw.matches('\n').count() + if page.ends_with('\n') { 0 } else { 1 };
    let shown = lines.len().min(MAX_LINES);
    let text = if lines.len() > MAX_LINES {
        lines[lines.len() - MAX_LINES..].join("\n")
    } else {
        page.to_owned()
    };
    (text, truncated, total, shown)
}

/// Upstream capture + `resultMessages`: the visible text (with truncation
/// and timeout annotations) then the status notice, blank-line joined.
fn assemble_output(
    text: String,
    truncated: bool,
    total_lines: usize,
    shown_lines: usize,
    timeout_ms: u64,
    notice: Option<String>,
    output_file: &Path,
) -> ToolOutput {
    let mut output = if text.is_empty() {
        "(no output)".to_owned()
    } else {
        text
    };
    if truncated {
        output.push_str(&format!(
            "\n\n[showing lines {}-{} of {total_lines}; full output saved to {}]",
            total_lines - shown_lines + 1,
            total_lines,
            output_file.display()
        ));
    }
    if notice.as_deref() == Some("Timed out before completion") {
        output.push_str(&format!(
            "\n\nCommand exceeded timeout of {timeout_ms} ms. Retry with a larger timeout if the command is expected to take longer."
        ));
    }
    let mut messages: Vec<String> = vec![output];
    if let Some(notice) = notice {
        messages.push(notice);
    }
    ok_text(messages.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx(directory: &Path) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.to_path_buf(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    async fn shell(directory: &Path, input: Value) -> ToolOutput {
        let tool = ShellTool::with_shell(PathBuf::from("/bin/sh"));
        tool.execute(ctx(directory), input).await
    }

    #[tokio::test]
    async fn captures_command_output() {
        let directory = tempdir().unwrap();
        let output = shell(directory.path(), json!({"command": "echo hello"})).await;
        assert_eq!(output.content, json!("hello\n"));
        assert!(output.error.is_none());
    }

    #[tokio::test]
    async fn merges_stderr() {
        let directory = tempdir().unwrap();
        let output = shell(
            directory.path(),
            json!({"command": "echo out; echo err 1>&2"}),
        )
        .await;
        assert_eq!(output.content, json!("out\nerr\n"));
    }

    #[tokio::test]
    async fn runs_in_workdir() {
        let directory = tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("sub")).unwrap();
        std::fs::write(directory.path().join("sub/marker"), "").unwrap();
        let output = shell(
            directory.path(),
            json!({"command": "test -f marker && echo yes", "workdir": "sub"}),
        )
        .await;
        assert_eq!(output.content, json!("yes\n"));
    }

    #[tokio::test]
    async fn missing_workdir_fails_with_wrapped_message() {
        let directory = tempdir().unwrap();
        let output = shell(
            directory.path(),
            json!({"command": "echo hi", "workdir": "gone"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("Unable to execute command: echo hi")
        );
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let directory = tempdir().unwrap();
        let output = shell(
            directory.path(),
            json!({"command": "sleep 5", "timeout": 100}),
        )
        .await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains("Command exceeded timeout of 100 ms."),
            "unexpected: {text}"
        );
        assert!(text.contains("Timed out before completion"));
    }

    #[tokio::test]
    async fn truncates_large_output_with_notice() {
        let directory = tempdir().unwrap();
        let output = shell(
            directory.path(),
            json!({"command": "i=1; while [ $i -le 3000 ]; do echo line$i; i=$((i+1)); done"}),
        )
        .await;
        let text = output.content.as_str().unwrap();
        assert!(
            text.contains("[showing lines 1001-3000 of 3000; full output saved to "),
            "unexpected: {text}"
        );
        assert!(text.contains("\nline3000\n\n[showing lines"));
    }

    #[tokio::test]
    async fn cancellation_kills_process() {
        let directory = tempdir().unwrap();
        let tool = ShellTool::with_shell(PathBuf::from("/bin/sh"));
        let token = CancellationToken::new();
        let ctx = ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.path().to_path_buf(),
            call_id: "call_2".to_owned(),
            cancellation: token.clone(),
        };
        let task =
            tokio::spawn(async move { tool.execute(ctx, json!({"command": "sleep 30"})).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
        let output = task.await.unwrap();
        assert_eq!(
            output.error.as_deref(),
            Some("Unable to execute command: sleep 30")
        );
    }

    #[tokio::test]
    async fn background_is_rejected() {
        let directory = tempdir().unwrap();
        let output = shell(
            directory.path(),
            json!({"command": "echo hi", "background": true}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("Unable to execute command: echo hi")
        );
    }

    #[tokio::test]
    async fn truncation_total_counts_whole_file() {
        let directory = tempdir().unwrap();
        // ~400 KiB capture: the 50 KiB page window holds only the tail, so a
        // page-relative total would be far below 4000. No trailing newline.
        let output = shell(
            directory.path(),
            json!({"command": "i=1; while [ $i -le 3999 ]; do printf 'line %04d %090d\\n' $i $i; i=$((i+1)); done; printf 'line 4000 %090d' 4000"}),
        )
        .await;
        let text = output.content.as_str().unwrap();
        // `total` is the file-wide line count (a page-relative count would
        // say `of ~506`); `shown` stays page-relative (upstream
        // `shown = min(lines.length, maxLines)` on the maxBytes window).
        assert!(
            text.contains(" of 4000; full output saved to "),
            "unexpected: {}",
            text.chars().rev().take(200).collect::<String>()
        );
        assert!(!text.contains(" of 506;"));
    }

    #[test]
    fn denied_shell_env_falls_back_to_zsh_or_bash() {
        let lock = SHELL_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let directory = tempdir().unwrap();
        let fish = directory.path().join("fish");
        std::fs::write(&fish, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fish, std::fs::Permissions::from_mode(0o755)).unwrap();
        let original = std::env::var_os("SHELL");
        // SAFETY: serialized against other SHELL readers by SHELL_ENV_LOCK.
        unsafe { std::env::set_var("SHELL", &fish) };
        let tool = ShellTool::new();
        match &original {
            Some(value) => unsafe { std::env::set_var("SHELL", value) },
            None => unsafe { std::env::remove_var("SHELL") },
        }
        let picked = tool.shell.to_string_lossy().to_lowercase();
        assert!(
            picked.ends_with("/zsh") || picked.ends_with("/bash") || picked.ends_with("/sh"),
            "picked: {picked}"
        );
        assert!(!picked.contains("fish"), "picked: {picked}");
        drop(lock);
    }
}

#[cfg(test)]
mod rtk_integration_tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn ctx(directory: &Path) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.to_path_buf(),
            call_id: "call_rtk".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    /// Live end-to-end: a rewritable command runs through `rtk` and still
    /// produces output (RTK passes non-rewritten commands through, so this
    /// works whether or not rtk is installed). Ignored by default.
    #[tokio::test]
    #[ignore = "requires rtk on PATH"]
    async fn shell_runs_rewritten_command() {
        let _ = TempDir::new();
        let dir = std::env::temp_dir();
        let tool = ShellTool::with_shell(std::path::PathBuf::from("/bin/sh"));
        let output = tool
            .execute(ctx(&dir), json!({"command": "git status"}))
            .await;
        assert!(output.error.is_none(), "unexpected error: {output:?}");
        // Whatever rtk does, the tool still returns text (never blank-fails).
        assert!(output.content.is_string());
    }
}
