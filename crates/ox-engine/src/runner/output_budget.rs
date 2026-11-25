//! The wire's view-only tool-output budget.
//!
//! Live tool outputs enter the outbound request verbatim; one unbounded output
//! (a webfetch, an MCP tool) can blow the context window on its own. This
//! bounds only the *view*: the stored `Message` keeps the full text, so the
//! model can still reach any byte through history / `ctx_search` — the budget
//! trades nothing but this request's bytes (head + marker + tail).
//!
//! The cut itself is [`ox_core::text::bound_head_tail`]; what lives here is the
//! tool-output rule: the budget numbers and the marker the model reads.

use std::borrow::Cow;

use ox_core::text;
use ox_llm::chat_completions::wire::ChatMessage;

/// Task-1 data: p99 = 21.5 KiB, only 0.49% of prod outputs exceed 32 KiB.
pub(crate) const TOOL_OUTPUT_CAP_BYTES: usize = 32 * 1024;
/// Four single-output caps: enough for several full results, but a tool loop
/// cannot fill the window with outputs alone. A constant, not a config knob:
/// the budget is a wire invariant, the same for every session.
pub(crate) const TOOL_OUTPUT_TOTAL_BUDGET_BYTES: usize = 128 * 1024;
const HEAD_BYTES: usize = 16 * 1024;

/// Replacement for an old completed result once the aggregate budget is spent.
/// The full text stays in the durable row: `ctx_search source=transcript`
/// reaches it. The `{"output": ...}` envelope keeps the result shape the
/// model already sees, so no dialect has to learn a new content form.
pub(crate) const AGGREGATE_MARKER: &str = concat!(
    r#"{"output":"[older tool output elided by aggregate budget; "#,
    "full output stored in session history, ctx_search source=transcript]\"}"
);

#[must_use]
pub(crate) fn bound_tool_output(text: &str) -> Cow<'_, str> {
    // Under the cap the bytes go out verbatim and nothing is allocated; over
    // it, the head of the output (what the tool started saying) and the tail
    // (how it ended) survive, with the marker's counts telling the model
    // exactly what is missing and where the full text lives.
    text::bound_head_tail(text, TOOL_OUTPUT_CAP_BYTES, HEAD_BYTES, |kept| {
        format!(
            "[output truncated: {} bytes total; showing first {} and last {} bytes; full output stored in session history, ctx_search source=transcript]",
            text.len(),
            kept.head.len(),
            kept.tail.len(),
        )
    })
}

/// Bound the *total* tool-result bytes of one outbound request.
///
/// Walk backwards so the newest results (the ones the model still has to
/// act on) stay verbatim until the budget is spent — the newest result is
/// never replaced, even if it alone were oversized. Every tool result's
/// bytes count, including errors and already-elided ones; only older
/// completed results are replaced. Errors, unsettled calls and an existing
/// marker are left alone, and no id or non-tool message is touched, so the
/// call/result pairing the wire requires survives the pass.
pub(crate) fn bound_aggregate_tool_outputs(messages: &mut [ChatMessage]) {
    let mut counted = 0_usize;
    let mut newest = true;
    for message in messages.iter_mut().rev() {
        let ChatMessage::Tool { content, .. } = message else {
            continue;
        };
        let is_newest = newest;
        newest = false;
        counted += content.len();
        if is_newest
            || counted <= TOOL_OUTPUT_TOTAL_BUDGET_BYTES
            || content == AGGREGATE_MARKER
            || !is_completed_output(content)
        {
            continue;
        }
        *content = AGGREGATE_MARKER.to_owned();
    }
}

/// Completed results are exactly the `{"output": ...}` envelope written by
/// `step::context::tool_result_message`; errors and never-settled calls use
/// the `{"error": ...}` envelope and must pass through untouched.
fn is_completed_output(content: &str) -> bool {
    content.starts_with(r#"{"output":""#) && content.ends_with("\"}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(bytes: usize) -> String {
        format!(r#"{{"output":"{}"}}"#, "x".repeat(bytes))
    }

    fn tool(id: &str, content: String) -> ChatMessage {
        ChatMessage::tool(id, content)
    }

    #[test]
    fn newest_results_are_kept_and_old_completed_results_elided() {
        let size = TOOL_OUTPUT_CAP_BYTES - AGGREGATE_MARKER.len(); // keep the envelope one cap
        let body = output(size - 13);
        assert_eq!(body.len(), size);
        let mut messages: Vec<ChatMessage> = (0..5)
            .map(|index| tool(&format!("call_{index}"), body.clone()))
            .collect();
        let before = messages.clone();

        bound_aggregate_tool_outputs(&mut messages);

        assert_eq!(messages[4], before[4], "newest result stays verbatim");
        assert_eq!(
            messages[1], before[1],
            "a result inside the budget stays verbatim"
        );
        assert_eq!(
            messages[0],
            tool("call_0", AGGREGATE_MARKER.to_owned()),
            "the oldest result becomes the marker"
        );
        for (index, message) in messages.iter().enumerate() {
            let ChatMessage::Tool { tool_call_id, .. } = message else {
                panic!("all messages are tool results");
            };
            let ChatMessage::Tool {
                tool_call_id: expected,
                ..
            } = &before[index]
            else {
                panic!("baseline message is a tool result");
            };
            assert_eq!(tool_call_id, expected, "ids survive the pass");
        }
    }

    #[test]
    fn budget_boundary_is_inclusive() {
        let body = output(TOOL_OUTPUT_TOTAL_BUDGET_BYTES / 4 - 13);
        assert_eq!(body.len(), TOOL_OUTPUT_TOTAL_BUDGET_BYTES / 4);
        let mut messages: Vec<ChatMessage> = (0..4)
            .map(|index| tool(&format!("call_{index}"), body.clone()))
            .collect();
        let before = messages.clone();

        bound_aggregate_tool_outputs(&mut messages);
        assert_eq!(messages, before, "exactly at the budget nothing is elided");
    }

    #[test]
    fn marker_shape_points_at_transcript_history() {
        let parsed: serde_json::Value =
            serde_json::from_str(AGGREGATE_MARKER).expect("marker is a JSON tool result envelope");
        let text = parsed["output"]
            .as_str()
            .expect("the envelope carries an output string");
        assert!(text.contains("session history"));
        assert!(text.contains("ctx_search source=transcript"));
        assert!(
            is_completed_output(AGGREGATE_MARKER),
            "marker keeps the result shape"
        );
    }

    #[test]
    fn errors_unsettled_markers_and_non_tool_messages_are_untouched() {
        let oversized = output(TOOL_OUTPUT_TOTAL_BUDGET_BYTES);
        let mut messages = vec![
            ChatMessage::system("system bytes"),
            ChatMessage::user("user bytes"),
            tool(
                "call_error",
                r#"{"error":{"type":"denied","message":"no"}}"#.to_owned(),
            ),
            tool(
                "call_unsettled",
                r#"{"error":{"type":"aborted","message":"gone"}}"#.to_owned(),
            ),
            tool("call_marked", AGGREGATE_MARKER.to_owned()),
            ChatMessage::assistant("assistant bytes"),
            tool("call_new", oversized),
        ];
        let before = messages.clone();

        // The newest result overshoots the budget on its own: it still stays,
        // and none of the older results is a replaceable completed one.
        bound_aggregate_tool_outputs(&mut messages);

        assert_eq!(messages, before, "only old completed results are replaced");
    }
}
