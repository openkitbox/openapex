//! The compaction boundary: upstream `CompactionMessage`
//! (`routes/session/index.tsx:2167`). A rule with the label centred in it,
//! then the summary as markdown; and the `CompactionQueued` row (:2226) shown
//! while a compaction waits in the inbox. Pure: item fields and width in,
//! lines out.
//!
//! Deviation: the `Provider compaction` label (`native.messages`) is not
//! ported; the wire item carries no `native` field.

use ox_client::store::CompactionStatus;
use ox_schema::support::TokenUsage;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::dock::format_tokens;
use crate::theme;

/// What `render` needs from one boundary.
pub struct View<'a> {
    pub status: CompactionStatus,
    pub text: &'a str,
    pub cancelled: bool,
    pub tokens: Option<&'a TokenUsage>,
    /// The spinner glyph drawn while the compaction runs (the transcript's
    /// 80ms frame).
    pub spinner: char,
}

/// `Locale.number(input) in · Locale.number(output) out`; absent while the
/// compaction runs, without tokens, or when both counts are zero
/// (index.tsx:2179).
fn usage(view: &View<'_>) -> Option<String> {
    if view.status == CompactionStatus::Running {
        return None;
    }
    let tokens = view.tokens?;
    let input = tokens.input + tokens.cache.read + tokens.cache.write;
    let output = tokens.output + tokens.reasoning;
    (input + output > 0).then(|| {
        format!(
            "{} in \u{b7} {} out",
            format_tokens(input),
            format_tokens(output)
        )
    })
}

#[must_use]
pub fn lines(view: &View<'_>, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let failed = view.status == CompactionStatus::Failed && !view.cancelled;
    let ink: Style = if failed { palette.warn } else { palette.dim };

    let mut parts: Vec<String> = Vec::new();
    match view.status {
        CompactionStatus::Running => parts.push(view.spinner.into()),
        CompactionStatus::Failed if !view.cancelled => parts.push("\u{2717}".into()),
        _ => {}
    }
    parts.push("Compaction".into());
    if view.cancelled {
        parts.push("\u{b7} cancelled".into());
    }
    if let Some(usage) = usage(view) {
        parts.push(format!("\u{b7} {usage}"));
    }
    let label = format!(" {} ", parts.join(" "));
    let mut lines = vec![rule(label, ink, ink, width)];

    let body = view.text.trim();
    if !body.is_empty() && !view.cancelled {
        lines.push(Line::default());
        lines.extend(super::markdown::render(body, width, 3));
    }
    lines
}

/// Upstream `CompactionQueued`: `── ◇ Compaction queued ──`, the rules in the
/// border colour and the label muted.
#[must_use]
pub fn queued_line(width: u16) -> Line<'static> {
    let palette = theme::palette();
    rule(
        " \u{25c7} Compaction queued ".to_owned(),
        palette.panel_border,
        palette.dim,
        width,
    )
}

/// `label` centred in a full-width rule. Two equal grow boxes share what the
/// label leaves; yoga rounds the odd column onto the left rule.
fn rule(label: String, border: Style, ink: Style, width: u16) -> Line<'static> {
    let free = (width as usize).saturating_sub(label.width());
    let left = free.div_ceil(2);
    let right = free - left;
    Line::from(vec![
        Span::styled("\u{2500}".repeat(left), border),
        Span::styled(label, ink),
        Span::styled("\u{2500}".repeat(right), border),
    ])
}
