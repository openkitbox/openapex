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

#[cfg(test)]
mod tests {
    use ox_client::store::CompactionStatus;
    use ox_schema::support::{TokenCache, TokenUsage};
    use ratatui::text::Line;

    use super::{View, lines, queued_line};

    fn tokens(input: i64, output: i64, read: i64, write: i64, reasoning: i64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            reasoning,
            cache: TokenCache { read, write },
            estimated: None,
        }
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn view<'a>(status: CompactionStatus, body: &'a str, t: Option<&'a TokenUsage>) -> View<'a> {
        View {
            status,
            text: body,
            cancelled: false,
            tokens: t,
            spinner: '⠙',
        }
    }

    #[test]
    fn running_shows_the_spinner_frame_and_no_usage() {
        let t = tokens(100, 50, 0, 0, 0);
        let rows = text(&lines(&view(CompactionStatus::Running, "", Some(&t)), 40));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains(" ⠙ Compaction "), "{}", rows[0]);
        assert!(!rows[0].contains(" in "), "{}", rows[0]);
    }

    #[test]
    fn the_queued_row_is_a_centred_rule_in_the_border_colour() {
        let palette = super::theme::palette();
        let line = queued_line(30);
        let rows = text(std::slice::from_ref(&line));
        // " ◇ Compaction queued " is 21 wide; 9 free → 5 left, 4 right.
        assert_eq!(
            rows[0],
            format!("{} ◇ Compaction queued {}", "─".repeat(5), "─".repeat(4))
        );
        assert_eq!(line.spans[0].style, palette.panel_border);
        assert_eq!(line.spans[1].style, palette.dim);
    }

    #[test]
    fn completed_reports_input_including_cache_and_output_including_reasoning() {
        let t = tokens(1_000, 200, 300, 100, 50);
        let rows = text(&lines(&view(CompactionStatus::Completed, "", Some(&t)), 60));
        assert!(
            rows[0].contains(" Compaction · 1.4K in · 250 out "),
            "{}",
            rows[0]
        );
    }

    #[test]
    fn zero_usage_is_omitted() {
        let t = tokens(0, 0, 0, 0, 0);
        let rows = text(&lines(&view(CompactionStatus::Completed, "", Some(&t)), 40));
        assert!(rows[0].contains(" Compaction "), "{}", rows[0]);
        assert!(!rows[0].contains('·'), "{}", rows[0]);
    }

    #[test]
    fn the_label_is_centred_with_the_odd_column_on_the_left() {
        let rows = text(&lines(&view(CompactionStatus::Completed, "", None), 25));
        // " Compaction " is 12 wide; 13 free → 7 left, 6 right.
        assert_eq!(
            rows[0],
            format!("{} Compaction {}", "─".repeat(7), "─".repeat(6))
        );
        assert_eq!(unicode_width::UnicodeWidthStr::width(rows[0].as_str()), 25);
    }

    #[test]
    fn failure_marks_the_label_and_uses_the_error_colour() {
        let palette = super::theme::palette();
        let v = view(CompactionStatus::Failed, "provider exploded", None);
        let out = lines(&v, 50);
        let rows = text(&out);
        assert!(rows[0].contains(" ✗ Compaction "), "{}", rows[0]);
        assert_eq!(out[0].spans[1].style.fg, palette.warn.fg);
        assert!(
            rows.iter().any(|r| r.contains("provider exploded")),
            "{rows:?}"
        );
    }

    #[test]
    fn cancel_says_so_keeps_the_muted_colour_and_hides_the_body() {
        let palette = super::theme::palette();
        let v = View {
            status: CompactionStatus::Failed,
            text: "aborted by user",
            cancelled: true,
            tokens: None,
            spinner: '⠋',
        };
        let out = lines(&v, 50);
        let rows = text(&out);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains(" Compaction · cancelled "), "{}", rows[0]);
        assert!(!rows[0].contains('✗'), "{}", rows[0]);
        assert_eq!(out[0].spans[1].style.fg, palette.dim.fg);
    }

    #[test]
    fn the_summary_follows_a_blank_row_indented_three_columns() {
        let rows = text(&lines(
            &view(CompactionStatus::Completed, "Goal: ship the thing", None),
            50,
        ));
        assert_eq!(rows[1], "");
        assert_eq!(rows[2].trim_end(), "   Goal: ship the thing");
    }
}
