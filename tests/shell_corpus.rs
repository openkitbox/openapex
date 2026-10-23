//! Regression smoke over real shell commands from ox's own history.
//!
//! `tests/fixtures/shell-corpus.jsonl` holds commands from the stage-0 corpus
//! A/B (`~/.ox/tmp/shell-ab/`): the portable, read-only subset of commands that
//! produced byte-identical output under the host shell and brush. The paths
//! they reference live in the developer's repos, so most fail with a normal
//! non-zero exit inside a tempdir — that is fine. What this test pins is the
//! engine contract: every command parses, runs, and settles on its own, with
//! no engine-level error and no hang.

use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use ox::runner::{Tool, ToolContext};
use ox::tool::shell::ShellTool;

/// The fixture as `(command, exit code under the host shell)` pairs.
fn corpus() -> Vec<(String, i64)> {
    include_str!("fixtures/shell-corpus.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("bad fixture {line}: {error}"));
            let command = entry["command"].as_str().expect("command").to_owned();
            let exit = entry["zsh_exit"].as_i64().unwrap_or(0);
            (command, exit)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_corpus_command_settles_without_an_engine_error() {
    let corpus = corpus();
    assert!(corpus.len() >= 40, "corpus shrank: {}", corpus.len());
    let directory = tempfile::tempdir().expect("tempdir");
    let tool = ShellTool::new();
    for (command, _zsh_exit) in corpus {
        let ctx = ToolContext {
            session_id: "ses_corpus".to_owned(),
            directory: directory.path().to_path_buf(),
            call_id: "call_corpus".to_owned(),
            cancellation: CancellationToken::new(),
        };
        let input = json!({
            "command": command,
            // A five-second deadline and a wider yield window: a command that
            // needs longer is a regression worth failing on.
            "timeout": 5_000,
            "yield_time_ms": 8_000,
        });
        let output = tokio::time::timeout(Duration::from_secs(30), tool.execute(ctx, input))
            .await
            .unwrap_or_else(|_| panic!("command hung the engine: {command}"));
        assert!(
            output.error.is_none(),
            "engine error for `{command}`: {output:?}"
        );
        let metadata = output.metadata.as_ref().expect("session metadata");
        assert_eq!(
            metadata["status"], "completed",
            "command did not settle in its window: {command}\n{}",
            output.content
        );
    }
}
