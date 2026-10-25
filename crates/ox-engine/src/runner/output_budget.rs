//! The wire's view-only tool-output budget.
//!
//! Live tool outputs enter the outbound request verbatim; one unbounded output
//! (a webfetch, an MCP tool) can blow the context window on its own. This
//! bounds only the *view*: the stored `Message` keeps the full text, so the
//! model can still reach any byte through history / `ctx_search` — the budget
//! trades nothing but this request's bytes (head + marker + tail).

use std::borrow::Cow;
use std::fmt::Write as _;

/// Task-1 data: p99 = 21.5 KiB, only 0.49% of prod outputs exceed 32 KiB.
pub(crate) const TOOL_OUTPUT_CAP_BYTES: usize = 32 * 1024;
const HEAD_BYTES: usize = 16 * 1024;

pub(crate) fn bound_tool_output(text: &str) -> Cow<'_, str> {
    // Under the cap the bytes go out verbatim and nothing is allocated.
    if text.len() <= TOOL_OUTPUT_CAP_BYTES {
        return Cow::Borrowed(text);
    }
    // Over the cap: the head of the output (what the tool started saying) and
    // the tail (how it ended), snapped to char boundaries so no character is
    // ever cut in half. The marker carries the counts so the model knows
    // exactly what is missing and where the full text lives.
    let head_end = floor_char_boundary(text, HEAD_BYTES);
    let tail_start = ceil_char_boundary(text, text.len() - (TOOL_OUTPUT_CAP_BYTES - HEAD_BYTES));
    let head = &text[..head_end];
    let tail = &text[tail_start..];
    // One allocation: head + marker + tail are built in place. The spare 192
    // covers the marker's byte length for any realistic count (the counts add
    // at most one digit per ~10x size), and `write!` on `String` is
    // infallible.
    let mut out = String::with_capacity(head.len() + tail.len() + 192);
    out.push_str(head);
    let _ = write!(
        out,
        "\n[output truncated: {} bytes total; showing first {} and last {} bytes; full output stored in session history, ctx_search source=transcript]\n",
        text.len(),
        head.len(),
        tail.len(),
    );
    out.push_str(tail);
    Cow::Owned(out)
}

/// The largest index `<= at` that starts a character.
fn floor_char_boundary(s: &str, mut at: usize) -> usize {
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest index `>= at` that starts a character.
fn ceil_char_boundary(s: &str, mut at: usize) -> usize {
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}
