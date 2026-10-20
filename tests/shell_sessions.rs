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

#[tokio::test]
async fn a_fast_command_settles_within_one_call() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
    let ctx = context(directory.path());
    let output = call(&tool, &ctx, json!({ "command": "echo hello" })).await;
    assert_eq!(output.error, None);
    assert_eq!(output.content, json!("hello\n"));
    let metadata = output.metadata.expect("metadata");
    assert_eq!(metadata["status"], "completed");
    assert_eq!(metadata["exit"], 0);
}

#[tokio::test]
async fn a_long_command_returns_a_session_that_interact_observes() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
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

    let observed = call(
        &tool,
        &ctx,
        json!({ "action": "interact", "session_id": id, "yield_time_ms": 20 }),
    )
    .await;
    assert!(observed.content.as_str().unwrap().contains("started"));

    let stopped = call(&tool, &ctx, json!({ "action": "stop", "session_id": id })).await;
    assert_eq!(stopped.error, None);
    assert_eq!(stopped.metadata.as_ref().unwrap()["status"], "completed");
}

#[tokio::test]
async fn interact_feeds_stdin_to_the_running_command() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
    let ctx = context(directory.path());
    // `read` blocks until stdin supplies a line.
    let started = call(
        &tool,
        &ctx,
        json!({ "command": "read line; echo got:$line", "yield_time_ms": 50 }),
    )
    .await;
    let id = session_id(&started);
    assert_eq!(started.metadata.as_ref().unwrap()["status"], "running");

    let fed = call(
        &tool,
        &ctx,
        json!({ "action": "interact", "session_id": id, "chars": "hello\n", "yield_time_ms": 500 }),
    )
    .await;
    let text = fed.content.as_str().unwrap();
    assert!(text.contains("got:hello"), "unexpected output: {text}");
}

#[tokio::test]
async fn interact_without_a_session_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
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

#[tokio::test]
async fn stop_without_a_session_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
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

#[tokio::test]
async fn a_timeout_settles_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
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

#[tokio::test]
async fn an_unknown_action_is_an_input_error() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
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

#[tokio::test]
async fn a_cancelled_call_stops_its_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::with_shell("/bin/sh".into());
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
