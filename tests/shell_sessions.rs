//! End-to-end coverage for the shell session model: one tool call starts a
//! command, a second observes or feeds it, and a third stops it. The HTTP
//! side of the same table is exercised through `/api/shell`.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox::runner::ToolContext;
use ox::tool::shell::ShellTool;

/// Run one tool call against a temp workspace.
async fn call(tool: &ShellTool, ctx: &ToolContext, input: Value) -> ox::runner::ToolOutput {
    use ox::runner::Tool;
    tool.execute(ctx.clone(), input).await
}

fn context(directory: &std::path::Path) -> ToolContext {
    ToolContext {
        session_id: "ses_shell".to_owned(),
        directory: directory.to_path_buf(),
        call_id: "call_shell".to_owned(),
        cancellation: CancellationToken::new(),
        registry: std::sync::Arc::new(ox::runner::ToolRegistry::new()),
    }
}

fn session_id(output: &ox::runner::ToolOutput) -> String {
    output
        .metadata
        .as_ref()
        .and_then(|metadata| metadata["shellID"].as_str())
        .expect("session metadata")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fast_command_settles_within_one_call() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(&tool, &ctx, json!({ "command": "echo hello" })).await;
    assert_eq!(output.error, None);
    assert_eq!(output.content, json!("hello\n"));
    let metadata = output.metadata.expect("metadata");
    assert_eq!(metadata["status"], "completed");
    assert_eq!(metadata["exit"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_command_returns_a_session_that_interact_observes() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "command": "echo started; sleep 30", "yield_time_ms": 50 }),
    )
    .await;
    assert_eq!(output.error, None);
    let metadata = output.metadata.as_ref().expect("metadata");
    assert_eq!(metadata["status"], "running");
    let id = session_id(&output);
    assert!(
        output
            .content
            .as_str()
            .unwrap()
            .contains(&format!("action=\"interact\" and session_id={id}"))
    );

    // The engine has a startup latency (embedded brush session), so poll the
    // session with bounded waits instead of assuming the first observe lands
    // after `echo started`.
    let mut observed = String::new();
    for _ in 0..50 {
        let output = call(
            &tool,
            &ctx,
            json!({ "action": "interact", "session_id": id, "yield_time_ms": 200 }),
        )
        .await;
        observed.push_str(output.content.as_str().unwrap_or_default());
        if observed.contains("started") {
            break;
        }
    }
    assert!(
        observed.contains("started"),
        "unexpected output: {observed}"
    );

    let stopped = call(&tool, &ctx, json!({ "action": "stop", "session_id": id })).await;
    assert_eq!(stopped.error, None);
    assert_eq!(stopped.metadata.as_ref().unwrap()["status"], "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_commands_see_eof_not_a_hang() {
    // pi-shell runs commands with stdin at /dev/null and offers no writer, so
    // a command that would block on input fails fast instead. This is the
    // documented replacement for the old `interact.chars` feeding.
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "command": "read line; echo got:$line", "yield_time_ms": 2_000 }),
    )
    .await;
    assert_eq!(output.error, None);
    let metadata = output.metadata.as_ref().expect("metadata");
    assert_eq!(metadata["status"], "completed", "{output:?}");
    assert!(
        output.content.as_str().unwrap().contains("got:"),
        "unexpected: {output:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interact_without_a_session_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "action": "interact", "session_id": "sh_missing" }),
    )
    .await;
    assert_eq!(
        output.error.as_deref(),
        Some("Unknown shell session: sh_missing")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_without_a_session_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "action": "stop", "session_id": "sh_missing" }),
    )
    .await;
    assert_eq!(
        output.error.as_deref(),
        Some("Unknown shell session: sh_missing")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_settles_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "command": "sleep 5", "timeout": 100, "yield_time_ms": 2_000 }),
    )
    .await;
    let text = output.content.as_str().unwrap();
    assert!(text.contains("Timed out before completion"), "{text}");
    assert!(
        text.contains("Command exceeded timeout of 100 ms."),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_action_is_an_input_error() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let output = call(&tool, &ctx, json!({ "action": "resume" })).await;
    assert!(
        output
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Expected one of \"run\" | \"interact\" | \"stop\"")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_call_stops_its_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new();
    let ctx = context(directory.path());
    let token = ctx.cancellation.clone();
    let task =
        tokio::spawn(async move { call(&tool, &ctx, json!({ "command": "sleep 30" })).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    token.cancel();
    let output = task.await.expect("join");
    assert_eq!(
        output.error.as_deref(),
        Some("Unable to execute command: sleep 30")
    );
}

// ---------------------------------------------------------------------------
// Idle eviction: the table otherwise only grows (every tool call spawns one
// entry and only an explicit DELETE removes it).
// ---------------------------------------------------------------------------

mod eviction {
    use super::*;

    use ox::tool::shell::sessions::{SHELL_SESSION_TTL_MS, ShellSessions, SpawnRequest};

    fn request(command: &str, timeout_ms: u64) -> SpawnRequest {
        SpawnRequest {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            timeout_ms: Some(timeout_ms),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_settled_session_is_evicted_after_the_ttl() {
        let table = ShellSessions::new();
        let session = table.spawn(request("echo hi", 10_000)).unwrap();
        assert!(
            session.wait_settled(Duration::from_secs(5)).await,
            "the command settles"
        );
        let now = ox::schema::id::now_millis();
        table.evict_idle(now);
        assert!(
            table.get(&session.id).is_some(),
            "a freshly settled session survives the sweep"
        );
        table.evict_idle(now + SHELL_SESSION_TTL_MS + 1_000);
        assert!(
            table.get(&session.id).is_none(),
            "a session settled past the TTL is evicted"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_session_is_never_evicted() {
        let table = ShellSessions::new();
        let session = table.spawn(request("sleep 30", 500)).unwrap();
        table.evict_idle(ox::schema::id::now_millis() + SHELL_SESSION_TTL_MS + 1_000);
        assert!(
            table.get(&session.id).is_some(),
            "a running session (no completion timestamp) survives any sweep"
        );
        let _ = session.wait_settled(Duration::from_secs(5)).await;
    }
}
