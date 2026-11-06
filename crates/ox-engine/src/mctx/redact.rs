//! Secret redaction for text that is about to become a project memory.
//!
//! [`redact_secrets`] owns one decision: which substrings of a memory's text
//! look like credentials and must not be persisted. It is pure (no I/O, no
//! state) and idempotent: `redact_secrets(redact_secrets(x)) ==
//! redact_secrets(x)`. Text with no match comes back borrowed, uncopied.
//!
//! The memory store functions call it on every write, so no caller can skip
//! it; the dedup paths that compare a candidate against stored rows
//! ([`crate::mctx::promotion`], the dreamer's retrospective) apply it first
//! too, because what they compare against is already redacted. Rows already
//! in the database are not backfilled: text stored before this module existed
//! keeps whatever it contained, and read, render and search paths never
//! redact.
//!
//! # Reference and deviation ledger
//!
//! Ported from Codex `codex-rs/secrets/src/sanitizer.rs`. The Bearer, AWS
//! access key id and assignment patterns, their replacements and the apply
//! order are verbatim apart from the boundary change below. Deliberate
//! deviations:
//!
//! - **`sk-` keys accept `_` and `-`** (`sk-[A-Za-z0-9_-]{20,}` instead of
//!   `sk-[A-Za-z0-9]{20,}`): Anthropic keys (`sk-ant-api03-…`) contain
//!   hyphens, which the Codex pattern cannot match.
//! - **`sk-` gets a leading boundary.** Codex's pattern has none; with the
//!   widened class it would match the inner `sk-` of kebab-case names such
//!   as `task-runner-memory-…`, `risk-…` or `disk-…` and eat the path.
//! - **Every boundary is ASCII (`(?-u:\b)`) instead of Unicode `\b`.**
//!   Unicode `\b` counts CJK ideographs as word characters, so a secret glued
//!   to Chinese text (`密钥sk-…`, `用ghp_…`, `中AKIA…`, `密钥token=…`) would not
//!   match, and memories in this project are often Chinese. This covers the
//!   `sk-`, AWS (both sides), GitHub, Slack, Bearer and assignment-keyword
//!   (both sides) boundaries.
//! - **GitHub tokens** (`gh[pousr]_[A-Za-z0-9]{20,}`) are added: ox sessions
//!   handle `gh` credentials, which Codex's set misses.
//! - **Slack tokens** (`xox[baprs]-[A-Za-z0-9-]{10,}`) are added for the same
//!   reason.
//! - The two added patterns run after the AWS pattern and before the
//!   assignment pattern, keeping Codex's order for the rest.
//!
//! # Known false positives
//!
//! The assignment pattern redacts any value of 8 or more characters after
//! `api_key`/`token`/`secret`/`password` and `:` or `=`, so
//! `password = hunter2abc` is redacted. Shorter values are left alone, so
//! `token: 4096` is not. This is Codex's behavior, kept as is.
//!
//! Because `sk-` needs a leading boundary, `sk-` inside a kebab-case word
//! (`.ox/plans/task-runner-memory-redaction-plan`) is not matched. The price
//! is that a key glued to a preceding ASCII letter or digit (`xsk-…`) is
//! missed.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

const REDACTED: &str = "[REDACTED_SECRET]";

static BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:(?-u:\b)Bearer)[ \t]+[A-Za-z0-9._~+/-]{16,}=*")
        .expect("the Bearer token pattern is a valid constant regex")
});
static SK_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)sk-[A-Za-z0-9_-]{20,}")
        .expect("the sk- key pattern is a valid constant regex")
});
static AWS_ACCESS_KEY_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)AKIA[0-9A-Z]{16}(?-u:\b)")
        .expect("the AWS access key id pattern is a valid constant regex")
});
static GITHUB_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)gh[pousr]_[A-Za-z0-9]{20,}")
        .expect("the GitHub token pattern is a valid constant regex")
});
static SLACK_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)xox[baprs]-[A-Za-z0-9-]{10,}")
        .expect("the Slack token pattern is a valid constant regex")
});
static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?-u:\b)(api[_-]?key|token|secret|password)(?-u:\b)(\s*[:=]\s*)(["']?)[^\s"']{8,}"#,
    )
    .expect("the secret assignment pattern is a valid constant regex")
});

/// `input` with credential-shaped substrings replaced by
/// `[REDACTED_SECRET]`. `Cow::Owned` means at least one replacement happened.
#[must_use]
pub(crate) fn redact_secrets(input: &str) -> Cow<'_, str> {
    let text = Cow::Borrowed(input);
    let text = replace(text, &BEARER_TOKEN, "Bearer [REDACTED_SECRET]");
    let text = replace(text, &SK_KEY, REDACTED);
    let text = replace(text, &AWS_ACCESS_KEY_ID, REDACTED);
    let text = replace(text, &GITHUB_TOKEN, REDACTED);
    let text = replace(text, &SLACK_TOKEN, REDACTED);
    replace(text, &SECRET_ASSIGNMENT, "$1$2$3[REDACTED_SECRET]")
}

/// One pattern pass: keeps `text` (borrowed or not) when nothing changed.
///
/// A match that rewrites to identical bytes (the assignment pattern matches
/// its own `token: [REDACTED_SECRET]` output) counts as no change, so
/// already-redacted text stays `Cow::Borrowed`.
fn replace<'a>(text: Cow<'a, str>, regex: &Regex, replacement: &str) -> Cow<'a, str> {
    let replaced: Option<String> = match regex.replace_all(&text, replacement) {
        Cow::Owned(replaced) if replaced != *text => Some(replaced),
        Cow::Owned(_) | Cow::Borrowed(_) => None,
    };
    match replaced {
        Some(replaced) => Cow::Owned(replaced),
        None => text,
    }
}
