//! End-to-end coverage for the shell session model: one tool call starts a
//! command, a second observes or feeds it, and a third stops it. The HTTP
//! side of the same table is exercised through `/api/shell`.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox::runner::ToolContext;
use ox::tool::rtk::RtkRewriter;
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
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
    let ctx = context(directory.path());
    let output = call(&tool, &ctx, json!({ "command": "echo hello" })).await;
    assert_eq!(output.error, None);
    assert_eq!(output.content, json!("hello\n"));
    let metadata = output.metadata.expect("metadata");
    assert_eq!(metadata["status"], "completed");
    assert_eq!(metadata["exit"], 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_command_returns_a_session_that_interact_observes() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "command": "echo started; /bin/sleep 30", "yield_time_ms": 50 }),
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
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
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
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
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
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_settles_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
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
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
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
async fn background_without_completion_sink_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
    let ctx = context(directory.path());
    let output = call(
        &tool,
        &ctx,
        json!({ "command": "echo should-not-run", "background": true }),
    )
    .await;
    assert_eq!(
        output.error.as_deref(),
        Some(
            "Background shell completion notification is unavailable; run in the foreground or configure a completion sink."
        )
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_call_stops_its_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = ShellTool::new().with_rewriter(RtkRewriter::disabled());
    let ctx = context(directory.path());
    let token = ctx.cancellation.clone();
    let task =
        tokio::spawn(async move { call(&tool, &ctx, json!({ "command": "/bin/sleep 30" })).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    token.cancel();
    let output = task.await.expect("join");
    assert_eq!(
        output.error.as_deref(),
        Some("Unable to execute command: /bin/sleep 30")
    );
}

mod completion {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use ox::tool::shell::sessions::{
        Completion, CompletionCallback, HandBack, ShellSessions, SpawnRequest, Status,
    };

    fn recorder() -> (CompletionCallback, mpsc::UnboundedReceiver<Completion>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let callback: CompletionCallback = Arc::new(move |completion| {
            sender.send(completion).expect("completion receiver");
        });
        (callback, receiver)
    }

    fn request(
        command: &str,
        timeout_ms: Option<u64>,
        callback: CompletionCallback,
    ) -> SpawnRequest {
        SpawnRequest {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            timeout_ms,
            completion: Some(callback),
            foreground: false,
            owner: None,
        }
    }

    async fn one(receiver: &mut mpsc::UnboundedReceiver<Completion>) -> Completion {
        tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .expect("completion delivered in time")
            .expect("completion callback")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn success_completion_contains_command_exit_and_bounded_output() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(request("echo complete", Some(10_000), callback))
            .unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        let completion = one(&mut completions).await;
        assert_eq!(completion.id, session.id);
        assert_eq!(completion.command, "echo complete");
        assert_eq!(completion.status, Status::Exited);
        assert_eq!(completion.exit, Some(0));
        assert_eq!(completion.output, "complete\n");
        assert!(completion.output.len() <= ox::tool::shell::sessions::OUTPUT_CAPACITY);
    }

    /// A panicking completion sink must not wedge the session: it still
    /// settles, `wait_settled` returns, and `stop` on the settled session
    /// returns instead of waiting for a terminal state that never comes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_completion_callback_still_settles_the_session() {
        let table = ShellSessions::new();
        let callback: CompletionCallback = Arc::new(|_| panic!("sink exploded"));
        let session = table
            .spawn(request("echo boom", Some(10_000), callback))
            .unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);
        assert_eq!(session.exit().status, Some(Status::Exited));

        tokio::time::timeout(Duration::from_secs(5), session.stop(true))
            .await
            .expect("stop returns on a settled session after a callback panic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn error_completion_preserves_nonzero_exit() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(request("exit 7", Some(10_000), callback))
            .unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        let completion = one(&mut completions).await;
        assert_eq!(completion.status, Status::Exited);
        assert_eq!(completion.exit, Some(7));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn timeout_completion_reports_timeout() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(request("/bin/sleep 5", Some(20), callback))
            .unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        assert_eq!(one(&mut completions).await.status, Status::Timeout);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn cancellation_completion_reports_killed() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(request("/bin/sleep 30", None, callback))
            .unwrap();
        session.stop(true).await;
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        assert_eq!(one(&mut completions).await.status, Status::Killed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn terminal_completion_is_delivered_once_when_stop_races_drive() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table.spawn(request("sleep 30", None, callback)).unwrap();
        let first = session.stop(true);
        let second = session.stop(true);
        tokio::join!(first, second);
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        let completion = one(&mut completions).await;
        assert_eq!(completion.status, Status::Killed);
        assert!(
            completions.try_recv().is_err(),
            "completion delivered twice"
        );
    }

    /// Opens its gate file on drop, so a failing assertion cannot leave a
    /// polling shell behind to hang the runtime shutdown.
    struct OpenOnDrop(std::path::PathBuf);

    impl Drop for OpenOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }

    fn foreground_request(command: &str, callback: CompletionCallback) -> SpawnRequest {
        SpawnRequest {
            foreground: true,
            owner: None,
            ..request(command, None, callback)
        }
    }

    /// Wait (bounded, event driven) for the gate file to exist.
    fn gated(gate: &std::path::Path, tail: &str) -> String {
        format!(
            "while [ ! -f {} ]; do /bin/sleep 0.02; done; {tail}",
            gate.display()
        )
    }

    /// A completion from a fresh background run proves every earlier
    /// delivery decision has been made and delivered, then asserts nothing
    /// else is queued.
    async fn barrier(
        table: &ShellSessions,
        receiver: &mut mpsc::UnboundedReceiver<Completion>,
        callback: CompletionCallback,
    ) {
        let session = table
            .spawn(request("echo barrier", None, callback))
            .unwrap();
        let completion = one(receiver).await;
        assert_eq!(
            completion.id, session.id,
            "an unexpected completion arrived"
        );
        assert!(receiver.try_recv().is_err(), "extra completion queued");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn a_foreground_run_that_settles_before_hand_back_never_notifies() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(foreground_request("echo inline", callback.clone()))
            .unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        assert_eq!(session.hand_back(), HandBack::Settled);
        barrier(&table, &mut completions, callback).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn a_foreground_run_handed_back_while_running_notifies_once_on_settle() {
        let dir = tempfile::tempdir().unwrap();
        let gate = dir.path().join("gate");
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(foreground_request(
                &gated(&gate, "echo late"),
                callback.clone(),
            ))
            .unwrap();
        // Held: the run is alive and nothing has been delivered.
        assert!(!session.settled());
        assert!(completions.try_recv().is_err());

        assert_eq!(session.hand_back(), HandBack::Background);
        // Idempotent: a second hand-back neither re-arms nor delivers.
        assert_eq!(session.hand_back(), HandBack::Background);
        std::fs::write(&gate, b"").unwrap();

        let completion = one(&mut completions).await;
        assert_eq!(completion.id, session.id);
        assert_eq!(completion.exit, Some(0));
        assert_eq!(completion.output, "late\n");
        barrier(&table, &mut completions, callback).await;
    }

    /// The session deadline is dropped at hand-back (upstream
    /// `shell.timeout(id, 0)`) and only then: the same 200 ms deadline times a
    /// run out when it stays foreground, and does not when it is handed back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn hand_back_clears_the_deadline_that_a_foreground_run_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();

        let kept = table
            .spawn(SpawnRequest {
                timeout_ms: Some(200),
                ..foreground_request("/bin/sleep 30", callback.clone())
            })
            .unwrap();
        assert!(kept.wait_settled(Duration::from_secs(5)).await);
        assert_eq!(kept.exit().status, Some(Status::Timeout));

        let gate = dir.path().join("gate");
        let _open = OpenOnDrop(gate.clone());
        let freed = table
            .spawn(SpawnRequest {
                timeout_ms: Some(200),
                ..foreground_request(&gated(&gate, "echo free"), callback.clone())
            })
            .unwrap();
        assert_eq!(freed.hand_back(), HandBack::Background);
        let until = tokio::time::Instant::now() + Duration::from_millis(700);
        while tokio::time::Instant::now() < until {
            assert!(!freed.settled(), "deadline survived hand-back");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::fs::write(&gate, b"").unwrap();
        let completion = one(&mut completions).await;
        assert_eq!(completion.id, freed.id);
        assert_eq!(completion.status, Status::Exited);
        barrier(&table, &mut completions, callback).await;
    }

    /// Stop while the gate is `Held`: the foreground call is still waiting, so
    /// the stop settles the run quietly and the tool returns it inline. No
    /// completion is delivered, and a later hand-back reports `Settled`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn stop_while_held_settles_quietly_and_never_notifies() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(foreground_request("/bin/sleep 30", callback.clone()))
            .unwrap();
        session.stop(true).await;
        assert_eq!(session.exit().status, Some(Status::Killed));

        assert_eq!(session.hand_back(), HandBack::Settled);
        barrier(&table, &mut completions, callback).await;
    }

    /// Stop while the gate is `Armed` (handed back, still running): exactly one
    /// `Killed` completion, even with a second concurrent stop and a repeated
    /// hand-back after the fact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn stop_while_armed_notifies_exactly_once() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(foreground_request("/bin/sleep 30", callback.clone()))
            .unwrap();
        assert_eq!(session.hand_back(), HandBack::Background);

        tokio::join!(session.stop(true), session.stop(true));
        assert!(session.wait_settled(Duration::from_secs(5)).await);

        let completion = one(&mut completions).await;
        assert_eq!(completion.id, session.id);
        assert_eq!(completion.status, Status::Killed);
        // Already delivered: a late hand-back neither re-arms nor re-delivers.
        assert_eq!(session.hand_back(), HandBack::Background);
        barrier(&table, &mut completions, callback).await;
    }

    // Coverage gap, deliberately recorded: a panic inside the driver
    // (`TerminalGuard`'s fallback settle) has no injection seam. `drive` is
    // private and its only panic sources are inside pi-shell's `run`, which
    // cannot be made to unwind from here, so that path is covered by the
    // guard's construction (created first in `drive`), not by a test.

    /// STRESS ONLY: hand-off racing the settle. The interleaving is not
    /// controlled, so this proves only that whichever side wins, a completion
    /// is delivered exactly when `hand_back` reported `Background`, and never
    /// twice. Both branches are pinned deterministically by
    /// `a_foreground_run_that_settles_before_hand_back_never_notifies` and
    /// `a_foreground_run_handed_back_while_running_notifies_once_on_settle`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn stress_hand_back_racing_settle_never_double_notifies_or_loses_one() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let mut background = 0;
        let mut inline = 0;
        for _ in 0..12 {
            let session = table
                .spawn(foreground_request("echo race", callback.clone()))
                .unwrap();
            // Poll until the settle is imminent, so the two sides collide.
            for _ in 0..2_000 {
                if session.size() > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            match session.hand_back() {
                HandBack::Background => {
                    background += 1;
                    let completion = one(&mut completions).await;
                    assert_eq!(completion.id, session.id);
                }
                HandBack::Settled => {
                    inline += 1;
                    assert!(session.wait_settled(Duration::from_secs(5)).await);
                }
            }
            assert!(session.wait_settled(Duration::from_secs(5)).await);
            barrier(&table, &mut completions, callback.clone()).await;
        }
        assert_eq!(background + inline, 12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn oversized_output_completion_is_bounded_to_the_tail() {
        let table = ShellSessions::new();
        let (callback, mut completions) = recorder();
        let session = table
            .spawn(request(
                "head -c 8388609 /dev/zero | tr '\\0' z",
                Some(10_000),
                callback,
            ))
            .unwrap();
        assert!(session.wait_settled(Duration::from_secs(10)).await);

        let completion = one(&mut completions).await;
        assert_eq!(completion.status, Status::Exited);
        assert_eq!(completion.exit, Some(0));
        // The model reads at most a 50 KiB tail, so a completion carries that
        // tail and a flag, never the whole 8 MiB capture.
        assert_eq!(completion.output.len(), 50 * 1024);
        assert!(completion.truncated);
        assert!(completion.output.bytes().all(|byte| byte == b'z'));
        assert_eq!(
            session.size(),
            (ox::tool::shell::sessions::OUTPUT_CAPACITY + 1) as u64
        );
    }
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
            completion: None,
            foreground: false,
            owner: None,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn a_settled_session_with_a_live_child_is_never_evicted() {
        let table = ShellSessions::new();
        let session = table.spawn(request("/bin/sleep 30 &", 10_000)).unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);
        assert!(session.retains_interpreter());

        table.evict_idle(ox::schema::id::now_millis() + SHELL_SESSION_TTL_MS + 1_000);
        assert!(
            table.get(&session.id).is_some(),
            "eviction must not drop the interpreter while its child is live"
        );
        session.stop(true).await;
    }
}

// ---------------------------------------------------------------------------
// Interpreter retention: a finished command must not pin its brush
// interpreter, because every tool call spawns one and the table keeps the
// entry for the TTL.
// ---------------------------------------------------------------------------

mod retention {
    use super::*;

    use ox::tool::shell::sessions::{SHELL_SESSION_TTL_MS, ShellSessions, SpawnRequest};

    fn request(command: &str) -> SpawnRequest {
        SpawnRequest {
            command: command.to_owned(),
            cwd: std::env::temp_dir(),
            timeout_ms: Some(10_000),
            completion: None,
            foreground: false,
            owner: None,
        }
    }

    /// The release decision precedes the settle notification, so ownership is
    /// already final when `wait_settled` returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_settled_session_releases_its_interpreter() {
        let table = ShellSessions::new();
        let session = table.spawn(request("echo hi")).unwrap();
        assert!(
            session.wait_settled(Duration::from_secs(5)).await,
            "the command settles"
        );
        assert!(
            !session.retains_interpreter(),
            "a settled session releases its interpreter instead of pinning it for the TTL"
        );
        // The capture outlives the interpreter: paging and the model-facing
        // tail read the buffer, never the shell.
        assert_eq!(session.tail(), "hi\n");
        assert_eq!(session.info().exit, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_session_keeps_its_interpreter() {
        let table = ShellSessions::new();
        let session = table.spawn(request("sleep 30")).unwrap();
        assert!(
            session.retains_interpreter(),
            "a running session still owns the interpreter it is executing in"
        );
        session.stop(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn stopping_a_settled_background_child_releases_its_interpreter() {
        let table = ShellSessions::new();
        let session = table.spawn(request("/bin/sleep 30 &")).unwrap();
        assert!(
            session.wait_settled(Duration::from_secs(5)).await,
            "the command settles"
        );
        assert!(
            session.retains_interpreter(),
            "the child keeps the shell alive"
        );

        session.stop(true).await;

        assert!(
            !session.retains_interpreter(),
            "stop must drop a settled session's retained interpreter"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn a_background_child_releases_the_interpreter_once_it_exits() {
        let table = ShellSessions::new();
        // The child lives one second; the release watcher observes its exit.
        let session = table.spawn(request("/bin/sleep 1 &")).unwrap();
        assert!(session.wait_settled(Duration::from_secs(5)).await);
        assert!(
            session.retains_interpreter(),
            "the live child pins the shell"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while session.retains_interpreter() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the interpreter was never released after its child exited"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        table.evict_idle(ox::schema::id::now_millis() + SHELL_SESSION_TTL_MS + 1_000);
        assert!(
            table.get(&session.id).is_none(),
            "a settled session with no live child is evictable"
        );
    }

    /// `sleep` is a brush builtin, and a background builtin has no pid, so
    /// pi-shell's `live_background_job_count` cannot see it (the retention
    /// guard covers child processes only).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn a_live_background_child_keeps_its_interpreter() {
        let table = ShellSessions::new();
        let session = table.spawn(request("/bin/sleep 5 &")).unwrap();
        assert!(
            session.wait_settled(Duration::from_secs(5)).await,
            "the command settles"
        );
        assert!(
            session.retains_interpreter(),
            "dropping the interpreter would SIGKILL a live `&` child"
        );
    }

    /// The counter's blind spot, pinned so it is a documented limitation
    /// rather than a surprise: a background *builtin* is not counted, so the
    /// interpreter is released and the job dies with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_background_builtin_does_not_pin_the_interpreter() {
        let table = ShellSessions::new();
        let session = table.spawn(request("sleep 5 &")).unwrap();
        assert!(
            session.wait_settled(Duration::from_secs(5)).await,
            "the command settles"
        );
        assert!(
            !session.retains_interpreter(),
            "pi-shell tracks background child processes, not background builtins"
        );
    }
}
