use std::collections::BTreeMap;
use std::future::poll_fn;

use anyhow::Result;
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
use super::tool::{ToolTasks, fail_unsettled_tools, settle_tool_calls};
use super::usage::{attempt_tokens, observe_request_cost};
use crate::mctx::citation::{self, CitationGate};
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
    stream: BoxChatStream,
) -> Result<Attempt> {
    let mut tool_tasks = ToolTasks::new(token);
    let result = consume_owned(
        ctx,
        token,
        params,
        request,
        dispatch,
        started,
        stream,
        &mut tool_tasks,
    )
    .await;
    if !matches!(result, Ok(Attempt::Done(StepOutcome::Completed { .. }))) {
        tool_tasks.cancellation.cancel();
    }
    let settlement = tool_tasks.finish().await;
    // A persistence failure must never enter provider retry/continuation.
    settlement?;
    result
}

#[expect(
    clippy::too_many_arguments,
    reason = "step owner adds the task scope to the existing stream inputs"
)]
async fn consume_owned(
    ctx: &DrainCtx,
    token: &CancellationToken,
    params: &StepParams<'_>,
    request: &ChatRequest,
    dispatch: i64,
    started: &mut bool,
    mut stream: BoxChatStream,
    tool_tasks: &mut ToolTasks,
) -> Result<Attempt> {
    let mut text = String::new();
    // Keeps a `<mem-used>` mark out of the deltas; `text` stays raw and is
    // cleaned once, at the end (`flush_blocks`).
    let mut gate = CitationGate::default();
    let mut reasoning = String::new();
    let mut text_ordinal: Option<i64> = None;
    let mut reasoning_ordinal: Option<i64> = None;
    let mut next_text_ordinal: i64 = 0;
    let mut next_reasoning_ordinal: i64 = 0;
    let mut finish: Option<(FinishReason, String)> = None;
    let mut usage: Option<Usage> = None;
    // Tool-call fragments by stream index.
    let mut tool_acc: BTreeMap<u32, ToolAccum> = BTreeMap::new();
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
                    (&text, &mut gate),
                    &reasoning,
                )
                .await?;
                // Settle open tool inputs and in-flight executions as
                // aborted (upstream `interruptTools` + `failUnsettledTools`)
                // BEFORE `step.failed`.
                fail_unsettled_tools(ctx, params, &mut tool_acc, aborted_error(TOOLS_INTERRUPTED))
                    .await?;
                tool_tasks.cancellation.cancel();
                tool_tasks.finish().await?;
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
                    (&text, &mut gate),
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
                tool_tasks.cancellation.cancel();
                tool_tasks.finish().await?;
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
                    if let Some(delta) = gate.admit(delta) {
                        publish_text_delta(ctx, params, text_ordinal.unwrap_or_default(), delta)
                            .await?;
                    }
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
                        tool_tasks,
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
        (&text, &mut gate),
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
        tool_tasks.cancellation.cancel();
        tool_tasks.finish().await?;
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
    settle_tool_calls(ctx, params, &mut tool_acc, tool_tasks, length_finish).await?;
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
    // Await actual execution/cleanup and durable settlement before step end.
    tool_tasks.finish().await?;
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

/// Publish one text delta of the open text block.
async fn publish_text_delta(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    ordinal: i64,
    delta: String,
) -> Result<()> {
    ctx.bus
        .publish(SessionEventData::TextDelta {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
            ordinal,
            delta,
        })
        .await
}

/// Publish `text.ended`/`reasoning.ended` for any open blocks. Idempotent:
/// ordinals are taken (set to `None`) as they close.
///
/// `text` is the raw accumulated text with its citation gate: whatever the
/// gate still holds goes out as a last delta, the `<mem-used>` marks are
/// stripped from the ended payload, and the memories they cite are stamped
/// as used (a by-product; its failure is logged, never raised).
async fn flush_blocks(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    text_ordinal: &mut Option<i64>,
    reasoning_ordinal: &mut Option<i64>,
    text: (&str, &mut CitationGate),
    reasoning: &str,
) -> Result<()> {
    let (text, gate) = text;
    if let Some(ordinal) = text_ordinal.take() {
        if let Some(rest) = gate.finish() {
            publish_text_delta(ctx, params, ordinal, rest).await?;
        }
        let (cleaned, cited) = citation::strip(text);
        ctx.bus
            .publish(SessionEventData::TextEnded {
                session_id: ctx.session_id.clone(),
                assistant_message_id: params.assistant_message_id.clone(),
                ordinal,
                // Same strip as the persistence boundary: the ended payload is
                // what live clients render as the part's final text, so a
                // copied tag run must not survive here either.
                text: strip_copied_tag_notation(&cleaned).into_owned(),
                state: None,
            })
            .await?;
        let project = ctx.directory.to_string_lossy();
        citation::record(&ctx.db, &project, &cited, ox_schema::id::now_millis()).await;
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

/// Map a stream's raw finish reason to the upstream literal union.
///
/// Both wire vocabularies land here: a Chat Completions endpoint reports
/// `stop`/`length`/`tool_calls`, while a Messages endpoint reports Anthropic's
/// `end_turn`/`max_tokens`/`tool_use`/`stop_sequence`. An unmapped value is
/// [`FinishReason::Unknown`], which the step treats as a truncated stream, so a
/// missing vocabulary entry turns every turn of that protocol into
/// `provider.invalid-output` rather than a visible protocol error.
fn map_finish(raw: &str) -> FinishReason {
    match raw {
        "stop" | "end_turn" | "stop_sequence" => FinishReason::Stop,
        "length" | "max_tokens" => FinishReason::Length,
        "tool_calls" | "tool-calls" | "tool_use" => FinishReason::ToolCalls,
        "content_filter" | "content-filter" | "refusal" => FinishReason::ContentFilter,
        // A server-side tool pause ends the turn as far as ox is concerned: it
        // runs no server tools, so there is nothing to resume.
        "pause_turn" => FinishReason::Stop,
        "error" => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}
