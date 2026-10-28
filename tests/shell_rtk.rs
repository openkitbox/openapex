//! Integration tests for the RTK pre-execution rewrite in the shell tool.
//!
//! Every test drives a **mock rtk** (a shell script in a tempdir), never the
//! host's binary: the rewrite contract under test is `rtk hook opencode
//! <command>` answering `{"command": ...}` or `{}`, plus fail-open on every
//! error shape. The mock logs the commands it was asked about, which also
//! proves ox passes command strings through verbatim.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ox::runner::{Tool, ToolContext, ToolRegistry};
use ox::tool::rtk::RtkRewriter;
use ox::tool::shell::ShellTool;
use ox::tool::shell::sessions::ShellSessions;
use serde_json::json;

#[cfg(unix)]
/// The mock's behaviour table, keyed by the command ox hands it.
const MOCK: &str = r#"#!/bin/sh
if [ "$1" != "hook" ] || [ "$2" != "opencode" ]; then
  exit 0
fi
cmd="$3"
printf '%s\n' "$cmd" >> "__LOG__"
case "$cmd" in
  "git status") printf '{"command":"echo rewritten-output"}\n' ;;
  "git slow") printf '{"command":"sleep 30"}\n' ;;
  "echo badjson") printf 'not-json\n' ;;
  "echo hookfail") exit 1 ;;
  "echo same") printf '{"command":"echo same"}\n' ;;
  "echo empty") printf '{"command":""}\n' ;;
  "echo slowhook") sleep 5; printf '{}\n' ;;
  *) printf '{}\n' ;;
esac
"#;

#[cfg(unix)]
/// Write the mock rtk and return `(binary, log)` paths.
fn mock_rtk(dir: &Path) -> (PathBuf, PathBuf) {
    let binary = dir.join("rtk-mock.sh");
    let log = dir.join("rtk-mock.log");
    std::fs::write(&binary, MOCK.replace("__LOG__", &log.display().to_string())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (binary, log)
}

fn ctx(directory: &Path) -> ToolContext {
    ToolContext {
        session_id: "ses_rtk".to_owned(),
        directory: directory.to_path_buf(),
        call_id: "call_rtk".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: Arc::new(ToolRegistry::new()),
    }
}

/// A tool over a private session table and the given rewriter.
fn tool(rewriter: RtkRewriter) -> ShellTool {
    ShellTool::with_sessions(Arc::new(ShellSessions::default())).with_rewriter(rewriter)
}

/// Run `command` to settlement and return the model-facing text.
async fn run_text(tool: &ShellTool, dir: &Path, command: &str) -> String {
    let output = tool
        .execute(ctx(dir), json!({"command": command, "timeout": 20_000}))
        .await;
    assert!(
        output.error.is_none(),
        "tool error for `{command}`: {output:?}"
    );
    output.content.as_str().expect("text content").to_owned()
}

#[cfg(unix)]
#[tokio::test]
async fn rewritten_command_is_what_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, log) = mock_rtk(dir.path());
    let tool = tool(RtkRewriter::with_binary(binary));

    let text = run_text(&tool, dir.path(), "git status").await;
    assert!(
        text.contains("rewritten-output"),
        "rewrite not executed: {text}"
    );
    let logged = std::fs::read_to_string(log).unwrap();
    assert_eq!(logged.trim(), "git status", "hook saw the original command");
}

#[cfg(unix)]
#[tokio::test]
async fn no_op_answer_runs_the_original_command() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, _) = mock_rtk(dir.path());
    let tool = tool(RtkRewriter::with_binary(binary));
    assert_eq!(
        run_text(&tool, dir.path(), "echo plain").await.trim(),
        "plain"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn every_bad_answer_fails_open() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, _) = mock_rtk(dir.path());
    let tool = tool(RtkRewriter::with_binary(binary));
    for command in ["echo badjson", "echo hookfail", "echo same", "echo empty"] {
        let expected = command.strip_prefix("echo ").unwrap();
        let text = run_text(&tool, dir.path(), command).await;
        assert_eq!(text.trim(), expected, "`{command}` did not run unchanged");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hook_timeout_fails_open_without_blocking_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, _) = mock_rtk(dir.path());
    let tool = tool(RtkRewriter::with_binary(binary).with_timeout(Duration::from_millis(200)));

    let start = Instant::now();
    let text = run_text(&tool, dir.path(), "echo slowhook").await;
    assert_eq!(text.trim(), "slowhook");
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "the hook deadline was not honoured: {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn missing_binary_fails_open() {
    let dir = tempfile::tempdir().unwrap();
    let tool = tool(RtkRewriter::with_binary(dir.path().join("no-such-rtk")));
    assert_eq!(run_text(&tool, dir.path(), "echo hi").await.trim(), "hi");
}

#[cfg(unix)]
#[tokio::test]
async fn commands_reach_the_hook_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, log) = mock_rtk(dir.path());
    let tool = tool(RtkRewriter::with_binary(binary));

    for command in ["echo a | cat", "echo one && echo two"] {
        run_text(&tool, dir.path(), command).await;
    }
    let logged = std::fs::read_to_string(log).unwrap();
    assert_eq!(
        logged.lines().collect::<Vec<_>>(),
        vec!["echo a | cat", "echo one && echo two"],
        "ox must not parse or re-segment the command itself"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn background_commands_are_never_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, _) = mock_rtk(dir.path());
    let sessions = Arc::new(ShellSessions::default());
    let tool = ShellTool::with_sessions(Arc::clone(&sessions))
        .with_rewriter(RtkRewriter::with_binary(binary))
        .with_completion(Arc::new(|_, _| {}));

    // Both routes to an immediately-owned session: the legacy `background`
    // flag and the documented `yield_time_ms=0`.
    for input in [
        json!({"command": "git status", "background": true, "timeout": 0}),
        json!({"command": "git status", "yield_time_ms": 0, "timeout": 0}),
    ] {
        let output = tool.execute(ctx(dir.path()), input).await;
        assert!(output.error.is_none(), "{output:?}");
        let id = output.metadata.as_ref().unwrap()["shellID"]
            .as_str()
            .unwrap()
            .to_owned();
        let session = sessions.get(&id).expect("background session");
        assert_eq!(
            session.command, "git status",
            "background session was rewritten"
        );
        session.stop(true).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rewritten_running_session_says_output_is_buffered() {
    let dir = tempfile::tempdir().unwrap();
    let (binary, _) = mock_rtk(dir.path());
    let sessions = Arc::new(ShellSessions::default());
    let tool = ShellTool::with_sessions(Arc::clone(&sessions))
        .with_rewriter(RtkRewriter::with_binary(binary));

    // `git slow` rewrites to `sleep 30`, which outlives the short yield
    // window. The window is non-zero, so this IS a rewritable foreground run.
    let output = tool
        .execute(
            ctx(dir.path()),
            json!({"command": "git slow", "yield_time_ms": 50, "timeout": 0}),
        )
        .await;
    assert!(output.error.is_none(), "{output:?}");
    assert!(
        output
            .content
            .as_str()
            .unwrap_or_default()
            .contains("buffers its output until it exits"),
        "missing buffering notice: {}",
        output.content
    );
    let id = output.metadata.as_ref().unwrap()["shellID"]
        .as_str()
        .unwrap()
        .to_owned();
    let session = sessions.get(&id).expect("running session");
    assert_eq!(session.command, "sleep 30");
    session.stop(true).await;
}

/// Live check against the host's real rtk: the production `PATH`-probing
/// constructor must find it, and a real `git status` must come back in rtk's
/// compressed shape. Ignored by default because it needs rtk installed.
#[tokio::test]
#[ignore = "requires rtk on PATH"]
async fn live_git_status_runs_through_real_rtk() {
    let rewriter = RtkRewriter::new();
    assert_eq!(
        rewriter.rewrite("git status").await.as_deref(),
        Some("rtk git status")
    );

    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tool = tool(rewriter);
    let output = tool
        .execute(
            ctx(&repo),
            json!({"command": "git status", "timeout": 20_000}),
        )
        .await;
    assert!(output.error.is_none(), "{output:?}");
    let text = output.content.as_str().unwrap_or_default();
    assert_eq!(output.metadata.as_ref().unwrap()["exit"], 0);
    assert!(
        !text.contains("On branch"),
        "expected rtk's compressed status, got raw git output: {text}"
    );
}
