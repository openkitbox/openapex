//! Action executors and the narrow host ports they need.
//!
//! ox-automation decides *what* runs; sessions, subagents, durable shell jobs
//! and permission grants are provided by the server as ports. Every port call
//! carries a [`RunTicket`] so a callback from an older run can be rejected
//! before it produces a side effect.

use std::time::Duration;

use anyhow::Result;
use futures_util::future::BoxFuture;

use crate::scheduler::{ClaimedRun, RunResult};

pub mod code;
pub mod prompt;
pub mod shell;

/// Shared ceiling for shell and code hand-off output.
pub const OUTPUT_BYTES: usize = 32 * 1024;

/// The identity of one claimed run. Completion callbacks must carry it so a
/// writer can compare it with the row's current `run_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunTicket<'a> {
    pub id: &'a str,
    pub run_count: u64,
}

impl<'a> RunTicket<'a> {
    #[must_use]
    pub fn new(run: &'a ClaimedRun) -> Self {
        Self {
            id: run.info.id.as_str(),
            run_count: run.info.run_count,
        }
    }

    /// Whether a callback for this ticket is still the current one.
    #[must_use]
    pub fn guard(&self, current_run_count: u64) -> bool {
        self.run_count == current_run_count
    }
}

/// A creation-time grant checked before an unattended action is admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant<'a> {
    /// Run the stored shell command.
    Shell,
    /// Execute a code program at all.
    Execute,
    /// Call a scanned tool path.
    Tool(&'a str),
    /// Start a named subagent, including from `code.send`.
    Subagent(&'a str),
    /// Prompt a named existing session, including the creating session.
    Session(&'a str),
}

/// Read-only view of the grants approved when the automation was created.
/// Unknown or missing grants are denied: there is no interactive approval
/// during an unattended run. A denial becomes a failed `RunResult`; the
/// scheduler owns the pause/retry policy.
pub trait Authorizer: Send + Sync {
    fn allows(&self, ticket: &RunTicket<'_>, grant: Grant<'_>) -> bool;
}

/// Per-run budgets passed to every executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionLimits {
    pub output_bytes: usize,
    pub shell_timeout: Duration,
}

impl Default for ActionLimits {
    fn default() -> Self {
        Self {
            output_bytes: OUTPUT_BYTES,
            shell_timeout: Duration::from_secs(30),
        }
    }
}

/// Everything an executor needs from the host for one run.
#[derive(Clone, Copy)]
pub struct ActionContext<'a> {
    pub now_ms: i64,
    /// `None` fails closed: no grant can be assumed during an unattended run.
    pub authorizer: Option<&'a dyn Authorizer>,
    pub limits: &'a ActionLimits,
}

impl<'a> ActionContext<'a> {
    #[must_use]
    pub const fn new(
        now_ms: i64,
        authorizer: Option<&'a dyn Authorizer>,
        limits: &'a ActionLimits,
    ) -> Self {
        Self {
            now_ms,
            authorizer,
            limits,
        }
    }

    #[must_use]
    pub fn authorized(&self, ticket: &RunTicket<'_>, grant: Grant<'_>) -> bool {
        self.authorizer
            .is_some_and(|authorizer| authorizer.allows(ticket, grant))
    }
}

/// Dispatch one claimed automation action. The returned result is the
/// completion handed to the scheduler, which applies its `run_count` guard.
pub trait ActionExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        run: &'a ClaimedRun,
        ctx: ActionContext<'a>,
    ) -> BoxFuture<'a, Result<RunResult>>;
}

/// Keep the head of a bounded string: delivery information comes first.
#[must_use]
pub fn bounded_output(output: &str, limit: usize) -> std::borrow::Cow<'_, str> {
    const MARKER: &str = "[truncated]";
    if output.len() <= limit {
        return std::borrow::Cow::Borrowed(output);
    }
    if limit <= MARKER.len() {
        return std::borrow::Cow::Owned(MARKER.chars().take(limit).collect());
    }
    let mut cut = limit - MARKER.len();
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}{MARKER}", &output[..cut]))
}

/// Compose an executor, so the scheduler needs one handler for all actions.
pub struct Actions<P, S>
where
    P: prompt::PromptPort,
    S: shell::ShellJob,
{
    pub prompt: std::sync::Arc<prompt::PromptExecutor<P>>,
    pub shell: std::sync::Arc<shell::ShellExecutor<S>>,
    pub code: std::sync::Arc<code::CodeExecutor<P>>,
}

impl<P, S> ActionExecutor for Actions<P, S>
where
    P: prompt::PromptPort + 'static,
    S: shell::ShellJob + 'static,
{
    fn execute<'a>(
        &'a self,
        run: &'a ClaimedRun,
        ctx: ActionContext<'a>,
    ) -> BoxFuture<'a, Result<RunResult>> {
        Box::pin(async move {
            match &run.info.action {
                crate::types::Action::Prompt { .. } => self.prompt.execute(run, ctx).await,
                crate::types::Action::Shell { .. } => self.shell.execute(run, ctx).await,
                crate::types::Action::Code { .. } => self.code.execute(run, ctx).await,
            }
        })
    }
}
