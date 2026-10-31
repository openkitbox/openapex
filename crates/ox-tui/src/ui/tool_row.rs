//! One inline tool row: upstream `InlineTool` (`routes/session/index.tsx:2789`)
//! drawn through `InlineToolRow` (`message-parts.tsx:202`). Pure: the row's
//! texts, its state and the spinner glyph in, lines out.
//!
//! Deviation: upstream's `denied` (strikethrough for rejected permissions)
//! is not ported; such a call renders as failed.

use ox_client::store::ToolState;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;
use crate::ui::paste;

/// What one row says, borrowed from the block it draws.
#[derive(Debug, Clone, Copy)]
pub struct InlineSpec<'a> {
    /// Shown once the call completed or failed.
    pub icon: &'a str,
    /// Shown next to the spinner while there is nothing complete to say.
    pub pending: &'a str,
    /// The finished label; `None` keeps the spinner and `pending`.
    pub complete: Option<&'a str>,
    /// The failure message, shown below the row when expanded.
    pub error: Option<&'a str>,
}

/// How the row is drawn right now.
#[derive(Debug, Clone, Copy)]
pub struct RowView {
    pub state: ToolState,
    /// The current spinner glyph.
    pub spinner: char,
    /// The pointer is over a clickable row.
    pub hovered: bool,
    /// A failed row lists its error below.
    pub error_expanded: bool,
}

/// Columns before the icon (`paddingLeft={3}`).
const INDENT: usize = 3;
/// The icon cell, glyph plus gap (`INLINE_TOOL_ICON_WIDTH`).
const ICON_WIDTH: usize = 2;

/// The row, then the wrapped error below it when a failed row is expanded.
/// The head line is cut to `width` with `…`.
#[must_use]
pub fn lines(spec: &InlineSpec<'_>, view: RowView, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let failed = matches!(view.state, ToolState::Failed);
    let done = matches!(view.state, ToolState::Success | ToolState::Failed);
    // Upstream `fg`: failed wins, then hover, else muted.
    let style = if failed {
        palette.warn
    } else if view.hovered {
        palette.text
    } else {
        palette.dim
    };
    let head = match spec.complete.filter(|_| done) {
        Some(text) => format!("{:INDENT$}{} {text}", "", spec.icon),
        None => format!("{:INDENT$}{} {}", "", view.spinner, spec.pending),
    };
    let mut out = vec![Line::from(Span::styled(
        cut(&head, usize::from(width)),
        style,
    ))];
    if failed
        && view.error_expanded
        && let Some(error) = spec.error
    {
        let pad = INDENT + ICON_WIDTH;
        let usable = usize::from(width).saturating_sub(pad).max(1);
        let options = textwrap::Options::new(usable)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);
        let clean = paste::normalize(error);
        for line in clean.lines() {
            for part in textwrap::wrap(line, &options) {
                out.push(Line::from(Span::styled(
                    format!("{:pad$}{part}", ""),
                    palette.warn,
                )));
            }
        }
    }
    out
}

/// `text` within `width` display columns, ending in `…` when cut.
fn cut(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        kept.push(ch);
        used += w;
    }
    kept.push('\u{2026}');
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn spec<'a>(complete: Option<&'a str>, error: Option<&'a str>) -> InlineSpec<'a> {
        InlineSpec {
            icon: "\u{2699}",
            pending: "Updating todos\u{2026}",
            complete,
            error,
        }
    }

    fn view(state: ToolState) -> RowView {
        RowView {
            state,
            spinner: '\u{2819}',
            hovered: false,
            error_expanded: false,
        }
    }

    #[test]
    fn a_running_row_shows_the_spinner_frame_and_pending_text_muted() {
        let palette = theme::palette();
        for state in [ToolState::Input, ToolState::Running, ToolState::Progress] {
            let out = lines(&spec(Some("done"), None), view(state), 40);
            assert_eq!(
                text(&out),
                ["   \u{2819} Updating todos\u{2026}"],
                "state {state:?}"
            );
            assert_eq!(out[0].spans[0].style.fg, palette.dim.fg, "state {state:?}");
        }
    }

    #[test]
    fn a_completed_row_shows_icon_and_text_muted() {
        let palette = theme::palette();
        let out = lines(
            &spec(Some("Cleared todos"), None),
            view(ToolState::Success),
            40,
        );
        assert_eq!(text(&out), ["   \u{2699} Cleared todos"], "icon then text");
        assert!(
            out[0].spans.iter().all(|s| s.style.fg == palette.dim.fg),
            "muted: {out:?}"
        );
    }

    #[test]
    fn a_hovered_completed_row_uses_the_text_colour() {
        let palette = theme::palette();
        let mut v = view(ToolState::Success);
        v.hovered = true;
        let out = lines(&spec(Some("Cleared todos"), None), v, 40);
        assert_eq!(out[0].spans[0].style.fg, palette.text.fg, "{out:?}");
    }

    #[test]
    fn a_done_row_without_complete_text_keeps_the_spinner() {
        let out = lines(&spec(None, None), view(ToolState::Success), 40);
        assert_eq!(
            text(&out),
            ["   \u{2819} Updating todos\u{2026}"],
            "upstream fallback"
        );
    }

    #[test]
    fn a_failed_row_is_collapsed_and_error_coloured() {
        let palette = theme::palette();
        let out = lines(
            &spec(Some("Todo update failed"), Some("boom")),
            view(ToolState::Failed),
            40,
        );
        assert_eq!(text(&out), ["   \u{2699} Todo update failed"], "collapsed");
        assert!(
            out[0].spans.iter().all(|s| s.style.fg == palette.warn.fg),
            "error colour wins over hover and muted: {out:?}"
        );
    }

    #[test]
    fn an_expanded_failed_row_lists_the_ansi_free_wrapped_error() {
        let palette = theme::palette();
        let mut v = view(ToolState::Failed);
        v.hovered = true;
        v.error_expanded = true;
        let out = lines(
            &spec(
                Some("Todo update failed"),
                Some("\u{1b}[31mone two three\u{1b}[0m four"),
            ),
            v,
            14,
        );
        let rows = text(&out);
        assert_eq!(
            rows[0], "   \u{2699} Todo upd\u{2026}",
            "header fits 14 columns"
        );
        assert_eq!(
            &rows[1..],
            ["     one two", "     three", "     four"],
            "{rows:?}"
        );
        assert!(
            out.iter()
                .flat_map(|l| l.spans.iter())
                .all(|s| s.style.fg == palette.warn.fg),
            "{out:?}"
        );
    }

    #[test]
    fn expansion_only_applies_to_failed_rows() {
        let mut v = view(ToolState::Success);
        v.error_expanded = true;
        let out = lines(&spec(Some("ok"), Some("output")), v, 40);
        assert_eq!(out.len(), 1, "{:?}", text(&out));
    }

    #[test]
    fn the_header_is_cut_to_the_width() {
        let out = lines(
            &spec(Some("a very long label that will not fit"), None),
            view(ToolState::Success),
            20,
        );
        let row = &text(&out)[0];
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(row.as_str()),
            20,
            "{row:?}"
        );
        assert!(row.ends_with('\u{2026}'), "{row:?}");
    }
}
