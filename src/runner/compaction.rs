//! Compaction, ported from `packages/core/src/session/compaction.ts`
//! (faithful-but-minimal: the summarization flow only; auto thresholds,
//! budget shrinking and native provider windows are out of scope).
//!
//! Upstream compacts through a dedicated agent and a budgeted request
//! pipeline; collapsed to one flow:
//!
//! 1. `session.compaction.started` → the projector appends a `running`
//!    compaction row.
//! 2. The session transcript (from the latest completed compaction onward)
//!    becomes a tools-free chat request: a fixed summarization system
//!    prompt, the transcript as user/assistant messages, then the summary
//!    instruction as the final user turn (upstream appends the same prompt
//!    user message in `summarize`).
//! 3. The reply streams as ephemeral `session.compaction.delta` events and
//!    settles with `session.compaction.ended {text}` — the projector flips
//!    the running row to `completed` with the summary. On failure the row
//!    settles `failed` via `session.compaction.failed`.
//!
//! Afterwards history selection starts at the completed row (upstream
//! `SessionHistory` boundary), so later requests open with the checkpoint
//! user message instead of the compacted transcript.

use crate::cardinal::stream::StreamEvent;
use crate::cardinal::wire::{ChatMessage, ChatRequest};
use crate::schema::event::SessionEventData;
use crate::schema::message::CompactionReason;
use crate::schema::support::SessionError;

use super::drain::DrainCtx;
use super::unknown_error;

/// The compaction request's system prompt. Upstream routes the summarization
/// through a dedicated `compaction` agent whose system prompt is exactly
/// this scoping: summarize, don't continue.
pub(crate) const COMPACTION_SYSTEM: &str = "You summarize conversations so another agent can resume the work. Summarize only; do not continue the task.";

/// Upstream `buildPrompt(false)` (compaction.ts:145) verbatim: the
/// first-time summary instruction appended as the final user message.
pub(crate) const SUMMARY_PROMPT: &str = "You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.\n\nSummarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.\n\nYou MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.\n<template>\n## Objective\n- [one or two brief sentences describing what the user is trying to accomplish]\n\n## Requirements\n- [constraints, preferences, requirements, and scope boundaries stated by the user, or \"(none)\"]\n\n## Decisions\n- [decisions already made and why, or \"(none)\"]\n\n## Work State\nBreak the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.\n### Completed\n- [goals that have been completed; otherwise \"(none)\"]\n\n### Active\n- [goals currently being worked on; otherwise \"(none)\"]\n\n### Blocked\n- [anything blocking progress, and why; otherwise \"(none)\"]\n\n## Next Move\n1. [ordered list of next actions, or \"(none)\"]\n\n## Relevant Files\nList the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write \"(none)\".\n- `[file or directory path]`: [brief reason it matters]\n\n## Important Context\n- [facts the next agent cannot continue without and cannot easily find on its own; or \"(none)\"]\n</template>\n\nRules:\n- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.\n- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.\n- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.\n- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.\n- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.\n- Do not mention the summary process or that context was compacted.\n\nDo not continue the task or call tools.\n\nReturn only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.";

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

/// Build the summarization request: system prompt + transcript (no working
/// system prompt, no tools) + the summary instruction as the last user
/// turn. Upstream `summarize`: `prepared.request` + `Message.user(prompt)`.
pub(crate) fn summarization_request(model: &str, transcript: &[ChatMessage]) -> ChatRequest {
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
        content: SUMMARY_PROMPT.to_owned(),
    });
    // `tools` stays empty → omitted from the wire; upstream disables tools
    // for compaction (`input.tools` defaults to a rejecting definition set).
    ChatRequest::new(model.to_owned(), messages)
}

/// Run one compaction to completion: publish `started`, stream the
/// summarization request, publish `ended` (or `failed`). Returns the
/// summary on success; the failure error after publishing
/// `compaction.failed` otherwise.
pub(crate) async fn compact(
    ctx: &DrainCtx,
    reason: CompactionReason,
    input_id: Option<String>,
) -> std::result::Result<String, SessionError> {
    let (_, model) = crate::runner::drain::resolve_selection(&ctx.db, &ctx.session_id, &ctx.config)
        .await
        .map_err(|error| unknown_error(error.to_string()))?;

    ctx.bus
        .publish(SessionEventData::CompactionStarted {
            session_id: ctx.session_id.clone(),
            reason,
            // Upstream sends the newest kept transcript excerpt as `recent`;
            // the minimal port keeps none (the checkpoint message omits the
            // recent-context block when empty).
            recent: String::new(),
            input_id: input_id.clone(),
        })
        .await
        .map_err(|error| unknown_error(error.to_string()))?;

    let failure = |kind: &str, message: String| SessionError {
        kind: kind.to_owned(),
        message,
        status: None,
        response: None,
    };

    let run = async {
        let rows = match super::step::load_history(&ctx.db, &ctx.session_id).await {
            Ok(rows) => rows,
            Err(error) => return Err(failure("unknown", error.to_string())),
        };
        let request = summarization_request(
            &format!("{}/{}", model.provider_id, model.id),
            &super::step::build_chat_messages("", &rows),
        );
        let mut stream = match ctx.chat.stream_chat(&request).await {
            Ok(stream) => stream,
            Err(error) => {
                return Err(failure(
                    "compaction.failed",
                    format!("compaction request failed: {error}"),
                ));
            }
        };
        let mut summary = String::new();
        loop {
            let next = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await;
            match next {
                Some(Ok(StreamEvent::TextDelta(delta))) => {
                    summary.push_str(&delta);
                    if let Err(error) = ctx
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
                    return Err(failure(
                        "compaction.failed",
                        format!("compaction stream failed: {error}"),
                    ));
                }
                None => break,
            }
        }
        if summary.trim().is_empty() {
            return Err(failure(
                "compaction.failed",
                "Compaction produced no summary".to_owned(),
            ));
        }
        Ok(summary)
    }
    .await;

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
                    recent: String::new(),
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

/// Whether a provider rejection body/`status` is a context-length overflow
/// (upstream `isContextOverflowFailure`, via the signatures cardinal passes
/// through). Matched case-insensitively on the body.
pub(crate) fn is_context_overflow(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("context") || body.contains("token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_message_shape_matches_upstream() {
        let message = checkpoint_message("S2", "");
        assert!(message.starts_with("<conversation-checkpoint>\n"));
        assert!(message.contains("<summary>\nS2\n</summary>"));
        assert!(!message.contains("recent-context"), "empty recent omitted");

        let with_recent = checkpoint_message("S2", "E19");
        assert!(with_recent.contains("<recent-context>\nE19\n</recent-context>"));
        assert!(with_recent.ends_with("</conversation-checkpoint>"));
    }

    #[test]
    fn summarization_request_has_no_tools_and_prompt_last() {
        let transcript = vec![
            ChatMessage::System {
                content: "working system".to_owned(),
            },
            ChatMessage::User {
                content: "hello".to_owned(),
            },
        ];
        let request = summarization_request("p/m", &transcript);
        assert!(request.tools.is_empty(), "compaction never sends tools");
        let wire = serde_json::to_value(&request).expect("serialize");
        assert!(wire.get("tools").is_none(), "tools key omitted");
        assert!(
            wire.get("tool_choice").is_none(),
            "tool_choice omitted without tools"
        );
        assert_eq!(request.model, "p/m");
        assert!(
            matches!(&request.messages[0], ChatMessage::System { content } if content == COMPACTION_SYSTEM),
            "summarization system prompt first"
        );
        assert!(
            !format!("{:?}", request.messages).contains("working system"),
            "the working system prompt is replaced"
        );
        assert!(
            matches!(request.messages.last(), Some(ChatMessage::User { content }) if content.contains("You MUST summarize")),
            "summary instruction is the final user turn"
        );
    }

    #[test]
    fn overflow_signature_matches_400_bodies_only() {
        assert!(is_context_overflow(
            400,
            "This model's maximum context length is 4097 tokens"
        ));
        assert!(is_context_overflow(
            400,
            "Request too large: TOO MANY TOKENS"
        ));
        assert!(!is_context_overflow(429, "maximum context length exceeded"));
        assert!(!is_context_overflow(400, "invalid api key"));
    }
}
