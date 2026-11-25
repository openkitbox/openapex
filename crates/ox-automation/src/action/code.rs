//! Code actions: scan, authorize, run, then hand `send` to the prompt executor.
//!
//! A `done: true` result sets `RunResult.terminal`. The current scheduler only
//! treats `at` triggers as one-shot, so the wiring or a scheduler follow-up
//! must honour that flag; this module never writes automation rows itself.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::prompt::{PromptExecutor, PromptPort};
use super::{ActionContext, ActionExecutor, Grant, RunTicket, bounded_output};
use crate::scheduler::{ClaimedRun, RunResult};
use crate::types::{Action, CodeResult, CodeSend, Target};

/// Explicit conversion between the runtime's strict result and the automation
/// domain result. These types are deliberately separate: ox-codemode has no
/// knowledge of automations.
impl From<ox_codemode::CodeResult> for CodeResult {
    fn from(result: ox_codemode::CodeResult) -> Self {
        Self {
            state: result.state,
            done: result.done,
            send: result.send.map(|send| crate::types::CodeSend {
                to: send.to.map(|to| crate::types::CodeSendTarget {
                    subagent: to.subagent,
                }),
                prompt: send.prompt,
            }),
        }
    }
}

/// One program result plus the optional hand-off it requested. Keeping `send`
/// out of [`RunResult`] makes it impossible to persist a hand-off as state.
struct ProgramResult {
    result: RunResult,
    send: Option<CodeSend>,
}

/// Executes one code action under grants approved at creation time. `send` is
/// delivered by the prompt executor with the same run ticket.
pub struct CodeExecutor<P> {
    prompt: Arc<PromptExecutor<P>>,
    invoker: Arc<dyn ox_codemode::ToolInvoker>,
    limits: ox_codemode::Limits,
}

impl<P: PromptPort> CodeExecutor<P> {
    #[must_use]
    pub const fn new(
        prompt: Arc<PromptExecutor<P>>,
        invoker: Arc<dyn ox_codemode::ToolInvoker>,
        limits: ox_codemode::Limits,
    ) -> Self {
        Self {
            prompt,
            invoker,
            limits,
        }
    }

    async fn run_program(
        &self,
        run: &ClaimedRun,
        code: &str,
        args: Option<&Value>,
        ctx: ActionContext<'_>,
    ) -> Result<ProgramResult> {
        let ticket = RunTicket::new(run);
        if !ctx.authorized(&ticket, Grant::Execute) {
            return Ok(ProgramResult {
                result: RunResult::failed(Some(
                    "missing creation grant to execute code".to_owned(),
                )),
                send: None,
            });
        }
        let scanned = match ox_codemode::scan::scan(code) {
            Ok(scanned) => scanned,
            Err(err) => {
                return Ok(ProgramResult {
                    result: RunResult::failed(Some(
                        bounded_output(
                            &format!("code rejected by scan: {err}"),
                            ctx.limits.output_bytes,
                        )
                        .into_owned(),
                    )),
                    send: None,
                });
            }
        };
        let mut allowed = BTreeSet::new();
        for path in &scanned.tools {
            if !ctx.authorized(&ticket, Grant::Tool(path)) {
                return Ok(ProgramResult {
                    result: RunResult::failed(Some(format!(
                        "missing creation grant for tool {path}"
                    ))),
                    send: None,
                });
            }
            allowed.insert(path.clone());
        }

        let bindings = json!({
            "state": run.info.state,
            "now": ctx.now_ms,
            "runs": run.info.run_count,
            "args": args,
        })
        .to_string();
        let code = code.to_owned();
        let invoker = self.invoker.clone();
        let limits = self.limits;
        let output = tokio::task::spawn_blocking(move || {
            ox_codemode::execute_scoped(&code, &allowed, &bindings, invoker, &limits)
        })
        .await
        .map_err(|err| anyhow::anyhow!("code task failed: {err}"))?;
        match output {
            Ok(output) => {
                let logs = bounded_output(&output.logs, ctx.limits.output_bytes).into_owned();
                let decoded = match output.result {
                    None => None,
                    Some(value) => ox_codemode::CodeResult::decode(&value)
                        .map_err(|err| anyhow::anyhow!("invalid code result: {err}"))?,
                };
                let Some(decoded) = decoded else {
                    return Ok(ProgramResult {
                        result: RunResult::success(Some(logs)),
                        send: None,
                    });
                };
                let decoded = CodeResult::from(decoded);
                let mut result = RunResult::success(Some(logs));
                result.state = decoded.state;
                result.terminal = decoded.done.unwrap_or(false);
                Ok(ProgramResult {
                    result,
                    send: decoded.send,
                })
            }
            Err(err) => Ok(ProgramResult {
                result: RunResult::failed(Some(
                    bounded_output(&err.to_string(), ctx.limits.output_bytes).into_owned(),
                )),
                send: None,
            }),
        }
    }
}

fn denied_send(message: String) -> RunResult {
    let mut result = RunResult::failed(Some(message));
    result.state = None;
    result.terminal = false;
    result
}

impl<P: PromptPort> ActionExecutor for CodeExecutor<P> {
    fn execute<'a>(
        &'a self,
        run: &'a ClaimedRun,
        ctx: ActionContext<'a>,
    ) -> BoxFuture<'a, Result<RunResult>> {
        Box::pin(async move {
            let Action::Code { code, args } = &run.info.action else {
                return Ok(RunResult::failed(Some("not a code action".to_owned())));
            };
            let ProgramResult { mut result, send } =
                self.run_program(run, code, args.as_ref(), ctx).await?;
            let Some(send) = send else { return Ok(result) };
            let ticket = RunTicket::new(run);
            let target = match &send.to {
                Some(to) => {
                    if !ctx.authorized(&ticket, Grant::Subagent(&to.subagent)) {
                        return Ok(denied_send(format!(
                            "missing creation grant to send to subagent {}",
                            to.subagent
                        )));
                    }
                    Some(Target::Subagent {
                        subagent: to.subagent.clone(),
                    })
                }
                None => {
                    let Some(session) = run.info.session_id.as_deref() else {
                        return Ok(denied_send(
                            "send needs the session that created the automation".to_owned(),
                        ));
                    };
                    if !ctx.authorized(&ticket, Grant::Session(session)) {
                        return Ok(denied_send(format!(
                            "missing creation grant to send to session {session}"
                        )));
                    }
                    None
                }
            };
            let handed = self
                .prompt
                .deliver(
                    run,
                    &send.prompt,
                    target.as_ref(),
                    super::prompt::MessageKind::Send,
                    ctx,
                )
                .await?;
            if !handed.success {
                result = denied_send(
                    handed
                        .output
                        .unwrap_or_else(|| "the prompt hand-off failed".to_owned()),
                );
            } else if let (Some(delivery), Some(logs)) =
                (handed.output.as_deref(), result.output.as_deref())
            {
                result.output = Some(
                    bounded_output(&format!("{delivery}\n{logs}"), ctx.limits.output_bytes)
                        .into_owned(),
                );
            } else {
                result.output = handed.output;
            }
            Ok(result)
        })
    }
}
