//! Bounded text views.
//!
//! Several ox rules keep a *view* of a text inside a byte budget while the
//! stored copy keeps every byte: a tool output on the wire, a subagent result
//! injected into its parent. This module owns the cut itself — the budget
//! numbers and the marker wording belong to the callers, which know why the
//! budget exists and where the full text lives.

use std::borrow::Cow;

/// The two halves [`bound_head_tail`] keeps, both ending/starting on a char
/// boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadTail<'text> {
    /// The kept prefix, `text[..x]`: at most the requested head bytes.
    pub head: &'text str,
    /// The kept suffix, `text[y..]`: at most the budget left after the head.
    pub tail: &'text str,
}

/// Reduce `text` to a `cap`-byte content budget: `head` bytes from the front,
/// whatever budget remains from the back, and `note` rendered between them by
/// the caller.
///
/// Invariants:
/// - `text.len() <= cap` returns `Cow::Borrowed(text)`: no allocation, no
///   marker, byte-identical output. This is the common case on hot paths.
/// - `cap` bounds the *content* bytes kept (`head.len() + tail.len() <= cap`),
///   never the marker: the returned string exceeds `cap` by the rendered note
///   plus the two newlines around it.
/// - The head wins: it keeps `head` bytes (snapped inward) and the tail takes
///   the rest of the budget, so a head-heavy budget still opens with the text
///   and shows only its end.
/// - Both cuts snap inward to char boundaries, so multi-byte text can come in
///   under `cap`; a character is never split. The halves never overlap: over
///   the cap the suffix starts after the prefix ends.
/// - The marker is `"\n" + note + "\n"`, so a note may itself span lines.
///
/// Callers pass a closure because the marker usually reports the byte counts
/// of the two kept halves, which only this function knows.
///
/// A budget that cannot express a head/tail split still has a defined result —
/// no build profile aborts, and no view ever comes out over budget or repeated:
/// - `cap == 0` keeps no content at all: the result is the marker alone. An
///   empty `text` still takes the borrowing path above.
/// - `head > cap` clamps the head to `cap` and leaves the tail empty, so the
///   head wins outright rather than the two halves overlapping.
///
/// Neither case needs a debug-only gate: the tail budget is derived from the
/// clamped head, so the arithmetic cannot underflow in any profile.
#[must_use]
pub fn bound_head_tail<'text, Note>(
    text: &'text str,
    cap: usize,
    head: usize,
    note: Note,
) -> Cow<'text, str>
where
    Note: FnOnce(HeadTail<'text>) -> String,
{
    if text.len() <= cap {
        return Cow::Borrowed(text);
    }
    // Clamp the head into the budget first: with `head <= cap` guaranteed by
    // construction, the tail budget is a plain remainder that never wraps.
    let head_bytes = head.min(cap);
    // The tail budget comes from the *clamped* head, not the snapped one, so
    // head + tail never exceeds `cap` and a mid-character cut costs one byte
    // rather than borrowing from the tail.
    let tail_bytes = cap.saturating_sub(head_bytes);
    let head_end = floor_char_boundary(text, head_bytes);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_bytes));
    let kept = HeadTail {
        head: &text[..head_end],
        tail: &text[tail_start..],
    };
    let marker = note(kept);
    // Two allocations on this path: the note's own string and this buffer,
    // sized for both halves plus the marker and the newlines around it.
    let mut out = String::with_capacity(kept.head.len() + kept.tail.len() + marker.len() + 2);
    out.push_str(kept.head);
    out.push('\n');
    out.push_str(&marker);
    out.push('\n');
    out.push_str(kept.tail);
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

    /// A marker that reports what the primitive kept, the way both callers do.
    fn note(kept: HeadTail<'_>) -> String {
        format!("[... {} + {} ...]", kept.head.len(), kept.tail.len())
    }

    #[test]
    fn under_the_cap_borrows_and_changes_nothing() {
        let text = "short output";
        let bounded = bound_head_tail(text, 32, 16, note);
        assert!(
            matches!(bounded, Cow::Borrowed(_)),
            "under the cap the view must borrow, got owned bytes"
        );
        assert_eq!(bounded, text);
    }

    #[test]
    fn exactly_the_cap_borrows() {
        let text = "x".repeat(64);
        let bounded = bound_head_tail(&text, 64, 32, note);
        assert!(
            matches!(bounded, Cow::Borrowed(_)),
            "a text of exactly cap bytes is not over budget"
        );
        assert_eq!(bounded.as_ref(), text.as_str());
    }

    #[test]
    fn empty_text_is_borrowed() {
        let bounded = bound_head_tail("", 16, 8, note);
        assert!(matches!(bounded, Cow::Borrowed(_)));
        assert_eq!(bounded, "");
    }

    #[test]
    fn over_the_cap_keeps_head_and_tail_with_the_marker_between() {
        let text: String = ('a'..='z').cycle().take(100).collect();
        let bounded = bound_head_tail(&text, 40, 30, note);
        let expected = format!("{}\n[... 30 + 10 ...]\n{}", &text[..30], &text[90..]);
        assert_eq!(bounded.as_ref(), expected);
    }

    #[test]
    fn head_and_tail_never_exceed_the_content_budget() {
        let text = "y".repeat(10_000);
        let bounded = bound_head_tail(&text, 100, 60, note);
        let marker = "\n[... 60 + 40 ...]\n";
        assert_eq!(
            bounded.len(),
            100 + marker.len(),
            "the marker is not part of the content budget"
        );
    }

    #[test]
    fn multibyte_text_is_cut_on_char_boundaries() {
        // 3-byte characters: a 7-byte head lands inside the third one, and the
        // 5-byte tail budget starts inside the sixth.
        let text = "中文中文中文";
        assert_eq!(text.len(), 18);
        let bounded = bound_head_tail(text, 12, 7, note);
        assert_eq!(bounded.as_ref(), "中文\n[... 6 + 3 ...]\n文");
    }

    #[test]
    fn multibyte_cuts_land_under_the_cap_rather_than_splitting_a_char() {
        let text = "🦀".repeat(10); // 4 bytes each, 40 total
        let bounded = bound_head_tail(&text, 10, 5, note);
        // The head snaps down to 4 bytes (one crab) and the 5-byte tail budget
        // snaps up to 4 kept bytes (one crab): 8 content bytes, no split.
        assert_eq!(bounded.as_ref(), "🦀\n[... 4 + 4 ...]\n🦀");
    }

    #[test]
    fn the_note_already_containing_newlines_is_kept_verbatim() {
        let text = "z".repeat(20);
        let bounded = bound_head_tail(&text, 10, 5, |_| "line one\nline two".to_owned());
        assert_eq!(
            bounded.as_ref(),
            format!("{}\nline one\nline two\n{}", &text[..5], &text[15..]),
        );
    }

    #[test]
    fn a_zero_head_budget_keeps_only_the_tail() {
        let text = "q".repeat(20);
        let bounded = bound_head_tail(&text, 8, 0, |kept| {
            assert_eq!(kept.head, "");
            "marker".to_owned()
        });
        assert_eq!(bounded.as_ref(), "\nmarker\nqqqqqqqq");
    }

    #[test]
    fn one_byte_over_the_cap_drops_only_the_byte_between_the_halves() {
        let text = "abcdefghij"; // 10 bytes
        let bounded = bound_head_tail(text, 9, 4, note);
        // A 4-byte head leaves 5 of the 9 budgeted content bytes for the tail,
        // so the halves cover 9 bytes and exactly one byte — `e`, at index 4 —
        // falls between them: the view is one byte short of the source, with
        // nothing kept twice.
        assert_eq!(
            bounded.as_ref(),
            "abcd\n[... 4 + 5 ...]\nfghij",
            "the single over-cap byte must be the only one dropped, got {bounded:?}"
        );
        assert_eq!(
            bounded.len(),
            9 + "\n[... 4 + 5 ...]\n".len(),
            "content bytes stay at the cap and the marker sits outside it, got {bounded:?}"
        );
    }

    #[test]
    fn a_zero_budget_keeps_the_marker_and_no_content() {
        let text = "some output";
        let bounded = bound_head_tail(text, 0, 0, note);
        // A zero-byte budget cannot express content, so the view degrades to
        // the marker alone instead of keeping the whole source by accident.
        assert_eq!(
            bounded.as_ref(),
            "\n[... 0 + 0 ...]\n",
            "cap 0 must keep no content bytes, got {bounded:?}"
        );
    }

    #[test]
    fn a_head_larger_than_the_budget_is_clamped_and_never_repeats_text() {
        let text = "abcdefghijklmnopqrst"; // 20 bytes
        let bounded = bound_head_tail(text, 8, 30, note);
        // The head is clamped to the whole budget and the tail gets nothing:
        // the view opens with the text and never keeps a byte twice, which is
        // what an unclamped subtraction would have produced.
        assert_eq!(
            bounded.as_ref(),
            "abcdefgh\n[... 8 + 0 ...]\n",
            "an over-budget head must be clamped with an empty tail, got {bounded:?}"
        );
        assert_eq!(
            bounded.len(),
            8 + "\n[... 8 + 0 ...]\n".len(),
            "the clamped view holds exactly cap content bytes, got {bounded:?}"
        );
    }
}
