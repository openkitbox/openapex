use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use ox_llm::cardinal::ChatError;
use ox_llm::cardinal::wire::ChatRequest;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::support::{FinishReason, ModelRef, SessionError, TokenUsage};

use super::super::drain::DrainCtx;
use super::super::retry::{self, RetryKind};
use super::error::{
    invalid_output_error, pairing_rejection_ids, to_session_error, tool_call_digest,
};
use super::request::build_request;
use super::stream::consume;
/// Upstream `STEP_INTERRUPTED` (runner/step.ts).
const STEP_INTERRUPTED: &str = "Step interrupted";
/// Upstream `TOOLS_INTERRUPTED` (runner/step.ts).
pub(super) const TOOLS_INTERRUPTED: &str = "Tool execution interrupted";
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
pub(super) enum Attempt {
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
pub(super) enum AttemptFailure {
    /// A transport-grade failure: connect error, mid-stream read failure
    /// (`EarlyClose`), malformed SSE framing, or a connection error.
    Transport(ChatError),
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
    /// Tell the provider quota guard about a transport failure, so a
    /// throttle seen by a live turn also holds the dreamer back.
    fn observe_quota(&self) {
        if let AttemptFailure::Transport(error) = self {
            crate::mctx::quota::observe(error, id::now_millis());
        }
    }

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
                ChatError::EarlyClose | ChatError::Sse(_) | ChatError::Transport(_),
            ) => true,
            AttemptFailure::Transport(
                ChatError::Http { .. }
                | ChatError::Upstream(_)
                // A configuration error, not a stream event.
                | ChatError::NoRoute { .. },
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
            AttemptFailure::Transport(error) => {
                super::super::provider_error::is_overflow_error(error)
            }
            AttemptFailure::IncompleteStream | AttemptFailure::EmptyStream => false,
        }
    }

    /// The session error the row settles with (upstream `toSessionError`).
    fn session_error(&self) -> SessionError {
        match self {
            AttemptFailure::Transport(error) => to_session_error(error),
            AttemptFailure::IncompleteStream => invalid_output_error(),
            AttemptFailure::EmptyStream => to_session_error(&ChatError::EarlyClose),
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
            None => match build_request(ctx, params).await {
                Ok(request) => request,
                Err(error) if error.is::<super::super::request_budget::NeedsCompaction>() => {
                    return Ok(StepOutcome::Overflow {
                        error: SessionError {
                            kind: "context-overflow".to_owned(),
                            message: error.to_string(),
                            status: None,
                            response: None,
                        },
                    });
                }
                Err(error) => return Err(error),
            },
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
                crate::mctx::quota::observe(&error, id::now_millis());
                // The flight recorder for the pairing-400 class
                // (docs/tool-call-pairing-400.md): what ox put on the wire
                // (post pairing-guard) beside what the upstream named as
                // unanswered. One log line decides ox-vs-downstream on the
                // next recurrence — the id the upstream complained about
                // cannot be in `sent` if the fault is not ours.
                if let ChatError::Http {
                    status: 400, body, ..
                } = &error
                {
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
                if super::super::provider_error::is_overflow_error(&error) {
                    return Ok(StepOutcome::Overflow {
                        error: to_session_error(&error),
                    });
                }
                // Connect failure: nothing was published for this attempt.
                if let Some(delay) = backoff.on_failure(retry::classify(&error), false) {
                    let delay = delay.max(error.retry_after().unwrap_or_default());
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
                failure.observe_quota();
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
                let delay = backoff
                    .on_failure(failure.retry_kind(), published)
                    .map(|delay| {
                        let advice = match &failure {
                            AttemptFailure::Transport(error) => error.retry_after(),
                            _ => None,
                        };
                        delay.max(advice.unwrap_or_default())
                    });
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

/// Interrupted settlement: an open assistant row settles with `step.failed`
/// carrying the upstream `aborted` error; a never-started step settles
/// silently (the actor publishes `execution.interrupted`).
pub(super) async fn settle_interrupted(
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
pub(super) async fn note_output(
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
