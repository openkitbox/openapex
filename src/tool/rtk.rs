//! RTK (`rtk`) shell-command rewriting — path A integration.
//!
//! RTK ("Rust Token Killer") compresses command output before an agent reads
//! it by rewriting the command (`git status` → `rtk git status`). It already
//! ships an OpenCode integration (`hooks/opencode/rtk.ts` in the RTK repo),
//! and ox is an OpenCode-compatible server, so the seam is the same: hand the
//! command to `rtk hook opencode <command>` and run what it answers.
//!
//! This module is a **thin, optional, fail-open** caller:
//!
//! - The `rtk` binary is discovered on `PATH` (never by sourcing rc files —
//!   the shell tool runs with the process PATH, so a bare `rtk …` must be
//!   spawnable); discovery and capability are resolved once, lazily.
//! - Capability is probed with `rtk hook opencode --help` (exit 0 = the
//!   subcommand exists). This mirrors RTK's own OpenCode plugin, which uses
//!   the same probe instead of a version number.
//! - Every failure — absent binary, non-zero exit, timeout, non-JSON stdout,
//!   an empty or identical answer — returns `None`, so the caller runs the
//!   command **unchanged**. A broken rtk can never block a tool call.
//! - `RTK_DISABLED=1` in the ox process environment disables rewriting for
//!   the whole session (RTK's own hook checks this; the `rtk hook opencode`
//!   subprocess does not, because that check lives in the plugin shim).
//!
//! The rewrite rules themselves are RTK's single source of truth
//! (`rtk hook opencode`); ox holds none of them.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::OnceCell;

use crate::which;

/// How long a single `rtk hook opencode` invocation may take before the
/// rewrite is abandoned (the command runs unchanged). Matches the RTK
/// plugin's default.
const HOOK_TIMEOUT: Duration = Duration::from_secs(3);

/// The binary name to resolve on `PATH`.
const BINARY: &str = "rtk";

/// Lazily-resolved RTK rewriter. A single instance is shared across every
/// shell call (one process-wide probe, then per-call hook invocations).
pub(crate) struct RtkRewriter {
    /// `Some(path)` = a capable rtk binary; `None` = absent or incapable.
    /// Resolved at most once.
    binary: OnceCell<Option<PathBuf>>,
}

impl RtkRewriter {
    /// A rewriter that resolves (and probes) the binary on first use.
    pub(crate) fn new() -> Self {
        Self {
            binary: OnceCell::new(),
        }
    }

    /// Rewrite `command` through RTK, or `None` to keep it unchanged.
    ///
    /// Returns `None` whenever rewriting is disabled, rtk is unavailable,
    /// the hook fails, or the answer is empty/same-as-input.
    pub(crate) async fn rewrite(&self, command: &str) -> Option<String> {
        if disabled() {
            return None;
        }
        let binary = self.binary().await?;
        rewrite_with(binary, command).await
    }

    /// The capable rtk binary, resolved once.
    async fn binary(&self) -> Option<&PathBuf> {
        self.binary
            .get_or_init(|| async {
                let path = which::find_on_path(BINARY)?;
                if probe(&path).await {
                    tracing::debug!(path = %path.display(), "rtk: hook opencode available");
                    Some(path)
                } else {
                    tracing::debug!(
                        path = %path.display(),
                        "rtk: binary found but lacks `hook opencode`; rewriting disabled"
                    );
                    None
                }
            })
            .await
            .as_ref()
    }
}

/// `RTK_DISABLED=1` in the ox process environment disables rewriting for the
/// whole session (RTK's own escape hatch).
fn disabled() -> bool {
    std::env::var_os("RTK_DISABLED").is_some_and(|value| value == "1")
}

/// Whether `binary` supports the `hook opencode` subcommand. A broken or
/// wrong-architecture binary fails to spawn and reads as incapable.
async fn probe(binary: &PathBuf) -> bool {
    run(binary, &["hook", "opencode", "--help"]).await.is_some()
}

/// Invoke `rtk hook opencode <command>` and read the answered rewrite.
/// `None` on any failure or on a no-op answer (`{}` / same command).
async fn rewrite_with(binary: &PathBuf, command: &str) -> Option<String> {
    let stdout = run(binary, &["hook", "opencode", command]).await?;
    let answer: Value = serde_json::from_str(stdout.trim()).ok()?;
    let rewritten = answer
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty() && *text != command)?;
    Some(rewritten.to_owned())
}

/// Run the binary with `args` and return trimmed stdout, or `None` on any
/// failure (spawn error, timeout, non-zero exit, non-UTF-8 stdout).
async fn run(binary: &PathBuf, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(HOOK_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_skips_missing_names() {
        // A name that cannot exist resolves to None.
        assert!(which::find_on_path("ox-definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn find_on_path_locates_an_executable_binary() {
        // `sh` is on PATH on every unix; skip if the environment is unusual.
        // The shared finder rejects non-executable files on Unix, so a hit
        // must carry the executable bit.
        if let Some(path) = which::find_on_path("sh") {
            assert!(path.is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert!(mode & 0o111 != 0, "not executable: {}", path.display());
            }
        }
    }

    #[tokio::test]
    async fn absent_binary_resolves_to_no_rewrite() {
        // An instance whose probe would look for a missing binary never
        // rewrites. We can't override PATH safely here (shared process env),
        // so this asserts the fail-open contract on an obviously bogus name.
        let rewriter = RtkRewriter::new();
        // With no rtk on PATH (CI/dev without rtk), this is `None`; with rtk
        // installed, `echo hi` is not rewritable and is still `None`.
        assert_eq!(rewriter.rewrite("echo hi").await, None);
    }

    #[test]
    fn parse_answer_picks_command_field() {
        let answer: Value = serde_json::from_str(r#"{"command":"rtk git status"}"#).unwrap();
        assert_eq!(
            answer.get("command").and_then(Value::as_str),
            Some("rtk git status")
        );
        let empty: Value = serde_json::from_str("{}").unwrap();
        assert!(empty.get("command").is_none());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Live check against a real `rtk` on PATH. Ignored by default because it
    /// depends on the host having rtk installed.
    #[tokio::test]
    #[ignore = "requires rtk on PATH"]
    async fn live_rewrite_git_status() {
        let rewriter = RtkRewriter::new();
        assert_eq!(
            rewriter.rewrite("git status").await.as_deref(),
            Some("rtk git status")
        );
        // An unrewritable command stays unchanged.
        assert_eq!(rewriter.rewrite("echo hi").await, None);
    }
}
