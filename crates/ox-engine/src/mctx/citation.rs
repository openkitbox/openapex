//! `<mem-used>` citation feedback: the model marks the memories that shaped
//! a reply, the harness strips the mark and stamps the rows as used.
//!
//! A pinboard memory is injected on every turn, so the store never learns
//! whether the model read it; `ctx_search` hits are the only retrievals that
//! reach [`memory::record_use`](crate::mctx::memory::record_use). The static
//! guidance ([`crate::mctx::CTX_GUIDANCE`]) asks the model to end a reply
//! that relied on `<project-memory>` / `<user-profile>` entries with
//! `<mem-used>#12, #40</mem-used>`; this module owns the two sides of that
//! contract:
//!
//! - [`CitationGate`] keeps the mark out of the streamed deltas, so a live
//!   client never shows it (the reply's final text goes through [`strip`] at
//!   the same boundary that strips copied tag notation).
//! - [`strip`] removes every mark from the finished text and reports the ids
//!   it carried; [`record`] turns them into `last_used_at` stamps, the signal
//!   the expiry pass ([`crate::mctx::dreamer`]'s `expire`) and the render
//!   order already read.
//!
//! Ids the mark names that are not active memories of the session's owner
//! (or the user tier) are ignored by the store, so a hallucinated id costs
//! one no-op `UPDATE`.

use std::borrow::Cow;
use std::sync::LazyLock;

use ox_db::Db;
use regex::Regex;

use crate::mctx::memory::record_use;

/// The opening mark, as the model is told to write it.
const OPEN: &str = "<mem-used>";
/// The closing mark.
const CLOSE: &str = "</mem-used>";

/// A whole mark with the whitespace before it and, when nothing follows, the
/// whitespace after it: stripping the usual trailing mark leaves no dangling
/// blank lines, while a mark inside the text keeps the line break after it.
static CITATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\s*<mem-used>(.*?)</mem-used>[ \t]*(?:\s+$)?").expect("citation regex")
});

/// Strip every citation mark from a finished reply.
///
/// Returns the text as the user should see it (borrowed and byte-identical
/// when there is no mark) and the memory ids the marks named, deduplicated in
/// order of appearance. A mark's body is `#12, #40` or `12 40`: tokens split
/// on commas and whitespace, an optional leading `#`, digits only — anything
/// else is not a memory id and is dropped.
#[must_use]
pub(crate) fn strip(text: &str) -> (Cow<'_, str>, Vec<String>) {
    if !text.contains(OPEN) {
        return (Cow::Borrowed(text), Vec::new());
    }
    let mut ids: Vec<String> = Vec::new();
    for capture in CITATION.captures_iter(text) {
        for token in capture[1].split([',', ' ', '\t', '\n', '\r']) {
            let id = token.trim().trim_start_matches('#');
            if !id.is_empty()
                && id.bytes().all(|byte| byte.is_ascii_digit())
                && !ids.iter().any(|seen| seen == id)
            {
                ids.push(id.to_owned());
            }
        }
    }
    let cleaned = CITATION.replace_all(text, "");
    (cleaned, ids)
}

/// Stamp the cited memories as used now. A failure is logged, not returned:
/// the citation is a by-product of the reply and must never fail the step.
pub(crate) async fn record(db: &Db, project: &str, ids: &[String], now: i64) {
    if ids.is_empty() {
        return;
    }
    let borrowed: Vec<&str> = ids.iter().map(String::as_str).collect();
    if let Err(error) = record_use(db, project, &borrowed, now).await {
        tracing::warn!(error = %error, "mem-used: record memory use failed");
    }
    tracing::debug!(count = ids.len(), "mem-used: memories cited");
}

/// A streaming filter that withholds a citation mark from the deltas.
///
/// Text flows through unchanged until a `<` that could begin [`OPEN`]
/// arrives; from there the gate holds the bytes back until they either stop
/// matching (released as ordinary text) or complete a mark (dropped through
/// its [`CLOSE`]). An opened mark the stream never closes is released by
/// [`Self::finish`], so no text is lost — the final text is cleaned by
/// [`strip`] anyway. Holds at most the unfinished mark, never the reply.
#[derive(Debug, Default)]
pub(crate) struct CitationGate {
    held: String,
}

impl CitationGate {
    /// Admit one delta: the text safe to show now, or `None` when all of it
    /// is held back. A delta with no `<` passes through without a copy.
    pub(crate) fn admit(&mut self, delta: String) -> Option<String> {
        if self.held.is_empty() && !delta.contains('<') {
            return (!delta.is_empty()).then_some(delta);
        }
        self.held.push_str(&delta);
        let mut out = String::new();
        loop {
            let Some(start) = self.held.find('<') else {
                out.push_str(&self.held);
                self.held.clear();
                break;
            };
            out.push_str(&self.held[..start]);
            self.held.drain(..start);
            match self.classify() {
                Held::Partial => break,
                Held::NotAMark => {
                    out.push('<');
                    self.held.drain(..1);
                }
                Held::Closed(end) => {
                    self.held.drain(..end);
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Release whatever the gate still holds when the stream ends. The gate
    /// is empty afterwards, so a second call (another flush path) is a no-op.
    pub(crate) fn finish(&mut self) -> Option<String> {
        (!self.held.is_empty()).then(|| std::mem::take(&mut self.held))
    }

    /// What the held text (which starts with `<`) is so far.
    fn classify(&self) -> Held {
        let held = self.held.as_str();
        if held.len() < OPEN.len() {
            return if OPEN.starts_with(held) {
                Held::Partial
            } else {
                Held::NotAMark
            };
        }
        if !held.starts_with(OPEN) {
            return Held::NotAMark;
        }
        match held[OPEN.len()..].find(CLOSE) {
            Some(at) => Held::Closed(OPEN.len() + at + CLOSE.len()),
            None => Held::Partial,
        }
    }
}

/// The gate's reading of its held text.
enum Held {
    /// Could still become (or complete) a mark: keep holding.
    Partial,
    /// Cannot be a mark: the leading `<` is ordinary text.
    NotAMark,
    /// A complete mark ends at this byte offset: drop it.
    Closed(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_marks_and_collects_ids() {
        let (text, ids) = strip("Use rustls.\n\n<mem-used>#12, #40 7</mem-used>\n");
        assert_eq!(text, "Use rustls.");
        assert_eq!(ids, ["12", "40", "7"]);
    }

    #[test]
    fn strip_is_borrowed_without_a_mark() {
        let (text, ids) = strip("plain");
        assert!(matches!(text, Cow::Borrowed(_)));
        assert!(ids.is_empty());
    }

    #[test]
    fn gate_passes_plain_text_and_drops_split_marks() {
        let mut gate = CitationGate::default();
        assert_eq!(gate.admit("hello ".into()).as_deref(), Some("hello "));
        assert_eq!(gate.admit("<mem-".into()), None);
        assert_eq!(gate.admit("used>#1</mem-".into()), None);
        assert_eq!(gate.admit("used> tail".into()).as_deref(), Some(" tail"));
        assert_eq!(gate.finish(), None);
    }

    #[test]
    fn gate_releases_ordinary_angle_brackets() {
        let mut gate = CitationGate::default();
        assert_eq!(
            gate.admit("a <b> c <me".into()).as_deref(),
            Some("a <b> c ")
        );
        assert_eq!(gate.admit("mo>".into()).as_deref(), Some("<memo>"));
        let mut open = CitationGate::default();
        assert_eq!(open.admit("x <mem-used>#1".into()).as_deref(), Some("x "));
        assert_eq!(open.finish().as_deref(), Some("<mem-used>#1"));
    }
}
