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
static MALFORMED_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"§[0-9]+">(?:§(?:[0-9]+§)?)?"#).expect("malformed tag regex"));

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
}
