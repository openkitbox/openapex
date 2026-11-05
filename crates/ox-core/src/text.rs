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
