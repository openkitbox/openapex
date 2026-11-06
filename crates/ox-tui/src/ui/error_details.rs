//! The error details view (upstream `component/dialog-error-details.tsx:17-130`
//! and `util/error-details.ts:1-21`).
//!
//! [`ErrorDetails`] holds one error and the reader's scroll position. The copy
//! text and the investigate prompt are pure functions of it; the view does no
//! I/O and knows nothing about who failed. The owner turns [`DetailsKey::Copy`]
//! and [`DetailsKey::Investigate`] into a clipboard write and a draft.
//!
//! Deviations from upstream:
//! - D4: `✓ copied` shows as soon as the owner issues the copy
//!   ([`ErrorDetails::set_copied`]); upstream waits for the clipboard write to
//!   succeed. ox reports the clipboard result through a global toast.
//! - D5: no `source` line and no `diagnosticRef` (`Reference: …`), so neither
//!   the copy text nor the prompt carries one; nothing in ox produces them.

use std::cell::{Cell, RefCell};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::dialog::{TEXT_INSET, dim_backdrop, put};
use super::output_dialog;
use crate::theme::Palette;

/// `dialogWidth("medium")` (`ui/dialog.tsx:30-34`); the MCP dialog resets to
/// it when the details close (`dialog-mcp.tsx:170`).
const PANEL_WIDTH: u16 = 60;
/// The body scrollbox's `maxHeight={20}`, also the page step of
/// `pageup`/`pagedown` (`dialog-error-details.tsx:72-77, :116`).
const BODY_ROWS: usize = 20;
/// Rows around the body: padding, header, gap, gap, footer, padding.
const CHROME_ROWS: usize = 6;
const PROMPT_LEAD: &str = "Investigate why this OpenCode component failed in the current project.";
const PROMPT_TAIL: &str = "Inspect the relevant project and global OpenCode configuration, startup or loading behavior, required environment variables or credentials, dependencies, and logs. Identify the root cause and recommend a fix. Do not expose credentials.";

/// What a key did to the details view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailsKey {
    /// `esc`: return to whatever opened the view.
    Back,
    /// `c`: copy [`ErrorDetails::text`].
    Copy,
    /// `i`: start a session drafted with [`ErrorDetails::prompt`].
    Investigate,
    /// The body scrolled.
    Changed,
    /// The key had no effect.
    Idle,
}

/// One error, ready to read, copy or investigate.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorDetails {
    title: String,
    error: String,
    /// Extra lines for the copy text and the prompt; not drawn (upstream shows
    /// only the title and the error).
    context: Option<String>,
    copied: bool,
    /// First visible body line, clamped by every render.
    top: Cell<usize>,
    /// Largest useful `top`, as of the last render.
    max_top: Cell<usize>,
    /// `error` wrapped to the width in the first field.
    wrapped: RefCell<Option<(usize, Vec<String>)>>,
}

impl ErrorDetails {
    /// Details titled `title` for `error`; `context` adds lines to the copy
    /// text and the investigate prompt only.
    #[must_use]
    pub fn new(title: String, error: String, context: Option<String>) -> Self {
        Self {
            title,
            error,
            context,
            copied: false,
            top: Cell::new(0),
            max_top: Cell::new(0),
            wrapped: RefCell::new(None),
        }
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The copy text: title, context, then `Error: <error>`
    /// (`errorDetails().text`).
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = self.title.clone();
        if let Some(context) = self
            .context
            .as_deref()
            .filter(|context| !context.is_empty())
        {
            text.push('\n');
            text.push_str(context);
        }
        text.push_str("\nError: ");
        text.push_str(&self.error);
        text
    }

    /// The investigate draft (`errorDetails().prompt`).
    #[must_use]
    pub fn prompt(&self) -> String {
        format!("{PROMPT_LEAD}\n\n{}\n\n{PROMPT_TAIL}", self.text())
    }

    /// Show `✓ copied` in place of the copy hint.
    pub fn set_copied(&mut self) {
        self.copied = true;
    }

    /// A key while the view is open: `esc` goes back, `c` copies, `i`
    /// investigates, the arrows, page keys, home and end scroll the body.
    pub fn on_key(&mut self, key: KeyEvent) -> DetailsKey {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let here = self.top.get().min(self.max_top.get());
        match key.code {
            KeyCode::Esc => DetailsKey::Back,
            KeyCode::Char('c') if plain => DetailsKey::Copy,
            KeyCode::Char('i') if plain => DetailsKey::Investigate,
            KeyCode::Up => self.scroll_to(here.saturating_sub(1)),
            KeyCode::Down => self.scroll_to(here.saturating_add(1)),
            KeyCode::PageUp => self.scroll_to(here.saturating_sub(BODY_ROWS)),
            KeyCode::PageDown => self.scroll_to(here.saturating_add(BODY_ROWS)),
            KeyCode::Home => self.scroll_to(0),
            KeyCode::End => self.scroll_to(usize::MAX),
            _ => DetailsKey::Idle,
        }
    }

    fn scroll_to(&mut self, target: usize) -> DetailsKey {
        let next = target.min(self.max_top.get());
        if next == self.top.get() {
            return DetailsKey::Idle;
        }
        self.top.set(next);
        DetailsKey::Changed
    }
}

/// `text` word-wrapped to `width` cells; blank lines survive and a word longer
/// than the width is broken (`wrapMode="word"`).
#[must_use]
fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let options = textwrap::Options::new(width.max(1))
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push(String::new());
        } else {
            out.extend(textwrap::wrap(line, &options).into_iter().map(String::from));
        }
    }
    out
}

/// Draw the view over `area`: dimmed backdrop, a raised panel with the bold
/// title and `esc`, the scrolled error body, and the legacy footer
/// (`investigate i`, `copy details c` on the left, `scroll ↑/↓` on the right).
pub fn draw(buf: &mut Buffer, area: Rect, details: &ErrorDetails, pal: &Palette) {
    dim_backdrop(buf, area, pal);
    let width = PANEL_WIDTH.min(area.width.saturating_sub(2));
    if width <= TEXT_INSET * 2 || area.height < 8 {
        return;
    }
    let span = usize::from(width - TEXT_INSET * 2);
    let mut cache = details.wrapped.borrow_mut();
    if cache.as_ref().is_none_or(|(cached, _)| *cached != span) {
        // MCP stderr carries ANSI colours and tabs; wrap printable text only.
        *cache = Some((
            span,
            wrap_lines(&output_dialog::clean(&details.error), span),
        ));
    }
    let lines = cache
        .as_ref()
        .map_or(&[][..], |(_, lines)| lines.as_slice());

    // Same vertical placement as the select dialog: yoga rounds `height / 4`.
    let top = area.y + ((u32::from(area.height) * 2 + 4) / 8) as u16;
    let free = usize::from(area.bottom().saturating_sub(top)).saturating_sub(CHROME_ROWS);
    let body_h = lines.len().clamp(1, BODY_ROWS).min(free.max(1));
    let height = u16::try_from(body_h + CHROME_ROWS)
        .map_or(area.bottom() - top, |h| h.min(area.bottom() - top));
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: top,
        width,
        height,
    };
    Clear.render(panel, buf);
    buf.set_style(panel, pal.raised_base);

    let text_x = panel.x + TEXT_INSET;
    let right = panel.right() - TEXT_INSET;
    let line_w = right - text_x;

    // Header: title left, `esc` right.
    let esc_x = right - 3;
    put(
        buf,
        panel,
        text_x,
        top + 1,
        esc_x.saturating_sub(text_x + 1),
        &details.title,
        pal.text.add_modifier(Modifier::BOLD),
    );
    put(buf, panel, esc_x, top + 1, 3, "esc", pal.dim);

    let max_top = lines.len().saturating_sub(body_h);
    let first = details.top.get().min(max_top);
    details.top.set(first);
    details.max_top.set(max_top);
    let body_y = top + 3;
    for (row, line) in lines.iter().skip(first).take(body_h).enumerate() {
        put(
            buf,
            panel,
            text_x,
            body_y + row as u16,
            line_w,
            line,
            pal.text,
        );
    }

    // Footer: actions left (gap 2), the scroll hint right.
    let y = body_y + body_h as u16 + 1;
    let mut x = put(buf, panel, text_x, y, line_w, "investigate", pal.text);
    x = put(buf, panel, x, y, right.saturating_sub(x), " i", pal.dim) + 2;
    if details.copied {
        x = put(
            buf,
            panel,
            x,
            y,
            right.saturating_sub(x),
            "\u{2713} copied",
            pal.success,
        );
    } else {
        x = put(
            buf,
            panel,
            x,
            y,
            right.saturating_sub(x),
            "copy details",
            pal.text,
        );
        x = put(buf, panel, x, y, right.saturating_sub(x), " c", pal.dim);
    }
    let scroll = "scroll";
    let keys = " \u{2191}/\u{2193}";
    let wanted = (scroll.width() + keys.width()) as u16;
    let scroll_x = right.saturating_sub(wanted);
    if scroll_x >= x + 2 {
        let after = put(buf, panel, scroll_x, y, wanted, scroll, pal.text);
        put(
            buf,
            panel,
            after,
            y,
            right.saturating_sub(after),
            keys,
            pal.dim,
        );
    }
}
