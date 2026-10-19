//! Gateway to the third-party `codebase-memory-mcp` binary.
//!
//! ox deliberately has **no MCP client**; this module adapts the binary's
//! one-shot `cli` mode instead: `<binary> cli --quiet <tool>`, with the tool
//! arguments as a single JSON object on stdin (the binary reads stdin to EOF,
//! so the writer must close it).
//!
//! The five registered tool names (`index_repository`, `search_graph`,
//! `trace_path`, `get_code_snippet`, `get_architecture`) are BARE names
//! borrowed from the binary's own CLI contract so its error hints match
//! verbatim; the borrow enters the registry only through `register_cbm`.
//!
//! Scope: process plumbing only. The `Tool` impls live in `cbm::tools`;
//! this module knows nothing about the model-facing schemas. The binary is
//! resolved through the shared [`crate::which`] helper, stderr is capped at
//! 4 KiB while still being drained, and the only execution control is
//! cancellation — there is no timeout. `list_projects` is the one
//! subcommand that takes its flags on the command line (`--format json`,
//! never stdin); every other tool reads a JSON object from stdin.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::runner::ToolOutput;
use crate::tool::{fail, ok_text};
use crate::which;

pub(crate) mod tools;

#[cfg(test)]
mod teststub;

/// Name of the binary on `PATH`.
const BINARY_NAME: &str = "codebase-memory-mcp";

/// The one spawn-failure message both subcommands report.
const NOT_FOUND: &str = "codebase-memory-mcp not found or not executable";

/// The cancellation failure message.
const CANCELLED: &str = "command cancelled";

/// Model-facing stderr is capped at 4 KiB; the binary self-bounds stdout.
const STDERR_CAP: usize = 4096;

/// Upper bound for reader-task cleanup after a cancelled or failed wait.
const READER_CLEANUP: Duration = Duration::from_millis(500);

/// A discovered `codebase-memory-mcp` binary.
pub struct Cbm {
    binary: PathBuf,
}

impl Cbm {
    /// A handle for `binary`. Used by [`Cbm::discover`] and by tests that
    /// install a stub in place of the real binary.
    pub(crate) fn new(binary: PathBuf) -> Self {
        Self { binary }
    }

    /// Probe `PATH` for the binary. `None` when it is absent, so callers can
    /// simply skip registering the CBM tools on machines without it.
    pub(crate) fn discover() -> Option<Self> {
        Some(Self::new(which::find_on_path(BINARY_NAME)?))
    }

    /// Run one tool through `<binary> cli --quiet <tool>`.
    ///
    /// `args` is written to stdin as one JSON object and the pipe is closed
    /// (the binary blocks until EOF). Exit 0 returns the trimmed stdout;
    /// a non-zero exit returns the capped stderr diagnostic. The only
    /// execution control is `cancel` — there is deliberately no timeout.
    async fn call(
        &self,
        tool: &str,
        args: &Value,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let payload = match serde_json::to_vec(args) {
            Ok(payload) => payload,
            // Defensive: serializing a `serde_json::Value` is infallible today.
            Err(error) => return fail(format!("could not serialize tool arguments: {error}")),
        };
        match self.run(tool, &[], cwd, Some(&payload), cancel).await {
            Ok(ran) if ran.status.success() => {
                ok_text(String::from_utf8_lossy(&ran.stdout).trim_end().to_owned())
            }
            Ok(ran) => failure_output(&ran.status, &ran.stderr),
            Err(output) => output,
        }
    }

    /// Resolve which indexed CBM project covers `dir`.
    ///
    /// `list_projects` takes its flags on the command line and never reads
    /// stdin, so it runs with stdin closed and `dir` as its working
    /// directory. The project whose `root_path` is the longest prefix of
    /// `dir` wins (see [`longest_prefix`]); no match is a model-facing
    /// failure naming the directory and the available project names.
    async fn project_for(
        &self,
        dir: &Path,
        cancel: &CancellationToken,
    ) -> Result<String, ToolOutput> {
        let ran = self
            .run("list_projects", &["--format", "json"], dir, None, cancel)
            .await?;
        if !ran.status.success() {
            return Err(failure_output(&ran.status, &ran.stderr));
        }
        let response: ProjectsResponse = serde_json::from_slice(&ran.stdout).map_err(|error| {
            fail(format!(
                "codebase-memory-mcp list_projects returned invalid JSON: {error}"
            ))
        })?;
        match longest_prefix(dir, &response.projects) {
            Some(name) => Ok(name.to_owned()),
            None => {
                let names: Vec<&str> = response
                    .projects
                    .iter()
                    .map(|project| project.name.as_str())
                    .collect();
                let available = if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                };
                Err(fail(format!(
                    "no indexed project covers {}; available projects: {available}",
                    dir.display()
                )))
            }
        }
    }

    /// The shared command base for both subcommands. `extra` holds
    /// per-subcommand flags appended after the tool name (`call` passes
    /// none, `project_for` passes `--format json`); callers choose only
    /// stdin: piped for tools that read a JSON object, null for
    /// `list_projects`.
    fn command(&self, tool: &str, extra: &[&str], cwd: &Path) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("cli")
            .arg("--quiet")
            .arg(tool)
            .args(extra)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Spawn one subcommand and wait for it, honoring `cancel`. The readers
    /// run on separate tasks so a full stdout pipe can never deadlock the
    /// wait; a pre-cancelled token returns before spawning anything.
    async fn run(
        &self,
        tool: &str,
        extra: &[&str],
        cwd: &Path,
        payload: Option<&[u8]>,
        cancel: &CancellationToken,
    ) -> Result<Ran, ToolOutput> {
        if cancel.is_cancelled() {
            return Err(fail(CANCELLED));
        }
        let mut command = self.command(tool, extra, cwd);
        command.stdin(if payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return Err(fail(NOT_FOUND)),
        };
        if let (Some(payload), Some(mut stdin)) = (payload, child.stdin.take()) {
            // A write error means the child already exited; its exit status is
            // the diagnostic that matters, so it is reported below instead.
            let _ = stdin.write_all(payload).await;
            // Dropping `stdin` closes the pipe — the child waits for this EOF.
        }
        let stdout = tokio::spawn(read_all(child.stdout.take(), None));
        let stderr = tokio::spawn(read_all(child.stderr.take(), Some(STDERR_CAP)));
        let outcome = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => Outcome::Completed(status),
                Err(_) => Outcome::WaitFailed,
            },
            () = cancel.cancelled() => Outcome::Cancelled,
        };
        match outcome {
            Outcome::Completed(status) => {
                let stdout = match stdout.await {
                    Ok(Ok(stdout)) => stdout,
                    Ok(Err(error)) => {
                        // The child is reaped; the sibling reader must still
                        // be stopped rather than left running on a full pipe.
                        stderr.abort();
                        let _ = stderr.await;
                        return Err(fail(format!(
                            "could not read codebase-memory-mcp output: {error}"
                        )));
                    }
                    Err(_) => {
                        stderr.abort();
                        let _ = stderr.await;
                        return Err(fail("codebase-memory-mcp output reader failed"));
                    }
                };
                let stderr = match stderr.await {
                    Ok(Ok(stderr)) => stderr,
                    // Unreadable diagnostics must not fail a successful call.
                    Ok(Err(_)) | Err(_) if status.success() => Vec::new(),
                    Ok(Err(error)) => {
                        return Err(fail(format!(
                            "could not read codebase-memory-mcp diagnostics: {error}"
                        )));
                    }
                    Err(_) => {
                        return Err(fail("codebase-memory-mcp diagnostics reader failed"));
                    }
                };
                Ok(Ran {
                    status,
                    stdout,
                    stderr,
                })
            }
            Outcome::Cancelled => {
                reap(child, stdout, stderr).await;
                Err(fail(CANCELLED))
            }
            Outcome::WaitFailed => {
                reap(child, stdout, stderr).await;
                Err(fail("could not wait for codebase-memory-mcp"))
            }
        }
    }
}

/// The select outcome; the kill path is shared by cancellation and a failed
/// `wait` so both reap the child exactly once.
enum Outcome {
    Completed(ExitStatus),
    WaitFailed,
    Cancelled,
}

/// One completed invocation: exit status plus the captured streams.
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    /// Already capped at [`STDERR_CAP`] bytes by [`read_all`].
    stderr: Vec<u8>,
}

/// Kill and reap the child, then drop the pipe readers. Grandchildren can
/// inherit the pipes and hold them open; reader cleanup is bounded so a
/// cancelled call never waits on them.
async fn reap(
    mut child: Child,
    stdout: JoinHandle<std::io::Result<Vec<u8>>>,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    stdout.abort();
    stderr.abort();
    let _ = tokio::time::timeout(READER_CLEANUP, async {
        let _ = stdout.await;
        let _ = stderr.await;
    })
    .await;
}

/// The model-facing failure for a non-zero exit: the binary's diagnostic, or
/// its exit status when the diagnostic was empty or unreadable.
fn failure_output(status: &ExitStatus, stderr: &[u8]) -> ToolOutput {
    let diagnostic = cap_stderr(stderr);
    if diagnostic.is_empty() {
        fail(format!("codebase-memory-mcp exited with {status}"))
    } else {
        fail(diagnostic)
    }
}

/// `list_projects --format json` response shape.
#[derive(Deserialize)]
struct ProjectsResponse {
    projects: Vec<ProjectEntry>,
}

/// One indexed project from `list_projects`.
#[derive(Deserialize)]
struct ProjectEntry {
    name: String,
    root_path: PathBuf,
}

/// The project whose root is the longest prefix of `dir`.
///
/// Both sides are compared on one basis per call: when `dir` canonicalizes,
/// only roots that also canonicalize are considered and canonicals are
/// compared; otherwise the comparison is purely lexical. Empty roots are
/// skipped.
fn longest_prefix<'a>(dir: &Path, projects: &'a [ProjectEntry]) -> Option<&'a str> {
    match dir.canonicalize() {
        Ok(canonical_dir) => projects
            .iter()
            .filter_map(|project| {
                if project.root_path.as_os_str().is_empty() {
                    return None;
                }
                let root = project.root_path.canonicalize().ok()?;
                canonical_dir
                    .starts_with(&root)
                    .then_some((project.name.as_str(), root.as_os_str().len()))
            })
            .max_by_key(|(_, length)| *length)
            .map(|(name, _)| name),
        Err(_) => projects
            .iter()
            .filter_map(|project| {
                if project.root_path.as_os_str().is_empty() {
                    return None;
                }
                dir.starts_with(&project.root_path)
                    .then_some((project.name.as_str(), project.root_path.as_os_str().len()))
            })
            .max_by_key(|(_, length)| *length)
            .map(|(name, _)| name),
    }
}

/// Read `pipe` to EOF, keeping at most `cap` bytes while still draining the
/// rest (so the child never blocks on a full pipe). `None` keeps everything.
async fn read_all<R>(pipe: Option<R>, cap: Option<usize>) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok(kept);
        }
        match cap {
            Some(cap) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..read.min(room)]);
            }
            None => kept.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Lossy stderr capped at [`STDERR_CAP`] bytes without splitting a UTF-8
/// character (the binary emits JSON diagnostics; truncation must not panic).
fn cap_stderr(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut end = text.len().min(STDERR_CAP);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::teststub::{MAIN_STUB, SILENT_STUB, SLOW_STUB, STDERR_STUB, stub, write_projects};
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    #[tokio::test]
    async fn call_forwards_tool_and_arguments() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let args = json!({"project": "ox", "name_pattern": "Registry"});
        let output = cbm
            .call(
                "search_graph",
                &args,
                directory.path(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(output.content, json!("stub search_graph ok"));
        assert!(output.error.is_none());
        let captured =
            std::fs::read_to_string(directory.path().join(".cbm-stub-stdin.json")).unwrap();
        assert_eq!(captured, serde_json::to_string(&args).unwrap());
    }

    #[tokio::test]
    async fn call_passes_expected_argv() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let output = cbm
            .call(
                "search_graph",
                &json!({}),
                directory.path(),
                &CancellationToken::new(),
            )
            .await;
        assert!(output.error.is_none());
        let argv = std::fs::read_to_string(directory.path().join(".cbm-stub-argv")).unwrap();
        assert_eq!(argv, "cli\n--quiet\nsearch_graph\n");
    }

    #[tokio::test]
    async fn project_for_passes_format_json_argv() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        write_projects(
            directory.path(),
            json!({"projects": [{"name": "ox", "root_path": directory.path().to_string_lossy()}]}),
        );
        assert_eq!(
            cbm.project_for(directory.path(), &CancellationToken::new())
                .await
                .unwrap(),
            "ox"
        );
        let argv = std::fs::read_to_string(directory.path().join(".cbm-stub-argv")).unwrap();
        assert_eq!(argv, "cli\n--quiet\nlist_projects\n--format\njson\n");
    }

    #[tokio::test]
    async fn call_runs_in_given_cwd() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let cwd = directory.path().join("nested/work");
        std::fs::create_dir_all(&cwd).unwrap();
        let output = cbm
            .call("search_graph", &json!({}), &cwd, &CancellationToken::new())
            .await;
        assert!(output.error.is_none());
        assert!(cwd.join(".cbm-stub-stdin.json").is_file());
    }

    #[tokio::test]
    async fn call_reports_stderr_verbatim_on_failure() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let output = cbm
            .call(
                "search_graph",
                &json!({"cbm_fail": true}),
                directory.path(),
                &CancellationToken::new(),
            )
            .await;
        assert!(output.content.is_null());
        assert_eq!(
            output.error.as_deref(),
            Some(r#"{"error":"stub boom","hint":"fix it"}"#)
        );
    }

    #[tokio::test]
    async fn call_reports_silent_failure_status() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), SILENT_STUB);
        let output = cbm
            .call(
                "search_graph",
                &json!({}),
                directory.path(),
                &CancellationToken::new(),
            )
            .await;
        let message = output.error.expect("status failure");
        assert!(
            message.starts_with("codebase-memory-mcp exited with "),
            "{message}"
        );
    }

    #[tokio::test]
    async fn call_reports_missing_binary() {
        let directory = tempdir().unwrap();
        let cbm = Cbm::new(directory.path().join("missing-codebase-memory-mcp"));
        let output = cbm
            .call(
                "search_graph",
                &json!({}),
                directory.path(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(output.error.as_deref(), Some(NOT_FOUND));
    }

    #[tokio::test]
    async fn call_cancellation_kills_in_flight_child() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), SLOW_STUB);
        let token = CancellationToken::new();
        let cancel = token.clone();
        let cwd = directory.path().to_path_buf();
        let task =
            tokio::spawn(async move { cbm.call("search_graph", &json!({}), &cwd, &cancel).await });
        // Wait until the stub is past spawn and inside its sleep: cancelling
        // earlier would only exercise the pre-spawn guard.
        let marker = directory.path().join(".cbm-stub-started");
        let started = Instant::now();
        while !marker.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "stub never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let cancelled_at = Instant::now();
        token.cancel();
        let output = task.await.unwrap();
        assert_eq!(output.error.as_deref(), Some(CANCELLED));
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(2),
            "cancellation took {:?}",
            cancelled_at.elapsed()
        );
    }

    #[tokio::test]
    async fn pre_cancelled_call_never_spawns() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), SLOW_STUB);
        let token = CancellationToken::new();
        token.cancel();
        let output = cbm
            .call("search_graph", &json!({}), directory.path(), &token)
            .await;
        assert_eq!(output.error.as_deref(), Some(CANCELLED));
        assert!(!directory.path().join(".cbm-stub-started").exists());
        assert!(!directory.path().join(".cbm-stub-stdin.json").exists());
    }

    #[tokio::test]
    async fn pre_cancelled_project_for_never_spawns() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), SLOW_STUB);
        let token = CancellationToken::new();
        token.cancel();
        let output = cbm.project_for(directory.path(), &token).await.unwrap_err();
        assert_eq!(output.error.as_deref(), Some(CANCELLED));
        assert!(!directory.path().join(".cbm-stub-started").exists());
    }

    #[tokio::test]
    async fn project_for_picks_longest_prefix_and_reports_no_match() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let workspace = directory.path().join("ws");
        let ox = workspace.join("ox");
        let src = ox.join("src");
        let elsewhere = directory.path().join("elsewhere");
        for path in [&workspace, &ox, &src, &elsewhere] {
            std::fs::create_dir_all(path).unwrap();
        }
        let projects = json!({
            "projects": [
                {"name": "ox", "root_path": ox.to_string_lossy()},
                {"name": "outer", "root_path": workspace.to_string_lossy()},
            ]
        });
        write_projects(&src, projects.clone());
        write_projects(&elsewhere, projects);

        assert_eq!(
            cbm.project_for(&src, &CancellationToken::new())
                .await
                .unwrap(),
            "ox"
        );

        let output = cbm
            .project_for(&elsewhere, &CancellationToken::new())
            .await
            .unwrap_err();
        let message = output.error.expect("no-match failure");
        assert!(message.contains("no indexed project"), "{message}");
        assert!(message.contains("ox"), "{message}");
        assert!(message.contains("outer"), "{message}");
    }

    #[tokio::test]
    async fn project_for_ignores_stale_and_empty_roots() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let workspace = directory.path().join("ws");
        let ox = workspace.join("ox");
        let src = ox.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let stale = workspace.join("ox-stale");
        let matching = json!({
            "projects": [
                {"name": "empty", "root_path": ""},
                {"name": "stale", "root_path": stale.to_string_lossy()},
                {"name": "ox", "root_path": ox.to_string_lossy()},
            ]
        });
        write_projects(&src, matching);
        assert_eq!(
            cbm.project_for(&src, &CancellationToken::new())
                .await
                .unwrap(),
            "ox"
        );

        // Without the live root, the stale and empty entries must not match.
        write_projects(
            &src,
            json!({
                "projects": [
                    {"name": "empty", "root_path": ""},
                    {"name": "stale", "root_path": stale.to_string_lossy()},
                ]
            }),
        );
        let output = cbm
            .project_for(&src, &CancellationToken::new())
            .await
            .unwrap_err();
        let message = output.error.expect("no-match failure");
        assert!(message.contains("no indexed project"), "{message}");
    }

    #[tokio::test]
    async fn project_for_matches_symlinked_root() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), MAIN_STUB);
        let workspace = directory.path().join("ws");
        let real = workspace.join("real");
        let src = real.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = workspace.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_projects(
            &src,
            json!({"projects": [{"name": "ox", "root_path": link.to_string_lossy()}]}),
        );
        // The directory is reached through the symlink too, so this only
        // matches when both sides are compared after canonicalization.
        assert_eq!(
            cbm.project_for(&link.join("src"), &CancellationToken::new())
                .await
                .unwrap(),
            "ox"
        );
    }

    #[tokio::test]
    async fn call_caps_stderr_at_char_boundary() {
        let directory = tempdir().unwrap();
        let cbm = stub(&directory.path().join("codebase-memory-mcp"), STDERR_STUB);
        // The multi-byte character straddles byte 4096: its first byte is at
        // offset 4095, so a naive `[..cap]` would split it.
        let stderr = format!("{}é{}", "a".repeat(STDERR_CAP - 1), "b".repeat(8 * 1024));
        std::fs::write(directory.path().join(".cbm-stub-stderr.txt"), stderr).unwrap();
        let output = cbm
            .call(
                "search_graph",
                &json!({}),
                directory.path(),
                &CancellationToken::new(),
            )
            .await;
        let message = output.error.expect("stderr failure");
        assert_eq!(message.len(), STDERR_CAP - 1);
        assert!(message.chars().all(|character| character == 'a'));
    }
}
