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
fn clean(raw: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ox_client::rest::{ShellInfo, ShellTail};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use crate::ui::dialog::{Dialog, DialogOutcome, render};

    fn shell(status: &str, exit: Option<i32>, command: &str) -> ShellInfo {
        serde_json::from_value(serde_json::json!({
            "id": "sh_1", "status": status, "command": command,
            "cwd": "/", "shell": "zsh", "file": "/tmp/o", "exit": exit,
            "metadata": {}, "time": { "started": 1 },
        }))
        .expect("shell json")
    }

    fn tail(text: &str, omitted: bool) -> ShellTail {
        ShellTail {
            text: text.to_owned(),
            omitted,
        }
    }

    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

    fn open(text: &str, omitted: bool) -> Dialog {
        Dialog::Output(Output::new(
            &shell("running", None, "tail -f log"),
            tail(text, omitted),
        ))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn screen(dialog: &Dialog, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), dialog))
            .expect("draw");
        rows(terminal.backend().buffer(), width, height)
    }

    fn rows(buf: &Buffer, width: u16, height: u16) -> String {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn clean_strips_ansi_and_normalises_line_ends() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\r\nnext\rlast"), "red\nnext\nlast");
    }

    #[test]
    fn clean_expands_tabs_and_drops_other_control_characters() {
        assert_eq!(clean("a\tb\x07\x08c"), "a    bc");
    }

    #[test]
    fn status_text_names_every_terminal_state() {
        assert_eq!(status_text(&shell("running", None, "x")), "Running");
        assert_eq!(
            status_text(&shell("exited", Some(2), "x")),
            "Exited · code 2"
        );
        assert_eq!(status_text(&shell("exited", None, "x")), "Exited");
        assert_eq!(status_text(&shell("timeout", None, "x")), "Timed out");
        assert_eq!(status_text(&shell("killed", None, "x")), "Killed");
    }

    #[test]
    fn wrap_lines_keeps_blank_lines_and_breaks_long_words() {
        assert_eq!(wrap_lines("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap_lines("abcdefgh", 3), ["abc", "def", "gh"]);
        assert!(wrap_lines("", 10).is_empty());
    }

    #[test]
    fn a_new_viewer_holds_clean_text_and_the_omitted_flag() {
        let output = Output::new(
            &shell("running", None, "ls"),
            tail("\x1b[1mhi\x1b[0m\r\n", true),
        );
        assert_eq!(output.text(), "hi\n");
        assert!(output.omitted());
        assert_eq!(output.shell_id(), "sh_1");
    }

    #[test]
    fn esc_closes_and_ctrl_r_asks_for_a_refresh_of_this_shell() {
        let mut dialog = open("x", false);
        assert_eq!(dialog.on_key(key(KeyCode::Esc)), DialogOutcome::Cancelled);
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(
            dialog.on_key(ctrl_r),
            DialogOutcome::Action {
                key: REFRESH_KEY,
                value: "sh_1".into()
            }
        );
        assert_eq!(dialog.on_key(key(KeyCode::Char('r'))), DialogOutcome::Idle);
        assert_eq!(dialog.tag(), Some(TAG));
        assert_eq!(dialog.title(), "Shell output");
    }

    #[test]
    fn it_starts_at_the_bottom_of_a_long_output() {
        let text = screen(&open(&numbered(100), false), 100, 30);
        assert!(text.contains("line 100"), "{text}");
        assert!(!text.contains("line 1\n"), "{text}");
    }

    #[test]
    fn home_shows_the_top_and_end_follows_the_bottom_again() {
        let mut dialog = open(&numbered(100), false);
        screen(&dialog, 100, 30);
        assert_eq!(dialog.on_key(key(KeyCode::Home)), DialogOutcome::Changed);
        let top = screen(&dialog, 100, 30);
        assert!(top.contains("line 1\n") || top.contains("line 1 "), "{top}");
        assert!(!top.contains("line 100"), "{top}");
        assert_eq!(dialog.on_key(key(KeyCode::End)), DialogOutcome::Changed);
        assert!(screen(&dialog, 100, 30).contains("line 100"));
    }

    #[test]
    fn arrows_and_pages_move_by_a_line_and_a_viewport() {
        let mut dialog = open(&numbered(100), false);
        screen(&dialog, 100, 30);
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        let (max, page) = (output.max_top.get(), output.page.get());
        assert!(page > 3 && max > page, "viewport {page}, max {max}");

        dialog.on_key(key(KeyCode::Home));
        dialog.on_key(key(KeyCode::Down));
        dialog.on_key(key(KeyCode::Down));
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        assert_eq!(output.top.get(), 2);
        dialog.on_key(key(KeyCode::PageDown));
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        assert_eq!(output.top.get(), 2 + page);
        dialog.on_key(key(KeyCode::PageUp));
        dialog.on_key(key(KeyCode::Up));
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        assert_eq!(output.top.get(), 1);
    }

    #[test]
    fn scrolling_down_to_the_end_resumes_following() {
        let mut dialog = open(&numbered(100), false);
        screen(&dialog, 100, 30);
        dialog.on_key(key(KeyCode::Home));
        for _ in 0..200 {
            dialog.on_key(key(KeyCode::PageDown));
        }
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        assert!(output.following(), "past the end means follow");
        assert_eq!(dialog.on_key(key(KeyCode::Down)), DialogOutcome::Idle);
    }

    #[test]
    fn up_from_following_leaves_the_bottom_by_one_line() {
        let mut dialog = open(&numbered(100), false);
        screen(&dialog, 100, 30);
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        let max = output.max_top.get();
        dialog.on_key(key(KeyCode::Up));
        let Dialog::Output(output) = &dialog else {
            panic!("output");
        };
        assert_eq!(output.top.get(), max - 1);
        assert!(!output.following());
    }

    #[test]
    fn a_refresh_keeps_the_scroll_position_of_the_same_shell() {
        let mut old = Output::new(&shell("running", None, "x"), tail(&numbered(100), false));
        old.follow = false;
        old.top.set(7);
        let mut fresh = Output::new(&shell("exited", Some(0), "x"), tail(&numbered(120), false));
        fresh.keep_scroll_of(&old);
        assert_eq!(fresh.top.get(), 7);
        assert_eq!(fresh.text().lines().count(), 120);
    }

    #[test]
    fn the_chrome_shows_title_command_status_note_and_hints() {
        let dialog = Dialog::Output(Output::new(
            &shell("exited", Some(3), "make all\nsecond\nthird\nfourth"),
            tail("done\n", true),
        ));
        let text = screen(&dialog, 100, 30);
        for want in [
            "Shell output",
            "Exited · code 3",
            "make all",
            "third",
            "Earlier output omitted",
            "done",
            "↑/↓ scroll · end follow · esc back · ctrl+r refresh",
        ] {
            assert!(text.contains(want), "missing {want:?}\n{text}");
        }
        assert!(
            !text.contains("fourth"),
            "the command shows three lines\n{text}"
        );
    }

    #[test]
    fn no_note_without_omitted_output_and_an_empty_capture_says_so() {
        let text = screen(&open("", false), 100, 30);
        assert!(!text.contains("Earlier output omitted"), "{text}");
        assert!(text.contains("No captured output"), "{text}");
    }

    #[test]
    fn long_lines_wrap_to_the_panel_width() {
        let long = "word ".repeat(80);
        let text = screen(&open(&long, false), 100, 30);
        let widest = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        assert!(widest <= 100, "{widest}");
        assert!(
            text.lines().filter(|l| l.contains("word word")).count() >= 3,
            "{text}"
        );
    }

    #[test]
    fn it_never_panics_in_a_tiny_area() {
        let dialog = open(&numbered(10), true);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
        for (width, height) in [
            (40, 0),
            (40, 1),
            (0, 0),
            (1, 5),
            (5, 5),
            (19, 3),
            (30, 7),
            (60, 9),
        ] {
            terminal
                .draw(|frame| {
                    let area = ratatui::layout::Rect::new(0, 0, width, height);
                    render(frame, area, &dialog);
                })
                .expect("draw");
        }
    }
}
