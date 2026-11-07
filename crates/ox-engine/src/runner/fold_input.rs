//! The fold's view of a transcript: what a summarization request carries,
//! as opposed to what the live request carries.
//!
//! The live renderer ([`super::step::build_chat_messages`]) is byte-stable
//! on purpose — the historian's prefix reuse depends on it — so a
//! summarization request is rendered by it and then *tiered* here, in place,
//! before it goes out. Two degrees, both over the `tool` role only, because
//! that is where a transcript's bytes are and where its narrative is not:
//!
//! - [`tighten_tool_outputs`] — every request. A summary needs to know what
//!   a tool was asked and roughly what came back, not the 32 KiB the live
//!   request keeps for the model to act on. The cap here is a few KiB, head
//!   and tail, with the same marker shape the live view uses.
//! - [`elide_tool_outputs`] — the overflow fallback. When the request still
//!   does not fit, every tool result is replaced by one fixed sentence; the
//!   user's and the assistant's own text are never cut. A summary that lost
//!   its tool outputs is a worse summary; a fold that failed is no summary.
//!
//! Reference: codex `compact.rs` (`COMPACT_USER_MESSAGE_MAX_TOKENS`, the
//! `"Output exceeded the available model context and was truncated"`
//! elision), reduced to ox's single renderer.

use std::borrow::Cow;

use ox_core::text;
use ox_llm::cardinal::wire::ChatMessage;

/// One tool result's cap inside a summarization request. The live request's
/// cap is 32 KiB; a summary reads the shape of a result, not its body.
pub(crate) const SUMMARY_TOOL_OUTPUT_CAP_BYTES: usize = 4 * 1024;
const SUMMARY_TOOL_OUTPUT_HEAD_BYTES: usize = 2 * 1024;

/// What a tool result becomes once the request has to give it up entirely.
/// A sentence the model can read as "there was output here", in the same
/// `{"output": …}` envelope a live result uses, so the transcript's shape
/// (call, then result) survives the elision.
pub(crate) const ELIDED_TOOL_OUTPUT: &str =
    r#"{"output":"Output exceeded the available model context and was truncated"}"#;

/// Bound every `tool` message to [`SUMMARY_TOOL_OUTPUT_CAP_BYTES`], head and
/// tail, in place. Results under the cap are not touched (and not copied).
pub(crate) fn tighten_tool_outputs(messages: &mut [ChatMessage]) {
    for message in messages {
        let ChatMessage::Tool { content, .. } = message else {
            continue;
        };
        if let Cow::Owned(bounded) = bound_for_summary(content) {
            *content = bounded;
        }
    }
}

/// Replace every `tool` message's content with [`ELIDED_TOOL_OUTPUT`], in
/// place. Returns how many results were elided; `0` means the request had
/// nothing left to give up on this rung.
pub(crate) fn elide_tool_outputs(messages: &mut [ChatMessage]) -> usize {
    let mut elided = 0;
    for message in messages {
        let ChatMessage::Tool { content, .. } = message else {
            continue;
        };
        if content != ELIDED_TOOL_OUTPUT {
            *content = ELIDED_TOOL_OUTPUT.to_owned();
            elided += 1;
        }
    }
    elided
}

/// The head + marker + tail view of one result for a summary request.
fn bound_for_summary(content: &str) -> Cow<'_, str> {
    text::bound_head_tail(
        content,
        SUMMARY_TOOL_OUTPUT_CAP_BYTES,
        SUMMARY_TOOL_OUTPUT_HEAD_BYTES,
        |kept| {
            format!(
                "[output truncated for summary: {} bytes total; showing first {} and last {} bytes]",
                content.len(),
                kept.head.len(),
                kept.tail.len(),
            )
        },
    )
}
