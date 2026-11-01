use std::borrow::Cow;

use anyhow::Result;

use crate::mctx::temporal;
use ox_llm::cardinal::wire::{
    ChatMessage, FunctionCall, FunctionType, ImageUrl, ToolCall, UserPart,
};
use ox_schema::message::{
    AssistantContent, Compaction, CompactionCompleted, CompactionMessage, Message, ToolState,
};
use ox_schema::support::{Content, Record, SessionError};

use super::super::compaction;
use super::super::drain::DrainCtx;
use super::super::output_budget::bound_tool_output;
use super::content::{content_is_blank, prefix_content, user_message_content};
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
/// of [`super::super::historian_trigger::prompt_tokens`] — the same
/// `tokens.input + tokens.cache.read` sum over the projected JSON, read
/// from the already-decoded tail so the step-boundary fold trigger costs no
/// query (plan D1: one measurement, one meaning).
///
/// Parity with the raw reader is pinned per row shape by
/// `tests::assistant_tokens_parity_with_raw_prompt_tokens`
/// (bottom of this file) — change both readers or neither.
pub(super) fn assistant_tokens(message: &Message) -> Option<i64> {
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
