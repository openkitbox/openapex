//! Durable shell jobs for unattended commands.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::future::BoxFuture;

use super::{ActionContext, ActionExecutor, Grant, RunTicket, bounded_output};
use crate::scheduler::{ClaimedRun, RunResult};
use crate::types::Action;

/// What a durable shell job reported. `timed_out` means the job was stopped,
/// never moved to the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    pub exit_code: Option<i32>,
    pub output: String,
    pub timed_out: bool,
}

/// Request to start one durable job for one claimed run.
pub struct ShellRequest<'a> {
    pub ticket: RunTicket<'a>,
    pub command: &'a str,
    pub cwd: &'a str,
    pub timeout: Duration,
}

/// Host port that owns the durable job table and process lifecycle.
pub trait ShellJob: Send + Sync {
    fn run<'a>(&'a self, req: ShellRequest<'a>) -> BoxFuture<'a, Result<ShellOutcome>>;
}

/// Runs a stored command once, recording its exit code and bounded output.
pub struct ShellExecutor<S> {
    jobs: Arc<S>,
}

impl<S: ShellJob> ShellExecutor<S> {
    #[must_use]
    pub const fn new(jobs: Arc<S>) -> Self {
        Self { jobs }
    }
}

impl<S: ShellJob> ActionExecutor for ShellExecutor<S> {
    fn execute<'a>(
        &'a self,
        run: &'a ClaimedRun,
        ctx: ActionContext<'a>,
    ) -> BoxFuture<'a, Result<RunResult>> {
        Box::pin(async move {
            let Action::Shell { command } = &run.info.action else {
                return Ok(RunResult::failed(Some("not a shell action".to_owned())));
            };
            let ticket = RunTicket::new(run);
            if !ctx.authorized(&ticket, Grant::Shell) {
                return Ok(RunResult::failed(Some(
                    "missing creation grant to run the shell command".to_owned(),
                )));
            }
            let outcome = match self
                .jobs
                .run(ShellRequest {
                    ticket,
                    command,
                    cwd: &run.info.directory,
                    timeout: ctx.limits.shell_timeout,
                })
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => {
                    return Ok(RunResult::failed(Some(
                        bounded_output(&err.to_string(), ctx.limits.output_bytes).into_owned(),
                    )));
                }
            };
            let label = if outcome.timed_out {
                "timed out".to_owned()
            } else {
                match outcome.exit_code {
                    Some(code) => format!("Exit code: {code}"),
                    None => "Exit code: unknown".to_owned(),
                }
            };
            let output = bounded_output(
                &format!("{label}\n{}", outcome.output),
                ctx.limits.output_bytes,
            )
            .into_owned();
            Ok(RunResult {
                success: !outcome.timed_out && outcome.exit_code == Some(0),
                output: Some(output),
                state: None,
                terminal: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{ShellExecutor, ShellJob, ShellOutcome, ShellRequest};
    use crate::action::test_support::{info, run as make_run};
    use crate::action::{
        ActionContext, ActionExecutor, ActionLimits, Authorizer, Grant, RunTicket,
    };
    use crate::types::Action;

    struct AllowAll;

    impl Authorizer for AllowAll {
        fn allows(&self, _: &RunTicket<'_>, _: Grant<'_>) -> bool {
            true
        }
    }

    struct DenyAll;

    impl Authorizer for DenyAll {
        fn allows(&self, _: &RunTicket<'_>, _: Grant<'_>) -> bool {
            false
        }
    }

    struct MockShell {
        outcome: Mutex<ShellOutcome>,
        commands: Mutex<Vec<String>>,
        expected_count: Mutex<Option<u64>>,
    }

    impl MockShell {
        fn new(outcome: ShellOutcome) -> Self {
            Self {
                outcome: Mutex::new(outcome),
                commands: Mutex::new(Vec::new()),
                expected_count: Mutex::new(None),
            }
        }

        fn set_outcome(&self, outcome: ShellOutcome) {
            *self.outcome.lock().expect("mock lock") = outcome;
        }

        fn expect_count(&self, count: u64) {
            *self.expected_count.lock().expect("mock lock") = Some(count);
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().expect("mock lock").clone()
        }
    }

    impl ShellJob for MockShell {
        fn run<'a>(
            &'a self,
            req: ShellRequest<'a>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<ShellOutcome>> {
            Box::pin(async move {
                if let Some(expected) = *self.expected_count.lock().expect("mock lock") {
                    anyhow::ensure!(
                        crate::action::RunTicket::guard(&req.ticket, expected),
                        "stale shell ticket must be rejected"
                    );
                }
                self.commands
                    .lock()
                    .expect("mock lock")
                    .push(format!("{}:{}:{:?}", req.command, req.cwd, req.timeout));
                Ok(self.outcome.lock().expect("mock lock").clone())
            })
        }
    }

    #[tokio::test]
    async fn action_shell_result() -> anyhow::Result<()> {
        let limits = ActionLimits::default();
        let allow = AllowAll;
        let deny = DenyAll;
        let shell = Arc::new(MockShell::new(ShellOutcome {
            exit_code: Some(2),
            output: "x".repeat(40 * 1024),
            timed_out: false,
        }));
        let executor = ShellExecutor::new(shell.clone());
        let mut command = info(
            Action::Shell {
                command: "long".to_owned(),
            },
            None,
        );
        shell.expect_count(command.run_count);

        let run = make_run(command.clone());
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&allow), &limits))
            .await?;
        assert!(!result.success, "a non-zero exit code must fail the run");
        let output = result.output.expect("shell output");
        assert!(
            output.starts_with("Exit code: 2\n"),
            "exit code is recorded"
        );
        assert!(
            output.len() <= limits.output_bytes,
            "output must stay within 32 KiB, got {}",
            output.len()
        );
        assert!(output.contains("[truncated]"), "oversized output is marked");

        shell.set_outcome(ShellOutcome {
            exit_code: Some(0),
            output: "ok".to_owned(),
            timed_out: false,
        });
        command.run_count = 1;
        shell.expect_count(1);
        let run = make_run(command.clone());
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&allow), &limits))
            .await?;
        assert!(result.success, "a zero exit code succeeds");
        assert_eq!(result.output.as_deref(), Some("Exit code: 0\nok"));

        shell.set_outcome(ShellOutcome {
            exit_code: None,
            output: "partial".to_owned(),
            timed_out: true,
        });
        command.run_count = 2;
        shell.expect_count(2);
        let run = make_run(command.clone());
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&allow), &limits))
            .await?;
        assert!(
            !result.success,
            "a timed-out shell job fails, it is not backgrounded"
        );
        assert!(
            result
                .output
                .as_deref()
                .is_some_and(|o| o.contains("timed out")),
            "timeout must be reported in the output"
        );

        command.run_count = 3;
        let run = make_run(command);
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&deny), &limits))
            .await?;
        assert!(
            !result.success,
            "a missing creation grant must fail the run"
        );
        assert_eq!(shell.commands().len(), 3, "denied commands never start");

        assert_eq!(
            shell.commands()[0],
            format!("long:/work:{:?}", limits.shell_timeout)
        );
        Ok(())
    }
}
