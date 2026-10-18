//! One streaming step, ported from `packages/core/src/session/runner/step.ts`
//! (text-only for task 6a; the tool/continuation branches are structured for
//! 6b).
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
//! - On a clean finish: `session.step.streamed` then `session.step.ended`
//!   with upstream's token shape (`input` = prompt minus cached, `output` =
//!   completion, `reasoning`, `cache.read` = cached, `cache.write` = 0).
//! - An interruption after the step started settles the step with
//!   `session.step.failed` carrying the upstream `aborted` error.
//! - Transport errors before any output retry with backoff (upstream's
//!   SessionRunnerRetry publishes `session.retry.scheduled`); 6a caps at 3
//!   attempts. 6b replaces the flat cap with upstream's retry policy
//!   (recoverable overflow/continuation handling).
//!
//! The context assembly lives in one named function
//! ([`build_chat_messages`]) so 6b and the e2e suite can trust the mapping.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::cardinal::CardinalError;
use crate::cardinal::chat::BoxChatStream;
use crate::cardinal::stream::{AssembledToolCall, StreamEvent};
use crate::cardinal::wire::{
    ChatMessage, ChatRequest, FunctionCall, FunctionType, ToolCall, Usage,
};
use crate::id;
use crate::prompt;
use crate::schema::event::SessionEventData;
use crate::schema::message::{AssistantContent, Message};
use crate::schema::support::{
    FinishReason, ModelRef, Prompt, SessionError, TokenCache, TokenUsage,
};

use super::drain::DrainCtx;

/// Upstream `STEP_INTERRUPTED` (runner/step.ts).
const STEP_INTERRUPTED: &str = "Step interrupted";
/// Transport attempts before the step fails (upstream policy arrives in 6b).
const MAX_ATTEMPTS: usize = 3;
/// Backoff base; attempt N sleeps `base * N`.
const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Terminal states of one step, consumed by the drain loop.
#[derive(Debug)]
pub(crate) enum StepOutcome {
    /// The stream finished. `tool_calls` would continue the turn (6b
    /// executes them; 6a turns always end here).
    Completed {
        finish: FinishReason,
        tool_calls: Vec<ToolCall>,
    },
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
    // Tool-call fragments by stream index (6b executes these; 6a only
    // assembles them for the outcome).
    let mut tool_acc: BTreeMap<u32, AssembledToolCall> = BTreeMap::new();

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
                    // TODO(task 6b): publish tool.input.started/delta/ended and
                    // session.tool.called as the fragments arrive.
                    let call = tool_acc.entry(index).or_default();
                    if let Some(id) = id {
                        call.id = id;
                    }
                    if let Some(name) = name {
                        call.name = name;
                    }
                    call.arguments.push_str(&arguments);
                }
                StreamEvent::Finish { reason } => {
                    finish = Some((map_finish(&reason), reason));
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
        // Upstream treats a missing finish reason as invalid provider output.
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

    ctx.bus
        .publish(SessionEventData::StepStreamed {
            session_id: ctx.session_id.clone(),
            assistant_message_id: params.assistant_message_id.clone(),
        })
        .await?;
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
        tool_calls: assemble_tool_calls(tool_acc),
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

/// Reconstruct wire tool calls from streamed fragments, in index order.
fn assemble_tool_calls(acc: BTreeMap<u32, AssembledToolCall>) -> Vec<ToolCall> {
    acc.into_values()
        .map(|call| ToolCall {
            id: call.id,
            kind: FunctionType::Function,
            function: FunctionCall {
                name: call.name,
                arguments: call.arguments,
            },
        })
        .collect()
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
// Context assembly (one named mapping; 6b/e2e trust this function)
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
///   tool_calls: from its tool content}`; reasoning content is dropped.
/// - tool results (`{role: "tool", tool_call_id, content}`) ride in 6b with
///   tool execution.
/// - compaction rows collapse into the context summary (6b); idle/shell/
///   switch markers never reach the model.
pub(crate) fn build_chat_messages(system: &str, messages: &[Message]) -> Vec<ChatMessage> {
    let mut out = vec![ChatMessage::System {
        content: system.to_owned(),
    }];
    for message in messages {
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
                let mut parts: Vec<&str> = Vec::new();
                for content in &assistant.content {
                    if let AssistantContent::Text { text, .. } = content {
                        parts.push(text);
                    }
                    // Reasoning is dropped; tool content mapping is 6b.
                }
                if parts.is_empty() {
                    continue;
                }
                out.push(ChatMessage::Assistant {
                    content: Some(parts.join("")),
                    tool_calls: None,
                });
            }
            Message::Compaction(_)
            | Message::Idle { .. }
            | Message::Shell { .. }
            | Message::AgentSwitched { .. }
            | Message::ModelSwitched { .. }
            | Message::LocationSwitched { .. } => {}
        }
    }
    out
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
async fn load_history(db: &crate::db::Db, session_id: &str) -> Result<Vec<Message>> {
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
