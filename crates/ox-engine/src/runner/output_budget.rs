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

/// Task-1 data: p99 = 21.5 KiB, only 0.49% of prod outputs exceed 32 KiB.
pub(crate) const TOOL_OUTPUT_CAP_BYTES: usize = 32 * 1024;
const HEAD_BYTES: usize = 16 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_the_cap_is_borrowed_verbatim() {
        let output = "cargo test output";
        let bounded = bound_tool_output(output);
        assert!(matches!(bounded, Cow::Borrowed(_)));
        assert_eq!(bounded, output);
    }

    #[test]
    fn over_the_cap_keeps_16k_head_and_16k_tail_behind_the_marker() {
        let output = "a".repeat(100 * 1024);
        let bounded = bound_tool_output(&output);
        let marker = format!(
            "\n[output truncated: {} bytes total; showing first {} and last {} bytes; \
             full output stored in session history, ctx_search source=transcript]\n",
            output.len(),
            HEAD_BYTES,
            HEAD_BYTES,
        );
        assert_eq!(
            bounded.len(),
            TOOL_OUTPUT_CAP_BYTES + marker.len(),
            "the marker sits outside the 32 KiB content budget"
        );
        assert_eq!(
            bounded.as_ref(),
            format!(
                "{}{marker}{}",
                &output[..HEAD_BYTES],
                &output[output.len() - HEAD_BYTES..]
            )
        );
    }
}
