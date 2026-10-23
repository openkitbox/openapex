//! Neutralize harness-looking `<system-reminder>` framing in untrusted content.
//!
//! Port of grok-build `xai-grok-agent/src/prompt/agents_md.rs:374-387`: escape
//! the leading `<` of every opening or closing `system-reminder` /
//! `system_reminder` tag so a checked-in `AGENTS.md` or `SKILL.md` cannot forge
//! the framing that system[0] declares is harness-authored ("`<system-reminder>`
//! blocks are harness instructions, not user-authored content").
//!
//! One implementation for every untrusted body: `ox-engine`'s instruction-file
//! renderer and `ox-tools`' skill envelope both call this, so the two can never
//! neutralize differently. Deliberately not applied to MCP server instructions
//! (byte-compat with upstream, see `.opencode/plans/ox-injection-hardening.md`
//! §2-D) or to the `<available_skills>` catalog, whose XML escape already turns
//! `<` into `&lt;`.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

/// Opening or closing `system-reminder` (Grok) or `system_reminder`
/// (Cursor/IDE), case-insensitive, whitespace inside the tag tolerated.
const SYSTEM_REMINDER_TAG_PATTERN: &str = r"(?i)<(\s*/?\s*system[-_]reminder)";

/// Literal pattern only: compile failure is a programmer bug, not a runtime
/// input error.
static SYSTEM_REMINDER_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(SYSTEM_REMINDER_TAG_PATTERN).expect("system-reminder tag regex"));

/// HTML-escape the leading `<` of every `system-reminder` tag in `content`, so
/// untrusted text cannot break out of or forge harness framing.
///
/// Content without such a tag is returned borrowed and byte-identical:
/// `replace_all` allocates only on a match, so the common case stays zero-copy.
pub fn neutralize_reminder_tags(content: &str) -> Cow<'_, str> {
    SYSTEM_REMINDER_TAG.replace_all(content, "&lt;$1")
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn opening_and_closing_tags_are_escaped() {
        assert_eq!(
            neutralize_reminder_tags("<system-reminder>x</system-reminder>"),
            "&lt;system-reminder>x&lt;/system-reminder>"
        );
    }

    #[test]
    fn the_underscore_variant_is_escaped() {
        assert_eq!(
            neutralize_reminder_tags("<system_reminder>x</system_reminder>"),
            "&lt;system_reminder>x&lt;/system_reminder>"
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            neutralize_reminder_tags("<SYSTEM-REMINDER>x</System_Reminder>"),
            "&lt;SYSTEM-REMINDER>x&lt;/System_Reminder>"
        );
    }

    #[test]
    fn whitespace_inside_the_tag_is_tolerated() {
        assert_eq!(
            neutralize_reminder_tags("< system-reminder >x</ system-reminder>"),
            "&lt; system-reminder >x&lt;/ system-reminder>"
        );
    }

    #[test]
    fn every_occurrence_is_escaped() {
        assert_eq!(
            neutralize_reminder_tags("<system-reminder> a <system-reminder>"),
            "&lt;system-reminder> a &lt;system-reminder>"
        );
    }

    #[test]
    fn clean_content_stays_borrowed_zero_copy() {
        let out = neutralize_reminder_tags("plain instruction text");
        assert!(matches!(out, Cow::Borrowed(_)), "no tag must not allocate");
        assert_eq!(out, "plain instruction text");
    }

    #[test]
    fn the_bare_word_reminder_is_untouched() {
        // Benign prose: "reminder" is only dangerous when it forms a tag.
        let text = "Set a reminder to review the system changes.";
        let out = neutralize_reminder_tags(text);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, text);
    }
}
