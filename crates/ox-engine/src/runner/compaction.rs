//! Compaction, ported from `packages/core/src/session/compaction.ts`
//! (faithful-but-minimal: the summarization flow only; auto thresholds,
//! budget shrinking and native provider windows are out of scope).
//!
//! Upstream compacts through a dedicated agent and a budgeted request
//! pipeline; collapsed to one flow, in this exact wire order:
//!
//! 1. `session.compaction.started` → the projector appends a `running`
//!    compaction row (also the background-fold in-flight marker).
//! 2. The session transcript (from the latest completed compaction onward)
//!    becomes a tools-free chat request: a fixed summarization system
//!    prompt, the transcript as user/assistant messages, then the summary
//!    instruction as the final user turn (upstream appends the same prompt
//!    user message in `summarize`). The reply streams as ephemeral
//!    `session.compaction.delta` events.
//! 3. `session.compaction.ended {text}` settles the running row to
//!    `completed` with the summary (on failure `session.compaction.failed`
//!    settles it `failed`).
//!
//! The background fold skips this lifecycle entirely (plan D4): no
//! `started`, no deltas, no failure event — its checkpoint lands as a quiet
//! `idle` row through `publish_quiet`, invisible to live subscribers.
//!
//! Afterwards history selection starts at the completed row (upstream
//! `SessionHistory` boundary), so later requests open with the checkpoint
//! user message instead of the compacted transcript.

use crate::cardinal::stream::StreamEvent;
use crate::cardinal::wire::{ChatMessage, ChatRequest};
use anyhow::Result;
use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::message::{CompactionReason, Message};
use ox_schema::support::{ModelRef, SessionError};
use std::sync::Arc;

use super::drain::DrainCtx;
use super::historian_trigger::prompt_tokens;
use super::resolve_selection;
use super::unknown_error;

/// The compaction request's system prompt. Upstream routes the summarization
/// through a dedicated `compaction` agent whose system prompt is exactly
/// this scoping: summarize, don't continue.
pub(crate) const COMPACTION_SYSTEM: &str = "You summarize conversations so another agent can resume the work. Summarize only; do not continue the task.";

/// Upstream `buildPrompt(false)` (compaction.ts:145) verbatim: the
/// first-time summary instruction appended as the final user message.
pub(crate) const SUMMARY_PROMPT: &str = "You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.\n\nSummarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.\n\nYou MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.\n<template>\n## Objective\n- [one or two brief sentences describing what the user is trying to accomplish]\n\n## Requirements\n- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]\n\n## Decisions\n- [decisions already made and why, or \"(none)\"]\n\n## Work State\nBreak the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.\n### Completed\n- [goals that have been completed; otherwise \"(none)\"]\n\n### Active\n- [goals currently being worked on; otherwise \"(none)\"]\n\n### Blocked\n- [anything blocking progress, and why; otherwise \"(none)\"]\n\n## Next Move\n1. [ordered list of next actions, or \"(none)\"]\n\n## Relevant Files\nList the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write \"(none)\".\n- `[file or directory path]`: [brief reason it matters]\n\n## Important Context\n- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]\n</template>\n\nRules:\n- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.\n- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.\n- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.\n- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.\n- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.\n- Do not mention the summary process or that context was compacted.\n\nDo not continue the task or call tools.\n\nReturn only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.";
/// Upstream `buildPrompt(true)` (compaction.ts:214-230, shared 201-207)
/// verbatim: the checkpoint-update instruction used when the transcript
/// already contains a `<conversation-checkpoint>` — the previous summary is
/// the baseline to update, and newer history always takes precedence.
pub(crate) const UPDATE_PROMPT: &str = "Update the existing checkpoint in the conversation above into one consolidated summary.\n\nNewer history always takes precedence over the existing checkpoint. Preserve previous information unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is no longer relevant to continuing the work, you may remove it.\n\nIncorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next Move: move completed work out of Active, remove resolved blockers and answered questions, and preserve unresolved or pending work.\n\nReturn only the updated Markdown sections. Do not reproduce the `<conversation-checkpoint>`, `<summary>`, or `<recent-context>` wrapper tags from the previous checkpoint.\n\nSummarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.\n\nYou MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.\n<template>\n## Objective\n- [one or two brief sentences describing what the user is trying to accomplish]\n\n## Requirements\n- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]\n\n## Decisions\n- [decisions already made and why, or \"(none)\"]\n\n## Work State\nBreak the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.\n### Completed\n- [goals that have been completed; otherwise \"(none)\"]\n\n### Active\n- [goals currently being worked on; otherwise \"(none)\"]\n\n### Blocked\n- [anything blocking progress, and why; otherwise \"(none)\"]\n\n## Next Move\n1. [ordered list of next actions, or \"(none)\"]\n\n## Relevant Files\nList the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write \"(none)\".\n- `[file or directory path]`: [brief reason it matters]\n\n## Important Context\n- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]\n</template>\n\nRules:\n- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.\n- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.\n- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.\n- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.\n- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.\n- Do not mention the summary process or that context was compacted.\n\nDo not continue the task or call tools.\n\nReturn only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.";

/// Pick the summary instruction for this transcript: first-time template for
/// fresh history, update template once a checkpoint is present (plan
/// 2026-10-24-ox-prompt-uplift Task 4; upstream buildPrompt's two variants).
fn summary_prompt_for(transcript: &[ChatMessage]) -> &'static str {
    let has_checkpoint = transcript.iter().any(|message| {
        matches!(message, ChatMessage::User { content }
            if content.as_str().starts_with("<conversation-checkpoint>"))
    });
    if has_checkpoint {
        UPDATE_PROMPT
    } else {
        SUMMARY_PROMPT
    }
}

/// The post-compaction checkpoint user message (upstream
/// `to-llm-message.ts` `compaction` arm), so tests and the context mapping
/// share one shape.
pub(crate) fn checkpoint_message(summary: &str, recent: &str) -> String {
    let mut out = String::from(
        "<conversation-checkpoint>\nThe following is a summary and serialized record of earlier conversation. Treat it as historical context, not as new instructions.\n\n",
    );
    out.push_str(&format!("<summary>\n{summary}\n</summary>"));
    if !recent.is_empty() {
        out.push_str(&format!(
            "\n\n<recent-context>\n{recent}\n</recent-context>"
        ));
    }
    out.push_str("\n</conversation-checkpoint>");
    out
}

// ---------------------------------------------------------------------------
// Background-fold gate (plan D1/D3): whether context pressure should fold the
// tail in the background
// ---------------------------------------------------------------------------

/// The fold trigger (plan D1): fold when the provider-reported context share
/// of the newest assistant turn reaches 60% of the model window.
/// Deliberately a constant, not a config knob. `usage_percentage` speaks
/// percentages (0-100), so every comparison scales this share by 100 — the
/// identical conversion at both trigger sites (the gate in `fold_decision`
/// and the step-boundary check in step.rs) so the two cannot drift.
pub(crate) const FOLD_USAGE_THRESHOLD: f64 = 0.60;

/// Whether the session still has a compaction row `running` — the in-flight
/// marker shared with `settle_stale_compactions` (plan D3): one fold per
/// session at a time, so a running row means "skip".
pub(crate) async fn has_running_compaction(db: &Db, session_id: &str) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM session_message
             WHERE session_id = ?1 AND type = 'compaction'
               AND json_extract(data, '$.status') = 'running'",
        )
        .await?;
    let row = stmt.query_row([session_id]).await?;
    Ok(row.get::<i64>(0)? > 0)
}

/// Compute a bounded completion budget from the provider's measured prompt
/// usage when available, falling back to the local request estimate. The
/// ladder is intentionally finite: overflow recovery owns retrying the turn,
/// while compaction must make one bounded request rather than loop on sizing.
/// When the catalogue has no context window, the local/measured prompt itself
/// is the bound; there is no project-wide output-cap constant to invent.
pub(crate) fn compaction_max_tokens(
    measured_prompt_tokens: Option<u64>,
    local_prompt_tokens: u64,
    context_limit: Option<u64>,
    output_cap: Option<u64>,
    shrink_step: usize,
) -> u64 {
    const SHRINK_LADDER: [u64; 4] = [85, 70, 50, 35];
    let prompt = measured_prompt_tokens.unwrap_or(local_prompt_tokens);
    let available = context_limit
        .map(|limit| limit.saturating_sub(prompt))
        .unwrap_or(prompt)
        .max(1);
    let shrink = SHRINK_LADDER[shrink_step.min(SHRINK_LADDER.len() - 1)];
    let bounded = available.saturating_mul(shrink) / 100;
    bounded
        .max(1)
        .min(output_cap.filter(|cap| *cap > 0).unwrap_or(u64::MAX))
}

fn model_limits(ctx: &DrainCtx, model: &ModelRef) -> (Option<u64>, Option<u64>) {
    let entries = ctx.model_catalog.cached();
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.id == model.id && entry.provider == model.provider_id)
    else {
        return (None, None);
    };
    (
        ctx.model_catalog.window_of(&entry.id).map(u64::from),
        entry.output.and_then(|value| u64::try_from(value).ok()),
    )
}

fn measured_prompt_tokens(messages: &[Message]) -> Option<u64> {
    messages.iter().rev().find_map(|message| {
        let Message::Assistant(assistant) = message else {
            return None;
        };
        let tokens = assistant.tokens.as_ref()?;
        u64::try_from(tokens.input.saturating_add(tokens.cache.read)).ok()
    })
}

fn apply_compaction_budget(
    ctx: &DrainCtx,
    model: &ModelRef,
    request: &mut ChatRequest,
    measured_prompt: Option<u64>,
) {
    let local_prompt = crate::mctx::tokens::heuristic_bytes(
        crate::cardinal::wire::serialized_len(request).unwrap_or_default(),
    ) as u64;
    let (context_limit, output_cap) = model_limits(ctx, model);
    request.max_tokens = Some(compaction_max_tokens(
        measured_prompt,
        local_prompt,
        context_limit,
        output_cap,
        0,
    ));
}

/// Build the summarization request: system prompt + transcript (no working
/// system prompt, no tools) + the summary instruction as the last user
/// turn. Upstream `summarize`: `prepared.request` + `Message.user(prompt)`.
pub(crate) fn summarization_request(model: &ModelRef, transcript: &[ChatMessage]) -> ChatRequest {
    let mut messages = Vec::with_capacity(transcript.len() + 2);
    messages.push(ChatMessage::System {
        content: COMPACTION_SYSTEM.to_owned(),
    });
    // The transcript carries its own system prompt first; the compaction
    // request replaces it with the summarization system prompt above.
    let body: Vec<ChatMessage> = transcript
        .iter()
        .filter(|message| !matches!(message, ChatMessage::System { .. }))
        .cloned()
        .collect();
    messages.extend(body);
    messages.push(ChatMessage::User {
        content: summary_prompt_for(transcript).to_owned().into(),
    });
    // `tools` stays empty → omitted from the wire; upstream disables tools
    // for compaction (`input.tools` defaults to a rejecting definition set).
    let mut request = ChatRequest::new(model.id.clone(), messages);
    request.apply_reasoning_effort(model.variant.as_deref());
    request
}

/// Upstream `NUDGE` (compaction.ts): the one re-ask sent when the reply
/// skips the summary template.
const NUDGE: &str = "The previous response did not fill in the required summary template. Do not call tools. Return the summary as text using the exact section headings from the template.";

fn append_nudge(request: &mut ChatRequest) {
    request.messages.push(ChatMessage::User {
        content: NUDGE.to_owned().into(),
    });
}

/// Upstream `filled`: the reply mentions at least one template heading on a
/// line of its own. The headings come from the template embedded in the
/// summary prompt (every `##`/`###` line).
fn filled(summary: &str) -> bool {
    let headings: Vec<&str> = SUMMARY_PROMPT
        .lines()
        .filter(|line| line.starts_with("##"))
        .collect();
    summary.lines().any(|line| headings.contains(&line.trim()))
}

/// Stream one summarization request to completion, publishing
/// `compaction.delta` per fragment when `emit_deltas` (upstream `stream`).
/// The background fold passes `false`: `compaction.delta` is ephemeral —
/// broadcasting is its only existence — so its silence means not publishing
/// the deltas at all (plan D4: no compaction frame may reach the wire).
async fn stream_summary(
    ctx: &DrainCtx,
    request: &ChatRequest,
    emit_deltas: bool,
) -> std::result::Result<String, SessionError> {
    let failure = |kind: &str, message: String| SessionError {
        kind: kind.to_owned(),
        message,
        status: None,
        response: None,
    };
    let mut stream = match ctx.chat.stream_chat(request).await {
        Ok(stream) => stream,
        Err(error) => {
            let kind = if super::provider_error::is_overflow_error(&error) {
                "compaction.overflow"
            } else {
                "compaction.failed"
            };
            return Err(failure(kind, format!("compaction request failed: {error}")));
        }
    };
    let mut summary = String::new();
    loop {
        let next = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await;
        match next {
            Some(Ok(StreamEvent::TextDelta(delta))) => {
                summary.push_str(&delta);
                if emit_deltas
                    && let Err(error) = ctx
                        .bus
                        .publish(SessionEventData::CompactionDelta {
                            session_id: ctx.session_id.clone(),
                            text: delta,
                        })
                        .await
                {
                    return Err(failure("unknown", error.to_string()));
                }
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                let kind = if super::provider_error::is_overflow_error(&error) {
                    "compaction.overflow"
                } else {
                    "compaction.failed"
                };
                return Err(failure(kind, format!("compaction stream failed: {error}")));
            }
            None => break,
        }
    }
    Ok(summary)
}

/// Run one compaction to completion: resolve the model, publish
/// `compaction.started` (the projector appends the `running` row — also the
/// in-flight marker for background folds), stream the summarization request, then
/// settle the row through `publish_outcome`. Returns the summary on success;
/// the failure error after publishing `compaction.failed` otherwise.
pub(crate) async fn compact(
    ctx: &DrainCtx,
    reason: CompactionReason,
    input_id: Option<String>,
) -> std::result::Result<String, SessionError> {
    let (_, model) = resolve_selection(&ctx.db, &ctx.session_id, &ctx.config, &ctx.model_catalog)
        .await
        .map_err(|error| unknown_error(error.to_string()))?;

    publish_started(ctx, reason, input_id.clone()).await?;
    let run = summarize(ctx, &model).await;
    publish_outcome(ctx, reason, input_id, model, run, String::new()).await
}

/// The read side of one compaction (upstream `summarize`): load the
/// transcript, build the summarization request and stream it to completion,
/// re-asking once when the reply skipped the summary template. Pure
/// orchestration over the read side — no lifecycle event, no row writes;
/// the reply still streams as ephemeral `compaction.delta` events.
pub(crate) async fn summarize(
    ctx: &DrainCtx,
    model: &ModelRef,
) -> std::result::Result<String, SessionError> {
    let rows = super::step::load_history(&ctx.db, &ctx.session_id)
        .await
        .map_err(|error| SessionError {
            kind: "unknown".to_owned(),
            message: error.to_string(),
            status: None,
            response: None,
        })?;
    summarize_transcript(ctx, model, &rows, true).await
}

/// The hidden-completion core every summarization shares (upstream
/// `summarize`'s reply handling): stream the request, re-ask once when the
/// reply skipped the summary template, then fail with the upstream
/// classification when it still did not fill.
async fn summarized_reply(
    ctx: &DrainCtx,
    mut request: ChatRequest,
    emit_deltas: bool,
) -> std::result::Result<String, SessionError> {
    let failure = |kind: &str, message: String| SessionError {
        kind: kind.to_owned(),
        message,
        status: None,
        response: None,
    };
    let base_max_tokens = request.max_tokens;
    let mut shrink_step = 0;
    let mut summary = loop {
        match stream_summary(ctx, &request, emit_deltas).await {
            Ok(summary) => break summary,
            Err(error)
                if error.kind == "compaction.overflow"
                    && shrink_step < 4
                    && request.max_tokens.is_some() =>
            {
                shrink_step += 1;
                request.max_tokens = Some(compaction_max_tokens(
                    base_max_tokens,
                    base_max_tokens.unwrap_or(1),
                    None,
                    None,
                    shrink_step,
                ));
            }
            Err(error) => return Err(error),
        }
    };
    // Upstream `send` (compaction.ts): a reply that ignores the summary
    // template gets one nudge before it counts as a failure. The nudge is a
    // content-only re-ask: preserve the already-derived positive `max_tokens`
    // instead of recalculating the budget for the extra user turn.
    if !filled(&summary) {
        append_nudge(&mut request);
        summary = loop {
            match stream_summary(ctx, &request, emit_deltas).await {
                Ok(summary) => break summary,
                Err(error)
                    if error.kind == "compaction.overflow"
                        && shrink_step < 4
                        && request.max_tokens.is_some() =>
                {
                    shrink_step += 1;
                    request.max_tokens = Some(compaction_max_tokens(
                        base_max_tokens,
                        base_max_tokens.unwrap_or(1),
                        None,
                        None,
                        shrink_step,
                    ));
                }
                Err(error) => return Err(error),
            }
        };
        if !filled(&summary) {
            return Err(failure(
                "compaction.failed",
                if summary.trim().is_empty() {
                    "Compaction produced no summary"
                } else {
                    "Compaction summary did not match the required template"
                }
                .to_owned(),
            ));
        }
    }
    Ok(summary)
}

/// The summarization proper over a given transcript: build the request,
/// stream it, re-ask once when the reply skipped the template. Split from
/// [`summarize`] so the background fold can feed its snapshot (plan D4).
/// `emit_deltas` is the blocking path's wire contract (`started → delta →
/// ended`); the fold passes `false` — its deltas must not broadcast.
async fn summarize_transcript(
    ctx: &DrainCtx,
    model: &ModelRef,
    rows: &[ox_schema::message::Message],
    emit_deltas: bool,
) -> std::result::Result<String, SessionError> {
    let messages = super::step::build_chat_messages("", "", &[], rows);
    let measured_prompt = measured_prompt_tokens(rows);
    let mut request = summarization_request(model, &messages);
    apply_compaction_budget(ctx, model, &mut request, measured_prompt);
    summarized_reply(ctx, request, emit_deltas).await
}

/// The global pass's input: every segment's summary under one `[seq a..b]`
/// head, blank-line separated. The consolidation completion is the only
/// reader, and it must see all of them to write one summary of the whole
/// range — the segments themselves never reach the checkpoint. Publishing them
/// made the checkpoint grow with the session: a hierarchical fold over the
/// whole session appended every segment's summary to the summary it published,
/// which is the head of every later request, which raised the usage reading
/// that opened the next fold. Measured on a real session, that took checkpoints
/// from 23 KB / 2 segments to 267 KB / 33 segments in one day, 97% of it
/// segment text (179 KB summary, global part 6–8 KB) — ~57k tokens of a 262k
/// window resident in the checkpoint alone. The model reads the global summary
/// and, for the newest rows, the verbatim `recent` tail; anything older is
/// consolidated, not replayed.
fn segment_details(segments: &[(i64, i64, String)]) -> String {
    segments
        .iter()
        .map(|(start, end, summary)| format!("[seq {start}..{end}]\n{summary}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The template fallback's summarization, hierarchical when the snapshot
/// needs more than one chunk (plan W3): every [`chunk_rows`] segment is
/// summarized by the same hidden completion the flat path pays
/// ([`summarize_transcript`]), then one final completion consolidates the
/// segment summaries into the checkpoint under the verbatim [`SUMMARY_PROMPT`].
/// Only that consolidation is published — see [`segment_details`] for why the
/// segments stay internal. Atomic: any segment failing fails the whole
/// summarization, so no partial text ever reaches a checkpoint. A single chunk
/// is the flat path verbatim — one completion, no extra call. Silent like the
/// fold it serves: `emit_deltas` stays `false`.
async fn summarize_hierarchical(
    ctx: &DrainCtx,
    model: &ModelRef,
    rows: &[(i64, serde_json::Value)],
    count_tokens: &(impl Fn(&str) -> usize + Sync),
) -> std::result::Result<String, SessionError> {
    let failure = |message: String| SessionError {
        kind: "unknown".to_owned(),
        message,
        status: None,
        response: None,
    };
    let decode = |chunk: &[(i64, serde_json::Value)]| {
        chunk
            .iter()
            .map(|(_, value)| super::step::decode_row(value))
            .collect::<Result<Vec<_>>>()
            .map_err(|error| failure(error.to_string()))
    };
    let chunks = chunk_rows(rows, count_tokens);
    if chunks.len() <= 1 {
        let messages = decode(chunks.first().copied().unwrap_or(&[]))?;
        return summarize_transcript(ctx, model, &messages, false).await;
    }
    let mut segments: Vec<(i64, i64, String)> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let messages = decode(chunk)?;
        let summary = summarize_transcript(ctx, model, &messages, false).await?;
        let start = chunk.first().map_or(0, |(seq, _)| *seq);
        let end = chunk.last().map_or(0, |(seq, _)| *seq);
        segments.push((start, end, summary));
    }
    let input = segment_details(&segments);
    let segment_tokens = count_tokens(&input);
    // The global pass: the segment summaries are the transcript; the summary
    // instruction is the same verbatim SUMMARY_PROMPT the flat path asks.
    let mut request = summarization_request(
        model,
        &[ChatMessage::User {
            content: input.into(),
        }],
    );
    apply_compaction_budget(ctx, model, &mut request, None);
    let global = summarized_reply(ctx, request, false).await?;
    // The consolidation is the checkpoint. The segment count is the one number
    // that says whether the fold's input range was right: an incremental fold
    // sees one or two segments, and a rising count means it is re-reading rows
    // an earlier fold already folded.
    tracing::debug!(
        session = %ctx.session_id,
        segments = segments.len(),
        segment_tokens,
        summary_tokens = count_tokens(&global),
        "hierarchical fold consolidated"
    );
    Ok(global)
}

/// The lifecycle head of one compaction: `session.compaction.started` makes
/// the projector append the `running` row. Published BEFORE the read side so
/// the wire order stays `started → delta… → ended` (main's contract, and the
/// in-flight marker a concurrent background fold gates on).
pub(crate) async fn publish_started(
    ctx: &DrainCtx,
    reason: CompactionReason,
    input_id: Option<String>,
) -> std::result::Result<(), SessionError> {
    ctx.bus
        .publish(SessionEventData::CompactionStarted {
            session_id: ctx.session_id.clone(),
            reason,
            // Upstream sends the newest kept transcript excerpt as `recent`;
            // the minimal port keeps none (the checkpoint message omits the
            // recent-context block when empty).
            recent: String::new(),
            input_id,
        })
        .await
        .map_err(|error| unknown_error(error.to_string()))?;
    Ok(())
}

/// The lifecycle tail of one compaction: the summarized outcome settles the
/// running row — `session.compaction.ended {text}` flips it to `completed`,
/// `session.compaction.failed` to `failed`. Returns the outcome unchanged
/// once the row settled.
pub(crate) async fn publish_outcome(
    ctx: &DrainCtx,
    reason: CompactionReason,
    input_id: Option<String>,
    model: ModelRef,
    run: std::result::Result<String, SessionError>,
    recent: String,
) -> std::result::Result<String, SessionError> {
    match run {
        Ok(summary) => {
            ctx.bus
                .publish(SessionEventData::CompactionEnded {
                    session_id: ctx.session_id.clone(),
                    reason,
                    model: Some(model),
                    provider_state: None,
                    provider_context: None,
                    text: summary.clone(),
                    recent,
                    cost: None,
                    tokens: None,
                })
                .await
                .map_err(|publish| unknown_error(publish.to_string()))?;
            Ok(summary)
        }
        Err(error) => {
            ctx.bus
                .publish(SessionEventData::CompactionFailed {
                    session_id: ctx.session_id.clone(),
                    reason,
                    error: error.clone(),
                    input_id,
                    cost: None,
                    tokens: None,
                })
                .await
                .map_err(|publish| unknown_error(publish.to_string()))?;
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Background fold (plan D1-D7): fold the tail silently wherever the usage
// gate passes, instead of blocking a turn on the overflow path
// ---------------------------------------------------------------------------

/// Upstream's verbatim summary-keep budget (`settings.keep`, compaction.ts:255):
/// the newest tail kept verbatim beside the summary, measured in tokens by
/// the session model family's counter (the bytes/4 estimate was the stand-in
/// the heuristic fallback still renders as). Deliberately a constant, not a
/// config knob.
const RECENT_KEEP_TOKENS: i64 = 15_000;

/// Upstream `TOOL_OUTPUT_MAX_CHARS` (recent-text.ts): one tool result's
/// verbatim cap inside the recent part.
const RECENT_TOOL_OUTPUT_MAX_CHARS: usize = 1_250;

/// Whether the busy period's end should fold the tail, as a pure decision
/// over the gates (plan D1/D3/D6). Gate order is the contract: a running
/// row wins first (someone else is compacting — D3), then mctx coverage
/// (the historian already folded everything past the checkpoint — D6),
/// then the usage threshold (D1).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FoldDecision {
    Fold,
    SkipInFlight,
    SkipCovered,
    SkipUnderThreshold,
}

pub(crate) fn fold_decision(
    mctx_enabled: bool,
    floor: i64,
    latest: i64,
    running: bool,
    usage_pct: Option<f64>,
) -> FoldDecision {
    if running {
        return FoldDecision::SkipInFlight;
    }
    // The floor is the transcript ordinal the mctx head stands in for; when
    // it already covers every row past the checkpoint, a fold would pay a
    // hidden completion to re-summarize compartments. `floor == 0` with mctx
    // on means the head is off/failed — fail open to folding (plan §9).
    if mctx_enabled && floor > 0 && floor >= latest {
        return FoldDecision::SkipCovered;
    }
    // `usage_pct` is a percentage of the window (0-100, `usage_percentage`);
    // the threshold is a share (0.60), so the comparison scales it by 100.
    if usage_pct.is_none_or(|pct| pct < FOLD_USAGE_THRESHOLD * 100.0) {
        return FoldDecision::SkipUnderThreshold;
    }
    FoldDecision::Fold
}

/// The newest transcript ordinal, or the negative sentinel of an empty
/// session.
async fn latest_seq(db: &Db, session_id: &str) -> Result<i64> {
    let conn = db.acquire().await?;
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(seq), -1) FROM session_message WHERE session_id = ?1",
            [session_id],
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no row"))?;
    Ok(row.get::<i64>(0)?)
}

/// The ordinal of the newest settled checkpoint — the row a later compaction
/// resumes from — or the negative sentinel when none exists.
async fn last_checkpoint_seq(db: &Db, session_id: &str) -> Result<i64> {
    let conn = db.acquire().await?;
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(seq), -1) FROM session_message
             WHERE session_id = ?1 AND type = 'compaction'
               AND json_extract(data, '$.status') = 'completed'",
            [session_id],
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no row"))?;
    Ok(row.get::<i64>(0)?)
}

/// One tail row as the recent, verbatim part of a checkpoint shows it
/// (upstream `messageToText`, recent-text.ts, over ox's row vocabulary).
/// Empty for rows the checkpoint part leaves out.
fn recent_row_text(row: &serde_json::Value) -> String {
    let kind = row.get("type").and_then(serde_json::Value::as_str);
    match kind {
        Some("user") => {
            let text = row
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            format!("[User]: {text}")
        }
        Some("assistant") => {
            let mut parts: Vec<String> = Vec::new();
            let Some(content) = row.get("content").and_then(serde_json::Value::as_array) else {
                return String::new();
            };
            for part in content {
                match part.get("type").and_then(serde_json::Value::as_str) {
                    // Reasoning is the provider's own summary of its thinking;
                    // written out it reads like an extracted thought (upstream
                    // drops it the same way).
                    Some("text") => {
                        if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                            parts.push(format!("[Assistant]: {text}"));
                        }
                    }
                    Some("tool") => {
                        let name = part
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        let input = match part.pointer("/state/input") {
                            Some(serde_json::Value::String(text)) => {
                                std::borrow::Cow::Borrowed(text.as_str())
                            }
                            Some(other) => std::borrow::Cow::Owned(other.to_string()),
                            None => std::borrow::Cow::Borrowed(""),
                        };
                        parts.push(format!("[Assistant tool call]: {name}({input})"));
                        match part
                            .pointer("/state/status")
                            .and_then(serde_json::Value::as_str)
                        {
                            Some("completed") => {
                                let output = serde_json::to_string(
                                    part.pointer("/state/content")
                                        .unwrap_or(&serde_json::Value::Null),
                                )
                                .unwrap_or_default();
                                parts.push(format!(
                                    "[Tool result]: {}",
                                    truncate_chars(&output, RECENT_TOOL_OUTPUT_MAX_CHARS)
                                ));
                            }
                            Some("error") => {
                                let message = part
                                    .pointer("/state/error/message")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or_default();
                                parts.push(format!("[Tool error]: {message}"));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        Some("synthetic") => {
            let text = row
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if text.is_empty() {
                String::new()
            } else {
                format!("[Synthetic context]: {text}")
            }
        }
        // Compaction and system rows are handled outside the recent part
        // (upstream's `kept` skips them); ox has no other visible rows.
        _ => String::new(),
    }
}

/// Cut at `max` code points so a multi-byte char is never split (upstream
/// `truncateToolOutput` counts the same way).
fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max).collect();
    out.push('…');
    out
}

/// The newest rows after `snapshot`, rendered verbatim and bounded to the
/// keep budget (plan D4): everything the checkpoint would otherwise hide,
/// because the fold's summary only saw rows at or before the snapshot. Rows
/// render oldest-first; the budget is spent newest-first, dropping whole
/// rows from the oldest end (upstream `oldestToDrop`). The budget is
/// counted with the injected token counter — one over per row's trailing
/// newline.
fn recent_text(tail: &[(i64, serde_json::Value)], count_tokens: &dyn Fn(&str) -> usize) -> String {
    let budget_tokens = RECENT_KEEP_TOKENS as usize;
    let mut kept: Vec<String> = Vec::new();
    let mut spent = 0usize;
    for (_, row) in tail.iter().rev() {
        let text = recent_row_text(row);
        if text.is_empty() {
            continue;
        }
        let cost = count_tokens(&text) + 1;
        if spent + cost > budget_tokens {
            break;
        }
        spent += cost;
        kept.push(text);
    }
    kept.reverse();
    kept.join("\n")
}

/// Whether the recent budget carries the whole tail row-by-row (plan D6):
/// [`recent_text`]'s exact cost walk — `recent_row_text` plus one newline
/// token per row against `RECENT_KEEP_TOKENS` tokens — answering "every
/// row is kept" instead of "keep the newest that fit". An empty tail fits.
/// The silent fold refuses a checkpoint whose recent part would silently
/// drop rows the summary never saw.
fn tail_fits_recent(
    tail: &[(i64, serde_json::Value)],
    count_tokens: &dyn Fn(&str) -> usize,
) -> bool {
    let budget_tokens = RECENT_KEEP_TOKENS as usize;
    let mut spent = 0usize;
    for (_, row) in tail.iter().rev() {
        let text = recent_row_text(row);
        if text.is_empty() {
            continue;
        }
        let cost = count_tokens(&text) + 1;
        if spent + cost > budget_tokens {
            return false;
        }
        spent += cost;
    }
    true
}

/// The head path's `recent` part, or `None` when the tail does not fit the
/// budget whole (plan D6/D7). The baseline summary saw none of the tail, so
/// the checkpoint may only take the head path when `recent` carries every
/// row past the floor; an overrun — a mid-fold burst — sends the caller to
/// the template instead of silently dropping rows.
fn head_recent(
    tail: &[(i64, serde_json::Value)],
    count_tokens: &dyn Fn(&str) -> usize,
) -> Option<String> {
    tail_fits_recent(tail, count_tokens).then(|| recent_text(tail, count_tokens))
}

/// The hierarchical fallback's per-segment budget (plan W3): one segment
/// summary covers at most this many tokens of row text. Deliberately a
/// constant, not a config knob.
const SEGMENT_TOKENS: usize = 24_000;

/// One fold row's contribution to a segment's cost: [`recent_row_text`] for
/// the rows that render into `recent`, the checkpoint message for a compaction
/// row. The compaction arm matters because the fold is incremental — the
/// previous checkpoint is its first input row — and a checkpoint that cost
/// nothing would ride a segment for free, letting one completion see far more
/// than [`SEGMENT_TOKENS`].
fn fold_row_text(row: &serde_json::Value) -> String {
    match row.get("type").and_then(serde_json::Value::as_str) {
        Some("compaction") => checkpoint_message(
            row.get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
            row.get("recent")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        ),
        _ => recent_row_text(row),
    }
}

/// Split fold rows into consecutive chunks of at most [`SEGMENT_TOKENS`]
/// tokens of row text (plan Task 7). Deterministic greedy fill: the same rows
/// always produce the same boundaries. A chunk's cost is the sum of
/// [`fold_row_text`] sizes counted by the injected counter; a row is added
/// to the current chunk unless it would bust the budget of a non-empty chunk,
/// so a row whose own text exceeds the cap starts — and forms — its own
/// chunk: never dropped, never split mid-row. Rows that render empty cost
/// nothing and never force a split. At least one chunk is always returned
/// (an empty input yields one empty chunk — the degenerate single-chunk
/// fold); the chunks borrow `rows`, zero copies.
pub fn chunk_rows<'a>(
    rows: &'a [(i64, serde_json::Value)],
    count_tokens: &dyn Fn(&str) -> usize,
) -> Vec<&'a [(i64, serde_json::Value)]> {
    let mut chunks: Vec<&'a [(i64, serde_json::Value)]> = Vec::new();
    let mut start = 0usize;
    let mut spent = 0usize;
    for (index, (_, row)) in rows.iter().enumerate() {
        let cost = count_tokens(&fold_row_text(row));
        if spent > 0 && spent + cost > SEGMENT_TOKENS {
            chunks.push(&rows[start..index]);
            start = index;
            spent = 0;
        }
        spent += cost;
    }
    chunks.push(&rows[start..]);
    chunks
}

/// One finished fold, for the outcome log (upstream `policy.ts` carries the
/// same numbers).
struct FoldStats {
    /// Where the checkpoint's summary came from (plan D11): the mctx
    /// baseline taken verbatim, or the template summarization.
    source: FoldSource,
    rows_before: i64,
    duration_ms: u128,
}

/// Where one fold's checkpoint summary came from (plan D6/D11) — the log's
/// only record of which path a fold took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldSource {
    /// The mctx baseline (`head_m0`) verbatim — zero completions.
    Head,
    /// The template summarization over the snapshot — the fail-open.
    Template,
}

/// The selected checkpoint source for one fold (plan D6): the head is the
/// fast path (the baseline is the summary), the template the fallback.
enum CheckpointPlan {
    /// `summary` is the baseline's `head_m0`; `floor` is the coverage the
    /// checkpoint's `recent` part must carry forward from.
    Head {
        summary: String,
        floor: i64,
    },
    Template,
}

/// Spawn the background fold (plan D1/D2/D11): a detached, silent fold of
/// the session tail — no wire events; the checkpoint lands as a quiet `idle`
/// row. Triggers funnel into this one entry wherever the gate passes (the
/// step boundary's refresh in step.rs, and the actor's `Complete` at a busy
/// period's end), so nothing here may assume a turn just ended.
/// Detached on purpose — a fold is a full hidden completion, and the caller
/// is not the one that must wait for it; the next turn's transcript simply
/// starts at whatever checkpoint the fold published. Every failure logs and
/// is swallowed: the overflow path stays as the blocking backstop, so a dead
/// fold must never take a turn with it.
pub(crate) fn spawn_background_fold(ctx: &DrainCtx) {
    // Single flight (plan D3): the flag closes the window between two folds
    // where neither's running row exists yet. Acquire-and-skip, never wait —
    // the next busy period's end re-runs the gate anyway.
    if ctx
        .fold_in_flight
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        tracing::debug!(session = %ctx.session_id, "background fold already in flight");
        return;
    }
    let ctx = DrainCtx {
        session_id: ctx.session_id.clone(),
        is_subagent: ctx.is_subagent,
        directory: ctx.directory.clone(),
        home: ctx.home.clone(),
        config: ctx.config.clone(),
        db: ctx.db.clone(),
        bus: ctx.bus.clone(),
        chat: Arc::clone(&ctx.chat),
        tools: Arc::clone(&ctx.tools),
        model_catalog: Arc::clone(&ctx.model_catalog),
        mcp: ctx.mcp.clone(),
        instruction_baselines: ctx.instruction_baselines.clone(),
        calibrations: ctx.calibrations.clone(),
        instruction_root: ctx.instruction_root.clone(),
        fold_in_flight: Arc::clone(&ctx.fold_in_flight),
    };
    tokio::spawn(async move {
        // Cleared on every exit path — including a panic unwind, so a dead
        // fold can never wedge the gate shut.
        let _guard = FlagGuard(&ctx.fold_in_flight);
        match run_background_fold(&ctx).await {
            Ok(Some(stats)) => tracing::info!(
                session = %ctx.session_id,
                source = ?stats.source,
                rows_before = stats.rows_before,
                duration_ms = stats.duration_ms,
                "fold published"
            ),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(session = %ctx.session_id, error = %error, "background fold failed")
            }
        }
    });
}

/// Releases the in-flight slot when the fold task ends, unwinding included.
struct FlagGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for FlagGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// The fold itself: gate, source selection (plan D6), then either the head
/// fast path — the mctx baseline becomes the checkpoint verbatim, the tail
/// rides `recent` — or the template summarization; settle the row. All
/// silently (plan D4): no `started`, no deltas, no failure event; the
/// checkpoint lands through [`settle_quiet_outcome`] as a quiet `idle` row.
/// A crash mid-fold loses only the fold itself — the next trigger re-runs
/// the gate (plan D8).
async fn run_background_fold(ctx: &DrainCtx) -> Result<Option<FoldStats>> {
    let started_at = std::time::Instant::now();
    let (_, model) = resolve_selection(&ctx.db, &ctx.session_id, &ctx.config, &ctx.model_catalog)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // One selection serves both the model and the policy: re-resolving either
    // inside the fold could pair a model with another selection's variant.
    let reasoning_effort = model.variant.clone();
    // One counter resolution per fold: the session's measured provider ÷ local
    // ratio scales every local count into the budget's units below.
    let count_tokens = crate::mctx::tokens::counter_fn(crate::mctx::calibration::ratio_for(
        &ctx.calibrations,
        &ctx.session_id,
    ));

    let latest = latest_seq(&ctx.db, &ctx.session_id).await?;
    let checkpoint = last_checkpoint_seq(&ctx.db, &ctx.session_id).await?;
    let running = has_running_compaction(&ctx.db, &ctx.session_id).await?;
    // D1: the usage criterion reads the provider's own numbers — the newest
    // assistant turn past the checkpoint, measured exactly like the
    // historian trigger measures it. Both mctx modes need it, so the budget
    // is derived unconditionally; the floor stays mctx-only.
    let budget = crate::mctx::budget_for(&ctx.config, &ctx.model_catalog, &model.id);
    let usage_pct = {
        let tail = super::step::rows_after_with_seq(&ctx.db, &ctx.session_id, checkpoint).await?;
        let usage = tail.iter().rev().find_map(|(_, row)| prompt_tokens(row));
        usage.and_then(|tokens| budget.usage_percentage(tokens))
    };
    // D6: the gate's pass is the fold's only pass — `(head_m0, floor)` is
    // both the coverage reading for the decision and the fast-path summary
    // candidate. It is never called a second time without a patch in between.
    let (head_m0, head_floor) = if ctx.config.magic_context.enabled {
        let (head_m0, _head_m1, floor) = super::step::mctx_head(ctx, budget).await;
        (head_m0, floor)
    } else {
        (None, 0)
    };
    let decision = fold_decision(
        ctx.config.magic_context.enabled,
        head_floor,
        latest,
        running,
        usage_pct,
    );
    if decision != FoldDecision::Fold {
        tracing::debug!(session = %ctx.session_id, ?decision, "background fold skipped");
        return Ok(None);
    }
    match select_checkpoint_source(
        ctx,
        budget,
        head_m0,
        head_floor,
        &count_tokens,
        reasoning_effort,
    )
    .await?
    {
        CheckpointPlan::Head { summary, floor } => {
            // D7: this is the SECOND tail read past the floor (the first was
            // the gate's fit check in `select_checkpoint_source`) — semantic,
            // not waste: `recent` must carry the tail as of the publish, so
            // rows that landed since the gate read stay covered. The baseline
            // saw none of the tail, so the checkpoint must carry every row
            // past the floor verbatim.
            let tail = super::step::rows_after_with_seq(&ctx.db, &ctx.session_id, floor).await?;
            if let Some(recent) = head_recent(&tail, &count_tokens) {
                let stats = FoldStats {
                    source: FoldSource::Head,
                    rows_before: floor.saturating_sub(checkpoint),
                    duration_ms: started_at.elapsed().as_millis(),
                };
                let settled = settle_quiet_outcome(ctx, model, Ok(summary), recent).await?;
                return Ok(settled.then_some(stats));
            }
            // Mid-fold growth busted the budget between the gate read and
            // this one: summarize instead of dropping rows (plan D6/D7).
            tracing::info!(
                session = %ctx.session_id,
                floor,
                "background fold tail outgrew the recent budget mid-fold; template fallback"
            );
            template_fold(ctx, &model, latest, checkpoint, started_at, &count_tokens).await
        }
        CheckpointPlan::Template => {
            template_fold(ctx, &model, latest, checkpoint, started_at, &count_tokens).await
        }
    }
}

/// D6: pick where one fold's checkpoint summary comes from. The head fast
/// path takes the mctx baseline verbatim when the tail it leaves fits the
/// recent budget whole; when it does not, one synchronous historian patch
/// folds the foldable range past the floor and the pass is re-rendered once.
/// Everything else — mctx off (no candidate at all), a failed pass, a failed
/// or insufficient patch, a tail that still busts — is the template.
async fn select_checkpoint_source(
    ctx: &DrainCtx,
    budget: crate::mctx::budget::Budget,
    head_m0: Option<String>,
    floor: i64,
    count_tokens: &(impl Fn(&str) -> usize + Sync),
    reasoning_effort: Option<String>,
) -> Result<CheckpointPlan> {
    let Some(summary) = head_m0 else {
        // The pass produced no head (off above, or failed): the template is
        // the fail-open, never a missing checkpoint.
        return Ok(CheckpointPlan::Template);
    };
    let tail = super::step::rows_after_with_seq(&ctx.db, &ctx.session_id, floor).await?;
    if tail_fits_recent(&tail, count_tokens) {
        return Ok(CheckpointPlan::Head { summary, floor });
    }
    // The head is real but its tail busts the budget. The baseline is about
    // to be replaced by the re-render — drop it before the completion runs
    // rather than hold it across the await.
    drop(tail);
    drop(summary);
    match super::historian_trigger::patch_fold(ctx, budget, floor, reasoning_effort).await {
        Ok(true) => {}
        Ok(false) => return Ok(CheckpointPlan::Template),
        Err(error) => {
            tracing::warn!(
                session = %ctx.session_id,
                error = %error,
                "background fold historian patch failed; template fallback"
            );
            return Ok(CheckpointPlan::Template);
        }
    }
    let (head_m0, _head_m1, floor) = super::step::mctx_head(ctx, budget).await;
    let Some(summary) = head_m0 else {
        return Ok(CheckpointPlan::Template);
    };
    let tail = super::step::rows_after_with_seq(&ctx.db, &ctx.session_id, floor).await?;
    if tail_fits_recent(&tail, count_tokens) {
        Ok(CheckpointPlan::Head { summary, floor })
    } else {
        // Still busting — the fence held the tail back, or the patch could
        // not start. The template, not a lossy checkpoint (plan D6 item 4).
        Ok(CheckpointPlan::Template)
    }
}

/// The fold's template path (plan D6 item 4): summarize the snapshot from the
/// previous checkpoint onward, carry the mid-fold tail in `recent`, settle the
/// row quietly. Every fallback lands here — mctx off, a failed head pass, a
/// failed or insufficient historian patch, a mid-fold budget bust.
///
/// The input is incremental: the read starts at the newest completed
/// checkpoint row (`checkpoint - 1`, so the row itself is included) and stops
/// at the snapshot. That row renders as the `<conversation-checkpoint>` user
/// message — the previous summary plus its verbatim tail — exactly the
/// transcript the *next* step would send, and exactly upstream's `[S1 + tail]`
/// shape (its `previous` argument is the same thing). Loading from the
/// beginning of the session instead re-summarized content that was already
/// folded: one hidden completion per re-covered segment, and a checkpoint that
/// published every segment's summary for ever (see [`segment_details`]).
async fn template_fold(
    ctx: &DrainCtx,
    model: &ModelRef,
    snapshot: i64,
    checkpoint: i64,
    started_at: std::time::Instant,
    count_tokens: &(impl Fn(&str) -> usize + Sync),
) -> Result<Option<FoldStats>> {
    // D4: snapshot. The summary describes only rows that existed now; rows
    // that land while it streams are carried verbatim in `recent` (I5).
    // Loaded raw with seqs: the hierarchical summarizer chunks over the raw
    // rows (its budget walk reads the same text the request will render) and
    // decodes per chunk. `checkpoint` is the negative sentinel when the
    // session has none, and the subtraction stays exact for any real ordinal.
    let rows: Vec<(i64, serde_json::Value)> =
        super::step::rows_after_with_seq(&ctx.db, &ctx.session_id, checkpoint - 1)
            .await?
            .into_iter()
            .filter(|(seq, _)| *seq <= snapshot)
            .collect();
    let run = summarize_hierarchical(ctx, model, &rows, count_tokens).await;
    let stats = FoldStats {
        source: FoldSource::Template,
        rows_before: snapshot.saturating_sub(checkpoint),
        duration_ms: started_at.elapsed().as_millis(),
    };
    // The tail read happens as late as possible, so the window for a row to
    // land after it and before the checkpoint appends is one publish wide.
    let recent = if run.is_ok() {
        let tail = super::step::rows_after_with_seq(&ctx.db, &ctx.session_id, snapshot).await?;
        recent_text(&tail, count_tokens)
    } else {
        String::new()
    };
    let settled = settle_quiet_outcome(ctx, model.clone(), run, recent).await?;
    Ok(settled.then_some(stats))
}

/// The quiet settlement of one background fold (plan D4): unlike
/// [`publish_outcome`] there is no failed event — the summary either lands
/// as a quiet `idle` checkpoint row or the whole result is discarded.
/// Coalescing comes first: a running compaction row means a blocking
/// compaction owns the session's compaction state, so the result is dropped
/// wholesale. (The check and the publish are not atomic; in the sub-ms gap an
/// overflow `started` can still slip in and adopt this summary — convergent,
/// because the overflow's own `ended` then appends the latest completed row.)
/// Returns whether a checkpoint row landed.
async fn settle_quiet_outcome(
    ctx: &DrainCtx,
    model: ModelRef,
    run: std::result::Result<String, SessionError>,
    recent: String,
) -> Result<bool> {
    if has_running_compaction(&ctx.db, &ctx.session_id).await? {
        tracing::info!(
            session = %ctx.session_id,
            "background fold superseded by a blocking compaction"
        );
        return Ok(false);
    }
    match run {
        Ok(summary) => {
            ctx.bus
                .publish_quiet(SessionEventData::CompactionEnded {
                    session_id: ctx.session_id.clone(),
                    reason: CompactionReason::Idle,
                    model: Some(model),
                    provider_state: None,
                    provider_context: None,
                    text: summary,
                    recent,
                    cost: None,
                    tokens: None,
                })
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(true)
        }
        Err(error) => {
            // Silent by design (plan D4): no `compaction.failed` event — the
            // log line is the failure's only trace.
            tracing::warn!(
                session = %ctx.session_id,
                error = %error.message,
                "background fold summary failed"
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod compaction_budget_tests {
    use super::{append_nudge, compaction_max_tokens};
    use crate::cardinal::wire::{ChatMessage, ChatRequest};

    #[test]
    fn measured_prompt_usage_is_preferred() {
        assert_eq!(compaction_max_tokens(Some(80), 20, Some(200), None, 0), 102);
    }

    #[test]
    fn local_estimate_is_used_without_measured_usage() {
        assert_eq!(compaction_max_tokens(None, 40, Some(100), None, 0), 51);
    }

    #[test]
    fn output_and_context_caps_bound_the_result() {
        assert_eq!(
            compaction_max_tokens(Some(90), 20, Some(200), Some(30), 0),
            30
        );
        assert_eq!(compaction_max_tokens(Some(90), 20, Some(80), None, 0), 1);
    }

    #[test]
    fn non_positive_output_cap_is_unavailable() {
        assert_eq!(
            compaction_max_tokens(Some(80), 20, Some(200), Some(0), 0),
            102
        );
    }

    #[test]
    fn nudge_reuses_existing_positive_max_tokens() {
        let mut request = ChatRequest::new(
            "model".to_owned(),
            vec![ChatMessage::User {
                content: "initial".to_owned().into(),
            }],
        );
        request.max_tokens = Some(42);

        append_nudge(&mut request);

        assert_eq!(request.max_tokens, Some(42));
        assert_eq!(request.messages.len(), 2);
    }

    #[test]
    fn shrink_ladder_is_finite_and_does_not_loop() {
        let values: Vec<_> = (0..=5)
            .map(|step| compaction_max_tokens(Some(100), 1, Some(200), None, step))
            .collect();
        assert_eq!(&values[..4], &[85, 70, 50, 35]);
        assert_eq!(values[4], 35);
        assert_eq!(values[5], 35);
    }
}
