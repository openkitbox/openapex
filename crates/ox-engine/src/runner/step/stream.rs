use std::collections::BTreeMap;
use std::future::poll_fn;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use ox_core::copied_tags::strip_copied_tag_notation;
use ox_llm::cardinal::ChatError;
use ox_llm::cardinal::chat::BoxChatStream;
use ox_llm::cardinal::stream::StreamEvent;
use ox_llm::cardinal::wire::{ChatRequest, Usage};
use ox_schema::event::SessionEventData;
use ox_schema::support::FinishReason;

use super::super::drain::DrainCtx;
use super::super::loop_guard::fingerprint;
use super::attempt::{
    Attempt, AttemptFailure, StepOutcome, StepParams, TOOLS_INTERRUPTED, note_output,
    settle_interrupted,
};
use super::error::{aborted_error, invalid_output_error, to_session_error};
use super::tool::{fail_unsettled_tools, join_tool_tasks, settle_tool_calls};
use super::usage::{attempt_tokens, observe_request_cost};
/// One accumulating tool call from `ToolCallDelta` fragments.
#[derive(Debug, Default)]
pub(super) struct ToolAccum {
    /// Call id (first fragment that carries one).
    pub(super) id: String,
    /// Function name (first fragment that carries one).
    pub(super) name: String,
    /// Concatenated argument JSON.
    pub(super) arguments: String,
    /// `tool.input.started` has gone out (id + name known).
    pub(super) input_started: bool,
    /// The call settled: input ended and `tool.called` (or a failure)
    /// published. Guards idempotent settlement.
    pub(super) called: bool,
}

/// Consume one stream attempt, publishing streaming events as they arrive
/// and settling the step when the stream terminates cleanly. `started` is
/// updated when `step.started` goes out (persisted across retry attempts);
/// `Attempt::Failed::published` reports whether THIS attempt received model
/// output, which `started` cannot say once a retry has started the row.
pub(super) async fn consume(
    ctx: &DrainCtx,
    token: &CancellationToken,
    params: &StepParams<'_>,
    request: &ChatRequest,
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
    // Model output was received in this attempt (unlike `started`, which a
    // retry announcement also sets).
    let mut produced = false;

    loop {
        enum Next {
            Event(Option<Result<StreamEvent, ChatError>>),
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
                    settle_interrupted(
                        ctx,
                        params,
                        *started,
                        (*started).then(|| attempt_tokens(ctx, request, usage)),
                    )
                    .await?,
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
                if produced {
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
                return Ok(Attempt::Failed {
                    failure: AttemptFailure::Transport(error),
                    published: produced,
                    tokens: None,
                });
            }
            Next::Event(Some(Ok(event))) => match event {
                StreamEvent::TextDelta(delta) => {
                    note_output(ctx, params, dispatch, started, &mut produced).await?;
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
                    note_output(ctx, params, dispatch, started, &mut produced).await?;
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
                    note_output(ctx, params, dispatch, started, &mut produced).await?;
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
                StreamEvent::Usage(seen) => {
                    usage = Some(seen);
                    observe_request_cost(ctx, &params.model.id, request, seen.prompt_tokens);
                }
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
    if !produced {
        // The stream produced nothing at all: a bounded failure so the retry
        // loop runs (the final attempt settles as step.failed).
        return Ok(Attempt::Failed {
            failure: AttemptFailure::EmptyStream,
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
            tokens: Some(attempt_tokens(ctx, request, usage)),
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
    let calls: Vec<u64> = tool_acc
        .values()
        .filter(|acc| acc.called)
        .map(|acc| fingerprint(&acc.name, &acc.arguments))
        .collect();

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
            settle_interrupted(
                ctx,
                params,
                *started,
                (*started).then(|| attempt_tokens(ctx, request, usage)),
            )
            .await?,
        ));
    }
    ctx.bus
        .publish(SessionEventData::StepEnded {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
            finish: mapped_finish,
            raw_finish: Some(raw_finish),
            provider_state: None,
            cost: ox_schema::support::Finite::Int(0),
            tokens: attempt_tokens(ctx, request, usage),
            snapshot: None,
            files: None,
        })
        .await?;
    Ok(Attempt::Done(StepOutcome::Completed {
        finish: mapped_finish,
        needs_continuation,
        calls,
    }))
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
                // Same strip as the persistence boundary: the ended payload is
                // what live clients render as the part's final text, so a
                // copied tag run must not survive here either.
                text: strip_copied_tag_notation(text).into_owned(),
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
                text: strip_copied_tag_notation(reasoning).into_owned(),
                state: None,
            })
            .await?;
    }
    Ok(())
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
