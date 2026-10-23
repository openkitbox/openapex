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
//! (byte-compat with upstream, see `.ox/plans/ox-injection-hardening.md`
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
