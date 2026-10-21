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

#[cfg(test)]
mod tests {
    use super::strip_copied_tag_notation;
    use std::borrow::Cow;

    fn strip(text: &str) -> String {
        strip_copied_tag_notation(text).into_owned()
    }

    #[test]
    fn text_without_a_section_sign_is_borrowed_and_byte_identical() {
        let text = "99 files  Actually executing now. Running fmt:";
        assert!(matches!(
            strip_copied_tag_notation(text),
            Cow::Borrowed(_)
        ));
        assert_eq!(strip_copied_tag_notation(text), text);
    }

    #[test]
    fn whitespace_only_text_without_a_section_sign_is_not_trimmed() {
        // The fast path must never change bytes: upstream's guarded wrapper
        // behaves the same (its raw function would trim, its wrapper does not).
        assert_eq!(strip_copied_tag_notation("   "), "   ");
        assert!(matches!(
            strip_copied_tag_notation("   "),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn leading_stutter_run_is_removed() {
        assert_eq!(
            strip("§194§ §194§ §194§ Task 2 实现完成"),
            "Task 2 实现完成"
        );
    }

    #[test]
    fn single_leading_tag_is_removed() {
        assert_eq!(strip("§42§ Hello"), "Hello");
    }

    #[test]
    fn long_leading_stutter_run_is_removed() {
        // Worst case observed in production: §183§ repeated 48 times.
        let text = format!("{}构建完成（1m26s，268 crates）", "§183§ ".repeat(48));
        assert_eq!(strip(&text), "构建完成（1m26s，268 crates）");
    }

    #[test]
    fn mid_text_pair_is_removed_and_surrounding_spaces_kept() {
        // Upstream expects the double space: only the pair itself is eaten.
        assert_eq!(
            strip("Looking at §40827§ the result is X"),
            "Looking at  the result is X"
        );
    }

    #[test]
    fn pair_after_bare_digits_is_removed() {
        assert_eq!(strip("2030  §42§ Hello"), "2030   Hello");
    }

    #[test]
    fn malformed_hybrid_is_removed() {
        assert_eq!(
            strip("Hello §40827\">Oracle confirmed"),
            "Hello Oracle confirmed"
        );
    }

    #[test]
    fn malformed_leading_hybrid_is_emptied() {
        assert_eq!(strip("§15298\">§15298§ "), "");
    }

    #[test]
    fn tag_only_text_is_emptied() {
        assert_eq!(strip("§42§ "), "");
    }

    #[test]
    fn stray_section_sign_is_removed() {
        assert_eq!(strip("a § b"), "a  b");
    }

    #[test]
    fn degree_sign_is_not_a_section_sign() {
        let text = "2030  2030  2030° Run clippy";
        assert!(matches!(
            strip_copied_tag_notation(text),
            Cow::Borrowed(_)
        ));
        assert_eq!(strip(text), text);
    }

    #[test]
    fn dangling_tag_with_improvised_closer_is_removed() {
        assert_eq!(strip("§103012$ Fixed it"), "Fixed it");
        assert_eq!(strip("§11865ҩ done"), "done");
    }

    #[test]
    fn dangling_tag_does_not_eat_following_text() {
        assert_eq!(strip("§42 files changed"), "files changed");
        assert_eq!(strip("§42important"), "important");
    }

    #[test]
    fn section_citation_keeps_its_digits() {
        assert_eq!(strip("§5.1"), "5.1");
        assert_eq!(strip("§42.5x"), "42.5x");
        assert_eq!(strip("§5.1.2"), "5.1.2");
    }

    #[test]
    fn multiple_dangling_tags_are_all_removed() {
        assert_eq!(strip("§1§ §2§ §3§ done"), "done");
        assert_eq!(strip("before §7$ after"), "before  after");
    }
}
