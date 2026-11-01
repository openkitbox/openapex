//! RTK (`rtk`) shell-command rewriting — the pre-execution compression seam.
//!
//! RTK compresses command output before an agent reads it by rewriting the
//! command (`git status` → `rtk git status`). It ships an OpenCode
//! integration, and ox is an OpenCode-compatible server, so the seam is the
//! same: hand the command to `rtk hook opencode <command>` and run what it
//! answers. RTK owns every rewrite rule; ox holds none.
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
//!   the whole session (RTK's own escape hatch).
//!
//! Rewriting replaces the old in-engine minimizer: there is no second,
//! compressed copy of the output anywhere in ox. Two consequences are baked
//! into the shell tool rather than hidden here:
//!
//! - RTK buffers a wrapped command's output until the command exits (measured
//!   against rtk 1.1.0), so commands that return an owned session at once are
//!   never rewritten — their whole point is streaming output.
//! - The raw original text is not kept by ox; `rtk recall` is RTK's recovery
//!   path, not ours.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use ox_core::which::find_on_path;
use serde_json::Value;
use tokio::sync::OnceCell;

/// How long a single `rtk hook opencode` invocation may take before the
/// rewrite is abandoned (the command runs unchanged). Matches the RTK
/// plugin's default; the measured local latency is ~10 ms.
const HOOK_TIMEOUT: Duration = Duration::from_secs(3);

/// The binary name to resolve on `PATH`.
const BINARY: &str = "rtk";

/// Lazily-resolved RTK rewriter. A single instance is shared across every
/// shell call (one process-wide probe, then per-call hook invocations).
pub struct RtkRewriter {
    /// `Some(path)` = a capable rtk binary; `None` = absent or incapable.
    /// Resolved at most once.
    binary: OnceCell<Option<PathBuf>>,
    /// Per-invocation hook deadline; tests shorten it to stay fast.
    timeout: Duration,
}

impl RtkRewriter {
    /// A rewriter that resolves (and probes) the binary on first use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            binary: OnceCell::new(),
            timeout: HOOK_TIMEOUT,
        }
    }

    /// A rewriter that never rewrites: the fail-open path made explicit, for
    /// callers whose output must not depend on the host's rtk installation.
    #[must_use]
    pub fn disabled() -> Self {
        let rewriter = Self::new();
        let _ = rewriter.binary.set(None);
        rewriter
    }

    /// A rewriter over an explicit binary, skipping the `PATH` lookup and the
    /// capability probe. Tests point this at a mock so no host rtk is needed.
    #[must_use]
    pub fn with_binary(path: PathBuf) -> Self {
        let rewriter = Self::new();
        let _ = rewriter.binary.set(Some(path));
        rewriter
    }

    /// Override the per-invocation hook deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Rewrite `command` through RTK, or `None` to keep it unchanged.
    ///
    /// Returns `None` whenever rewriting is disabled, rtk is unavailable,
    /// the hook fails, or the answer is empty/same-as-input.
    pub async fn rewrite(&self, command: &str) -> Option<String> {
        if disabled() {
            return None;
        }
        let binary = self.binary().await?;
        let stdout = run(binary, &["hook", "opencode", command], self.timeout).await?;
        let answer: Value = serde_json::from_str(stdout.trim()).ok()?;
        let rewritten = answer
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty() && *text != command)?;
        Some(rewritten.to_owned())
    }

    /// The capable rtk binary, resolved once.
    async fn binary(&self) -> Option<&PathBuf> {
        self.binary
            .get_or_init(|| async {
                let path = find_on_path(BINARY)?;
                if probe(&path, self.timeout).await {
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

impl Default for RtkRewriter {
    fn default() -> Self {
        Self::new()
    }
}

/// `RTK_DISABLED=1` in the ox process environment disables rewriting for the
/// whole session (RTK's own escape hatch).
fn disabled() -> bool {
    std::env::var_os("RTK_DISABLED").is_some_and(|value| value == "1")
}

/// Whether `binary` supports the `hook opencode` subcommand. A broken or
/// wrong-architecture binary fails to spawn and reads as incapable.
async fn probe(binary: &PathBuf, timeout: Duration) -> bool {
    run(binary, &["hook", "opencode", "--help"], timeout)
        .await
        .is_some()
}

/// Run the binary with `args` and return its stdout, or `None` on any
/// failure (spawn error, timeout, non-zero exit, non-UTF-8 stdout).
async fn run(binary: &PathBuf, args: &[&str], timeout: Duration) -> Option<String> {
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
