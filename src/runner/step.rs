//! One streaming step, ported from `packages/core/src/session/runner/step.ts`.
//!
//! Event choreography (matching upstream):
//!
//! - `session.step.started` is published lazily: on the first provider event
//!   of the step, or before a retry is scheduled — never for a stream that
//!   yields nothing (an empty connect-failure sequence retries cleanly and
//!   only the final failure starts the assistant row).
//! - Text/reasoning deltas stream as ephemeral events between
//!   `*.started`/`*.ended` pairs, each block with its own zero-based
//!   ordinal.
//! - Tool calls accumulate per stream index from
//!   `StreamEvent::ToolCallDelta` fragments: `session.tool.input.started`
//!   fires once the call is identified (id + name known), `*.input.delta`
//!   per fragment (ephemeral), `*.input.ended` when the arguments complete.
//!   Cardinal completes a call's arguments at `finish_reason`, so the
//!   settlement — `session.tool.called` plus the spawned execution — runs
//!   at `Finish` while the stream may still be open (upstream: tool
//!   execution is concurrent with the stream, llm.ts + step.ts).
//! - Each execution settles its tool part durably: `session.tool.success`
//!   or `session.tool.failed` (status `completed` / `error`). An
//!   interrupted in-flight execution settles as failed/`aborted`.
//! - On a clean finish: `session.step.streamed`, tool settlement, then
//!   `session.step.ended` with upstream's token shape (`input` = prompt
//!   minus cached, `output` = completion, `reasoning`, `cache.read` =
//!   cached, `cache.write` = 0).
//! - An interruption after the step started settles the step with
//!   `session.step.failed` carrying the upstream `aborted` error, after
//!   failing unsettled tool parts.
//! - Transport errors before any output retry with backoff (upstream's
//!   SessionRunnerRetry publishes `session.retry.scheduled`); capped at 3
//!   attempts. A 400 context-length overflow before any output surfaces as
//!   [`StepOutcome::Overflow`] so the drain can compact and retry once
//!   (upstream `Outcome.Compacted`).
//!
//! The context assembly lives in one named function
//! ([`build_chat_messages`]) so the e2e suite can trust the mapping.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cardinal::CardinalError;
use crate::cardinal::chat::BoxChatStream;
use crate::cardinal::stream::StreamEvent;
use crate::cardinal::wire::{
    ChatMessage, ChatRequest, FunctionCall, FunctionType, ToolCall, Usage,
};
use crate::id;
use crate::prompt;
use crate::schema::event::SessionEventData;
use crate::schema::message::{
    AssistantContent, Compaction, CompactionCompleted, CompactionMessage, Message, ToolState,
};
use crate::schema::support::{
    Content, FinishReason, ModelRef, Prompt, Record, SessionError, TokenCache, TokenUsage,
};

use super::ToolContext;
use super::compaction;
use super::compaction::is_context_overflow;
use super::drain::DrainCtx;

/// Upstream `STEP_INTERRUPTED` (runner/step.ts).
const STEP_INTERRUPTED: &str = "Step interrupted";
/// Upstream `TOOLS_INTERRUPTED` (runner/step.ts).
const TOOLS_INTERRUPTED: &str = "Tool execution interrupted";
/// Transport attempts before the step fails (upstream policy arrives later).
const MAX_ATTEMPTS: usize = 3;
/// Backoff base; attempt N sleeps `base * N`.
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Terminal states of one step, consumed by the drain loop.
#[derive(Debug)]
pub(crate) enum StepOutcome {
    /// The stream finished. `FinishReason::ToolCalls` continues the turn
    /// (the tool parts are already settled in the assistant row).
    Completed { finish: FinishReason },
    /// The provider rejected the request as context overflow before any
    /// output; the drain compacts and retries the step once.
    Overflow { error: SessionError },
    /// The cancellation token fired. The assistant row is settled
    /// (`step.failed` / `aborted`) when the step had started.
    Interrupted,
    /// The step failed terminally; the drain turns this into
    /// `session.execution.failed`.
    Failed(SessionError),
}

/// Everything one step needs, resolved by the drain loop per turn.
pub(crate) struct StepParams<'a> {
    pub assistant_message_id: String,
    pub agent: &'a str,
    pub model: &'a ModelRef,
}

/// Per-attempt result of consuming the stream.
enum Attempt {
    /// Terminal step outcome.
    Done(StepOutcome),
    /// Transport failed; `published` says whether provider output/events
    /// already went out (retry is only safe before any output).
    Transport {
        error: CardinalError,
        published: bool,
    },
}

/// Run one step end-to-end (context assembly → stream → durable settlement).
pub(crate) async fn run(
    ctx: &DrainCtx,
    token: &CancellationToken,
    params: &StepParams<'_>,
) -> Result<StepOutcome> {
    let rows = load_history(&ctx.db, &ctx.session_id).await?;
    let system = prompt::build(&ctx.directory);
    let mut request = ChatRequest::new(
        format!("{}/{}", params.model.provider_id, params.model.id),
        build_chat_messages(&system, &rows),
    );
    request.tools = ctx.tools.definitions();

    // Request dispatch time, published with step.started (before any output).
    let dispatch = id::now_millis();
    let mut started = false;

    let mut attempt: usize = 0;
    loop {
        attempt += 1;
        let stream = match ctx.chat.stream_chat(&request).await {
            Ok(stream) => stream,
            Err(error) => {
                // Context overflow before any output → the drain compacts
                // and retries the step once (upstream Outcome.Compacted).
                if !started && is_overflow_error(&error) {
                    return Ok(StepOutcome::Overflow {
                        error: to_session_error(&error),
                    });
                }
                // Connect failure: nothing was published for this attempt.
                if attempt < MAX_ATTEMPTS {
                    schedule_retry(ctx, params, &mut started, attempt, &error).await?;
                    if !sleep_backoff(token, attempt).await {
                        return settle_interrupted(ctx, params, started, None).await;
                    }
                    continue;
                }
                return fail_step(ctx, params, to_session_error(&error), None, started).await;
            }
        };
        match consume(ctx, token, params, dispatch, &mut started, stream).await? {
            Attempt::Done(outcome) => return Ok(outcome),
            Attempt::Transport { error, published } => {
                if !started && is_overflow_error(&error) {
                    return Ok(StepOutcome::Overflow {
                        error: to_session_error(&error),
                    });
                }
                if !published && attempt < MAX_ATTEMPTS {
                    schedule_retry(ctx, params, &mut started, attempt, &error).await?;
                    if !sleep_backoff(token, attempt).await {
                        return settle_interrupted(ctx, params, started, None).await;
                    }
                    continue;
                }
                return fail_step(ctx, params, to_session_error(&error), None, started).await;
            }
        }
    }
}

/// Whether a cardinal error is a context-length overflow rejection (upstream
/// `isContextOverflowFailure`): an HTTP 400 (or an in-band upstream failure
/// forwarded as a stream error) whose body carries the overflow signature.
fn is_overflow_error(error: &CardinalError) -> bool {
    match error {
        CardinalError::Http { status, body } => is_context_overflow(*status, body),
        CardinalError::Upstream(body) => is_context_overflow(400, body),
        _ => false,
    }
}

/// Interrupted settlement: an open assistant row settles with `step.failed`
/// carrying the upstream `aborted` error; a never-started step settles
/// silently (the actor publishes `execution.interrupted`).
async fn settle_interrupted(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    started: bool,
    tokens: Option<TokenUsage>,
) -> Result<StepOutcome> {
    if started {
        fail_step(
            ctx,
            params,
            SessionError {
                kind: "aborted".to_owned(),
                message: STEP_INTERRUPTED.to_owned(),
                status: None,
                response: None,
            },
            tokens,
            true,
        )
        .await?;
    }
    Ok(StepOutcome::Interrupted)
}

/// Backoff sleep, cancellable. Returns false when the token fired first.
async fn sleep_backoff(token: &CancellationToken, attempt: usize) -> bool {
    let delay = RETRY_BACKOFF_BASE * attempt as u32;
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = token.cancelled() => false,
    }
}

/// Publish `session.retry.scheduled` for `attempt` (1-based), starting the
/// assistant row first (upstream `startAssistant`).
async fn schedule_retry(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    started: &mut bool,
    attempt: usize,
    error: &CardinalError,
) -> Result<()> {
    if !*started {
        publish_started(ctx, params, id::now_millis()).await?;
        *started = true;
    }
    let delay = RETRY_BACKOFF_BASE * attempt as u32;
    ctx.bus
        .publish(SessionEventData::SessionRetryScheduled {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
            attempt: attempt as i64,
            at: id::now_millis() + delay.as_millis() as i64,
            error: to_session_error(error),
        })
        .await?;
    Ok(())
}

/// Publish `session.step.started` for the assistant row.
async fn publish_started(ctx: &DrainCtx, params: &StepParams<'_>, dispatch: i64) -> Result<()> {
    ctx.bus
        .publish(SessionEventData::StepStarted {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
            agent: params.agent.to_owned(),
            model: params.model.clone(),
            snapshot: None,
            started: dispatch,
        })
        .await?;
    Ok(())
}

/// Settle the assistant row as failed (`session.step.failed`) and surface
/// [`StepOutcome::Failed`]. Starts the row first when nothing was published.
async fn fail_step(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    error: SessionError,
    tokens: Option<TokenUsage>,
    started: bool,
) -> Result<StepOutcome> {
    if !started {
        publish_started(ctx, params, id::now_millis()).await?;
    }
    let outcome = StepOutcome::Failed(error.clone());
    ctx.bus
        .publish(SessionEventData::StepFailed {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
            error,
            finish: None,
            raw_finish: None,
            provider_state: None,
            cost: None,
            tokens,
            snapshot: None,
            files: None,
        })
        .await?;
    Ok(outcome)
}

/// One accumulating tool call from `ToolCallDelta` fragments.
#[derive(Debug, Default)]
struct ToolAccum {
    /// Call id (first fragment that carries one).
    id: String,
    /// Function name (first fragment that carries one).
    name: String,
    /// Concatenated argument JSON.
    arguments: String,
    /// `tool.input.started` has gone out (id + name known).
    input_started: bool,
    /// The call settled: input ended and `tool.called` (or a failure)
    /// published. Guards idempotent settlement.
    called: bool,
}

/// Consume one stream attempt, publishing streaming events as they arrive
/// and settling the step when the stream terminates cleanly. `started` is
/// updated when `step.started` goes out (persisted across retry attempts).
async fn consume(
    ctx: &DrainCtx,
    token: &CancellationToken,
    params: &StepParams<'_>,
    dispatch: i64,
    started: &mut bool,
    mut stream: BoxChatStream,
) -> Result<Attempt> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut text_ordinal: Option<i64> = None;
    let mut reasoning_ordinal: Option<i64> = None;
    let mut next_text_ordinal: i64 = 0;
    let mut next_reasoning_ordinal: i64 = 0;
    let mut finish: Option<(FinishReason, String)> = None;
    let mut usage: Option<Usage> = None;
    // Tool-call fragments by stream index.
    let mut tool_acc: BTreeMap<u32, ToolAccum> = BTreeMap::new();
    // In-flight tool executions paired with their call ids. Each task
    // publishes its own settlement; the step joins them before `step.ended`
    // (upstream: tools run while the stream continues, and the step waits
    // for all spawned tools).
    let mut tool_tasks: Vec<(String, JoinHandle<()>)> = Vec::new();

    loop {
        enum Next {
            Event(Option<Result<StreamEvent, CardinalError>>),
            Cancelled,
        }
        let next = tokio::select! {
            event = poll_fn(|cx| stream.as_mut().poll_next(cx)) => Next::Event(event),
            () = token.cancelled() => Next::Cancelled,
        };
        match next {
            Next::Cancelled => {
                // Dropping the stream aborts the HTTP connection.
                drop(stream);
                flush_blocks(
                    ctx,
                    params,
                    &mut text_ordinal,
                    &mut reasoning_ordinal,
                    &text,
                    &reasoning,
                )
                .await?;
                // Settle open tool inputs and in-flight executions as
                // aborted (upstream `interruptTools` + `failUnsettledTools`)
                // BEFORE `step.failed`.
                fail_unsettled_tools(ctx, params, &mut tool_acc, aborted_error(TOOLS_INTERRUPTED))
                    .await?;
                join_tool_tasks(ctx, params, &mut tool_tasks, token).await?;
                return Ok(Attempt::Done(
                    settle_interrupted(ctx, params, *started, Some(map_tokens(usage))).await?,
                ));
            }
            // `ChatStream` synthesizes `Done` before returning `None`, so an
            // exhausted stream is a clean end.
            Next::Event(None) => break,
            Next::Event(Some(Err(error))) => {
                flush_blocks(
                    ctx,
                    params,
                    &mut text_ordinal,
                    &mut reasoning_ordinal,
                    &text,
                    &reasoning,
                )
                .await?;
                // A failure before `Finish` leaves no spawned executions
                // (settlement happens at `Finish`); open inputs settle as
                // interrupted so no tool part stays streaming.
                fail_unsettled_tools(ctx, params, &mut tool_acc, aborted_error(TOOLS_INTERRUPTED))
                    .await?;
                join_tool_tasks(ctx, params, &mut tool_tasks, token).await?;
                let published = *started;
                return Ok(Attempt::Transport { error, published });
            }
            Next::Event(Some(Ok(event))) => match event {
                StreamEvent::TextDelta(delta) => {
                    if !*started {
                        publish_started(ctx, params, dispatch).await?;
                        *started = true;
                    }
                    if text_ordinal.is_none() {
                        let ordinal = next_text_ordinal;
                        next_text_ordinal += 1;
                        ctx.bus
                            .publish(SessionEventData::TextStarted {
                                session_id: ctx.session_id.clone(),
                                assistant_message_id: params.assistant_message_id.clone(),
                                ordinal,
                            })
                            .await?;
                        text_ordinal = Some(ordinal);
                    }
                    text.push_str(&delta);
                    let ordinal = text_ordinal.unwrap_or_default();
                    ctx.bus
                        .publish(SessionEventData::TextDelta {
                            session_id: ctx.session_id.clone(),
                            assistant_message_id: params.assistant_message_id.clone(),
                            ordinal,
                            delta,
                        })
                        .await?;
                }
                StreamEvent::ReasoningDelta(delta) => {
                    if !*started {
                        publish_started(ctx, params, dispatch).await?;
                        *started = true;
                    }
                    if reasoning_ordinal.is_none() {
                        let ordinal = next_reasoning_ordinal;
                        next_reasoning_ordinal += 1;
                        ctx.bus
                            .publish(SessionEventData::ReasoningStarted {
                                session_id: ctx.session_id.clone(),
                                assistant_message_id: params.assistant_message_id.clone(),
                                ordinal,
                                state: None,
                            })
                            .await?;
                        reasoning_ordinal = Some(ordinal);
                    }
                    reasoning.push_str(&delta);
                    let ordinal = reasoning_ordinal.unwrap_or_default();
                    ctx.bus
                        .publish(SessionEventData::ReasoningDelta {
                            session_id: ctx.session_id.clone(),
                            assistant_message_id: params.assistant_message_id.clone(),
                            ordinal,
                            delta,
                        })
                        .await?;
                }
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments,
                } => {
                    if !*started {
                        publish_started(ctx, params, dispatch).await?;
                        *started = true;
                    }
                    let entry = tool_acc.entry(index).or_default();
                    if entry.id.is_empty()
                        && let Some(seen) = id
                    {
                        entry.id = seen;
                    }
                    if entry.name.is_empty()
                        && let Some(seen) = name
                    {
                        entry.name = seen;
                    }
                    // `tool.input.started` fires once the call is identified
                    // (id + name known), like upstream `startToolInput`.
                    if !entry.input_started && !entry.id.is_empty() && !entry.name.is_empty() {
                        entry.input_started = true;
                        ctx.bus
                            .publish(SessionEventData::ToolInputStarted {
                                session_id: ctx.session_id.clone(),
                                assistant_message_id: params.assistant_message_id.clone(),
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                            })
                            .await?;
                    }
                    if !arguments.is_empty() {
                        entry.arguments.push_str(&arguments);
                        if entry.input_started {
                            // Ephemeral; the durable state keeps the final
                            // input from `tool.input.ended`.
                            ctx.bus
                                .publish(SessionEventData::ToolInputDelta {
                                    session_id: ctx.session_id.clone(),
                                    assistant_message_id: params.assistant_message_id.clone(),
                                    id: entry.id.clone(),
                                    delta: arguments,
                                })
                                .await?;
                        }
                    }
                }
                StreamEvent::Finish { reason } => {
                    finish = Some((map_finish(&reason), reason));
                    // Cardinal completes a tool call's arguments at the
                    // finish chunk, so the calls settle — input ended,
                    // `tool.called`, execution spawned — while the stream is
                    // still open (usage / `[DONE]` may follow). Upstream
                    // starts tool executions as tool-calls arrive,
                    // concurrent with reading the stream.
                    settle_tool_calls(ctx, params, &mut tool_acc, &mut tool_tasks, token).await?;
                }
                StreamEvent::Usage(seen) => usage = Some(seen),
                StreamEvent::Done => break,
            },
        }
    }

    flush_blocks(
        ctx,
        params,
        &mut text_ordinal,
        &mut reasoning_ordinal,
        &text,
        &reasoning,
    )
    .await?;
    if !*started {
        // The stream produced nothing at all: a transport-grade failure so
        // the retry loop runs (the final attempt settles as step.failed).
        return Ok(Attempt::Transport {
            error: CardinalError::EarlyClose,
            published: false,
        });
    }
    let Some((mapped_finish, raw_finish)) = finish else {
        // Upstream treats a missing finish reason as invalid provider
        // output; any open tool input settles as incomplete first
        // (upstream `failIncompleteTools`).
        fail_unsettled_tools(ctx, params, &mut tool_acc, incomplete_input_error()).await?;
        join_tool_tasks(ctx, params, &mut tool_tasks, token).await?;
        let outcome = fail_step(
            ctx,
            params,
            SessionError {
                kind: "unknown".to_owned(),
                message: "stream ended without a finish reason".to_owned(),
                status: None,
                response: None,
            },
            Some(map_tokens(usage)),
            true,
        )
        .await?;
        return Ok(Attempt::Done(outcome));
    };

    // Anything still unsettled without a `Finish`-driven settlement (a
    // stream that ended before its finish chunk carried no calls) is
    // idempotently settled here.
    settle_tool_calls(ctx, params, &mut tool_acc, &mut tool_tasks, token).await?;

    ctx.bus
        .publish(SessionEventData::StepStreamed {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
        })
        .await?;
    // Wait for every spawned execution; an interrupt while waiting aborts
    // the remainder and settles their parts aborted.
    join_tool_tasks(ctx, params, &mut tool_tasks, token).await?;
    // An interrupt that landed during the join settles the step
    // interrupted (upstream: failAssistant, no step.ended).
    if token.is_cancelled() {
        return Ok(Attempt::Done(
            settle_interrupted(ctx, params, *started, Some(map_tokens(usage))).await?,
        ));
    }
    ctx.bus
        .publish(SessionEventData::StepEnded {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
            finish: mapped_finish,
            raw_finish: Some(raw_finish),
            provider_state: None,
            cost: crate::schema::support::Finite::Int(0),
            tokens: map_tokens(usage),
            snapshot: None,
            files: None,
        })
        .await?;
    Ok(Attempt::Done(StepOutcome::Completed {
        finish: mapped_finish,
    }))
}

/// Upstream `TOOLS_INTERRUPTED` as an `aborted` session error.
fn aborted_error(message: &str) -> SessionError {
    SessionError {
        kind: "aborted".to_owned(),
        message: message.to_owned(),
        status: None,
        response: None,
    }
}

/// Upstream `INPUT_FAILURES.incomplete`.
fn incomplete_input_error() -> SessionError {
    SessionError {
        kind: "tool.input-incomplete".to_owned(),
        message:
            "Tool call arguments were not completed and were not executed. Reissue the call with complete arguments."
                .to_owned(),
        status: None,
        response: None,
    }
}

/// Settle every complete-but-uncalled accumulator, in index order: end its
/// input, publish `tool.called` with the parsed arguments, and spawn the
/// execution. Malformed arguments fail the call instead (upstream
/// `INPUT_FAILURES.malformed`). Idempotent via `ToolAccum::called`.
async fn settle_tool_calls(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    accs: &mut BTreeMap<u32, ToolAccum>,
    tasks: &mut Vec<(String, JoinHandle<()>)>,
    token: &CancellationToken,
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
                    ctx.bus
                        .publish(SessionEventData::ToolFailed {
                            session_id: ctx.session_id.clone(),
                            assistant_message_id: params.assistant_message_id.clone(),
                            id: acc.id.clone(),
                            error: SessionError {
                                kind: "tool.input-json".to_owned(),
                                message: "The tool input could not be parsed as JSON, so the call was not executed. Reissue the call with valid JSON arguments."
                                    .to_owned(),
                                status: None,
                                response: None,
                            },
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
        let input_value = Value::Object(input_record.clone().into_iter().collect());
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
        match ctx.tools.get(&acc.name) {
            Some(tool) => tasks.push((
                acc.id.clone(),
                spawn_tool_task(ctx, token, params, acc.id.clone(), tool, input_value),
            )),
            None => {
                ctx.bus
                    .publish(SessionEventData::ToolFailed {
                        session_id: ctx.session_id.clone(),
                        assistant_message_id: params.assistant_message_id.clone(),
                        id: acc.id.clone(),
                        error: SessionError {
                            kind: "unknown".to_owned(),
                            message: format!("Unknown tool: {}", acc.name),
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
    tool: Arc<dyn super::Tool>,
    input: Value,
) -> JoinHandle<()> {
    let bus = ctx.bus.clone();
    let session_id = ctx.session_id.clone();
    let message_id = params.assistant_message_id.clone();
    let directory = ctx.directory.clone();
    // A child token: interrupting the busy period cancels the tool at its
    // next await point.
    let cancellation = token.child_token();
    tokio::spawn(async move {
        let output = tool
            .execute(
                ToolContext {
                    session_id: session_id.clone(),
                    directory,
                    call_id: call_id.clone(),
                    cancellation,
                },
                input,
            )
            .await;
        let event = match output.error {
            None => SessionEventData::ToolSuccess {
                session_id,
                assistant_message_id: message_id,
                id: call_id,
                content: vec![Content::Text {
                    text: tool_output_text(output.content),
                }],
                metadata: None,
                executed: false,
                result_state: None,
            },
            Some(message) => SessionEventData::ToolFailed {
                session_id,
                assistant_message_id: message_id,
                id: call_id,
                error: SessionError {
                    kind: "unknown".to_owned(),
                    message,
                    status: None,
                    response: None,
                },
                content: None,
                metadata: None,
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

/// Fail every input-started-but-uncalled accumulator with `error`
/// (upstream `failUnsettledTools`): the projector settles the streaming
/// tool part into the error state.
async fn fail_unsettled_tools(
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
async fn join_tool_tasks(
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

/// Publish `text.ended`/`reasoning.ended` for any open blocks. Idempotent:
/// ordinals are taken (set to `None`) as they close.
async fn flush_blocks(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    text_ordinal: &mut Option<i64>,
    reasoning_ordinal: &mut Option<i64>,
    text: &str,
    reasoning: &str,
) -> Result<()> {
    if let Some(ordinal) = text_ordinal.take() {
        ctx.bus
            .publish(SessionEventData::TextEnded {
                session_id: ctx.session_id.clone(),
                assistant_message_id: params.assistant_message_id.clone(),
                ordinal,
                text: text.to_owned(),
                state: None,
            })
            .await?;
    }
    if let Some(ordinal) = reasoning_ordinal.take() {
        ctx.bus
            .publish(SessionEventData::ReasoningEnded {
                session_id: ctx.session_id.clone(),
                assistant_message_id: params.assistant_message_id.clone(),
                ordinal,
                text: reasoning.to_owned(),
                state: None,
            })
            .await?;
    }
    Ok(())
}

/// Upstream token shape (`SessionUsage.tokens`): input excludes cached
/// prompt tokens, cache write is not reported by cardinal's OpenAI dialect.
/// A stream that never carried usage settles as zeros.
fn map_tokens(usage: Option<Usage>) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage {
            input: 0,
            output: 0,
            reasoning: 0,
            cache: TokenCache { read: 0, write: 0 },
        };
    };
    let cached = usage.cached_tokens.unwrap_or(0);
    TokenUsage {
        input: (usage.prompt_tokens.saturating_sub(cached)) as i64,
        output: usage.completion_tokens as i64,
        reasoning: usage.reasoning_tokens.unwrap_or(0) as i64,
        cache: TokenCache {
            read: cached as i64,
            write: 0,
        },
    }
}

/// Map cardinal's raw `finish_reason` to the upstream literal union.
fn map_finish(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "tool-calls" => FinishReason::ToolCalls,
        "content_filter" | "content-filter" => FinishReason::ContentFilter,
        "error" => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

/// Non-schema transport failure (upstream `toSessionError` fallback).
fn to_session_error(error: &CardinalError) -> SessionError {
    SessionError {
        kind: "unknown".to_owned(),
        message: error.to_string(),
        status: None,
        response: None,
    }
}

// ---------------------------------------------------------------------------
// Context assembly (one named mapping; the e2e suite trusts this function)
// ---------------------------------------------------------------------------

/// Build the OpenAI message list: the system prompt, then the session
/// transcript in `seq` order.
///
/// Mapping (upstream `to-llm-message.ts`):
/// - `user`     → `{role: "user", content: joined text parts}`, dropped
///   entirely when the assembled content is empty (to-llm-message.ts:274);
///   file attachments degrade to inline markers (upstream inlines resolved
///   file content during prompt preparation, which lands in 6b).
/// - `synthetic`/`skill` → user-role text.
/// - `system`   → `{role: "system", content}` (to-llm-message.ts:291).
/// - `assistant` → `{role: "assistant", content: text parts joined,
///   tool_calls: the row's tool parts}`; reasoning content is dropped.
/// - A settled assistant tool part (`completed`/`error`, `executed: false`)
///   additionally becomes `{role: "tool", tool_call_id, content}` — joined
///   JSON content for completed calls, `{error: …}` for failed ones
///   (upstream `toolResult`).
/// - `compaction` (completed) → the checkpoint user message carrying the
///   summary (and `recent` when present); incomplete compactions are
///   skipped, and history selection starts at the latest completed row
///   anyway (upstream `SessionHistory` boundary).
/// - idle/shell/switch markers never reach the model.
pub(crate) fn build_chat_messages(system: &str, messages: &[Message]) -> Vec<ChatMessage> {
    // Upstream `messageEntries`: history starts at the latest completed
    // compaction row, so the checkpoint replaces everything before it.
    let boundary = messages.iter().rposition(|message| {
        matches!(
            message,
            Message::Compaction(CompactionMessage {
                compaction: Compaction::Completed(_),
                ..
            })
        )
    });
    let mut out = vec![ChatMessage::System {
        content: system.to_owned(),
    }];
    for message in messages.iter().skip(boundary.unwrap_or(0)) {
        match message {
            Message::User { prompt, .. } => {
                let content = user_prompt_text(prompt);
                if content.is_empty() {
                    // Upstream: a user message with no assembled content is
                    // omitted from the transcript.
                    continue;
                }
                out.push(ChatMessage::User { content });
            }
            Message::Synthetic { text, .. } | Message::Skill { text, .. } => {
                out.push(ChatMessage::User {
                    content: text.clone(),
                })
            }
            Message::System { text, .. } => out.push(ChatMessage::System {
                content: text.clone(),
            }),
            Message::Assistant(assistant) => {
                let mut parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                let mut tool_results: Vec<ChatMessage> = Vec::new();
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text { text, .. } => parts.push(text.clone()),
                        // Reasoning is dropped (upstream's provider-bound
                        // state replay has no ox wire counterpart yet).
                        AssistantContent::Reasoning { .. } => {}
                        AssistantContent::Tool {
                            id,
                            name,
                            executed,
                            state,
                            ..
                        } => {
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                kind: FunctionType::Function,
                                function: FunctionCall {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(&tool_input(state))
                                        .unwrap_or_else(|_| "{}".to_owned()),
                                },
                            });
                            // Host-executed parts echo their result;
                            // provider-executed ones don't (upstream
                            // `executed !== true`).
                            if *executed != Some(true)
                                && let Some(result) = tool_result_message(id, state)
                            {
                                tool_results.push(result);
                            }
                        }
                    }
                }
                if parts.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                out.push(ChatMessage::Assistant {
                    content: if parts.is_empty() {
                        None
                    } else {
                        Some(parts.join(""))
                    },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                });
                out.extend(tool_results);
            }
            Message::Compaction(CompactionMessage {
                compaction: Compaction::Completed(completed),
                ..
            }) => {
                let CompactionCompleted {
                    summary, recent, ..
                } = &**completed;
                out.push(ChatMessage::User {
                    content: compaction::checkpoint_message(summary, recent),
                });
            }
            Message::Compaction(_) => {}
            Message::Idle { .. }
            | Message::Shell { .. }
            | Message::AgentSwitched { .. }
            | Message::ModelSwitched { .. }
            | Message::LocationSwitched { .. } => {}
        }
    }
    out
}

/// A tool part's call input for the wire: parsed from the streaming state's
/// string, verbatim from the settled states (upstream `toolInput`).
fn tool_input(state: &ToolState) -> Record {
    match state {
        ToolState::Streaming { input } => serde_json::from_str(input).unwrap_or_default(),
        ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => input.clone(),
    }
}

/// The `{role: "tool"}` result message for a settled tool part
/// (upstream `toolResult`): joined text content for completed calls,
/// `{error: …}` for failed ones. A `streaming`/`running` part yields
/// `None` — its own row's result lands in the next request.
fn tool_result_message(id: &str, state: &ToolState) -> Option<ChatMessage> {
    let content = match state {
        ToolState::Completed { content, .. } => {
            let text = content
                .iter()
                .map(|content| match content {
                    Content::Text { text } => text.clone(),
                    Content::File { uri, mime, name } => format!(
                        "[file {} ({mime}) {uri}]",
                        name.clone().unwrap_or_else(|| "file".to_owned())
                    ),
                })
                .collect::<Vec<_>>()
                .join("");
            serde_json::to_string(&json!({ "output": text })).ok()?
        }
        ToolState::Error { error, .. } => serde_json::to_string(&json!({ "error": error })).ok()?,
        ToolState::Streaming { .. } | ToolState::Running { .. } => return None,
    };
    Some(ChatMessage::Tool {
        tool_call_id: id.to_owned(),
        content,
    })
}

/// A user prompt's wire text: the typed text plus attachment markers.
/// (Upstream `SessionPrompt.prepare` inlines resolved file content; 6a
/// stores attachments verbatim and marks them instead.)
fn user_prompt_text(prompt: &Prompt) -> String {
    let mut text = prompt.text.clone();
    if let Some(files) = &prompt.files {
        for file in files {
            let name = file.name.clone().unwrap_or_else(|| "file".to_owned());
            text.push_str(&format!("\n[attached file: {name} ({})]", file.mime));
        }
    }
    text
}

/// The session transcript in `seq` order, decoded from `session_message`.
pub(crate) async fn load_history(db: &crate::db::Db, session_id: &str) -> Result<Vec<Message>> {
    let session_id = session_id.to_owned();
    let rows: Vec<(String, String, String)> = db
        .conn()
        .call(move |c| {
            let mut stmt = c.prepare(
                "SELECT id, type, data FROM session_message
                 WHERE session_id = ?1 ORDER BY seq ASC",
            )?;
            let mapped = stmt.query_map(rusqlite::params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    rows.into_iter()
        .map(|(id, kind, data)| {
            let value = serde_json::from_str(&data)?;
            Ok(Message::from_row_data(&id, &kind, value)?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cardinal::wire::Usage;
    use crate::schema::message::{
        AssistantContent, AssistantMessage, AssistantTime, Message, TimeCreated,
    };
    use crate::schema::support::{ModelRef, Prompt};

    // Gap: upstream usage.ts maps `input` to the NON-cached prompt tokens —
    // input + cache.read must reconstruct prompt_tokens exactly once.
    #[test]
    fn map_tokens_input_excludes_cached_prompt_tokens() {
        let tokens = map_tokens(Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 10,
            cached_tokens: Some(40),
            reasoning_tokens: Some(5),
        }));
        assert_eq!(tokens.input, 60, "input = prompt_tokens − cached");
        assert_eq!(tokens.output, 10);
        assert_eq!(tokens.reasoning, 5);
        assert_eq!(tokens.cache.read, 40);
        assert_eq!(
            tokens.cache.write, 0,
            "OpenAI-compatible streams never report cache writes"
        );
        assert_eq!(tokens.input + tokens.cache.read, 100);

        // Absent usage and absent cache fields settle sanely.
        let none = map_tokens(None);
        assert_eq!(
            (none.input, none.output, none.cache.read, none.cache.write),
            (0, 0, 0, 0)
        );
        let uncached = map_tokens(Some(Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            cached_tokens: None,
            reasoning_tokens: None,
        }));
        assert_eq!(uncached.input, 7);
        assert_eq!(uncached.cache.read, 0);
    }

    fn user_row(id: &str, text: &str) -> Message {
        Message::User {
            id: id.to_owned(),
            metadata: None,
            time: TimeCreated { created: 1 },
            prompt: Prompt {
                text: text.to_owned(),
                files: None,
                agents: None,
                skills: None,
            },
        }
    }

    #[test]
    fn system_rows_map_to_system_role_and_empty_users_are_dropped() {
        let assistant = Message::Assistant(Box::new(AssistantMessage {
            id: "msg_a".to_owned(),
            metadata: None,
            time: AssistantTime {
                created: 2,
                streamed: None,
                completed: Some(3),
            },
            agent: "build".to_owned(),
            model: ModelRef {
                id: "m".to_owned(),
                provider_id: "p".to_owned(),
                variant: None,
            },
            content: vec![AssistantContent::Text {
                text: "hi".to_owned(),
                state: None,
            }],
            snapshot: None,
            finish: None,
            tokens: None,
            error: None,
            cost: None,
            raw_finish: None,
            provider_state: None,
            retry: None,
        }));
        let messages = vec![
            user_row("msg_u1", "question"),
            user_row("msg_u2", ""), // empty assembled content → dropped
            Message::System {
                id: "msg_s".to_owned(),
                metadata: None,
                time: TimeCreated { created: 4 },
                text: "server notice".to_owned(),
                description: None,
            },
            assistant,
        ];
        let chat = build_chat_messages("sys", &messages);
        let system_roles: Vec<&str> = chat
            .iter()
            .filter_map(|m| match m {
                ChatMessage::System { .. } => Some("system"),
                _ => None,
            })
            .collect();
        assert_eq!(
            system_roles,
            vec!["system", "system"],
            "prompt + system row"
        );

        let user_texts: Vec<&str> = chat
            .iter()
            .filter_map(|m| match m {
                ChatMessage::User { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            user_texts,
            vec!["question"],
            "empty user dropped, text kept"
        );

        assert!(chat.iter().any(
            |m| matches!(m, ChatMessage::Assistant { content: Some(text), .. } if text == "hi")
        ));
    }
}
