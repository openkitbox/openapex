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
                let Some(decoded) = output.result else {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ox_db::Db;
    use serde_json::json;

    use super::CodeExecutor;
    use crate::action::prompt::{
        PromptExecutor, PromptPort, SessionPromptRequest, SubagentRequest,
    };
    use crate::action::test_support::info;
    use crate::action::{
        ActionContext, ActionExecutor, ActionLimits, Authorizer, Grant, RunTicket,
    };
    use crate::lease::Lease;
    use crate::scheduler::{ClaimedRun, Clock, RunResult, Scheduler};
    use crate::store::AutomationStore;
    use crate::types::{Action, Outcome, Status};

    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    struct Grants(BTreeSet<String>);

    fn grant_key(grant: Grant<'_>) -> String {
        match grant {
            Grant::Shell => "shell".to_owned(),
            Grant::Execute => "execute".to_owned(),
            Grant::Tool(path) => format!("tool:{path}"),
            Grant::Subagent(name) => format!("subagent:{name}"),
            Grant::Session(id) => format!("session:{id}"),
        }
    }

    impl Authorizer for Grants {
        fn allows(&self, _: &RunTicket<'_>, grant: Grant<'_>) -> bool {
            self.0.contains(&grant_key(grant))
        }
    }

    #[derive(Default)]
    struct MockPrompt {
        deliveries: Mutex<Vec<String>>,
    }

    impl PromptPort for MockPrompt {
        fn prompt_session<'a>(
            &'a self,
            req: SessionPromptRequest<'a>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.deliveries
                    .lock()
                    .expect("mock lock")
                    .push(format!("session:{}:{}", req.session_id, req.text));
                Ok(())
            })
        }

        fn start_subagent<'a>(
            &'a self,
            req: SubagentRequest<'a>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.deliveries
                    .lock()
                    .expect("mock lock")
                    .push(format!("subagent:{}:{}", req.agent, req.text));
                Ok(())
            })
        }
    }

    struct MockInvoker;

    impl ox_codemode::ToolInvoker for MockInvoker {
        fn invoke(&self, _path: &str, _args: &str) -> anyhow::Result<String> {
            Ok(r#"{"ok":true}"#.to_owned())
        }
    }

    fn action(code: &str) -> Action {
        Action::Code {
            code: code.to_owned(),
            args: Some(json!({"input": 1})),
        }
    }

    #[tokio::test]
    async fn action_code_result() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let store = AutomationStore::new(&db);
        let limits = ActionLimits::default();
        let code_limits = ox_codemode::Limits {
            timeout: Duration::from_millis(1_000),
            ..ox_codemode::Limits::default()
        };
        let prompt = Arc::new(MockPrompt::default());
        let executor = CodeExecutor::new(
            Arc::new(PromptExecutor::new(prompt.clone())),
            Arc::new(MockInvoker),
            code_limits,
        );
        let grants = Grants(BTreeSet::from([
            "execute".to_owned(),
            "subagent:apex".to_owned(),
        ]));
        let no_send = Grants(BTreeSet::from(["execute".to_owned()]));

        let mut row = info(action(r#"return { state: { n: 1 } };"#), Some("ses_owner"));
        store.insert(&row).await?;
        let lease = Lease::acquire(&db, "scheduler", "host_a", 1_000, 60_000)
            .await?
            .expect("lease");
        let scheduler = Scheduler::new(&db, store, lease);
        let claimed = ClaimedRun {
            info: store.get(&row.id).await?.expect("inserted row"),
            late: false,
        };
        let result = executor
            .execute(&claimed, ActionContext::new(1_000, Some(&grants), &limits))
            .await?;
        assert!(result.success, "a granted code program must run");
        assert_eq!(result.state, Some(json!({"n": 1})), "state is handed back");
        assert!(!result.terminal);

        let clock = FixedClock(2_000);
        assert!(
            scheduler.complete(&clock, &claimed, result).await?,
            "the current ticket settles its run"
        );
        let saved = store.get(&row.id).await?.expect("row");
        assert_eq!(saved.state, Some(json!({"n": 1})), "state is persisted");
        assert_eq!(saved.last_outcome, Some(Outcome::Fired));

        row = saved;
        row.action = action("return { done: true };");
        store.save(&row, Status::Active).await?;
        let done = ClaimedRun {
            info: store.get(&row.id).await?.expect("row"),
            late: false,
        };
        let result = executor
            .execute(&done, ActionContext::new(2_000, Some(&grants), &limits))
            .await?;
        assert!(result.terminal, "done stops the automation");
        assert!(
            store
                .set_status(&done.info.id, Status::Done, 2_100, done.info.run_count)
                .await?,
            "the terminal write is guarded by the same run_count"
        );
        assert_eq!(store.get(&row.id).await?.expect("row").status, Status::Done);
        assert!(
            !store
                .set_status(
                    &done.info.id,
                    Status::Active,
                    2_200,
                    done.info.run_count.saturating_sub(1)
                )
                .await?,
            "a stale run_count cannot rewrite a done automation"
        );

        let mut send_row = info(
            action(
                r#"
                    console.log("handing off");
                    return { state: { n: 9 }, done: true,
                             send: { to: { subagent: "apex" }, prompt: "go" } };
                "#,
            ),
            Some("ses_owner"),
        );
        send_row.id = "aut_send".to_owned();
        send_row.run_count = 3;
        store.insert(&send_row).await?;
        let claimed = ClaimedRun {
            info: store.get(&send_row.id).await?.expect("row"),
            late: false,
        };
        let result = executor
            .execute(&claimed, ActionContext::new(3_000, Some(&no_send), &limits))
            .await?;
        assert!(!result.success, "a denied send must fail the run");
        assert_eq!(result.state, None, "no state is committed on denial");
        assert!(!result.terminal, "no done is committed on denial");
        assert!(
            prompt.deliveries.lock().expect("mock lock").is_empty(),
            "a denied send must not reach the prompt executor"
        );

        let result = executor
            .execute(&claimed, ActionContext::new(3_000, Some(&grants), &limits))
            .await?;
        assert!(result.success, "an authorized send is delivered");
        assert_eq!(result.state, Some(json!({"n": 9})));
        assert!(result.terminal, "done still stops after a hand-off");
        assert_eq!(
            prompt.deliveries.lock().expect("mock lock").as_slice(),
            ["subagent:apex:go"],
            "send goes through the prompt executor"
        );

        let mut stale = claimed.info.clone();
        stale.run_count += 1;
        assert!(
            !scheduler
                .complete(
                    &clock,
                    &ClaimedRun {
                        info: stale,
                        late: false
                    },
                    RunResult::success(Some("stale".to_owned()))
                )
                .await?,
            "a stale completion cannot write a newer run"
        );
        Ok(())
    }
}
