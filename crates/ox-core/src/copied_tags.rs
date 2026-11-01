//! Strip `§N§` tag notation that a model copied into its own assistant text.
//!
//! Port of magic-context `tag-content-primitives.ts::stripPersistedAssistantText`
//! (`~/github/magic-context/packages/plugin/src/hooks/magic-context/`). Applied
//! at ox's persistence boundary — the projector's `TextEnded` / `ReasoningEnded`
//! handlers — before a part's text is written to `session_message`.
//!
//! Why: `apply_overlay` (Request mode) prefixes every item with a stable `§N§ `
//! tag. Models learn the notation and echo it at the start of their own
//! text/reasoning parts, often predicting their own next tag and then
//! stuttering the same number (3..48 repeats observed in production). Persisted
//! copies feed back into later prompts and reinforce the loop; stripping at the
//! persistence boundary breaks it.
//!
//! Deliberately not stripped: user text, tool inputs/outputs (a `read` result
//! may legitimately contain section signs) and compaction summaries.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

/// Leading well-formed tag runs: `§N§` tokens (optionally whitespace-separated)
/// at the start of the text — the stutter case.
static LEADING_TAG_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:§[0-9]+§\s*)+").expect("leading tag regex"));

/// Complete `§[0-9]+§` pairs anywhere: cargo-cult copies mid-text.
static COMPLETE_TAG_PAIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"§[0-9]+§").expect("complete tag pair regex"));

/// Malformed tag/XML hybrids anywhere (`§15298">§15298§`): token-level
/// confusion between the tag format and quoted attribute syntax.
///
/// Note: the `[0-9]+§` tail inside the optional group is unreachable at this
/// chain position (complete pairs are removed by the preceding pass) and kept
/// as an exact port of upstream's regex.
static MALFORMED_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"§[0-9]+">(?:§(?:[0-9]+§)?)?"#).expect("malformed tag regex"));

/// Dangling tags: `§` + digits and an optional decimal tail, optionally closed
/// by one non-word closer. Upstream uses a `(?!\\.\d)` lookahead to leave
/// `§5.1`-style section citations for the stray-`§` pass; the `regex` crate has
/// no lookaround, so the decimal tail is captured and handled branch-wise.
/// Digits are pinned to ASCII (`[0-9]`, matching upstream JS `\d`) and the
/// closer class spells out ASCII word chars instead of `\w` — Rust `\w` is
/// Unicode-aware, which would exclude closers like the Cyrillic `ҩ` and break
/// the `§11865ҩ done` case.
static DANGLING_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"§([0-9]+)(\.[0-9]+)?([^\s§A-Za-z0-9_.])?").expect("dangling tag regex")
});

/// Lone section signs left after pair/malformed/dangling removal.
static STRAY_SECTION_SIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("§").expect("section sign regex"));

/// Strip copied MC tag notation from assistant text at the persistence boundary.
///
/// Text without a `§` is returned borrowed and byte-identical: trimming clean
/// text would change bytes for no reason (upstream's wrapper has the same
/// guard).
#[must_use]
pub fn strip_copied_tag_notation(text: &str) -> Cow<'_, str> {
    if !text.contains('§') {
        return Cow::Borrowed(text);
    }
    let out = Cow::Borrowed(text);
    let out = replace_pass(out, &LEADING_TAG_RUN);
    let out = replace_pass(out, &COMPLETE_TAG_PAIR);
    let out = replace_pass(out, &MALFORMED_TAG);
    let out = strip_dangling(out);
    let out = replace_pass(out, &STRAY_SECTION_SIGN);
    trim_cow(out)
}

/// One `replace_all("")` pass over the current value. Borrows (no allocation)
/// when the regex does not match; moves the owned string through when it does.
fn replace_pass<'a>(value: Cow<'a, str>, regex: &Regex) -> Cow<'a, str> {
    if regex.is_match(&value) {
        Cow::Owned(regex.replace_all(value.as_ref(), "").into_owned())
    } else {
        value
    }
}

/// Remove dangling tags, keeping section-citation digits (`§5.1` → `5.1`) and
/// eating at most one non-word closer (`§103012$` → ``, `§42 files` → ` files`).
fn strip_dangling(value: Cow<'_, str>) -> Cow<'_, str> {
    if !DANGLING_TAG.is_match(&value) {
        return value;
    }
    let mut out = String::with_capacity(value.len());
    let mut last = 0;
    for caps in DANGLING_TAG.captures_iter(&value) {
        let m = caps.get(0).expect("whole-match capture");
        out.push_str(&value[last..m.start()]);
        if caps.get(2).is_some() {
            // `§N.M…`: a section citation — drop only the `§`.
            out.push_str(&value[m.start() + '§'.len_utf8()..m.end()]);
        }
        last = m.end();
    }
    out.push_str(&value[last..]);
    Cow::Owned(out)
}

/// Trim without reallocating an owned buffer: trim the borrowed view, or cut
/// the owned string's head and tail in place.
fn trim_cow(value: Cow<'_, str>) -> Cow<'_, str> {
    match value {
        Cow::Borrowed(s) => Cow::Borrowed(s.trim()),
        Cow::Owned(mut s) => {
            let start = s.len() - s.trim_start().len();
            let end = s.trim_end().len();
            if end <= start {
                return Cow::Owned(String::new());
            }
            s.truncate(end);
            s.drain(..start);
            Cow::Owned(s)
        }
    }
}
