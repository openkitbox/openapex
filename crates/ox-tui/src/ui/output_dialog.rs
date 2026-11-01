//! The shell output viewer (upstream `component/dialog-shell-output.tsx`).
//!
//! [`Output`] is a snapshot of one shell's recent output plus its scroll
//! state. Wrapping is derived from the text and the panel width on demand and
//! cached per width; scrolling is a pair of pure moves over the line count the
//! last render saw. Nothing here does I/O: the app fetches, the dialog shows.
//!
//! Deviation from upstream: no live polling. Upstream re-reads the shell every
//! second while the viewer is open; this is a snapshot that `ctrl+r` refreshes.

use std::cell::{Cell, RefCell};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ox_client::rest::{ShellInfo, ShellTail};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::dialog::{DialogOutcome, TEXT_INSET, dim_backdrop, put};
use super::paste;
use crate::theme::Palette;

/// `Dialog::tag` of the viewer.
pub const TAG: &str = "shell-output";
/// Row action key (`ctrl+<key>`): fetch the output again.
pub const REFRESH_KEY: char = 'r';

/// `dialogWidth("xlarge")` (`ui/dialog.tsx:30-34`).
const PANEL_WIDTH: u16 = 116;
/// The command is clamped to three rows (`maxHeight={3}`).
const COMMAND_ROWS: usize = 3;
/// Spaces a tab expands to.
const TAB_WIDTH: usize = 4;
/// Page size before the first render has measured the viewport.
const DEFAULT_PAGE: usize = 10;
const NOTE: &str = "Earlier output omitted \u{b7} showing recent output";
const EMPTY: &str = "No captured output. Output redirected to files is not shown here.";
const HINTS: &str =
    "\u{2191}/\u{2193} scroll \u{b7} end follow \u{b7} esc back \u{b7} ctrl+r refresh";

/// A shell's recent output and where the reader is in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    shell_id: String,
    command: String,
    status: String,
    /// ANSI-free text with `\n` line ends.
    text: String,
    omitted: bool,
    /// Pinned to the newest line; `top` then follows the end.
    follow: bool,
    /// First visible wrapped line, clamped by every render.
    top: Cell<usize>,
    /// Largest useful `top` and the viewport height, as of the last render.
    max_top: Cell<usize>,
    page: Cell<usize>,
    /// `text` wrapped to the width in the first field.
    wrapped: RefCell<Option<(usize, Vec<String>)>>,
}

impl Output {
    #[must_use]
    pub fn new(shell: &ShellInfo, tail: ShellTail) -> Self {
        Self {
            shell_id: shell.id.clone(),
            command: shell.command.clone(),
            status: status_text(shell),
            text: clean(&tail.text),
            omitted: tail.omitted,
            follow: true,
            top: Cell::new(0),
            max_top: Cell::new(0),
            page: Cell::new(DEFAULT_PAGE),
            wrapped: RefCell::new(None),
        }
    }

    #[must_use]
    pub fn shell_id(&self) -> &str {
        &self.shell_id
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn omitted(&self) -> bool {
        self.omitted
    }

    #[must_use]
    pub fn following(&self) -> bool {
        self.follow
    }

    /// Carry the reader's position over from the viewer this one replaces.
    pub fn keep_scroll_of(&mut self, old: &Self) {
        self.follow = old.follow;
        self.top.set(old.top.get());
        self.max_top.set(old.max_top.get());
        self.page.set(old.page.get());
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DialogOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char(REFRESH_KEY) => DialogOutcome::Action {
                    key: REFRESH_KEY,
                    value: self.shell_id.clone(),
                },
                _ => DialogOutcome::Idle,
            };
        }
        let here = self.position();
        let page = self.page.get().max(1);
        match key.code {
            KeyCode::Esc => DialogOutcome::Cancelled,
            KeyCode::Up => self.scroll_to(here.saturating_sub(1)),
            KeyCode::Down => self.scroll_to(here.saturating_add(1)),
            KeyCode::PageUp => self.scroll_to(here.saturating_sub(page)),
            KeyCode::PageDown => self.scroll_to(here.saturating_add(page)),
            KeyCode::Home => self.scroll_to(0),
            KeyCode::End => self.scroll_to(usize::MAX),
            _ => DialogOutcome::Idle,
        }
    }

    /// The first visible line right now.
    fn position(&self) -> usize {
        let max = self.max_top.get();
        if self.follow {
            max
        } else {
            self.top.get().min(max)
        }
    }

    /// Move to `target`; reaching the end resumes following.
    fn scroll_to(&mut self, target: usize) -> DialogOutcome {
        let max = self.max_top.get();
        let before = (self.follow, self.top.get());
        self.follow = target >= max;
        self.top.set(target.min(max));
        if before == (self.follow, self.top.get()) {
            DialogOutcome::Idle
        } else {
            DialogOutcome::Changed
        }
    }
}

/// `Running`, `Exited · code N`, `Timed out` or `Killed` (upstream `status()`).
#[must_use]
fn status_text(shell: &ShellInfo) -> String {
    match (shell.status.as_str(), shell.exit) {
        ("running", _) => "Running".to_owned(),
        ("timeout", _) => "Timed out".to_owned(),
        ("killed", _) => "Killed".to_owned(),
        (_, Some(code)) => format!("Exited \u{b7} code {code}"),
        (_, None) => "Exited".to_owned(),
    }
}

/// Printable text: ANSI sequences gone, line ends as `\n`, tabs expanded and
/// the other control characters dropped (`stripAnsi` + `\r\n?` → `\n`).
#[must_use]
pub(crate) fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in paste::normalize(raw).chars() {
        match ch {
            '\t' => out.push_str(&" ".repeat(TAB_WIDTH)),
            '\n' => out.push('\n'),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// `text` wrapped to `width` cells; blank lines survive and a word longer than
/// the width is broken.
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

/// Draw the viewer over `area`: dimmed backdrop, a centred raised panel with
/// the title and status, the command, the omitted note, the scrolled output
/// and the key hints.
pub fn draw(buf: &mut Buffer, area: Rect, output: &Output, pal: &Palette) {
    dim_backdrop(buf, area, pal);
    let width = PANEL_WIDTH.min(area.width.saturating_sub(2));
    if width <= TEXT_INSET * 2 || area.height < 8 {
        return;
    }
    let span = usize::from(width - TEXT_INSET * 2);
    let command: Vec<String> = wrap_lines(&output.command, span)
        .into_iter()
        .take(COMMAND_ROWS)
        .collect();
    let note_rows = if output.omitted { 2 } else { 0 };
    // Padding, header, gap, command, gap, note + gap, output, gap, hints, padding.
    let fixed = 7 + command.len() + note_rows;
    let rows = usize::from(area.height);
    let wanted = (rows * 6 / 10).saturating_sub(6).max(3);
    let body_h = wanted.min(rows.saturating_sub(fixed)).max(1);
    let height = u16::try_from(fixed + body_h).map_or(area.height, |h| h.min(area.height));
    let top = area.y + (area.height - height) / 2;
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

    // Header: title left, status right.
    let status_w = u16::try_from(output.status.width())
        .unwrap_or(line_w)
        .min(line_w);
    put(
        buf,
        panel,
        text_x,
        top + 1,
        line_w.saturating_sub(status_w + 1),
        "Shell output",
        pal.text.add_modifier(Modifier::BOLD),
    );
    put(
        buf,
        panel,
        right - status_w,
        top + 1,
        status_w,
        &output.status,
        pal.dim,
    );

    let mut y = top + 3;
    for line in &command {
        put(buf, panel, text_x, y, line_w, line, pal.dim);
        y += 1;
    }
    y += 1;
    if output.omitted {
        put(buf, panel, text_x, y, line_w, NOTE, pal.dim);
        y += 2;
    }

    let mut cache = output.wrapped.borrow_mut();
    if cache.as_ref().is_none_or(|(cached, _)| *cached != span) {
        *cache = Some((span, wrap_lines(&output.text, span)));
    }
    let lines = cache
        .as_ref()
        .map_or(&[][..], |(_, lines)| lines.as_slice());
    let max_top = lines.len().saturating_sub(body_h);
    let first = if output.follow {
        max_top
    } else {
        output.top.get().min(max_top)
    };
    output.top.set(first);
    output.max_top.set(max_top);
    output.page.set(body_h);
    if lines.is_empty() {
        put(buf, panel, text_x, y, line_w, EMPTY, pal.dim);
    }
    for (row, line) in lines.iter().skip(first).take(body_h).enumerate() {
        put(buf, panel, text_x, y + row as u16, line_w, line, pal.text);
    }
    put(
        buf,
        panel,
        text_x,
        y + body_h as u16 + 1,
        line_w,
        HINTS,
        pal.dim,
    );
}
