use std::borrow::Cow;
use std::sync::Arc;

use anyhow::Result;

use crate::mctx::temporal;
use ox_llm::chat_completions::wire::{
    ChatMessage, FunctionCall, FunctionType, ImageUrl, ToolCall, UserPart,
};
use ox_llm::messages::replay::from_provider_state;
use ox_llm::responses::replay::{
    ResponsesReplayState, from_provider_state as responses_from_provider_state,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MctxDiagnostic {
    pub outcome: &'static str,
    pub m0_bytes: usize,
    pub m1_bytes: usize,
    pub head_bytes: usize,
    pub m0_hash: u64,
    pub m1_hash: u64,
    pub mctx_head_hash: u64,
}

#[cfg(test)]
mod responses_replay_tests {
    use ox_llm::responses::replay::{ResponsesOrigin, ResponsesReplayInput, ResponsesReplayState};
    use ox_schema::message::{
        AssistantContent, AssistantMessage, AssistantTime, Compaction, CompactionCompleted,
        CompactionMessage, CompactionReason, Message, TimeCreated,
    };
    use ox_schema::support::ModelRef;
    use serde_json::json;

    use super::{responses_compacted, responses_replay_state};

    fn assistant(id: &str, replay: Option<ResponsesReplayState>) -> Message {
        Message::Assistant(Box::new(AssistantMessage {
            id: id.to_owned(),
            metadata: None,
            time: AssistantTime {
                created: 1,
                streamed: None,
                completed: Some(2),
            },
            agent: "apex".to_owned(),
            model: ModelRef {
                id: "gpt-5".to_owned(),
                provider_id: "responses".to_owned(),
                variant: None,
            },
            content: vec![AssistantContent::Text {
                text: "answer".to_owned(),
                state: None,
            }],
            snapshot: None,
            finish: None,
            raw_finish: None,
            provider_state: replay
                .map(|state| state.into_provider_state().expect("state serializes")),
            cost: None,
            tokens: None,
            error: None,
            retry: None,
        }))
    }

    fn replay() -> ResponsesReplayState {
        ResponsesReplayState::new(
            ResponsesOrigin::new(
                "responses",
                "https://api.example/v1/responses",
                "responses/v1",
            ),
            ResponsesReplayInput {
                model: "gpt-5".to_owned(),
                items: vec![json!({"id": "raw-item"})],
                ..ResponsesReplayInput::default()
            },
        )
    }

    fn checkpoint(marked: bool) -> Message {
        let mut provider_state = ox_schema::support::Record::new();
        if marked {
            provider_state.insert(
                super::super::super::compaction::RESPONSES_COMPACTED_KEY.to_owned(),
                json!(true),
            );
        }
        Message::Compaction(CompactionMessage {
            id: "compaction-1".to_owned(),
            metadata: None,
            time: TimeCreated { created: 3 },
            compaction: Compaction::Completed(Box::new(CompactionCompleted {
                reason: CompactionReason::Manual,
                model: None,
                provider_state: Some(provider_state),
                summary: "summary".to_owned(),
                recent: String::new(),
                provider_context: None,
                cost: None,
                tokens: None,
            })),
        })
    }

    #[test]
    fn checkpoint_hides_replay_state_from_before_the_boundary() {
        let rows = [assistant("before", Some(replay())), checkpoint(true)];
        assert!(responses_compacted(&rows));
        assert!(
            responses_replay_state(&rows).is_none(),
            "a checkpoint must not carry pre-compaction raw items forward"
        );
    }

    #[test]
    fn replay_state_from_after_the_checkpoint_is_kept() {
        let rows = [
            assistant("before", Some(replay())),
            checkpoint(true),
            assistant("after", Some(replay())),
        ];
        assert!(responses_replay_state(&rows).is_some());
    }

    #[test]
    fn an_unmarked_checkpoint_is_not_treated_as_a_responses_compaction() {
        let rows = [checkpoint(false)];
        assert!(!responses_compacted(&rows));
    }
}

#[derive(Debug)]
pub(crate) struct MctxHead {
    pub m0: Option<String>,
    pub m1: Option<String>,
    pub floor: i64,
    pub diagnostic: MctxDiagnostic,
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn hash_head(m0: Option<&str>, m1: Option<&str>) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for part in [m0.unwrap_or(""), m1.unwrap_or("")] {
        for byte in part.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn mctx_diagnostic(m0: Option<&str>, m1: Option<&str>, outcome: &'static str) -> MctxDiagnostic {
    let m0_bytes = m0.map_or(0, str::len);
    let m1_bytes = m1.map_or(0, str::len);
    MctxDiagnostic {
        outcome,
        m0_bytes,
        m1_bytes,
        head_bytes: m0_bytes.saturating_add(m1_bytes),
        m0_hash: hash_bytes(m0.unwrap_or("").as_bytes()),
        m1_hash: hash_bytes(m1.unwrap_or("").as_bytes()),
        mctx_head_hash: hash_head(m0, m1),
    }
}

/// How the head pass meets the step: a normal step commits it, while a consumer
/// that needs the same head and floor later in the same turn only previews it so
/// the cache, floor and drop lifecycle are not advanced twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Execution {
    Commit,
    Preview,
}

/// The magic-context head for one step: the frozen `m[0]`/`m[1]` halves plus
/// the transcript floor they cover, or `(None, None, 0)` when the integration
/// is off or the pass failed (the prompt then stays byte-identical to the
/// integration-off one — a broken head must never fail the turn).
pub(crate) async fn mctx_head(
    ctx: &DrainCtx,
    budget: crate::mctx::budget::Budget,
    model: &ox_schema::support::ModelRef,
) -> MctxHead {
    mctx_head_with(ctx, budget, model, Execution::Commit).await
}

/// The same head and floor as [`mctx_head`], but computed with a preview pass:
/// no cache write, no floor advance, no queued drop consumed.
pub(crate) async fn mctx_head_preview(
    ctx: &DrainCtx,
    budget: crate::mctx::budget::Budget,
    model: &ox_schema::support::ModelRef,
) -> MctxHead {
    mctx_head_with(ctx, budget, model, Execution::Preview).await
}

async fn mctx_head_with(
    ctx: &DrainCtx,
    budget: crate::mctx::budget::Budget,
    model: &ox_schema::support::ModelRef,
    execution: Execution,
) -> MctxHead {
    if !ctx.config.magic_context.enabled {
        return MctxHead {
            m0: None,
            m1: None,
            floor: 0,
            diagnostic: mctx_diagnostic(None, None, "disabled"),
        };
    }
    let last_seq = match latest_seq(&ctx.db, &ctx.session_id).await {
        Ok(seq) => seq,
        Err(error) => {
            tracing::warn!(session = %ctx.session_id, error = %error, "mctx pass failed; continuing without a head");
            return MctxHead {
                m0: None,
                m1: None,
                floor: 0,
                diagnostic: mctx_diagnostic(None, None, "failed"),
            };
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
        calibration: crate::mctx::calibration::ratio_for(
            &ctx.calibrations,
            &ctx.session_id,
            &model.provider_id,
            &model.id,
        ),
        provider_id: &model.provider_id,
        model_id: &model.id,
    };
    let result = match execution {
        Execution::Commit => crate::mctx::pass::run(request).await,
        Execution::Preview => crate::mctx::pass::preview(request)
            .await
            .map(|(result, _applied)| result),
    };
    match result {
        Ok(result) => {
            let diagnostic = mctx_diagnostic(
                result.head_m0.as_deref(),
                result.head_m1.as_deref(),
                match &result.outcome {
                    crate::mctx::cache::PassOutcome::DeferReplayed { .. } => "defer_replayed",
                    crate::mctx::cache::PassOutcome::BustApplied { .. } => "bust_applied",
                },
            );
            MctxHead {
                m0: result.head_m0,
                m1: result.head_m1,
                floor: result.floor,
                diagnostic,
            }
        }
        Err(error) => {
            tracing::warn!(
                session = %ctx.session_id,
                error = %error,
                "mctx pass failed; continuing without a head"
            );
            MctxHead {
                m0: None,
                m1: None,
                floor: 0,
                diagnostic: mctx_diagnostic(None, None, "failed"),
            }
        }
    }
}

impl DrainCtx {
    /// Preview the live head and tail together without persisting their decisions.
    pub(crate) async fn preview_context(
        &self,
        budget: crate::mctx::budget::Budget,
        model: &ox_schema::support::ModelRef,
    ) -> Result<(
        Option<String>,
        Option<String>,
        Vec<ox_schema::message::Message>,
    )> {
        if !self.config.magic_context.enabled {
            let rows = super::history::load_history_after(&self.db, &self.session_id, 0).await?;
            return Ok((None, None, rows));
        }
        let project = self.directory.to_string_lossy();
        let request = crate::mctx::pass::PassRequest {
            db: &self.db,
            session_id: &self.session_id,
            project: &project,
            last_seq: latest_seq(&self.db, &self.session_id).await?,
            budget,
            forced: false,
            run_started: false,
            calibration: crate::mctx::calibration::ratio_for(
                &self.calibrations,
                &self.session_id,
                &model.provider_id,
                &model.id,
            ),
            provider_id: &model.provider_id,
            model_id: &model.id,
        };
        let (m0, m1, floor, applied) = match crate::mctx::pass::preview(request).await {
            Ok((head, applied)) => (head.head_m0, head.head_m1, head.floor, applied),
            Err(error) => {
                tracing::warn!(session = %self.session_id, error = %error, "mctx preview failed; continuing without a head");
                (None, None, 0, Vec::new())
            }
        };
        let rows =
            super::history::preview_overlaid_tail(&self.db, &self.session_id, floor, &applied)
                .await?;
        Ok((m0, m1, rows))
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
///
/// The answer is an *index into the rows it was given*, and 0 reads as "no
/// completed checkpoint here" (the render starts at the first row either way).
/// A caller that indexes the same history by ordinal maps this index back
/// through its own rows — the historian's region load does exactly that, so both
/// renderers start the conversation at the same row. The rows are an iterator so
/// such a caller does not have to build a vector of references first.
pub(crate) fn compaction_boundary<'row>(
    messages: impl DoubleEndedIterator<Item = &'row Message> + ExactSizeIterator,
) -> usize {
    let len = messages.len();
    messages
        .rev()
        .position(|message| {
            matches!(
                message,
                Message::Compaction(CompactionMessage {
                    compaction: Compaction::Completed(_),
                    ..
                })
            )
        })
        .map_or(0, |from_end| len - 1 - from_end)
}

/// The newest Responses replay state inside the current history window.
///
/// A completed checkpoint is a hard boundary: state from before it would put
/// raw items back into a request whose readable history was compacted away.
pub(super) fn responses_replay_state(messages: &[Message]) -> Option<Arc<ResponsesReplayState>> {
    messages[compaction_boundary(messages.iter())..]
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(assistant) => {
                responses_from_provider_state(assistant.provider_state.as_ref())
            }
            _ => None,
        })
}

/// Whether the latest checkpoint explicitly dropped Responses raw items.
pub(super) fn responses_compacted(messages: &[Message]) -> bool {
    messages
        .get(compaction_boundary(messages.iter()))
        .and_then(|message| match message {
            Message::Compaction(CompactionMessage {
                compaction: Compaction::Completed(completed),
                ..
            }) => completed.provider_state.as_ref(),
            _ => None,
        })
        .is_some_and(|state| state.contains_key(super::super::compaction::RESPONSES_COMPACTED_KEY))
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
///
/// The historian's prefix reuse (`mctx::historian::prefixed_request`) depends on
/// this function staying the single renderer for both paths: the hidden
/// completion's request must start with these exact bytes, so any change here
/// moves the live request and the fold together or not at all.
pub(crate) fn build_chat_messages(
    system_static: &str,
    instructions: &str,
    head: &[Option<&str>],
    messages: &[Message],
) -> Vec<ChatMessage> {
    let boundary = compaction_boundary(messages.iter());
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
                // Native state is proof from this exact turn only. A failed or
                // interrupted row keeps the old reasoning-as-text fallback.
                let replay_state = replay_reasoning
                    .then(|| from_provider_state(assistant.provider_state.as_ref()))
                    .flatten();
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
                        // per-provider gate; the endpoint owns dialects.
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
                if parts.is_empty() && tool_calls.is_empty() && replay_state.is_none() {
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
                    replay_state,
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
            // Every ox shell row is a background user shell, and upstream
            // skips background shells; the result reaches the model as the
            // `source: "shell"` synthetic instead.
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

/// The `{role: "tool"}` result for a tool part (upstream `toolResult`):
/// joined text content for completed calls, `{error: …}` for failed ones.
/// A `streaming`/`running` part is one that never settled — the process died
/// or the row was cut off before `tool.failed` could land — and it renders
/// as the fixed [`UNSETTLED_TOOL_RESULT`]: a call with no result is a request
/// strict providers reject outright, and the deterministic text keeps the
/// rendered bytes (and so the provider cache) stable across requests.
/// Completed text is bounded for the wire ([`bound_tool_output`]): the stored
/// row keeps the full text, only this request's view is head + marker + tail.
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
        ToolState::Streaming { .. } | ToolState::Running { .. } => UNSETTLED_TOOL_RESULT.to_owned(),
    };
    Some((
        ChatMessage::Tool {
            tool_call_id: id.to_owned(),
            content,
        },
        media,
    ))
}

/// The result a never-settled tool part renders as: the `{"error": …}`
/// envelope of an `aborted` error, fixed text so it is the same bytes on
/// every request (codex renders the same synthetic `aborted` output).
const UNSETTLED_TOOL_RESULT: &str =
    r#"{"error":{"type":"aborted","message":"Tool call did not settle before the turn ended"}}"#;

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

#[cfg(test)]
mod messages_replay_tests {
    use ox_core::projector;
    use ox_db::Db;
    use ox_llm::chat_completions::stream::StreamEvent;
    use ox_llm::chat_completions::wire::{ChatMessage, ChatRequest};
    use ox_llm::messages::replay::{MessagesOrigin, MessagesReplayState, ReplayBlock};
    use ox_llm::messages::stream::MessagesDecoder;
    use ox_llm::messages::wire::{MessagesPolicy, WireBlock, build_request};
    use ox_llm::sse::SseDecoder;
    use ox_schema::event::{DurableEnvelope, EventEnvelope, SessionEventData};
    use ox_schema::message::{AssistantContent, Message, ToolState};
    use ox_schema::support::{
        Content as SchemaContent, FinishReason, LocationRef, ModelRef, MoneyUsd, Record,
        TokenCache, TokenUsage,
    };
    use serde_json::{Value, json};

    use super::build_chat_messages;

    const PROVIDER: &str = "anthropic-native";
    const ENDPOINT: &str = "https://messages.example/v1/messages";
    const MODEL: &str = "claude-test";
    const SESSION: &str = "session-replay";

    fn replay_state() -> MessagesReplayState {
        MessagesReplayState::new(
            PROVIDER,
            ENDPOINT,
            MODEL,
            vec![
                ReplayBlock::Thinking {
                    index: 0,
                    thinking: "first thought".to_owned(),
                    signature: "sig-alpha".to_owned(),
                },
                ReplayBlock::RedactedThinking {
                    index: 1,
                    data: "opaque-tail".to_owned(),
                },
            ],
            Some(2),
            Some(3),
        )
    }

    fn row_with_replay(replay: MessagesReplayState) -> Message {
        Message::Assistant(Box::new(ox_schema::message::AssistantMessage {
            id: "assistant-1".to_owned(),
            metadata: None,
            time: ox_schema::message::AssistantTime {
                created: 1,
                streamed: None,
                completed: Some(2),
            },
            agent: "apex".to_owned(),
            model: ModelRef {
                id: MODEL.to_owned(),
                provider_id: PROVIDER.to_owned(),
                variant: None,
            },
            content: vec![
                AssistantContent::Reasoning {
                    text: "first thought".to_owned(),
                    state: None,
                    time: None,
                },
                AssistantContent::Text {
                    text: "answer".to_owned(),
                    state: None,
                },
            ],
            snapshot: None,
            finish: None,
            raw_finish: None,
            provider_state: Some(replay.into_provider_state().expect("state serializes")),
            cost: None,
            tokens: None,
            error: None,
            retry: None,
        }))
    }

    #[test]
    fn replay_row_keeps_reasoning_text_when_the_current_route_cannot_replay() {
        let messages = build_chat_messages("", "", &[], &[row_with_replay(replay_state())]);
        let ChatMessage::Assistant {
            reasoning_content,
            replay_state,
            ..
        } = &messages[1]
        else {
            panic!("assistant row must render as an assistant message");
        };
        assert!(
            replay_state.is_some(),
            "the state stays attached for the endpoint that can use it"
        );
        assert_eq!(
            reasoning_content.as_deref(),
            Some("first thought"),
            "a route that cannot replay native blocks must keep the old reasoning text"
        );
    }

    fn envelope(seq: i64, kind: &str, data: SessionEventData) -> EventEnvelope<SessionEventData> {
        EventEnvelope {
            id: format!("event-{seq}"),
            created: seq,
            kind: kind.to_owned(),
            data,
            durable: Some(DurableEnvelope {
                aggregate_id: SESSION.to_owned(),
                seq,
                version: 1,
            }),
            location: None,
            metadata: None,
        }
    }

    async fn project(db: &Db, seq: i64, kind: &str, data: SessionEventData) {
        let event = envelope(seq, kind, data);
        let tx = db.begin().await.expect("write transaction");
        projector::project(&tx, &event)
            .await
            .expect("durable event projects");
        tx.commit().await.expect("commit projection");
    }

    async fn reread(db: &Db) -> Vec<Message> {
        let conn = db.acquire().await.expect("read connection");
        let mut stmt = conn
            .prepare(
                "SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq",
            )
            .await
            .expect("session message query");
        let mut rows = stmt.query([SESSION]).await.expect("rows");
        let mut messages = Vec::new();
        while let Some(row) = rows.next().await.expect("next row") {
            let id = row.get::<String>(0).expect("id");
            let kind = row.get::<String>(1).expect("kind");
            let data = row.get::<String>(2).expect("data");
            let mut value: Value = serde_json::from_str(&data).expect("row JSON");
            Message::merge_row_columns(&id, &kind, &mut value);
            messages.push(serde_json::from_value(value).expect("message"));
        }
        messages
    }

    fn first_turn_events() -> Vec<StreamEvent> {
        let mut decoder = MessagesDecoder::new().with_replay(PROVIDER, ENDPOINT, MODEL);
        let mut events = Vec::new();
        for line in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"first thought"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"alpha"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"redacted_thinking","data":"-tail"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"answer"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call-1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":11}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            decoder.decode(line, &mut events).expect("first-turn SSE");
        }
        events
    }

    #[tokio::test]
    async fn two_round_messages_replay_survives_durable_rows() {
        let events = first_turn_events();
        let provider_state = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ProviderState(state) => Some(state.clone()),
                _ => None,
            })
            .expect("completed thinking blocks produce a provider state");
        let mut reasoning = String::new();
        let mut text = String::new();
        let mut arguments = String::new();
        let mut tool_id = None;
        let mut tool_name = None;
        for event in &events {
            match event {
                StreamEvent::ReasoningDelta(delta) => reasoning.push_str(delta),
                StreamEvent::TextDelta(delta) => text.push_str(delta),
                StreamEvent::ToolCallDelta {
                    id,
                    name,
                    arguments: fragment,
                    ..
                } => {
                    if let Some(id) = id {
                        tool_id = Some(id.clone());
                    }
                    if let Some(name) = name {
                        tool_name = Some(name.clone());
                    }
                    arguments.push_str(fragment);
                }
                _ => {}
            }
        }
        assert_eq!(reasoning, "first thought");
        assert_eq!(text, "answer");

        let db = Db::open_in_memory().await.expect("ephemeral database");
        project(
            &db,
            1,
            "session.created",
            SessionEventData::SessionCreated {
                session_id: SESSION.to_owned(),
                project_id: "project-1".to_owned(),
                location: LocationRef {
                    directory: "/work".to_owned(),
                    workspace_id: None,
                },
                subpath: None,
                parent_id: None,
                slug: "replay".to_owned(),
                title: None,
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                version: "test".to_owned(),
            },
        )
        .await;
        project(
            &db,
            2,
            "session.step.started",
            SessionEventData::StepStarted {
                session_id: SESSION.to_owned(),
                assistant_message_id: "assistant-1".to_owned(),
                agent: "apex".to_owned(),
                model: ModelRef {
                    id: MODEL.to_owned(),
                    provider_id: PROVIDER.to_owned(),
                    variant: None,
                },
                snapshot: None,
                started: 1,
            },
        )
        .await;
        let tool_id = tool_id.expect("tool id from SSE");
        let tool_name = tool_name.expect("tool name from SSE");
        let tool_input: Record =
            serde_json::from_value(serde_json::from_str(&arguments).expect("tool arguments"))
                .expect("tool input record");
        project(
            &db,
            3,
            "session.message.content.updated",
            SessionEventData::SessionMessageContentUpdated {
                session_id: SESSION.to_owned(),
                message_id: "assistant-1".to_owned(),
                content: vec![
                    AssistantContent::Reasoning {
                        text: reasoning,
                        state: None,
                        time: None,
                    },
                    AssistantContent::Text { text, state: None },
                    AssistantContent::Tool {
                        id: tool_id.clone(),
                        name: tool_name.clone(),
                        executed: None,
                        provider_state: None,
                        provider_result_state: None,
                        time: ox_schema::message::ToolTime {
                            created: 1,
                            ran: Some(2),
                            completed: Some(3),
                        },
                        state: Box::new(ToolState::Completed {
                            input: tool_input,
                            content: vec![SchemaContent::Text {
                                text: "done".to_owned(),
                            }],
                            metadata: None,
                        }),
                    },
                ],
            },
        )
        .await;
        project(
            &db,
            4,
            "session.step.ended",
            SessionEventData::StepEnded {
                session_id: SESSION.to_owned(),
                assistant_message_id: "assistant-1".to_owned(),
                finish: FinishReason::ToolCalls,
                raw_finish: None,
                provider_state: Some(provider_state),
                cost: MoneyUsd::Int(0),
                tokens: TokenUsage {
                    input: 9,
                    output: 11,
                    reasoning: 3,
                    cache: TokenCache { read: 0, write: 0 },
                    estimated: None,
                    reasoning_reported: None,
                },
                snapshot: None,
                files: None,
            },
        )
        .await;

        let rows = reread(&db).await;
        assert!(
            rows.iter().any(|row| matches!(row, Message::Assistant(_))),
            "session reread sees the assistant row"
        );
        let mut chat = build_chat_messages("", "", &[], &rows);
        chat.push(ChatMessage::user("continue"));
        let request = ChatRequest::new(MODEL, chat);
        let body = build_request(
            &request,
            &MessagesPolicy::anthropic(),
            &MessagesOrigin::new(PROVIDER, ENDPOINT),
        )
        .expect("second native Messages request");
        let assistant = body
            .messages
            .iter()
            .find(|message| message.role == "assistant")
            .expect("replayed assistant message");
        assert_eq!(
            assistant.content,
            vec![
                WireBlock::Thinking {
                    thinking: "first thought".to_owned(),
                    signature: Some("sig-alpha".to_owned()),
                },
                WireBlock::RedactedThinking {
                    data: "opaque-tail".to_owned(),
                },
                WireBlock::Text {
                    text: "answer".to_owned(),
                },
                WireBlock::ToolUse {
                    id: tool_id.clone(),
                    name: tool_name,
                    input: json!({"path": "x"}),
                },
            ],
            "second turn carries the original signed/redacted blocks in order"
        );
        let result = body
            .messages
            .iter()
            .find(|message| message.role == "user")
            .expect("tool result turn");
        assert!(
            result.content.iter().any(|block| matches!(
                block,
                WireBlock::ToolResult { tool_use_id, content, .. }
                    if tool_use_id == &tool_id && content.contains("done")
            )),
            "tool result matches the replayed tool_use"
        );
    }
}
