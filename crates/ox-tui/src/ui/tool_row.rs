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
