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

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "\n[output truncated: 102400 bytes total; showing first 16384 and last 16384 bytes; full output stored in session history, ctx_search source=transcript]\n";

    #[test]
    fn under_cap_is_borrowed_and_byte_identical() {
        for text in ["hello world", "你好，世界"] {
            let bound = bound_tool_output(text);
            assert!(matches!(bound, Cow::Borrowed(_)));
            assert_eq!(bound, text);
        }
    }

    #[test]
    fn over_cap_keeps_head_and_tail_with_marker() {
        let head = "H".repeat(20 * 1024);
        let middle = "M".repeat(60 * 1024);
        let tail = "E".repeat(20 * 1024);
        let text = format!("{head}{middle}{tail}");
        assert_eq!(text.len(), 100 * 1024);

        let out = bound_tool_output(&text);
        assert!(matches!(out, Cow::Owned(_)));
        // The kept head is the output's first 16 KiB, the kept tail its last.
        assert!(out.starts_with(&head[..HEAD_BYTES]));
        assert!(out.ends_with(&tail[tail.len() - HEAD_BYTES..]));
        assert!(out.contains("[output truncated: 102400 bytes total"));
        assert!(out.contains("showing first 16384 and last 16384 bytes"));
        assert!(out.contains(MARKER));
        assert!(out.len() <= TOOL_OUTPUT_CAP_BYTES + MARKER.len());
    }

    #[test]
    fn cjk_boundaries_never_split_a_char() {
        // 34134 chars x 3 bytes = 102402 bytes, just over the cap.
        let text = "汉".repeat(34_134);
        let out = bound_tool_output(&text);
        assert!(matches!(out, Cow::Owned(_)));
        // Kept runs are whole characters: 16384 snaps down to 16383 (a
        // multiple of 3), the tail start snaps up the same way.
        assert!(out.contains("showing first 16383 and last 16383 bytes"));
        assert!(
            out.trim_start_matches('汉')
                .starts_with("\n[output truncated:")
        );
        assert!(out.ends_with("汉"));
        assert!(!out.contains('\u{FFFD}'));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn exact_cap_boundary() {
        let at_cap = "A".repeat(TOOL_OUTPUT_CAP_BYTES);
        let bound = bound_tool_output(&at_cap);
        assert!(matches!(bound, Cow::Borrowed(_)));
        assert_eq!(bound, at_cap);

        let over = "A".repeat(TOOL_OUTPUT_CAP_BYTES + 1);
        let bound = bound_tool_output(&over);
        assert!(matches!(bound, Cow::Owned(_)));
        let marker = "\n[output truncated: 32769 bytes total; showing first 16384 and last 16384 bytes; full output stored in session history, ctx_search source=transcript]\n";
        assert!(bound.contains(marker));
        // The budget bounds the worst case against the cap, not against the
        // input: an output just over the cap comes out a hair larger (the
        // marker), anything bigger is crushed to cap + marker.
        assert!(bound.len() <= TOOL_OUTPUT_CAP_BYTES + marker.len());
    }
}
