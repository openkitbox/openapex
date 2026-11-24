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
