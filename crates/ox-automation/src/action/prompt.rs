//! Unattended prompt delivery: the session inbox or a child subagent.

use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;

use super::{ActionContext, ActionExecutor, Grant, RunTicket};
use crate::scheduler::{ClaimedRun, RunResult};
use crate::types::{Action, Target};

/// Which id namespace a prompt uses. A code hand-off must not be mistaken for
/// the tool-call message of the same run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Action,
    Send,
}

/// Queue a prompt into an existing session's inbox and dispatch it. The port
/// must never wait for a user form and must reject a target session that left
/// the automation's project: both are errors, not interactive approvals.
pub struct SessionPromptRequest<'a> {
    pub ticket: RunTicket<'a>,
    pub message_id: String,
    pub session_id: &'a str,
    pub text: &'a str,
    pub unattended: bool,
}

/// Start the deterministic child session for one run and prompt it. Creation
/// grants do not authorize subagents implicitly; the caller checks that, and
/// the port must reject an unknown, hidden or primary-only agent.
pub struct SubagentRequest<'a> {
    pub ticket: RunTicket<'a>,
    pub child_id: String,
    pub parent_id: &'a str,
    pub message_id: String,
    pub agent: &'a str,
    pub text: &'a str,
    pub unattended: bool,
}

/// Both prompt destinations available to an unattended run.
pub trait PromptPort: Send + Sync {
    fn prompt_session<'a>(&'a self, req: SessionPromptRequest<'a>) -> BoxFuture<'a, Result<()>>;
    fn start_subagent<'a>(&'a self, req: SubagentRequest<'a>) -> BoxFuture<'a, Result<()>>;
}

/// Delivers prompt actions and code hand-offs through the same port.
pub struct PromptExecutor<P> {
    port: Arc<P>,
}

impl<P: PromptPort> PromptExecutor<P> {
    #[must_use]
    pub const fn new(port: Arc<P>) -> Self {
        Self { port }
    }

    /// Deliver `text` to the requested target, or to the creating session when
    /// `to` is absent. `unattended` is set on every request: no form is awaited.
    pub async fn deliver(
        &self,
        run: &ClaimedRun,
        text: &str,
        to: Option<&Target>,
        kind: MessageKind,
        ctx: ActionContext<'_>,
    ) -> Result<RunResult> {
        let ticket = RunTicket::new(run);
        let suffix = if kind == MessageKind::Send {
            "_send"
        } else {
            ""
        };
        let message_id = format!("msg_{}_{}{suffix}", run.info.id, run.info.run_count);
        match to {
            Some(Target::Subagent { subagent }) => {
                let Some(parent_id) = run.info.session_id.as_deref() else {
                    return Ok(RunResult::failed(Some(
                        "subagent target needs the session that created the automation".to_owned(),
                    )));
                };
                if !ctx.authorized(&ticket, Grant::Subagent(subagent)) {
                    return Ok(RunResult::failed(Some(format!(
                        "missing creation grant to start subagent {subagent}"
                    ))));
                }
                let child_id = format!("ses_{}_{}", run.info.id, run.info.run_count);
                self.port
                    .start_subagent(SubagentRequest {
                        ticket,
                        child_id,
                        parent_id,
                        message_id,
                        agent: subagent,
                        text,
                        unattended: true,
                    })
                    .await?;
                Ok(RunResult::success(Some(format!(
                    "Prompt delivered to subagent {subagent}"
                ))))
            }
            _ => {
                let session = if let Some(Target::Session { session }) = to {
                    session.as_str()
                } else {
                    let Some(session) = run.info.session_id.as_deref() else {
                        return Ok(RunResult::failed(Some(
                            "prompt needs a target session".to_owned(),
                        )));
                    };
                    session
                };
                if !ctx.authorized(&ticket, Grant::Session(session)) {
                    return Ok(RunResult::failed(Some(format!(
                        "missing creation grant to prompt session {session}"
                    ))));
                }
                self.port
                    .prompt_session(SessionPromptRequest {
                        ticket,
                        message_id,
                        session_id: session,
                        text,
                        unattended: true,
                    })
                    .await?;
                Ok(RunResult::success(Some(format!(
                    "Prompt delivered to {session}"
                ))))
            }
        }
    }
}

impl<P: PromptPort> ActionExecutor for PromptExecutor<P> {
    fn execute<'a>(
        &'a self,
        run: &'a ClaimedRun,
        ctx: ActionContext<'a>,
    ) -> BoxFuture<'a, Result<RunResult>> {
        Box::pin(async move {
            let Action::Prompt { prompt, to } = &run.info.action else {
                return Ok(RunResult::failed(Some("not a prompt action".to_owned())));
            };
            self.deliver(run, prompt, to.as_ref(), MessageKind::Action, ctx)
                .await
        })
    }
}
