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
//!   cached, `cache.write` = 0). When the stream carries no usage, `input`
//!   is an estimate from the sent request's size (bytes/4 x the session
//!   calibration ratio) and the other fields are 0. Deviation: the estimate
//!   lives in the same field; the row carries `tokens.estimated = true` (ox
//!   extension, absent for reported usage); bytes/4 under-counts CJK,
//!   so folds may trigger slightly late; pre-existing zero rows are not
//!   rewritten. The estimate also lands on interrupted and incomplete-stream
//!   rows (their usage chunk never arrives, even for models that do report
//!   it), so those turns now add a prompt-size estimate (no cache split) to
//!   `session_v2.tokens_input` and the stats sums; a failed attempt that
//!   never opened a stream keeps `tokens: None`.
//! - An interruption after the step started settles the step with
//!   `session.step.failed` carrying the upstream `aborted` error, after
//!   failing unsettled tool parts.
//! - Failures retry per [`retry::classify`] / [`retry::Backoff`]
//!   (upstream's SessionRunnerRetry publishes `session.retry.scheduled`):
//!   network-class errors retry until recovery or interrupt, the bounded
//!   class up to `MAX_BOUNDED_RETRIES` consecutive failures, the never
//!   class fails immediately. The delay is computed once and used for both
//!   the event's `at` and the sleep. A 400 context-length overflow before
//!   any output surfaces as [`StepOutcome::Overflow`] so the drain can
//!   compact and retry once (upstream `Outcome.Compacted`).
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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::poll_fn;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::mctx::{search, temporal};
use crate::prompt;
use ox_core::copied_tags::strip_copied_tag_notation;
use ox_llm::cardinal::CardinalError;
use ox_llm::cardinal::chat::BoxChatStream;
use ox_llm::cardinal::stream::StreamEvent;
use ox_llm::cardinal::wire::{
    ChatMessage, ChatRequest, FunctionCall, FunctionType, ImageUrl, ToolCall, ToolChoice, Usage,
    UserContent, UserPart,
};
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::message::{
    AssistantContent, Compaction, CompactionCompleted, CompactionMessage, Message, ToolState,
};
use ox_schema::support::{
    Content, FileAttachment, FileSource, FinishReason, ModelRef, Prompt, Record, SessionError,
    SessionErrorResponse, TokenCache, TokenUsage,
};

use super::ToolContext;
use super::compaction;
use super::drain::DrainCtx;
use super::instructions;
use super::loop_guard::fingerprint;
use super::output_budget::bound_tool_output;
use super::retry::{self, RetryKind};

/// Upstream `STEP_INTERRUPTED` (runner/step.ts).
const STEP_INTERRUPTED: &str = "Step interrupted";
/// Upstream `TOOLS_INTERRUPTED` (runner/step.ts).
const TOOLS_INTERRUPTED: &str = "Tool execution interrupted";
/// Upstream `CONTINUE_AFTER_INCOMPLETE_STREAM` (runner/llm.ts): the
/// continuation prompt published after an interrupted stream so the model
/// resumes where it left off.
const CONTINUE_AFTER_INCOMPLETE_STREAM: &str = "The previous response was interrupted. Continue from where you left off without repeating completed content.";
/// Hits automatic recall may attach to one turn (the hint's line budget).
const RECALL_LIMIT: usize = 3;
/// Shortest typed prompt worth searching. A greeting or a `/clear` retrieves
/// nothing, and every turn that clears this bar pays one indexed query.
const RECALL_MIN_QUERY_CHARS: usize = 8;
/// Upstream `MAX_STEPS_PROMPT` (runner/max-steps.ts): the trailing assistant
/// message of a final step. Never persisted.
const MAX_STEPS_PROMPT: &str = "The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.";

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
        /// Fingerprints of the tool calls this step settled, in stream
        /// index order (input for the repeated-call detector).
        calls: Vec<u64>,
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
    /// The turn's last step: tools stay in the request (cache prefix) but
    /// are disabled, and any call the model still makes is rejected.
    pub final_step: bool,
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
    /// The stream ended without a single output event. Settles exactly like
    /// `EarlyClose` but retries bounded: a healthy connection that keeps
    /// answering with nothing must not loop forever.
    EmptyStream,
}

impl AttemptFailure {
    /// How this failure is retried; an incomplete stream is bounded.
    fn retry_kind(&self) -> RetryKind {
        match self {
            AttemptFailure::Transport(error) => retry::classify(error),
            AttemptFailure::IncompleteStream | AttemptFailure::EmptyStream => RetryKind::Bounded,
        }
    }

    /// Upstream `isInterruptedStream` (step.ts): interrupted streams
    /// continue via a continuation prompt; content errors (HTTP rejections,
    /// in-band provider failures) fail the step.
    fn is_interrupted_stream(&self) -> bool {
        match self {
            AttemptFailure::Transport(
                CardinalError::EarlyClose | CardinalError::Sse(_) | CardinalError::Transport(_),
            ) => true,
            AttemptFailure::Transport(
                CardinalError::Http { .. }
                | CardinalError::Upstream(_)
                // A configuration error, not a stream event.
                | CardinalError::NoRoute { .. },
            ) => false,
            AttemptFailure::IncompleteStream => true,
            // Retried through the `!published` arm, never as a continuation.
            AttemptFailure::EmptyStream => false,
        }
    }

    /// Upstream gates `Outcome.Compacted` on `!outputStarted`: overflow
    /// recovery is a pre-output concern.
    fn is_overflow(&self) -> bool {
        match self {
            AttemptFailure::Transport(error) => super::provider_error::is_overflow_error(error),
            AttemptFailure::IncompleteStream | AttemptFailure::EmptyStream => false,
        }
    }

    /// The session error the row settles with (upstream `toSessionError`).
    fn session_error(&self) -> SessionError {
        match self {
            AttemptFailure::Transport(error) => to_session_error(error),
            AttemptFailure::IncompleteStream => invalid_output_error(),
            AttemptFailure::EmptyStream => to_session_error(&CardinalError::EarlyClose),
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
    // Retry delay and caps; `attempt` stays monotonic for the events.
    let mut backoff = retry::Backoff::default();
    // The request of the previous attempt, kept when that attempt produced no
    // output so the retry does not re-read the transcript.
    let mut prepared: Option<ChatRequest> = None;
    loop {
        attempt += 1;
        // A retry whose previous attempt produced no output sends the same
        // request: only a continuation (published failure) rebuilds context.
        let request = match prepared.take() {
            Some(request) => request,
            None => build_request(ctx, params).await?,
        };

        // The connect / endpoint-queue phase is interruptible too: a request
        // parked behind a concurrency gate (or a hung connect) must not pin
        // the turn past an interrupt. Dropping the open future abandons the
        // queue slot or half-open connection.
        let opened = tokio::select! {
            opened = ctx.chat.stream_chat(&request) => opened,
            () = token.cancelled() => {
                return settle_interrupted(ctx, params, started, None).await;
            }
        };
        let stream = match opened {
            Ok(stream) => stream,
            Err(error) => {
                // The flight recorder for the pairing-400 class
                // (docs/tool-call-pairing-400.md): what ox put on the wire
                // (post pairing-guard) beside what the upstream named as
                // unanswered. One log line decides ox-vs-downstream on the
                // next recurrence — the id the upstream complained about
                // cannot be in `sent` if the fault is not ours.
                if let CardinalError::Http { status: 400, body } = &error {
                    let complained = pairing_rejection_ids(body);
                    let rejected = complained.is_some()
                        || body.contains("tool_call_id")
                        || body.contains("tool_calls");
                    if rejected {
                        tracing::warn!(
                            session = %ctx.session_id,
                            upstream_unanswered = ?complained,
                            sent_tool_calls = %tool_call_digest(&request),
                            "upstream rejected tool-call pairing"
                        );
                    }
                }
                // Context overflow → the drain compacts and retries the step
                // once (upstream Outcome.Compacted); connect errors never
                // carry published output.
                if super::provider_error::is_overflow_error(&error) {
                    return Ok(StepOutcome::Overflow {
                        error: to_session_error(&error),
                    });
                }
                // Connect failure: nothing was published for this attempt.
                if let Some(delay) = backoff.on_failure(retry::classify(&error), false) {
                    schedule_retry(
                        ctx,
                        params,
                        &mut started,
                        attempt,
                        delay,
                        &to_session_error(&error),
                    )
                    .await?;
                    if !sleep_backoff(token, delay).await {
                        return settle_interrupted(ctx, params, started, None).await;
                    }
                    prepared = Some(request);
                    continue;
                }
                return fail_step(ctx, params, to_session_error(&error), None, started).await;
            }
        };
        match consume(ctx, token, params, &request, dispatch, &mut started, stream).await? {
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
                let delay = backoff.on_failure(failure.retry_kind(), published);
                if let Some(delay) = delay
                    && (failure.is_interrupted_stream() || !published)
                {
                    if published {
                        // The partial assistant settles `step.failed` with
                        // the stream error first (upstream failAssistant —
                        // no step.ended, finish `error`).
                        fail_step(ctx, params, error.clone(), tokens, started).await?;
                    }
                    schedule_retry(ctx, params, &mut started, attempt, delay, &error).await?;
                    if !sleep_backoff(token, delay).await {
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
                    } else {
                        prepared = Some(request);
                    }
                    continue;
                }
                return fail_step(ctx, params, error, tokens, started).await;
            }
        }
    }
}

/// Assemble the chat request for one attempt: the transcript is rebuilt from
/// the durable rows (upstream refreshes context on retry and continuation),
/// so a continuation attempt observes the settled partial row plus the
/// continuation prompt.
async fn build_request(ctx: &DrainCtx, params: &StepParams<'_>) -> Result<ChatRequest> {
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
    super::historian_trigger::mctx_historian(ctx, budget, floor, params.model.variant.clone());
    let rows = if ctx.config.magic_context.enabled {
        // The §N§ overlay (tag prefixes + drop placeholders) is part of the
        // integration: with it off the request must stay byte-identical to
        // the integration-free one.
        load_overlaid_tail(&ctx.db, &ctx.session_id, floor).await?
    } else {
        load_history_after(&ctx.db, &ctx.session_id, floor).await?
    };
    // Step-boundary fold trigger (plan D2): the tail is already decoded
    // in memory, so the usage criterion costs no query — the newest
    // assistant row's provider-reported tokens against this attempt's
    // budget, the same reading `fold_decision` recomputes inside. At or
    // over the threshold a detached silent fold starts; it re-runs the
    // full gate itself, so this site stays cheap and may double-fire
    // harmlessly (single-flight CAS + the gate).
    let usage_pct = rows
        .iter()
        .rev()
        .find_map(assistant_tokens)
        .and_then(|tokens| budget.usage_percentage(tokens));
    if usage_pct.is_some_and(|pct| pct >= super::compaction::FOLD_USAGE_THRESHOLD * 100.0) {
        super::compaction::spawn_background_fold(ctx);
    }
    // Directory and root must share one namespace: the chain's
    // component-wise prefix check degenerates to leaf-only when the
    // directory still carries symlink components the canonical root lost.
    let (directory, project_root) = ctx.instruction_root.get_or_compute(&ctx.directory);
    let observed = prompt::gather(
        &directory,
        &ctx.home,
        project_root.as_deref(),
        ctx.mcp.instructions_block().as_deref(),
    );
    let (system_static, tool_count) = {
        let guidance: Vec<(&str, Option<&'static str>)> = ctx.tools.guidances().collect();
        // The baseline log's tool count is the registry size; the names
        // iterate in the same order the definitions do.
        let count = guidance.len();
        let profile = if ctx.is_subagent {
            prompt::WorkflowProfile::Subagent
        } else {
            prompt::WorkflowProfile::Parent
        };
        (prompt::system_static(&guidance, profile), count)
    };
    // The baseline log needs the static layer's size, so it is built
    // before the sync: assembling it is pure string work over the
    // registry's guidance, unaffected by the keyed sources.
    let instruction_update =
        sync_instructions(ctx, &observed, &rows, system_static.len(), tool_count).await;
    let instructions_initial = {
        let baselines = super::lock(&ctx.instruction_baselines);
        baselines
            .get(&ctx.session_id)
            .map(|b| b.initial_render.clone())
            .unwrap_or_default()
    };
    let mut chat_messages = build_chat_messages(
        &system_static,
        &instructions_initial,
        &[head_m0.as_deref(), head_m1.as_deref()],
        &rows,
    );
    // The delta row is durable for future steps; this step's already-loaded
    // rows predate it, so the text rides this request in memory (upstream
    // `generate.ts:55` places `Message.system(instructionUpdate)` after the
    // transcript and BEFORE the current prompt).
    if let Some(text) = instruction_update {
        let delta = ChatMessage::System { content: text };
        // Upstream `generate.ts:55` places the update before the current
        // prompt (the transcript's trailing user row on the first
        // attempt); later attempts end with tool output and tail-append.
        match chat_messages.last() {
            Some(ChatMessage::User { .. }) => {
                let at = chat_messages.len() - 1;
                chat_messages.insert(at, delta);
            }
            _ => chat_messages.push(delta),
        }
    }
    // Automatic recall: the turn's opening request gets the top few
    // memories/notes/compartments matching the prompt's own typed text, as
    // one compact `<recall>` block appended to that user message.
    //
    // The hint rides only the first request of a turn: both the trailing
    // transcript row and the trailing chat message must be that prompt (a
    // later step ends with tool output, and any row pushed after the prompt
    // means this request is not the opening one). So a long tool loop pays
    // at most one extra prefill of the trailing user message — the message
    // the block disappeared from on step 2 — and NO stored row is touched:
    // the bytes exist for this request only, and re-rendering the same rows
    // yields the same content again. Putting it in the tail rather than
    // `system`/`m[0]`/`m[1]` is what keeps the cached prefix intact.
    if ctx.config.magic_context.enabled
        && matches!(chat_messages.last(), Some(ChatMessage::User { .. }))
        && let Some(query) = recall_query(&rows)
    {
        let project = ctx.directory.to_string_lossy();
        let request = search::SearchRequest {
            project: project.as_ref(),
            session_id: &ctx.session_id,
            query: &query,
            sources: search::SourceFilter::RECALL,
            limit: RECALL_LIMIT,
            from_ms: None,
            to_ms: None,
        };
        // No embedder on this path (it belongs to the `ctx_search` tool), so
        // a hint is lexical. A failed query is silent by design: a turn must
        // never stall or fail because a hint could not be retrieved, and the
        // request is still well-formed without it.
        match search::search(&ctx.db, None, &request).await {
            Ok(hits) => {
                if let Some(block) = search::render_recall_block(&hits)
                    && let Some(ChatMessage::User {
                        content: UserContent::Text(text),
                    }) = chat_messages.last_mut()
                {
                    // Move the text out and put the appended version back:
                    // the only string this path rewrites is that one, and it
                    // is not cloned on the way. A parts-list user turn (prompt
                    // + media) keeps its bytes: the block is plain text.
                    let content = std::mem::take(text);
                    *text = append_recall(content, &block);
                }
            }
            Err(error) => {
                tracing::debug!(session = %ctx.session_id, error = %error, "automatic recall failed");
            }
        }
    }
    let mut request = ChatRequest::new(
        // cardinal routes on the bare model id; the providerID/modelID
        // qualified form is upstream's provider-adapter vocabulary and
        // cardinal answers "no route for model cardinal/<id>" to it.
        params.model.id.clone(),
        chat_messages,
    );
    request.tools = ctx.tools.definitions_shared();
    // The session's selected reasoning level (model variant) rides every
    // chat turn; no selection leaves the field off the wire.
    request.apply_reasoning_effort(params.model.variant.as_deref());
    if params.final_step {
        apply_final_step(&mut request);
    }
    Ok(request)
}

/// Final-step request shape (upstream llm.ts): keep `tools` so the provider
/// cache prefix survives, forbid calling them, and end the history with
/// [`MAX_STEPS_PROMPT`] as an assistant message. Runs after
/// `ChatRequest::new` (which pairs tool results), so the appended text-only
/// message cannot disturb pairing.
fn apply_final_step(request: &mut ChatRequest) {
    request.tool_choice = ToolChoice::None;
    request.messages.push(ChatMessage::Assistant {
        content: Some(MAX_STEPS_PROMPT.to_owned()),
        tool_calls: None,
        reasoning_content: None,
    });
}

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
async fn sleep_backoff(token: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = token.cancelled() => false,
    }
}

/// Publish `session.retry.scheduled` for `attempt` (1-based), due `delay`
/// from now, starting the assistant row first (upstream `startAssistant`).
async fn schedule_retry(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    started: &mut bool,
    attempt: usize,
    delay: Duration,
    error: &SessionError,
) -> Result<()> {
    if !*started {
        publish_started(ctx, params, id::now_millis()).await?;
        *started = true;
    }
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

/// Provider output arrived: mark the attempt produced and start the
/// assistant row (`step.started`) on the first output of the step.
async fn note_output(
    ctx: &DrainCtx,
    params: &StepParams<'_>,
    dispatch: i64,
    started: &mut bool,
    produced: &mut bool,
) -> Result<()> {
    *produced = true;
    if !*started {
        publish_started(ctx, params, dispatch).await?;
        *started = true;
    }
    Ok(())
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
/// updated when `step.started` goes out (persisted across retry attempts);
/// `Attempt::Failed::published` reports whether THIS attempt received model
/// output, which `started` cannot say once a retry has started the row.
async fn consume(
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

/// Upstream token shape (`SessionUsage.tokens`): input excludes cached
/// prompt tokens, cache write is not reported by cardinal's OpenAI dialect.
/// A stream that never carried usage stores `estimate()` as `input` and
/// zeros elsewhere; the estimate is not evaluated when usage was reported.
/// The estimate branch sets `estimated: Some(true)` so estimated rows can be
/// told from reported ones (`json_extract(data, '$.tokens.estimated')`); the
/// reported branch leaves it `None`.
fn tokens_for(usage: Option<Usage>, estimate: impl FnOnce() -> i64) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage {
            input: estimate(),
            output: 0,
            reasoning: 0,
            cache: TokenCache { read: 0, write: 0 },
            estimated: Some(true),
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
        estimated: None,
    }
}

/// Prompt-size estimate for a request body of `body_len` bytes:
/// `ceil(bytes / 4 * ratio)`. A non-finite or negative `ratio` counts as 1.0;
/// the product saturates at `i64::MAX`.
fn estimate_prompt_tokens(body_len: usize, ratio: f64) -> i64 {
    let ratio = if ratio.is_finite() && ratio >= 0.0 {
        ratio
    } else {
        1.0
    };
    let raw = crate::mctx::tokens::heuristic_bytes(body_len) as f64 * ratio;
    // `as` saturates for out-of-range floats, so this clamps to i64::MAX.
    raw.ceil() as i64
}

/// Settled token usage for one attempt. When the stream carried no usage the
/// prompt size is estimated from the request that was sent (its serialized
/// length is measured, not built); a serialization failure falls back to 0.
fn attempt_tokens(ctx: &DrainCtx, request: &ChatRequest, usage: Option<Usage>) -> TokenUsage {
    tokens_for(usage, || {
        let body_len = match ox_llm::cardinal::wire::serialized_len(request) {
            Ok(len) => len,
            Err(error) => {
                tracing::warn!(session = %ctx.session_id, error = %error, "prompt estimate skipped");
                return 0;
            }
        };
        let ratio = crate::mctx::calibration::ratio_for(&ctx.calibrations, &ctx.session_id);
        estimate_prompt_tokens(body_len, ratio)
    })
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

/// The ids an upstream named as unanswered in a tool-call pairing rejection,
/// when the body lists any (Moonshot names them after the marker; DeepSeek's
/// phrasing names none). None for any other body — the digest is what the
/// caller logs then.
fn pairing_rejection_ids(body: &str) -> Option<Vec<String>> {
    const MARKER: &str = "did not have response messages:";
    let at = body.find(MARKER)?;
    let rest = body[at + MARKER.len()..].lines().next().unwrap_or("");
    // The listed ids end where the enclosing JSON string does — a backslash
    // begins the escaped closing quote (verbatim upstream body, or wrapped
    // by cardinal into `\",\"`); provider ids never carry one, so cutting at
    // the first is safe. The plain `","` shape covers a non-escaped tail.
    let end = rest.find('\\').unwrap_or(rest.len());
    let rest = rest[..end].split("\",\"").next().unwrap_or(rest);
    let ids: Vec<String> = rest
        .split([',', ' '])
        .map(str::trim)
        .map(|id| id.trim_matches(|c: char| c == '"' || c == '\\' || c == '\''))
        .filter(|id| !id.is_empty() && *id != "...")
        .map(str::to_owned)
        .take(20)
        .collect();
    Some(ids)
}

/// Fold the provider's own count for this request into the session's token
/// calibration ([`crate::mctx::calibration`]).
///
/// The local side is `bytes / 4` over the exact JSON the wire carried, so the
/// ratio absorbs everything a local counter cannot see: the chat template, the
/// tool framing, the vendor's own tokenizer and the cache accounting. The
/// request is post pairing-guard and post every message push, i.e. the bytes the
/// provider was billed for.
///
/// Best effort, and only while it can still move the ratio: the session asks
/// for at most [`crate::mctx::calibration`]'s freeze window of samples, so the
/// per-chunk usage events after that cost a map lookup and nothing else. A
/// serialization failure only skips the sample; the observation never fails or
/// blocks the step.
fn observe_request_cost(ctx: &DrainCtx, model_id: &str, request: &ChatRequest, prompt_tokens: u64) {
    if !crate::mctx::calibration::wants_sample(&ctx.calibrations, &ctx.session_id, model_id) {
        return;
    }
    // Only the body's size is needed, so it is measured rather than built: a
    // transcript-sized copy per sampled step buys nothing here.
    let Ok(body_len) = ox_llm::cardinal::wire::serialized_len(request) else {
        return;
    };
    crate::mctx::calibration::observe(
        &ctx.calibrations,
        &ctx.session_id,
        model_id,
        prompt_tokens,
        crate::mctx::tokens::heuristic_bytes(body_len),
    );
}

/// The request's tool-call ids in wire order, capped for one log line. The
/// request is post pairing-guard: an id the upstream names that is absent
/// here was never sent by ox.
fn tool_call_digest(request: &ChatRequest) -> String {
    const CAP: usize = 60;
    let mut ids = Vec::new();
    let mut total = 0usize;
    for message in &request.messages {
        if let ChatMessage::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        {
            for call in calls {
                total += 1;
                if ids.len() < CAP {
                    ids.push(call.id.clone());
                }
            }
        }
    }
    if total > CAP {
        ids.push(format!("…(+{} more)", total - CAP));
    }
    ids.join(",")
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
        CardinalError::NoRoute { .. } => (
            // A routing configuration error: deterministic, request-shaped,
            // and independent of any endpoint, so it recurs identically on
            // retry — the same bucket as a 4xx rejection.
            "provider.invalid-request",
            None,
            None,
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
pub(crate) async fn mctx_head(
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
        // The session's measured provider ÷ local ratio: the pass fills `m[0]`
        // in the units its budget is written in.
        calibration: crate::mctx::calibration::ratio_for(&ctx.calibrations, &ctx.session_id),
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
async fn latest_seq(db: &ox_db::Db, session_id: &str) -> Result<i64> {
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

/// Upstream `messageEntries`: history starts at the latest completed
/// compaction row, so the checkpoint replaces everything before it. Shared
/// by `build_chat_messages` and the instruction wire fold.
pub(crate) fn compaction_boundary(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(|message| {
            matches!(
                message,
                Message::Compaction(CompactionMessage {
                    compaction: Compaction::Completed(_),
                    ..
                })
            )
        })
        .unwrap_or(0)
}

/// The provider-reported context cost of one decoded row: the decoded twin
/// of [`super::historian_trigger::prompt_tokens`] — the same
/// `tokens.input + tokens.cache.read` sum over the projected JSON, read
/// from the already-decoded tail so the step-boundary fold trigger costs no
/// query (plan D1: one measurement, one meaning).
///
/// Parity with the raw reader is pinned per row shape by
/// `tests::assistant_tokens_parity_with_raw_prompt_tokens`
/// (bottom of this file) — change both readers or neither.
fn assistant_tokens(message: &Message) -> Option<i64> {
    match message {
        Message::Assistant(assistant) => {
            let tokens = assistant.tokens.as_ref()?;
            Some(tokens.input + tokens.cache.read)
        }
        _ => None,
    }
}

/// The effective end of a row for gap measurement (upstream
/// `effectiveEndMs`): an assistant's completion once it settled, otherwise its
/// creation. Shell rows use `created` too — they never reach the wire, and
/// their tool output replays through the assistant row that called them.
fn effective_end_ms(message: &Message) -> i64 {
    match message {
        Message::Assistant(assistant) => assistant.time.completed.unwrap_or(assistant.time.created),
        other => other.time_created(),
    }
}

/// Build the OpenAI message list: the static system layer, the frozen
/// instruction layer, then the session transcript in `seq` order. An empty
/// instruction layer is filtered (upstream `baseTranscript`).
///
/// Mapping (upstream `to-llm-message.ts`):
/// - `user`     → `{role: "user", content}` — a string, or a parts list when
///   the prompt carries an image ([`user_message_content`]) — dropped entirely
///   when the assembled content is empty (to-llm-message.ts:274).
/// - `synthetic`/`skill` → user-role text.
/// - `system`   → `{role: "system", content}` (to-llm-message.ts:291).
/// - `assistant` → `{role: "assistant", content: text parts joined,
///   tool_calls: the row's tool parts, reasoning_content: reasoning parts
///   joined}`; `reasoning_content` is omitted when the row has no reasoning
///   (upstream `lowerAssistantMessage`). Empty text parts are filtered, an
///   interrupted or failed row degrades its reasoning to text, and a row must
///   never go out with neither `content` nor `tool_calls` — see the arm below.
/// - A settled assistant tool part (`completed`/`error`, `executed: false`)
///   additionally becomes `{role: "tool", tool_call_id, content}` — joined
///   JSON content for completed calls, `{error: …}` for failed ones
///   (upstream `toolResult`).
/// - `compaction` (completed) → the checkpoint user message carrying the
///   summary (and `recent` when present); incomplete compactions are
///   skipped, and history selection starts at the latest completed row
///   anyway (upstream `SessionHistory` boundary).
/// - idle/shell/switch markers never reach the model.
/// - A `user` turn more than [`temporal::TEMPORAL_THRESHOLD_SECONDS`] after the
///   previous row's effective end is prefixed with `<!-- +2h -->\n`
///   (temporal awareness; the marker is never stored).
pub(crate) fn build_chat_messages(
    system_static: &str,
    instructions: &str,
    head: &[Option<&str>],
    messages: &[Message],
) -> Vec<ChatMessage> {
    let boundary = compaction_boundary(messages);
    let mut out = vec![ChatMessage::System {
        content: system_static.to_owned(),
    }];
    // Upstream `baseTranscript`: empty instruction parts are filtered, so a
    // session with no sources sends exactly one system message.
    if !instructions.is_empty() {
        out.push(ChatMessage::System {
            content: instructions.to_owned(),
        });
    }
    // The head halves are synthetic user messages leading the transcript: they
    // stand in for the covered history. They stay SEPARATE because the baseline
    // (`m[0]`) is the provider's cache anchor — merging them would rewrite the
    // anchored bytes on every delta.
    for half in head.iter().flatten() {
        out.push(ChatMessage::User {
            content: (*half).to_owned().into(),
        });
    }
    // Temporal awareness (upstream `injectTemporalMarkers`): a user turn whose
    // gap from the previous row's effective end crosses the threshold gets a
    // `<!-- +2h -->` prefix. The marker is request-side rendering only and is
    // never stored, so re-rendering the same rows yields the same bytes and
    // there is no double-injection path. `prev_end_ms` starts empty: the head
    // halves stand in for an unknown span, so the first row above them has no
    // predecessor to measure against.
    let mut prev_end_ms: Option<i64> = None;
    for message in messages.iter().skip(boundary) {
        // Every row advances the cursor, including rows the wire drops
        // (upstream `collectTemporalCandidates` sets `prev = msg`
        // unconditionally).
        let previous_end_ms = prev_end_ms;
        prev_end_ms = Some(effective_end_ms(message));
        match message {
            Message::User { prompt, time, .. } => {
                let content = user_message_content(prompt);
                if content_is_blank(&content) {
                    // Upstream: a user message with no assembled content is
                    // omitted from the transcript.
                    continue;
                }
                // Deliberate deviation: only an authored `user` row is marked.
                // `synthetic`/`skill` rows are host notices, not spans of human
                // time (upstream's `hasMeaningfulUserText` sees its notices as
                // user parts; ox tells them apart by row type).
                let content = match previous_end_ms
                    .and_then(|previous| temporal::marker_prefix((time.created - previous) / 1000))
                {
                    Some(prefix) => prefix_content(content, prefix),
                    None => content,
                };
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
                    content: text.clone().into(),
                })
            }
            Message::System { text, .. } => out.push(ChatMessage::System {
                content: text.clone(),
            }),
            Message::Assistant(assistant) => {
                // Joined in place: the rows are borrowed, so one copy is the
                // floor, and collecting parts to `join` would copy each of them
                // a second time (replayed reasoning is the bulk of a long
                // session's wire text).
                let mut parts = String::new();
                let mut reasoning = String::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                let mut tool_results: Vec<ChatMessage> = Vec::new();
                // Upstream `reasoningInterrupted` (to-llm-message.ts:158): a
                // reasoning part with no completion time was cut off mid-thought.
                let reasoning_interrupted = assistant.content.iter().any(|content| {
                    matches!(
                        content,
                        AssistantContent::Reasoning { time, .. }
                            if time.as_ref().is_some_and(|time| time.completed.is_none())
                    )
                });
                // Reasoning is model-scoped proof: upstream replays it as
                // `reasoning_content` only for a clean row and degrades an
                // interrupted/failed one to readable text
                // (to-llm-message.ts:174-184).
                let replay_reasoning = assistant.error.is_none() && !reasoning_interrupted;
                for content in &assistant.content {
                    match content {
                        // Upstream `meaningful`: an empty text part carries
                        // nothing and is filtered (to-llm-message.ts:202-206).
                        AssistantContent::Text { text, .. } if !text.is_empty() => {
                            parts.push_str(text)
                        }
                        AssistantContent::Text { .. } => {}
                        // Replayed as `reasoning_content`: thinking vendors
                        // (GLM/Z.ai, Kimi) 400 follow-up turns without it
                        // (upstream `lowerAssistantMessage` replays the same
                        // joined text). Replayed regardless of the row's model
                        // — upstream degrades cross-model reasoning to text
                        // (to-llm-message.ts `sameModel`); ox has no
                        // per-provider gate, cardinal owns dialects.
                        AssistantContent::Reasoning { text, .. } if text.is_empty() => {}
                        AssistantContent::Reasoning { text, .. } if replay_reasoning => {
                            reasoning.push_str(text)
                        }
                        AssistantContent::Reasoning { text, .. } => {
                            parts.push_str(text);
                        }
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
                                    // its own turn — a parts-list user turn.
                                    tool_results.push(ChatMessage::User {
                                        content: media.into(),
                                    });
                                }
                            }
                        }
                    }
                }
                // No assistant message may reach the wire with neither content
                // nor tool calls: strict upstreams reject exactly that shape
                // ("Invalid assistant message: content or tool_calls must be
                // non-empty", live 2026-10-22). A row whose only substance is
                // reasoning keeps its thought as text instead.
                if parts.is_empty() && tool_calls.is_empty() && !reasoning.is_empty() {
                    parts = std::mem::take(&mut reasoning);
                }
                if parts.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                out.push(ChatMessage::Assistant {
                    // Upstream sends `null` only for a tool-call-only turn
                    // (openai-chat.ts:491); any other row carries its joined
                    // text, which is non-empty by the guard above.
                    content: if parts.is_empty() { None } else { Some(parts) },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    reasoning_content: if reasoning.is_empty() {
                        None
                    } else {
                        Some(reasoning)
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
                    content: compaction::checkpoint_message(summary, recent).into(),
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

/// The typed text that opens the current turn, when this request is that
/// turn's opening request: `None` unless the transcript's trailing row is the
/// user prompt (a later step trails tool output) and it carries enough text to
/// retrieve anything.
///
/// The rows are the request *view*, so their text carries the overlay's `§N§`
/// prefix. That prefix is not something the user typed: leaving it in would
/// make its number a query token (the tagger's `§1§` becomes the word `1`) and
/// every turn would match whatever the corpus says about that number. The same
/// strip the persistence boundary applies removes it.
fn recall_query(rows: &[Message]) -> Option<String> {
    let Some(Message::User { prompt, .. }) = rows.last() else {
        return None;
    };
    let typed = strip_copied_tag_notation(&prompt.text);
    let text = typed.trim();
    (text.chars().count() >= RECALL_MIN_QUERY_CHARS).then(|| text.to_owned())
}

/// Append the recall block to a user message's content: one blank line between
/// the prompt and the block, no trailing newline.
fn append_recall(mut content: String, block: &str) -> String {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(block);
    content
}

/// Per-step keyed-instruction sync: diff current sources against what the
/// wire shows, publish durable delta rows, and return the update text for
/// this request's tail. The first step of a session builds its baseline and
/// logs the injection-surface accounting once (`injection baseline`).
async fn sync_instructions(
    ctx: &DrainCtx,
    observed: &[prompt::Observed],
    rows: &[Message],
    system0_bytes: usize,
    tool_count: usize,
) -> Option<String> {
    let (deltas, baseline_sizes) = {
        let mut baselines = super::lock(&ctx.instruction_baselines);
        // The sizes are pure from `observed`, so they can be reported after
        // the lock is released: tracing must not hold the baseline mutex.
        let fresh = !baselines.contains_key(&ctx.session_id);
        let baseline = baselines
            .entry(ctx.session_id.clone())
            .or_insert_with(|| instructions::baseline(observed));
        let wire = instructions::fold(rows, compaction_boundary(rows));
        let deltas = instructions::diff(observed, &wire, baseline);
        let sizes = fresh.then(|| prompt::injection_sizes(observed, system0_bytes, tool_count));
        (deltas, sizes)
    };
    if let Some(sizes) = baseline_sizes {
        tracing::info!(
            session = %ctx.session_id,
            system0_bytes = sizes.system0_bytes,
            mcp_bytes = sizes.mcp_bytes,
            skills_bytes = sizes.skills_bytes,
            skills_catalog = ?sizes.skills_catalog,
            instructions_bytes = sizes.instructions_bytes,
            date_bytes = sizes.date_bytes,
            env_bytes = sizes.env_bytes,
            tools = sizes.tools,
            total_bytes = sizes.total_bytes,
            estimated_tokens = sizes.estimated_tokens,
            "injection baseline"
        );
    }
    if deltas.is_empty() {
        return None;
    }
    let text = instructions::render_updates(&deltas);
    let delta = instructions::delta_record(&deltas);
    if let Err(error) = ctx
        .bus
        .publish(SessionEventData::SessionInstructionsUpdated {
            session_id: ctx.session_id.clone(),
            delta,
            sources: instructions::source_records(&deltas),
            text: Some(text.clone()),
        })
        .await
    {
        tracing::warn!(session = %ctx.session_id, %error, "instruction update publish failed");
        return None;
    }
    let mut baselines = super::lock(&ctx.instruction_baselines);
    if let Some(baseline) = baselines.get_mut(&ctx.session_id) {
        instructions::apply(deltas, baseline);
    }
    Some(text)
}

/// A tool part's call input for the wire: parsed from the streaming state's
/// string, borrowed verbatim from the settled states (upstream `toolInput`).
///
/// Borrowed rather than cloned: every caller serializes or formats the record
/// immediately, and a settled part's input is the whole call (a `write` hunk
/// can be a file's worth of text), so the copy is one per tool part per request.
pub(crate) fn tool_input(state: &ToolState) -> Cow<'_, Record> {
    match state {
        ToolState::Streaming { input } => {
            Cow::Owned(serde_json::from_str(input).unwrap_or_default())
        }
        ToolState::Running { input, .. }
        | ToolState::Completed { input, .. }
        | ToolState::Error { input, .. } => Cow::Borrowed(input),
    }
}

/// The `{role: "tool"}` result for a settled tool part (upstream
/// `toolResult`): joined text content for completed calls, `{error: …}` for
/// failed ones. A `streaming`/`running` part yields `None` — its own row's
/// result lands in the next request. Completed text is bounded for the wire
/// ([`bound_tool_output`]): the stored row keeps the full text, only this
/// request's view is head + marker + tail.
///
/// Attachments cannot ride inside a `tool` message: providers accept an image
/// only as user content. They are returned separately so the caller can append
/// one follow-up user message directly after the result.
fn tool_result_message(id: &str, state: &ToolState) -> Option<(ChatMessage, Vec<UserPart>)> {
    let mut media: Vec<UserPart> = Vec::new();
    let content = match state {
        ToolState::Completed { content, .. } => {
            // Borrow the completed text parts; join only when the result really
            // is several parts (a `File` part contributes a marker), so one
            // output part reaches the envelope without a copy.
            let parts: Vec<Cow<'_, str>> = content
                .iter()
                .map(|content| match content {
                    Content::Text { text } => Cow::Borrowed(text.as_str()),
                    Content::File { uri, mime, name } => {
                        media.push(UserPart::Image {
                            image_url: ImageUrl { url: uri.clone() },
                        });
                        Cow::Owned(format!(
                            "[attached {} ({mime})]",
                            name.as_deref().unwrap_or("file")
                        ))
                    }
                })
                .collect();
            let text: Cow<'_, str> = match parts.as_slice() {
                [] => Cow::Borrowed(""),
                [one] => Cow::Borrowed(one.as_ref()),
                _ => Cow::Owned(parts.iter().map(|part| part.as_ref()).collect()),
            };
            // Straight into the envelope: an intermediate `json!` value would
            // own a second copy of the text before it is escaped.
            let bounded = bound_tool_output(&text);
            serde_json::to_string(&ToolOutput {
                output: bounded.as_ref(),
            })
            .ok()?
        }
        ToolState::Error { error, .. } => serde_json::to_string(&ToolError { error }).ok()?,
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

/// The `{"output": …}` envelope of a completed tool result. Serialized from a
/// borrowed view so the result text is escaped exactly once.
#[derive(serde::Serialize)]
struct ToolOutput<'a> {
    output: &'a str,
}

/// The `{"error": …}` envelope of a failed tool result.
#[derive(serde::Serialize)]
struct ToolError<'a> {
    error: &'a SessionError,
}

/// A user prompt's wire text: the typed text plus attachment markers.
/// (Upstream `SessionPrompt.prepare` inlines resolved file content; 6a
/// stores attachments verbatim and marks them instead.)
///
/// This is the history/summary rendering: [`build_chat_messages`] uses
/// [`user_message_content`] instead, because resolved skill bodies go on the
/// wire but must stay out of the historian's serialised `message.text`.
pub(crate) fn user_prompt_text(prompt: &Prompt) -> String {
    let mut text = prompt.text.clone();
    for file in prompt.files.iter().flatten() {
        push_marker(&mut text, file);
    }
    text
}

/// The `[attached file: name (mime)]` marker line for an attachment that is
/// not rendered into model content (and every attachment in summaries).
fn push_marker(text: &mut String, file: &FileAttachment) {
    let name = file.name.as_deref().unwrap_or("file");
    text.push_str(&format!("\n[attached file: {name} ({})]", file.mime));
}

/// The wire content of a user message, in upstream `toLLMMessage` order
/// (skills → typed text → attachments). Upstream emits one content part per
/// skill, the typed text and each attachment piece, and its OpenAI-Chat
/// lowering concatenates text parts with an empty separator
/// (`packages/ai/src/protocols/openai-chat.ts:436-438`,
/// `content.map((part) => part.text).join("")`), so ox concatenates adjacent
/// text too rather than inserting separators: byte parity, and a skill body's
/// closing `</skill_content>` abuts the typed text exactly as it does
/// upstream. A skill whose `text` is missing contributes nothing.
///
/// Attachments follow upstream `attachmentContent`:
/// - `text/plain` and `application/x-directory` are inlined as text;
/// - `image/png|jpeg|gif|webp` become an `image_url` data-URL part (after an
///   `Attached file: <path>` line when the source is a `file:` URI), which
///   turns the content into a parts list;
/// - anything else keeps the `[attached file: …]` marker of
///   [`user_prompt_text`]. Deviation: upstream sends PDFs as media and drops
///   unknown types; ox has no PDF content part and keeps a marker so the
///   model knows a file existed.
///
/// With no image the result is a plain string, byte-identical to the
/// pre-attachment rendering for prompts without renderable attachments.
pub(crate) fn user_message_content(prompt: &Prompt) -> UserContent {
    let mut out = ContentBuilder::default();
    for text in prompt
        .skills
        .iter()
        .flatten()
        .filter_map(|skill| skill.text.as_deref())
    {
        out.text.push_str(text);
    }
    out.text.push_str(&prompt.text);
    for file in prompt.files.iter().flatten() {
        render_attachment(&mut out, file);
    }
    out.finish()
}

/// Accumulates user content: text runs merge, an image closes the current run.
#[derive(Default)]
struct ContentBuilder {
    parts: Vec<UserPart>,
    text: String,
}

impl ContentBuilder {
    /// Close the current text run as a part of its own.
    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.parts.push(UserPart::Text {
                text: std::mem::take(&mut self.text),
            });
        }
    }

    fn image(&mut self, url: String) {
        self.flush_text();
        self.parts.push(UserPart::Image {
            image_url: ImageUrl { url },
        });
    }

    fn finish(mut self) -> UserContent {
        if self.parts.is_empty() {
            return UserContent::Text(self.text);
        }
        if !self.text.is_empty() {
            self.parts.push(UserPart::Text { text: self.text });
        }
        UserContent::Parts(self.parts)
    }
}

/// Nothing for the model to read: empty text, or no parts.
fn content_is_blank(content: &UserContent) -> bool {
    match content {
        UserContent::Text(text) => text.is_empty(),
        UserContent::Parts(parts) => parts.is_empty(),
    }
}

/// Put `prefix` at the very start of the content: on the first text, or as a
/// text part of its own when the content opens with an image.
fn prefix_content(content: UserContent, prefix: String) -> UserContent {
    match content {
        UserContent::Text(mut text) => {
            text.insert_str(0, &prefix);
            UserContent::Text(text)
        }
        UserContent::Parts(mut parts) => {
            match parts.first_mut() {
                Some(UserPart::Text { text }) => text.insert_str(0, &prefix),
                _ => parts.insert(0, UserPart::Text { text: prefix }),
            }
            UserContent::Parts(parts)
        }
    }
}

/// Render one attachment into `out` (upstream `attachmentContent`).
fn render_attachment(out: &mut ContentBuilder, file: &FileAttachment) {
    let location = match &file.source {
        FileSource::Uri { uri } => Some(uri.as_str()),
        FileSource::Inline => None,
    };
    match file.mime.as_str() {
        "text/plain" => {
            let label = file
                .name
                .as_deref()
                .or(location)
                .unwrap_or("inline attachment");
            let mark = out.text.len();
            out.text.push_str("\n\nAttached file: ");
            out.text.push_str(label);
            push_description(&mut out.text, file);
            out.text.push_str("\n\n");
            if !push_decoded_text(&mut out.text, &file.data) {
                out.text.truncate(mark);
                push_marker(&mut out.text, file);
            }
        }
        "application/x-directory" => {
            let label = location
                .and_then(file_uri_path)
                .or_else(|| file.name.clone())
                .or_else(|| location.map(str::to_owned));
            let mark = out.text.len();
            out.text.push_str("\n\nAttached directory: ");
            out.text.push_str(label.as_deref().unwrap_or("directory"));
            push_description(&mut out.text, file);
            let body_at = out.text.len();
            out.text.push_str("\n\n");
            if !push_decoded_text(&mut out.text, &file.data) {
                out.text.truncate(mark);
                push_marker(&mut out.text, file);
            } else if out.text.len() == body_at + 2 {
                out.text.truncate(body_at);
            }
        }
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => {
            if let Some(path) = location.and_then(file_uri_path) {
                // Upstream emits the location as a content part of its own.
                out.flush_text();
                out.parts.push(UserPart::Text {
                    text: format!("Attached file: {path}"),
                });
            }
            // The single unavoidable copy: the wire carries the payload inside
            // a `data:` URL (same boundary as tool-result images).
            out.image(format!("data:{};base64,{}", file.mime, file.data));
        }
        _ => push_marker(&mut out.text, file),
    }
}

fn push_description(text: &mut String, file: &FileAttachment) {
    if let Some(description) = &file.description {
        text.push_str("\nDescription: ");
        text.push_str(description);
    }
}

/// Append a base64 text payload to `out` as UTF-8 (lossy), decoded once and
/// copied once into `out`. `false` (with `out` untouched) when the stored
/// payload is not valid base64; the caller then falls back to a marker.
fn push_decoded_text(out: &mut String, data: &str) -> bool {
    use base64::Engine as _;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return false;
    };
    out.push_str(&String::from_utf8_lossy(&bytes));
    true
}

/// The local path of a `file:` URI (upstream `attachmentLocation` /
/// `fileURLToPath`): query and fragment dropped, percent-decoded. `None` for
/// other schemes, a remote host, or a path that is not valid UTF-8. A local
/// helper rather than the `url` crate, which ox-engine does not otherwise
/// depend on.
fn file_uri_path(uri: &str) -> Option<String> {
    let scheme_end = uri.find(':')?;
    if !uri[..scheme_end].eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = uri[scheme_end + 1..].strip_prefix("//")?;
    let rest = rest.split(['?', '#']).next()?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(slash) if rest[..slash].eq_ignore_ascii_case("localhost") => &rest[slash..],
        _ => return None,
    };
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_digit(*bytes.get(i + 1)?)?;
            let lo = hex_digit(*bytes.get(i + 2)?)?;
            decoded.push(hi << 4 | lo);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Value of an ASCII hex digit; unlike `from_str_radix`, no sign is accepted.
fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

/// The session transcript in `seq` order, decoded from `session_message`.
pub(crate) async fn load_history(db: &ox_db::Db, session_id: &str) -> Result<Vec<Message>> {
    load_history_after(db, session_id, i64::MIN).await
}

/// The text of the newest assistant row, if the transcript has one. Used by the
/// todo reminder to tell a stop from a hand-off to the user. Reads the tail
/// only: the reminder runs once per turn end, and a full transcript scan there
/// would be wasteful on long sessions.
pub(crate) async fn last_assistant_text(
    db: &ox_db::Db,
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
    let Some(row) = ox_db::optional(&mut stmt, [session_id]).await? else {
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
                    ox_schema::message::AssistantContent::Text { text, .. } => Some(text.as_str()),
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
    db: &ox_db::Db,
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
    db: &ox_db::Db,
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
    rows.into_iter().map(decode_row_owned).collect()
}

/// One `session_message` row as the decoded message JSON: the row's `data`
/// column with `id`/`type` merged back in, exactly the shape the tagger's
/// `part_path` addresses (`decode({ ...row.data, id, type })` upstream).
/// [`decode_row`] reads `id`/`type` off the same object.
async fn rows_after(
    db: &ox_db::Db,
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
    db: &ox_db::Db,
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
    let mut values = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let data: String = row.get(2)?;
        let seq: i64 = row.get(3)?;
        // Parsed inside the loop, not after collecting the raw JSON: holding
        // every row's string and every row's value at once would double the
        // tail's footprint for the whole load.
        let mut value: serde_json::Value =
            serde_json::from_str(&data).context("decode session_message.data")?;
        Message::merge_row_columns(&id, &kind, &mut value);
        values.push((seq, value));
    }
    Ok(values)
}

pub(crate) fn decode_row(value: &serde_json::Value) -> Result<Message> {
    decode_row_owned(value.clone())
}

/// [`decode_row`] without the row copy.
///
/// Both tail loaders hold the whole transcript as JSON values already (the §N§
/// overlay is applied across rows before anything is decoded), so each row's
/// value is dead the moment it is decoded: merging `id`/`type` in place and
/// deserializing from the moved value keeps the tail from being copied a second
/// time on the way into [`Message`]s.
pub(crate) fn decode_row_owned(value: serde_json::Value) -> Result<Message> {
    if value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        anyhow::bail!("session_message row without an id");
    }
    if value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        anyhow::bail!("session_message row without a type");
    }
    Ok(Message::from_merged_row(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_failure_retry_kind() {
        assert_eq!(
            AttemptFailure::IncompleteStream.retry_kind(),
            RetryKind::Bounded
        );
        assert_eq!(AttemptFailure::EmptyStream.retry_kind(), RetryKind::Bounded);
        assert!(!AttemptFailure::EmptyStream.is_interrupted_stream());
        assert!(!AttemptFailure::EmptyStream.is_overflow());
        assert_eq!(
            AttemptFailure::EmptyStream.session_error(),
            to_session_error(&CardinalError::EarlyClose)
        );
        let no_route = CardinalError::NoRoute { model: "m".into() };
        assert_eq!(
            AttemptFailure::Transport(no_route).retry_kind(),
            RetryKind::Never
        );
    }

    fn usage(prompt: u64, completion: u64, cached: Option<u64>, reasoning: Option<u64>) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
            reasoning_tokens: reasoning,
        }
    }

    #[test]
    fn tokens_for_reported_usage_keeps_the_upstream_mapping() {
        let got = tokens_for(Some(usage(1000, 50, Some(200), Some(7))), || {
            panic!("estimate must not be called")
        });
        assert_eq!(
            got,
            TokenUsage {
                input: 800,
                output: 50,
                reasoning: 7,
                cache: TokenCache {
                    read: 200,
                    write: 0
                },
                estimated: None,
            }
        );
    }

    #[test]
    fn tokens_for_missing_usage_stores_the_estimate_as_input() {
        assert_eq!(
            tokens_for(None, || 1234),
            TokenUsage {
                input: 1234,
                output: 0,
                reasoning: 0,
                cache: TokenCache { read: 0, write: 0 },
                estimated: Some(true),
            }
        );
    }

    #[test]
    fn estimated_flag_survives_serialization_only_for_estimates() {
        let estimated = serde_json::to_value(tokens_for(None, || 7)).unwrap();
        assert_eq!(estimated["estimated"], serde_json::json!(true));
        let reported = serde_json::to_value(tokens_for(Some(usage(1, 2, None, None)), || {
            panic!("estimate must not be called")
        }))
        .unwrap();
        assert!(reported.get("estimated").is_none(), "{reported}");
    }

    #[test]
    fn estimate_prompt_tokens_scales_bytes_by_the_ratio() {
        assert_eq!(estimate_prompt_tokens(4000, 1.0), 1000);
        assert_eq!(estimate_prompt_tokens(4000, 1.5), 1500);
        assert_eq!(estimate_prompt_tokens(0, 1.0), 0);
        assert_eq!(estimate_prompt_tokens(1, 1.0), 1);
        assert_eq!(estimate_prompt_tokens(4000, f64::NAN), 1000);
        assert_eq!(estimate_prompt_tokens(4000, f64::INFINITY), 1000);
        assert_eq!(estimate_prompt_tokens(4000, -2.0), 1000);
        assert_eq!(estimate_prompt_tokens(usize::MAX, 1e30), i64::MAX);
    }

    /// A decoded assistant row and the raw JSON the historian trigger reads,
    /// both built from the same `tokens` shape.
    fn parity_row(tokens: Option<serde_json::Value>) -> (Message, serde_json::Value) {
        let mut json = serde_json::json!({
            "type": "assistant",
            "id": "msg_a",
            "time": { "created": 1 },
            "agent": "build",
            "model": { "id": "m", "providerID": "p" },
            "content": [],
        });
        if let Some(tokens) = tokens {
            json["tokens"] = tokens;
        }
        let message: Message = serde_json::from_value(json.clone()).expect("assistant decodes");
        (message, json)
    }

    #[test]
    fn assistant_tokens_parity_with_raw_prompt_tokens() {
        let full = |input: i64, read: i64| {
            serde_json::json!({
                "input": input,
                "output": 5,
                "reasoning": 0,
                "cache": { "read": read, "write": 3 },
            })
        };
        let cases = [
            (Some(full(800, 200)), Some(1000)),
            (Some(full(800, 0)), Some(800)),
            (Some(full(0, 0)), Some(0)),
            (None, None),
        ];
        for (tokens, expected) in cases {
            let (message, row) = parity_row(tokens.clone());
            assert_eq!(assistant_tokens(&message), expected, "decoded {tokens:?}");
            assert_eq!(
                super::super::historian_trigger::prompt_tokens(&row),
                expected,
                "raw {tokens:?}"
            );
            assert_eq!(
                assistant_tokens(&message),
                super::super::historian_trigger::prompt_tokens(&row),
                "parity {tokens:?}"
            );
        }
    }

    fn request_with_tool() -> ChatRequest {
        let mut request = ChatRequest::new(
            "m",
            vec![ChatMessage::system("sys"), ChatMessage::user("go")],
        );
        request.tools = Arc::new(vec![ox_llm::cardinal::wire::ToolDef::function(
            "read",
            "read a file",
            serde_json::json!({"type": "object"}),
        )]);
        request
    }

    #[test]
    fn final_step_disables_tool_choice_and_appends_prompt() {
        use ox_llm::cardinal::wire::ToolChoice;
        let mut request = request_with_tool();
        let tools_before = serde_json::to_string(request.tools.as_slice()).unwrap();
        let messages_before = request.messages.clone();
        assert_eq!(request.tool_choice, ToolChoice::Auto);

        apply_final_step(&mut request);

        assert_eq!(request.tool_choice, ToolChoice::None);
        assert_eq!(
            serde_json::to_string(request.tools.as_slice()).unwrap(),
            tools_before,
            "tool definitions stay to keep the provider cache prefix"
        );
        assert_eq!(request.messages.len(), messages_before.len() + 1);
        assert_eq!(request.messages[..messages_before.len()], messages_before);
        assert_eq!(
            request.messages.last(),
            Some(&ChatMessage::Assistant {
                content: Some(MAX_STEPS_PROMPT.to_owned()),
                tool_calls: None,
                reasoning_content: None,
            })
        );
        assert_eq!(
            MAX_STEPS_PROMPT,
            "The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only."
        );
    }

    #[test]
    fn final_step_request_serializes_tool_choice_none_and_prompt() {
        let mut request = request_with_tool();
        apply_final_step(&mut request);
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tool_choice\":\"none\""), "{json}");
        assert!(json.contains("\"tools\":["), "{json}");
        assert!(json.contains(MAX_STEPS_PROMPT), "{json}");
    }

    #[test]
    fn final_step_keeps_tool_call_pairing_intact() {
        let call = ToolCall {
            id: "c1".to_owned(),
            kind: FunctionType::Function,
            function: FunctionCall {
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
            },
        };
        let mut request = ChatRequest::new(
            "m",
            vec![
                ChatMessage::user("go"),
                ChatMessage::assistant_tool_calls(vec![call]),
                ChatMessage::tool("c1", "ok"),
            ],
        );
        let before = request.messages.clone();
        assert_eq!(before.len(), 3, "already paired, no placeholder spliced");

        apply_final_step(&mut request);

        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.messages[..3], before);
        assert!(matches!(
            request.messages.last(),
            Some(ChatMessage::Assistant {
                tool_calls: None,
                ..
            })
        ));
    }

    #[test]
    fn tools_disabled_error_shape() {
        let error = tools_disabled_error();
        assert_eq!(error.kind, "tool.disabled");
        assert_eq!(
            error.message,
            "Tools are disabled after the maximum agent steps"
        );
        assert_eq!(error.status, None);
        assert!(error.response.is_none());
    }
}
