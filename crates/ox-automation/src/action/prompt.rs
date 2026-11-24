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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::ensure;

    use super::{PromptExecutor, PromptPort, SessionPromptRequest, SubagentRequest};
    use crate::action::test_support::{info, run as make_run};
    use crate::action::{
        ActionContext, ActionExecutor, ActionLimits, Authorizer, Grant, RunTicket,
    };
    use crate::types::{Action, Target};

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

    #[derive(Default)]
    struct MockPort {
        deliveries: Mutex<Vec<String>>,
        expected_count: Mutex<Option<u64>>,
    }

    impl MockPort {
        fn expect_count(&self, count: u64) {
            *self.expected_count.lock().expect("mock lock") = Some(count);
        }

        fn deliveries(&self) -> Vec<String> {
            self.deliveries.lock().expect("mock lock").clone()
        }

        fn check(&self, ticket: &RunTicket<'_>, unattended: bool) -> anyhow::Result<()> {
            let expected = self.expected_count.lock().expect("mock lock");
            ensure!(unattended, "unattended prompts must never wait for a form");
            if let Some(expected) = *expected {
                ensure!(
                    RunTicket::guard(ticket, expected),
                    "a stale run ticket must be rejected, got {} expected {expected}",
                    ticket.run_count
                );
            }
            Ok(())
        }
    }

    impl PromptPort for MockPort {
        fn prompt_session<'a>(
            &'a self,
            req: SessionPromptRequest<'a>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.check(&req.ticket, req.unattended)?;
                self.deliveries.lock().expect("mock lock").push(format!(
                    "session:{}:{}:{}",
                    req.session_id, req.message_id, req.text
                ));
                Ok(())
            })
        }

        fn start_subagent<'a>(
            &'a self,
            req: SubagentRequest<'a>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.check(&req.ticket, req.unattended)?;
                self.deliveries.lock().expect("mock lock").push(format!(
                    "subagent:{}:{}:{}:{}",
                    req.agent, req.parent_id, req.child_id, req.message_id
                ));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn action_prompt_targets() -> anyhow::Result<()> {
        let port = Arc::new(MockPort::default());
        let executor = PromptExecutor::new(port.clone());
        let limits = ActionLimits::default();
        let allow = AllowAll;
        let deny = DenyAll;

        let mut session = info(
            Action::Prompt {
                prompt: "check build".to_owned(),
                to: Some(Target::Session {
                    session: "ses_existing".to_owned(),
                }),
            },
            Some("ses_owner"),
        );
        port.expect_count(session.run_count);
        let run = make_run(session);
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&allow), &limits))
            .await?;
        assert!(result.success, "a granted session prompt must be delivered");

        session = run.info.clone();
        session.run_count = 1;
        session.action = Action::Prompt {
            prompt: "check tests".to_owned(),
            to: Some(Target::Subagent {
                subagent: "apex".to_owned(),
            }),
        };
        port.expect_count(1);
        let run = make_run(session);
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&allow), &limits))
            .await?;
        assert!(
            result.success,
            "a granted subagent prompt must be delivered"
        );

        let mut default = run.info.clone();
        default.run_count = 2;
        default.action = Action::Prompt {
            prompt: "default".to_owned(),
            to: None,
        };
        port.expect_count(2);
        let run = make_run(default);
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&allow), &limits))
            .await?;
        assert!(result.success, "no target means the creating session");

        assert_eq!(
            port.deliveries(),
            [
                "session:ses_existing:msg_aut_1_0:check build".to_owned(),
                "subagent:apex:ses_owner:ses_aut_1_1:msg_aut_1_1".to_owned(),
                "session:ses_owner:msg_aut_1_2:default".to_owned(),
            ],
            "each prompt target must use its own inbox/child path"
        );

        let mut denied = run.info.clone();
        denied.run_count = 3;
        denied.action = Action::Prompt {
            prompt: "no grant".to_owned(),
            to: Some(Target::Subagent {
                subagent: "apex".to_owned(),
            }),
        };
        let run = make_run(denied);
        let result = executor
            .execute(&run, ActionContext::new(1_000, Some(&deny), &limits))
            .await?;
        assert!(
            !result.success,
            "a missing creation grant must fail the run"
        );
        assert!(
            result
                .output
                .as_deref()
                .is_some_and(|o| o.contains("grant")),
            "the failure must name the missing authorization"
        );
        assert_eq!(
            port.deliveries().len(),
            3,
            "a denied prompt must not be sent"
        );

        let run = make_run(info(
            Action::Prompt {
                prompt: "no authorizer".to_owned(),
                to: None,
            },
            Some("ses_owner"),
        ));
        let result = executor
            .execute(&run, ActionContext::new(1_000, None, &limits))
            .await?;
        assert!(!result.success, "an absent authorizer fails closed");
        Ok(())
    }
}
