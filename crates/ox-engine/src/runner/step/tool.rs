use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use ox_schema::event::SessionEventData;
use ox_schema::support::{Content, Record, SessionError};

use super::super::ToolContext;
use super::super::drain::DrainCtx;
use super::attempt::{StepParams, TOOLS_INTERRUPTED};
use super::error::aborted_error;
use super::stream::ToolAccum;
/// The failure a tool call gets in a final step (upstream `executeTool`:
/// "Tools are disabled after the maximum agent steps").
fn tools_disabled_error() -> SessionError {
    SessionError {
        kind: "tool.disabled".to_owned(),
        message: "Tools are disabled after the maximum agent steps".to_owned(),
        status: None,
        response: None,
    }
}

/// Upstream `INPUT_EXCERPT_LIMIT` (publish-llm-event.ts).
const INPUT_EXCERPT_LIMIT: usize = 2048;

/// Upstream `INPUT_FAILURES` (publish-llm-event.ts): the model-facing
/// failure for a tool call that never executed.
fn input_failure(arguments: &str, length_finish: bool) -> SessionError {
    // Upstream `failToolInput`: a step that settled on the output token
    // limit reports every input failure as the length cut-off.
    let (kind, message) = if length_finish {
        (
            "tool.input-incomplete",
            "The tool call was not executed because the output token limit (stop reason: length) was reached before you could complete the arguments. Break large payloads into smaller tool calls and reissue the call with complete arguments.",
        )
    } else {
        (
            "tool.input-json",
            "The tool input could not be parsed as JSON, so the call was not executed. Reissue the call with valid JSON arguments.",
        )
    };
    SessionError {
        kind: kind.to_owned(),
        // Upstream `inputFailure`: append the input excerpt so the model can
        // see how far its arguments got.
        message: input_failure_message(message, arguments),
        status: None,
        response: None,
    }
}

/// Upstream `inputFailure` + `inputExcerpt`: append the first
/// [`INPUT_EXCERPT_LIMIT`] characters with a `[truncated]` marker.
fn input_failure_message(message: &str, arguments: &str) -> String {
    if arguments.is_empty() {
        return message.to_owned();
    }
    let length = arguments.chars().count();
    let text: String = arguments.chars().take(INPUT_EXCERPT_LIMIT).collect();
    let truncated = if length > INPUT_EXCERPT_LIMIT {
        "\n[truncated]"
    } else {
        ""
    };
    format!(
        "{message}\n\nInput excerpt (first {} of {length} characters):\n{text}{truncated}",
        text.chars().count()
    )
}

/// Settle every complete-but-uncalled accumulator, in index order: end its
/// input, publish `tool.called` with the parsed arguments, and spawn the
/// execution. Malformed arguments fail the call instead (upstream
/// `INPUT_FAILURES.malformed`). Idempotent via `ToolAccum::called`.
pub(super) async fn settle_tool_calls(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    accs: &mut BTreeMap<u32, ToolAccum>,
    tasks: &mut Vec<(String, JoinHandle<()>)>,
    token: &CancellationToken,
    length_finish: bool,
) -> Result<()> {
    for acc in accs.values_mut() {
        if !acc.input_started || acc.called || acc.id.is_empty() || acc.name.is_empty() {
            continue;
        }
        acc.called = true;
        ctx.bus
            .publish(SessionEventData::ToolInputEnded {
                session_id: ctx.session_id.clone(),
                assistant_message_id: params.assistant_message_id.clone(),
                id: acc.id.clone(),
                text: acc.arguments.clone(),
            })
            .await?;
        let trimmed = acc.arguments.trim();
        // `Record` = a JSON object; scalars/arrays/null fail the parse and
        // the call (upstream `INPUT_FAILURES.malformed`).
        let input_record: Record = if trimmed.is_empty() {
            Record::new()
        } else {
            match serde_json::from_str(trimmed) {
                Ok(input) => input,
                Err(_) => {
                    let error = input_failure(&acc.arguments, length_finish);
                    ctx.bus
                        .publish(SessionEventData::ToolFailed {
                            session_id: ctx.session_id.clone(),
                            assistant_message_id: params.assistant_message_id.clone(),
                            id: acc.id.clone(),
                            error,
                            content: None,
                            metadata: None,
                            executed: false,
                            result_state: None,
                        })
                        .await?;
                    continue;
                }
            }
        };
        let input_value = Value::Object(
            input_record
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        ctx.bus
            .publish(SessionEventData::ToolCalled {
                session_id: ctx.session_id.clone(),
                assistant_message_id: params.assistant_message_id.clone(),
                id: acc.id.clone(),
                input: input_record,
                executed: false,
                state: None,
            })
            .await?;
        if params.final_step {
            // Final step: tools are disabled. The call still gets a result
            // so the history stays fully paired.
            ctx.bus
                .publish(SessionEventData::ToolFailed {
                    session_id: ctx.session_id.clone(),
                    assistant_message_id: params.assistant_message_id.clone(),
                    id: acc.id.clone(),
                    error: tools_disabled_error(),
                    content: None,
                    metadata: None,
                    executed: false,
                    result_state: None,
                })
                .await?;
            continue;
        }
        match ctx.tools.get(&acc.name) {
            Some(tool) => tasks.push((
                acc.id.clone(),
                spawn_tool_task(ctx, token, params, acc.id.clone(), tool, input_value),
            )),
            None => {
                // Upstream tool.ts:275 — the registry has no such tool.
                ctx.bus
                    .publish(SessionEventData::ToolFailed {
                        session_id: ctx.session_id.clone(),
                        assistant_message_id: params.assistant_message_id.clone(),
                        id: acc.id.clone(),
                        error: SessionError {
                            kind: "tool.unknown".to_owned(),
                            message: format!(
                                "Tool is not available for this request: {}",
                                acc.name
                            ),
                            status: None,
                            response: None,
                        },
                        content: None,
                        metadata: None,
                        executed: false,
                        result_state: None,
                    })
                    .await?;
            }
        }
    }
    Ok(())
}

/// Spawn one tool execution; the task publishes `tool.success` or
/// `tool.failed` itself and the step only joins the handle (upstream runs
/// each execution in its own fiber).
fn spawn_tool_task(
    ctx: &DrainCtx,
    token: &CancellationToken,
    params: &StepParams<'_>,
    call_id: String,
    tool: Arc<dyn super::super::Tool>,
    input: Value,
) -> JoinHandle<()> {
    let bus = ctx.bus.clone();
    let session_id = ctx.session_id.clone();
    let message_id = params.assistant_message_id.clone();
    let directory = ctx.directory.clone();
    // A child token: interrupting the busy period cancels the tool at its
    // next await point.
    let cancellation = token.child_token();
    // The session's live toolset rides in the context: `capability_search`
    // indexes its catalog from here.
    let registry = ctx.tools.clone();
    tokio::spawn(async move {
        let output = tool
            .execute(
                ToolContext {
                    session_id: session_id.clone(),
                    directory,
                    call_id: call_id.clone(),
                    cancellation,
                    registry,
                },
                input,
            )
            .await;
        let event = match output.error {
            None => SessionEventData::ToolSuccess {
                session_id,
                assistant_message_id: message_id,
                id: call_id,
                content: {
                    let mut content = vec![Content::Text {
                        text: tool_output_text(output.content),
                    }];
                    // Attachments (images, PDFs) travel as `file` content parts,
                    // which is how the model receives them.
                    for attachment in output.attachments.unwrap_or_default() {
                        content.push(Content::File {
                            uri: attachment.uri,
                            mime: attachment.mime,
                            name: attachment.name,
                        });
                    }
                    content
                },
                metadata: tool_metadata(output.metadata),
                executed: false,
                result_state: None,
            },
            Some(message) => SessionEventData::ToolFailed {
                session_id,
                assistant_message_id: message_id,
                id: call_id,
                error: SessionError {
                    kind: "tool.execution".to_owned(),
                    message,
                    status: None,
                    response: None,
                },
                content: None,
                metadata: tool_metadata(output.metadata),
                executed: false,
                result_state: None,
            },
        };
        if let Err(error) = bus.publish(event).await {
            tracing::error!(error = %error, "tool settlement failed to publish");
        }
    })
}

/// A tool output's model-facing text: strings verbatim, anything else as
/// compact JSON.
fn tool_output_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

/// Client-facing `metadata` for a tool event. A JSON object becomes the
/// event's record; anything else (or nothing) is omitted, because the wire
/// type is an object map.
fn tool_metadata(value: Option<Value>) -> Option<ox_schema::support::Record> {
    match value {
        Some(Value::Object(map)) => Some(map.into_iter().collect()),
        _ => None,
    }
}

/// Fail every input-started-but-uncalled accumulator with one shared error
/// (upstream `failAssistant`'s failTools `uncalled` sweep): the projector
/// settles the streaming tool part into the error state.
pub(super) async fn fail_unsettled_tools(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    accs: &mut BTreeMap<u32, ToolAccum>,
    error: SessionError,
) -> Result<()> {
    for acc in accs.values_mut() {
        if !acc.input_started || acc.called {
            continue;
        }
        acc.called = true;
        ctx.bus
            .publish(SessionEventData::ToolFailed {
                session_id: ctx.session_id.clone(),
                assistant_message_id: params.assistant_message_id.clone(),
                id: acc.id.clone(),
                error: error.clone(),
                content: None,
                metadata: None,
                executed: false,
                result_state: None,
            })
            .await?;
    }
    Ok(())
}

/// Wait for every spawned execution. An interrupt while waiting aborts the
/// remaining tasks; a task cancelled before it settled gets its tool part
/// settled `aborted` here (a task that finished first already published its
/// own settlement, and the projector ignores a failure for a settled part).
pub(super) async fn join_tool_tasks(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    tasks: &mut [(String, JoinHandle<()>)],
    token: &CancellationToken,
) -> Result<()> {
    let mut interrupted = false;
    for (call_id, task) in tasks.iter_mut() {
        if interrupted {
            task.abort();
        }
        let result = if interrupted {
            (&mut *task).await
        } else {
            let outcome = tokio::select! {
                result = &mut *task => Ok(result),
                () = token.cancelled() => Err(()),
            };
            match outcome {
                Ok(result) => result,
                Err(()) => {
                    // Interrupted mid-wait: abort the execution and settle
                    // its part aborted below.
                    interrupted = true;
                    task.abort();
                    (&mut *task).await
                }
            }
        };
        if result.is_err() {
            ctx.bus
                .publish(SessionEventData::ToolFailed {
                    session_id: ctx.session_id.clone(),
                    assistant_message_id: params.assistant_message_id.clone(),
                    id: call_id.clone(),
                    error: aborted_error(TOOLS_INTERRUPTED),
                    content: None,
                    metadata: None,
                    executed: false,
                    result_state: None,
                })
                .await?;
        }
    }
    Ok(())
}
