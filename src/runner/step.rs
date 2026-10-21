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
//! - A mid-stream failure after published output never transparently
//!   retries: only an *interrupted stream* continues (upstream
//!   `Outcome.Continue`) — a transport read failure, or a stream that ends
//!   without a recognized finish reason. The partial assistant settles
//!   `session.step.failed` with the stream error (upstream `failAssistant`:
//!   no `step.ended`, finish `error`, open tool inputs fail with the same
//!   error), the retry announcement follows, and the continuation prompt
//!   (llm.ts `CONTINUE_AFTER_INCOMPLETE_STREAM`) is published as a
//!   synthetic user message before the attempt re-runs against a fresh
//!   assistant row.
//!
//! The context assembly lives in one named function
//! ([`build_chat_messages`]) so the e2e suite can trust the mapping.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cardinal::CardinalError;
use crate::cardinal::chat::BoxChatStream;
use crate::cardinal::stream::StreamEvent;
use crate::cardinal::wire::{
    ChatMessage, ChatRequest, FunctionCall, FunctionType, ImageUrl, ToolCall, Usage, UserPart,
};
use crate::id;
use crate::prompt;
use crate::schema::event::SessionEventData;
use crate::schema::message::{
    AssistantContent, Compaction, CompactionCompleted, CompactionMessage, Message, ToolState,
};
use crate::schema::support::{
    Content, FinishReason, ModelRef, Prompt, Record, SessionError, SessionErrorResponse,
    TokenCache, TokenUsage,
};

use super::ToolContext;
use super::compaction;
use super::drain::DrainCtx;

/// Upstream `STEP_INTERRUPTED` (runner/step.ts).
const STEP_INTERRUPTED: &str = "Step interrupted";
/// Upstream `TOOLS_INTERRUPTED` (runner/step.ts).
const TOOLS_INTERRUPTED: &str = "Tool execution interrupted";
/// Transport attempts before the step fails (upstream policy arrives later).
const MAX_ATTEMPTS: usize = 3;
/// Backoff base; attempt N sleeps `base * N`.
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Upstream `CONTINUE_AFTER_INCOMPLETE_STREAM` (runner/llm.ts): the
/// continuation prompt published after an interrupted stream so the model
/// resumes where it left off.
const CONTINUE_AFTER_INCOMPLETE_STREAM: &str = "The previous response was interrupted. Continue from where you left off without repeating completed content.";

/// Terminal states of one step, consumed by the drain loop.
#[derive(Debug)]
pub(crate) enum StepOutcome {
    /// The stream finished. `FinishReason::ToolCalls` continues the turn
    /// (the tool parts are already settled in the assistant row);
    /// `needs_continuation` records whether tool calls were executed, which
    /// owns the length-continuation boundary (upstream
    /// `Outcome.Completed.needsContinuation`).
    Completed {
        finish: FinishReason,
        needs_continuation: bool,
    },
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
    /// The attempt failed; `published` says whether provider output/events
    /// already went out. A published failure retries only as an upstream
    /// continuation (`Outcome.Continue`), never transparently.
    Failed {
        failure: AttemptFailure,
        published: bool,
        tokens: Option<TokenUsage>,
    },
}

/// Why an attempt died (upstream error classes on the retry/continue path).
enum AttemptFailure {
    /// A transport-grade failure: connect error, mid-stream read failure
    /// (`EarlyClose`), malformed SSE framing, or a connection error.
    Transport(CardinalError),
    /// The stream ended without a recognized finish reason (upstream
    /// `unknownFinish` → `InvalidProviderOutput` classified
    /// `incomplete-stream`).
    IncompleteStream,
}

impl AttemptFailure {
    /// Upstream `isInterruptedStream` (step.ts): interrupted streams
    /// continue via a continuation prompt; content errors (HTTP rejections,
    /// in-band provider failures) fail the step.
    fn is_interrupted_stream(&self) -> bool {
        match self {
            AttemptFailure::Transport(
                CardinalError::EarlyClose | CardinalError::Sse(_) | CardinalError::Transport(_),
            ) => true,
            AttemptFailure::Transport(CardinalError::Http { .. } | CardinalError::Upstream(_)) => {
                false
            }
            AttemptFailure::IncompleteStream => true,
        }
    }

    /// Upstream gates `Outcome.Compacted` on `!outputStarted`: overflow
    /// recovery is a pre-output concern.
    fn is_overflow(&self) -> bool {
        match self {
            AttemptFailure::Transport(error) => super::provider_error::is_overflow_error(error),
            AttemptFailure::IncompleteStream => false,
        }
    }

    /// The session error the row settles with (upstream `toSessionError`).
    fn session_error(&self) -> SessionError {
        match self {
            AttemptFailure::Transport(error) => to_session_error(error),
            AttemptFailure::IncompleteStream => invalid_output_error(),
        }
    }
}

/// Run one step end-to-end (context assembly → stream → durable settlement).
pub(crate) async fn run(
    ctx: &DrainCtx,
    token: &CancellationToken,
    params: &mut StepParams<'_>,
) -> Result<StepOutcome> {
    // Request dispatch time, published with step.started (before any output).
    let mut dispatch = id::now_millis();
    let mut started = false;

    let mut attempt: usize = 0;
    loop {
        attempt += 1;
        // The transcript is rebuilt per attempt (upstream refreshes context
        // on retry and continuation): a continuation attempt must observe
        // the settled partial row plus the continuation prompt.
        // The magic-context head replaces everything it covers: the pass mints
        // the frozen bytes and reports the ordinal floor, so the transcript
        // starts after the covered range.
        let budget = crate::mctx::budget_for(&ctx.config, &ctx.model_catalog, &params.model.id);
        let (head_m0, head_m1, floor) = mctx_head(ctx, budget).await;
        // Pressure is checked after the pass: the floor it reports is what the
        // live tail is measured above. Both windows were resolved once, when
        // the budget was derived.
        super::historian_trigger::mctx_historian(ctx, budget, floor);
        let rows = if ctx.config.magic_context.enabled {
            // The §N§ overlay (tag prefixes + drop placeholders) is part of the
            // integration: with it off the request must stay byte-identical to
            // the integration-free one.
            load_overlaid_tail(&ctx.db, &ctx.session_id, floor).await?
        } else {
            load_history_after(&ctx.db, &ctx.session_id, floor).await?
        };
        let system = prompt::build(&ctx.directory);
        let mut request = ChatRequest::new(
            // cardinal routes on the bare model id; the providerID/modelID
            // qualified form is upstream's provider-adapter vocabulary and
            // cardinal answers "no route for model cardinal/<id>" to it.
            params.model.id.clone(),
            build_chat_messages(&system, &[head_m0.as_deref(), head_m1.as_deref()], &rows),
        );
        request.tools = ctx.tools.definitions();

        let stream = match ctx.chat.stream_chat(&request).await {
            Ok(stream) => stream,
            Err(error) => {
                // Context overflow → the drain compacts and retries the step
                // once (upstream Outcome.Compacted); connect errors never
                // carry published output.
                if super::provider_error::is_overflow_error(&error) {
                    return Ok(StepOutcome::Overflow {
                        error: to_session_error(&error),
                    });
                }
                // Connect failure: nothing was published for this attempt.
                if attempt < MAX_ATTEMPTS {
                    schedule_retry(
                        ctx,
                        params,
                        &mut started,
                        attempt,
                        &to_session_error(&error),
                    )
                    .await?;
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
            Attempt::Failed {
                failure,
                published,
                tokens,
            } => {
                // Overflow recovery is a pre-output concern (upstream gates
                // Outcome.Compacted on `!outputStarted`).
                if !published && failure.is_overflow() {
                    return Ok(StepOutcome::Overflow {
                        error: failure.session_error(),
                    });
                }
                let error = failure.session_error();
                // Upstream retry policy (step.ts): transparent retry before
                // any output; after it, only an interrupted stream continues
                // (Outcome.Continue).
                if attempt < MAX_ATTEMPTS && (failure.is_interrupted_stream() || !published) {
                    if published {
                        // The partial assistant settles `step.failed` with
                        // the stream error first (upstream failAssistant —
                        // no step.ended, finish `error`).
                        fail_step(ctx, params, error.clone(), tokens, started).await?;
                    }
                    schedule_retry(ctx, params, &mut started, attempt, &error).await?;
                    if !sleep_backoff(token, attempt).await {
                        if published {
                            // The row already settled; the interrupt only
                            // ends the turn (upstream: retry.wait aborts
                            // without further step events).
                            return Ok(StepOutcome::Interrupted);
                        }
                        return settle_interrupted(ctx, params, started, None).await;
                    }
                    if published {
                        // The continuation prompt lands as a synthetic user
                        // message; the next attempt runs against a fresh
                        // assistant row (upstream runStep Continue branch).
                        ctx.bus
                            .publish(SessionEventData::SessionSynthetic {
                                session_id: ctx.session_id.clone(),
                                text: CONTINUE_AFTER_INCOMPLETE_STREAM.to_owned(),
                                description: None,
                                metadata: None,
                            })
                            .await?;
                        params.assistant_message_id = id::id("msg");
                        started = false;
                        dispatch = id::now_millis();
                    }
                    continue;
                }
                return fail_step(ctx, params, error, tokens, started).await;
            }
        }
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
    error: &SessionError,
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
            error: error.clone(),
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

/// Settle the assistant row as failed: start the row when the failure hit
/// before `step.started`, then publish `session.step.failed` (the projector
/// writes the error onto the row and closes it with finish `error`).
/// Shared with the drain's overflow give-up and the continuation settle
/// (upstream raises `StepFailedError` / runs `failAssistant` there).
pub(crate) async fn fail_step(
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
                // Upstream `publisher.streamed()`: an attempt that produced
                // output marks the row streamed even when the stream then
                // dies before `Finish`.
                if *started {
                    ctx.bus
                        .publish(SessionEventData::StepStreamed {
                            session_id: ctx.session_id.clone(),
                            assistant_message_id: params.assistant_message_id.clone(),
                        })
                        .await?;
                }
                // Upstream `failAssistant`: settlement only happens at
                // `Finish`, so open inputs fail with the stream error itself
                // (failTools `uncalled`) and no tool part stays streaming.
                fail_unsettled_tools(ctx, params, &mut tool_acc, to_session_error(&error)).await?;
                join_tool_tasks(ctx, params, &mut tool_tasks, token).await?;
                let published = *started;
                return Ok(Attempt::Failed {
                    failure: AttemptFailure::Transport(error),
                    published,
                    tokens: None,
                });
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
                    let mapped = map_finish(&reason);
                    finish = Some((mapped, reason));
                    // Cardinal completes a tool call's arguments at the
                    // finish chunk, so the calls settle — input ended,
                    // `tool.called`, execution spawned — while the stream is
                    // still open (usage / `[DONE]` may follow). Upstream
                    // starts tool executions as tool-calls arrive,
                    // concurrent with reading the stream. A length finish
                    // upgrades the malformed-input failure (upstream
                    // `failToolInput`).
                    settle_tool_calls(
                        ctx,
                        params,
                        &mut tool_acc,
                        &mut tool_tasks,
                        token,
                        mapped == FinishReason::Length,
                    )
                    .await?;
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
        return Ok(Attempt::Failed {
            failure: AttemptFailure::Transport(CardinalError::EarlyClose),
            published: false,
            tokens: None,
        });
    }
    let Some((mapped_finish, raw_finish)) =
        finish.filter(|(mapped, _)| *mapped != FinishReason::Unknown)
    else {
        // Upstream `unknownFinish`: a stream that ends without a recognized
        // finish reason is invalid provider output — an interrupted stream
        // that continues via the upstream prompt after published output
        // (run's Outcome.Continue gate). Open inputs fail with the same
        // error (failAssistant), and the attempt keeps its usage.
        fail_unsettled_tools(ctx, params, &mut tool_acc, invalid_output_error()).await?;
        join_tool_tasks(ctx, params, &mut tool_tasks, token).await?;
        return Ok(Attempt::Failed {
            failure: AttemptFailure::IncompleteStream,
            published: true,
            tokens: Some(map_tokens(usage)),
        });
    };

    // Anything still unsettled without a `Finish`-driven settlement (a
    // stream that ended before its finish chunk carried no calls) is
    // idempotently settled here.
    let length_finish = mapped_finish == FinishReason::Length;
    settle_tool_calls(
        ctx,
        params,
        &mut tool_acc,
        &mut tool_tasks,
        token,
        length_finish,
    )
    .await?;
    let needs_continuation = tool_acc.values().any(|acc| acc.called);

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
        needs_continuation,
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
async fn settle_tool_calls(
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
fn tool_metadata(value: Option<Value>) -> Option<crate::schema::support::Record> {
    match value {
        Some(Value::Object(map)) => Some(map.into_iter().collect()),
        _ => None,
    }
}

/// Upstream `unknownFinish` (step.ts): the session error for a stream that
/// ended without a recognized finish reason (`InvalidProviderOutput`
/// classified `incomplete-stream`).
fn invalid_output_error() -> SessionError {
    SessionError {
        kind: "provider.invalid-output".to_owned(),
        message: "The provider response ended with an unknown finish reason.".to_owned(),
        status: None,
        response: None,
    }
}

/// Fail every input-started-but-uncalled accumulator with one shared error
/// (upstream `failAssistant`'s failTools `uncalled` sweep): the projector
/// settles the streaming tool part into the error state.
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

/// Map a cardinal failure to a session error (upstream `toSessionError`):
/// the provider taxonomy for HTTP/stream failures, upstream's transport
/// family for connection-level faults.
fn to_session_error(error: &CardinalError) -> SessionError {
    let (kind, status, response) = match error {
        CardinalError::Http { status, body } => (
            // Any remaining 4xx is a deterministic rejection of this request;
            // server-side statuses report a provider failure.
            if (400..500).contains(status) {
                "provider.invalid-request"
            } else {
                "provider.unknown"
            },
            Some(*status),
            Some(SessionErrorResponse { body: body.clone() }),
        ),
        CardinalError::Upstream(body) => (
            // An in-band provider failure forwarded by cardinal.
            "provider.unknown",
            None,
            Some(SessionErrorResponse { body: body.clone() }),
        ),
        CardinalError::EarlyClose | CardinalError::Sse(_) | CardinalError::Transport(_) => {
            ("provider.transport", None, None)
        }
    };
    SessionError {
        kind: kind.to_owned(),
        message: error.to_string(),
        status: status.map(i64::from),
        response,
    }
}

// ---------------------------------------------------------------------------
// Context assembly (one named mapping; the e2e suite trusts this function)
// ---------------------------------------------------------------------------

/// The magic-context head for one step: the frozen `m[0]`/`m[1]` halves plus
/// the transcript floor they cover, or `(None, None, 0)` when the integration
/// is off or the pass failed (the prompt then stays byte-identical to the
/// integration-off one — a broken head must never fail the turn).
async fn mctx_head(
    ctx: &DrainCtx,
    budget: crate::mctx::budget::Budget,
) -> (Option<String>, Option<String>, i64) {
    if !ctx.config.magic_context.enabled {
        return (None, None, 0);
    }
    let last_seq = match latest_seq(&ctx.db, &ctx.session_id).await {
        Ok(seq) => seq,
        Err(error) => {
            tracing::warn!(session = %ctx.session_id, error = %error, "mctx pass failed; continuing without a head");
            return (None, None, 0);
        }
    };
    let project = ctx.directory.to_string_lossy().into_owned();
    let request = crate::mctx::pass::PassRequest {
        db: &ctx.db,
        session_id: &ctx.session_id,
        project: &project,
        last_seq,
        budget,
        forced: false,
        run_started: false,
    };
    match crate::mctx::pass::run(request).await {
        Ok(result) => (result.head_m0, result.head_m1, result.floor),
        Err(error) => {
            tracing::warn!(
                session = %ctx.session_id,
                error = %error,
                "mctx pass failed; continuing without a head"
            );
            (None, None, 0)
        }
    }
}

/// The highest transcript ordinal in a session.
async fn latest_seq(db: &crate::db::Db, session_id: &str) -> Result<i64> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT COALESCE(MAX(seq), 0) FROM session_message WHERE session_id = ?1")
        .await?;
    let mut rows = stmt.query([session_id]).await?;
    let seq = match rows.next().await? {
        Some(row) => row.get::<i64>(0)?,
        None => 0,
    };
    Ok(seq)
}

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
///   tool_calls: the row's tool parts, reasoning_content: reasoning parts
///   joined}`; `reasoning_content` is omitted when the row has no reasoning
///   (upstream `lowerAssistantMessage`).
/// - A settled assistant tool part (`completed`/`error`, `executed: false`)
///   additionally becomes `{role: "tool", tool_call_id, content}` — joined
///   JSON content for completed calls, `{error: …}` for failed ones
///   (upstream `toolResult`).
/// - `compaction` (completed) → the checkpoint user message carrying the
///   summary (and `recent` when present); incomplete compactions are
///   skipped, and history selection starts at the latest completed row
///   anyway (upstream `SessionHistory` boundary).
/// - idle/shell/switch markers never reach the model.
pub(crate) fn build_chat_messages(
    system: &str,
    head: &[Option<&str>],
    messages: &[Message],
) -> Vec<ChatMessage> {
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
    // The head halves are synthetic user messages leading the transcript: they
    // stand in for the covered history. They stay SEPARATE because the baseline
    // (`m[0]`) is the provider's cache anchor — merging them would rewrite the
    // anchored bytes on every delta.
    for half in head.iter().flatten() {
        out.push(ChatMessage::User {
            content: (*half).to_owned(),
        });
    }
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
                // A synthetic row with no text is display-only (a background
                // notice: its `description` is what the transcript shows), so
                // it must not become an empty user turn for the model.
                if text.is_empty() {
                    continue;
                }
                out.push(ChatMessage::User {
                    content: text.clone(),
                })
            }
            Message::System { text, .. } => out.push(ChatMessage::System {
                content: text.clone(),
            }),
            Message::Assistant(assistant) => {
                let mut parts: Vec<String> = Vec::new();
                let mut reasoning: Vec<String> = Vec::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                let mut tool_results: Vec<ChatMessage> = Vec::new();
                for content in &assistant.content {
                    match content {
                        AssistantContent::Text { text, .. } => parts.push(text.clone()),
                        // Replayed verbatim as `reasoning_content`: thinking
                        // vendors (GLM/Z.ai, Kimi) 400 follow-up turns without
                        // it (upstream `lowerAssistantMessage` replays the same
                        // joined text). Replayed regardless of the row's model
                        // — upstream degrades cross-model reasoning to text
                        // (to-llm-message.ts `sameModel`); ox has no
                        // per-provider gate, cardinal owns dialects.
                        AssistantContent::Reasoning { text, .. } if !text.is_empty() => {
                            reasoning.push(text.clone())
                        }
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
                                && let Some((result, media)) = tool_result_message(id, state)
                            {
                                tool_results.push(result);
                                if !media.is_empty() {
                                    // A provider accepts an image only as user
                                    // content, so it follows the tool result as
                                    // its own turn.
                                    tool_results.push(ChatMessage::UserMedia { content: media });
                                }
                            }
                        }
                    }
                }
                // A reasoning-only row still carries its reasoning_content on
                // the wire (upstream `meaningful` keeps it) — dropping the row
                // would 400 thinking vendors on the next turn.
                if parts.is_empty() && tool_calls.is_empty() && reasoning.is_empty() {
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
                    reasoning_content: if reasoning.is_empty() {
                        None
                    } else {
                        Some(reasoning.join(""))
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
pub(crate) fn tool_input(state: &ToolState) -> Record {
    match state {
        ToolState::Streaming { input } => serde_json::from_str(input).unwrap_or_default(),
        ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => input.clone(),
    }
}

/// The `{role: "tool"}` result for a settled tool part (upstream
/// `toolResult`): joined text content for completed calls, `{error: …}` for
/// failed ones. A `streaming`/`running` part yields `None` — its own row's
/// result lands in the next request.
///
/// Attachments cannot ride inside a `tool` message: providers accept an image
/// only as user content. They are returned separately so the caller can append
/// one follow-up user message directly after the result.
fn tool_result_message(id: &str, state: &ToolState) -> Option<(ChatMessage, Vec<UserPart>)> {
    let mut media: Vec<UserPart> = Vec::new();
    let content = match state {
        ToolState::Completed { content, .. } => {
            let text = content
                .iter()
                .map(|content| match content {
                    Content::Text { text } => text.clone(),
                    Content::File { uri, mime, name } => {
                        media.push(UserPart::Image {
                            image_url: ImageUrl { url: uri.clone() },
                        });
                        format!(
                            "[attached {} ({mime})]",
                            name.clone().unwrap_or_else(|| "file".to_owned())
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            serde_json::to_string(&json!({ "output": text })).ok()?
        }
        ToolState::Error { error, .. } => serde_json::to_string(&json!({ "error": error })).ok()?,
        ToolState::Streaming { .. } | ToolState::Running { .. } => return None,
    };
    Some((
        ChatMessage::Tool {
            tool_call_id: id.to_owned(),
            content,
        },
        media,
    ))
}

/// A user prompt's wire text: the typed text plus attachment markers.
/// (Upstream `SessionPrompt.prepare` inlines resolved file content; 6a
/// stores attachments verbatim and marks them instead.)
pub(crate) fn user_prompt_text(prompt: &Prompt) -> String {
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
    load_history_after(db, session_id, i64::MIN).await
}

/// The text of the newest assistant row, if the transcript has one. Used by the
/// todo reminder to tell a stop from a hand-off to the user. Reads the tail
/// only: the reminder runs once per turn end, and a full transcript scan there
/// would be wasteful on long sessions.
pub(crate) async fn last_assistant_text(
    db: &crate::db::Db,
    session_id: &str,
) -> Result<Option<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT data FROM session_message
             WHERE session_id = ?1 AND type = 'assistant'
             ORDER BY seq DESC LIMIT 1",
        )
        .await?;
    let Some(row) = crate::db::optional(&mut stmt, [session_id]).await? else {
        return Ok(None);
    };
    let raw: String = row.get(0)?;
    let value: Value = serde_json::from_str(&raw).context("decode session_message.data")?;
    let message =
        Message::from_row_data("", "assistant", value).context("decode assistant message row")?;
    Ok(match message {
        Message::Assistant(assistant) => Some(
            assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    crate::schema::message::AssistantContent::Text { text, .. } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    })
}

/// Load a session transcript above `floor`: rows with a greater ordinal. The
/// magic-context head stands in for everything at or below the floor it
/// reports, so the request carries only the live tail.
pub(crate) async fn load_history_after(
    db: &crate::db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<Message>> {
    rows_after(db, session_id, floor)
        .await?
        .into_iter()
        .map(|row| decode_row(&row))
        .collect()
}

/// The tail with the magic-context §N§ overlay applied: tags minted (stable
/// across passes), tag prefixes rendered beside each taggable item, and the
/// content of an applied drop replaced by its placeholder.
pub(crate) async fn load_overlaid_tail(
    db: &crate::db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<Message>> {
    let mut rows = rows_after(db, session_id, floor).await?;
    let tags = crate::mctx::tagger::Tagger
        .assign(db, session_id, &rows)
        .await
        .context("assign §N§ tags")?;
    let drops = crate::mctx::tagger::drops(db, session_id)
        .await
        .context("load drops")?;
    crate::mctx::tagger::apply_overlay(
        &mut rows,
        &tags,
        &drops,
        crate::mctx::tagger::OverlayMode::Request,
    );
    rows.into_iter().map(|row| decode_row(&row)).collect()
}

/// One `session_message` row as the decoded message JSON: the row's `data`
/// column with `id`/`type` merged back in, exactly the shape the tagger's
/// `part_path` addresses (`decode({ ...row.data, id, type })` upstream).
/// [`decode_row`] reads `id`/`type` off the same object.
async fn rows_after(
    db: &crate::db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<serde_json::Value>> {
    Ok(rows_after_with_seq(db, session_id, floor)
        .await?
        .into_iter()
        .map(|(_, value)| value)
        .collect())
}

/// As [`rows_after`], but each row is paired with its `seq` ordinal. The
/// historian addresses coverage by ordinal (`seq`), which the decoded message
/// JSON does not carry, so it reads the tail through this variant.
pub(crate) async fn rows_after_with_seq(
    db: &crate::db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<(i64, serde_json::Value)>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, type, data, seq FROM session_message
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )
        .await?;
    let mut rows = stmt.query((session_id, floor)).await?;
    let mut raw = Vec::new();
    while let Some(row) = rows.next().await? {
        raw.push((
            row.get::<String>(0)?,
            row.get::<String>(1)?,
            row.get::<String>(2)?,
            row.get::<i64>(3)?,
        ));
    }
    raw.into_iter()
        .map(|(id, kind, data, seq)| {
            let mut value: serde_json::Value = serde_json::from_str(&data)?;
            Message::merge_row_columns(&id, &kind, &mut value);
            Ok((seq, value))
        })
        .collect()
}

pub(crate) fn decode_row(value: &serde_json::Value) -> Result<Message> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("session_message row without an id")?;
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .context("session_message row without a type")?;
    Ok(Message::from_row_data(id, kind, value.clone())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cardinal::wire::Usage;
    use crate::schema::message::{
        AssistantContent, AssistantMessage, AssistantTime, Message, TimeCreated,
    };
    use crate::schema::support::{ModelRef, Prompt};

    /// A display-only notice must not become an empty user turn.
    #[test]
    fn an_empty_synthetic_notice_adds_no_message() {
        let messages = vec![
            Message::Synthetic {
                id: "msg_1".to_owned(),
                metadata: None,
                time: TimeCreated { created: 0 },
                text: String::new(),
                description: Some("historian folded 1..4 · 1 compartment".to_owned()),
            },
            Message::Synthetic {
                id: "msg_2".to_owned(),
                metadata: None,
                time: TimeCreated { created: 0 },
                text: "real guidance".to_owned(),
                description: None,
            },
        ];
        let built = build_chat_messages("system", &[], &messages);
        assert_eq!(built.len(), 2, "system + the one with text: {built:?}");
        assert!(matches!(
            &built[1],
            ChatMessage::User { content } if content == "real guidance"
        ));
    }

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

    fn assistant_row(id: &str, content: Vec<AssistantContent>) -> Message {
        Message::Assistant(Box::new(AssistantMessage {
            id: id.to_owned(),
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
            content,
            snapshot: None,
            finish: None,
            tokens: None,
            error: None,
            cost: None,
            raw_finish: None,
            provider_state: None,
            retry: None,
        }))
    }

    // Thinking vendors (GLM/Z.ai, Kimi) reject replayed assistant turns
    // without `reasoning_content`; rows without reasoning must not carry it.
    #[test]
    fn assistant_reasoning_parts_replay_as_reasoning_content() {
        let with_reasoning = assistant_row(
            "msg_a1",
            vec![
                AssistantContent::Reasoning {
                    text: "think ".to_owned(),
                    state: None,
                    time: None,
                },
                AssistantContent::Reasoning {
                    text: "deep".to_owned(),
                    state: None,
                    time: None,
                },
                AssistantContent::Text {
                    text: "answer".to_owned(),
                    state: None,
                },
            ],
        );
        let without_reasoning = assistant_row(
            "msg_a2",
            vec![AssistantContent::Text {
                text: "plain".to_owned(),
                state: None,
            }],
        );
        // A reasoning-only row must survive the emit guard: its
        // reasoning_content is what keeps thinking vendors from 400-ing.
        let reasoning_only = assistant_row(
            "msg_a3",
            vec![AssistantContent::Reasoning {
                text: "only thought".to_owned(),
                state: None,
                time: None,
            }],
        );
        let built = build_chat_messages(
            "sys",
            &[],
            &[with_reasoning, without_reasoning, reasoning_only],
        );
        assert!(matches!(
            &built[1],
            ChatMessage::Assistant {
                content: Some(text),
                reasoning_content: Some(reasoning),
                ..
            } if text == "answer" && reasoning == "think deep"
        ));
        assert!(
            matches!(
                &built[2],
                ChatMessage::Assistant {
                    reasoning_content: None,
                    ..
                },
            ),
            "no reasoning parts → no reasoning_content key on the wire"
        );
        assert!(
            matches!(
                &built[3],
                ChatMessage::Assistant {
                    content: None,
                    tool_calls: None,
                    reasoning_content: Some(reasoning),
                    ..
                } if reasoning == "only thought"
            ),
            "reasoning-only row survives with content: null"
        );
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
        let chat = build_chat_messages("sys", &[], &messages);
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

    // The head travels as two adjacent user messages, baseline first: the
    // baseline is the provider's cache anchor, so merging the halves would
    // rewrite the anchored bytes on every delta (the failure the cache kernel
    // exists to prevent). An absent/empty half injects nothing.
    #[test]
    fn head_halves_stay_separate_and_empty_halves_vanish() {
        let messages = Vec::new();
        let chat = build_chat_messages("sys", &[Some("M0"), Some("M1")], &messages);
        let heads: Vec<&str> = chat
            .iter()
            .filter_map(|m| match m {
                ChatMessage::User { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(heads, vec!["M0", "M1"], "two messages, baseline first");

        let chat = build_chat_messages("sys", &[Some("M0"), None], &messages);
        assert_eq!(
            chat.iter()
                .filter(|m| matches!(m, ChatMessage::User { .. }))
                .count(),
            1,
            "a None half injects nothing"
        );
    }
}
