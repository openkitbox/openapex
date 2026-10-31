use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_textarea::TextArea;

use crate::theme;

/// How long the cursor stays in each phase. Upstream's textarea blinks a block
/// cursor (on, off, on…) and every keystroke restarts the visible phase.
pub const CURSOR_BLINK: Duration = Duration::from_millis(530);

/// The text editor at the bottom of both routes.
///
/// It owns only the textarea and the placeholder (which must survive a submit:
/// the home screen picks one of three rotating phrases and expects it back).
/// Everything about the surrounding panel lives in [`crate::ui::prompt`].
pub struct Composer {
    textarea: TextArea<'static>,
    placeholder: String,
    /// Cursor phase: restarted by a keystroke, toggled by [`Composer::tick`].
    cursor_visible: bool,
    /// When the current phase started (the last keystroke).
    cursor_since: Instant,
}

impl Composer {
    pub fn new() -> Self {
        Self::new_with_placeholder("输入消息…")
    }

    pub fn new_with_placeholder(placeholder: &str) -> Self {
        Self {
            textarea: empty_textarea(),
            placeholder: placeholder.to_owned(),
            cursor_visible: true,
            cursor_since: Instant::now(),
        }
    }

    /// Put a draft back (home route, failed session creation).
    pub fn restore(&mut self, text: &str) {
        self.textarea = empty_textarea();
        self.textarea.insert_str(text);
        self.touch(Instant::now());
    }

    pub fn clear(&mut self) {
        self.textarea = empty_textarea();
        self.touch(Instant::now());
    }

    /// Returns `Some(text)` when the user submits.
    ///
    /// Enter submits; every newline chord the upstream keymap accepts
    /// (`shift+return,ctrl+return,alt+return,ctrl+j`) inserts a newline instead.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        self.touch(Instant::now());
        if is_newline(&key) {
            self.textarea.insert_newline();
            return None;
        }
        if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
            let trimmed = self.text().trim_end().to_owned();
            if trimmed.is_empty() {
                return None;
            }
            self.clear();
            return Some(trimmed);
        }
        self.textarea.input(key);
        None
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.text().is_empty()
    }

    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    /// Swap the rotating hint and drop whatever is typed.
    pub fn set_placeholder(&mut self, placeholder: &str) {
        self.placeholder = placeholder.to_owned();
        self.textarea = empty_textarea();
        self.touch(Instant::now());
    }

    /// Restart the visible phase. A keystroke (or a handled menu key) means the
    /// user is looking at the input, so the cursor comes back.
    pub fn touch(&mut self, now: Instant) {
        self.cursor_since = now;
        self.cursor_visible = true;
    }

    /// Advance the blink clock; `true` when the phase flipped and the frame
    /// needs repainting.
    pub fn tick(&mut self, now: Instant) -> bool {
        let visible = blink_visible(now.saturating_duration_since(self.cursor_since));
        if visible == self.cursor_visible {
            return false;
        }
        self.cursor_visible = visible;
        true
    }

    /// Logical cursor as `(row, column)` in characters.
    pub fn cursor(&self) -> (usize, usize) {
        self.textarea.cursor()
    }

    /// Visible rows: `1..=max(6, terminal_height / 3)` (upstream `maxHeight`).
    pub fn height(&self, terminal_height: u16) -> u16 {
        let max = max_height(terminal_height);
        (self.textarea.lines().len() as u16).clamp(1, max)
    }

    /// Replace the first `chars` characters with `with` (menu completion).
    pub fn replace_leading(&mut self, chars: usize, with: &str) {
        self.textarea.move_cursor(tui_textarea::CursorMove::Head);
        self.textarea.delete_str(chars);
        self.textarea.insert_str(with);
    }

    /// Drop the first `chars` characters (ctrl+c while the menu is open).
    pub fn delete_leading(&mut self, chars: usize) {
        self.textarea.move_cursor(tui_textarea::CursorMove::Head);
        self.textarea.delete_str(chars);
    }

    /// Show history text, with the cursor at the start (walking back) or the
    /// end (walking forward) like upstream.
    pub fn set_text(&mut self, text: &str, cursor_at_end: bool) {
        self.restore(text);
        if !cursor_at_end {
            self.textarea
                .move_cursor(tui_textarea::CursorMove::Jump(0, 0));
        }
    }

    pub fn cursor_at_start(&self) -> bool {
        self.textarea.cursor() == (0, 0)
    }

    pub fn cursor_at_end(&self) -> bool {
        let (row, col) = self.textarea.cursor();
        let lines = self.textarea.lines();
        row + 1 == lines.len() && col == lines[row].chars().count()
    }

    pub fn on_first_row(&self) -> bool {
        self.textarea.cursor().0 == 0
    }

    pub fn on_last_row(&self) -> bool {
        self.textarea.cursor().0 + 1 == self.textarea.lines().len()
    }

    pub fn move_to_start(&mut self) {
        self.textarea
            .move_cursor(tui_textarea::CursorMove::Jump(0, 0));
    }

    pub fn move_to_end(&mut self) {
        self.textarea.move_cursor(tui_textarea::CursorMove::Bottom);
        self.textarea.move_cursor(tui_textarea::CursorMove::End);
    }

    /// Replace the text from column `from` of the cursor's row up to the
    /// cursor with `with` (accepting an `@` menu entry). A single space is
    /// appended unless one already follows the cursor.
    pub fn replace_back_to(&mut self, from: usize, with: &str) {
        let (row, col) = self.textarea.cursor();
        let next = self.textarea.lines()[row].chars().nth(col);
        self.textarea
            .move_cursor(tui_textarea::CursorMove::Jump(row as u16, from as u16));
        self.textarea.delete_str(col.saturating_sub(from));
        self.textarea.insert_str(with);
        if next != Some(' ') {
            self.textarea.insert_char(' ');
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let cursor = cursor_style(self.cursor_visible);
        self.textarea.set_cursor_style(cursor);
        // tui-textarea's own placeholder prepends a cursor cell, which shifts the
        // hint one column right of where typed text lands. Paint it ourselves,
        // after the textarea, with the first glyph standing in for the cursor
        // (upstream draws its block cursor over that glyph).
        frame.render_widget(&self.textarea, area);
        if self.is_empty() {
            // Upstream `Locale.takeWidth(value, width).trimEnd()`
            // (`component/prompt/index.tsx:1628`); the available width there
            // equals this area's width on every terminal size.
            let shown = take_width(&self.placeholder, area.width as usize);
            let mut glyphs = shown.chars();
            let first = glyphs.next().unwrap_or(' ').to_string();
            let rest: String = glyphs.collect();
            let dim = theme::palette().dim;
            let line = Line::from(vec![
                Span::styled(first, dim.patch(cursor)),
                Span::styled(rest, dim),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
    }
}

/// The longest prefix of `text` that fits in `width` columns, trailing
/// whitespace removed.
fn take_width(text: &str, width: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        end = index + ch.len_utf8();
    }
    text[..end].trim_end()
}

/// A block cursor while visible, nothing at all while the blink is off.
fn cursor_style(visible: bool) -> Style {
    if visible {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

/// `true` during the first half of each blink period, `false` during the rest.
fn blink_visible(since_keystroke: Duration) -> bool {
    (since_keystroke.as_millis() / CURSOR_BLINK.as_millis()).is_multiple_of(2)
}

/// Upstream: `Math.max(6, Math.floor(terminalHeight / 3))`.
pub fn max_height(terminal_height: u16) -> u16 {
    (terminal_height / 3).max(6)
}

fn is_newline(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT),
        KeyCode::Char('j') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

fn empty_textarea() -> TextArea<'static> {
    // The placeholder is painted by `Composer::render`; tui-textarea's own
    // version inserts a cursor cell first and misaligns the text.
    let mut textarea = TextArea::default();
    // tui-textarea underlines the cursor's whole line by default. Upstream's
    // input has no cursor-line decoration, so typed text stays plain.
    textarea.set_cursor_line_style(Style::default());
    textarea
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

/// The `ctrl+f` transcript search field. Lives beside the composer (which keeps
/// its draft) and only exists while the search is being typed.
pub enum SearchAction {
    Edited,
    Cancel,
    Confirm,
}

pub struct SearchBox {
    textarea: TextArea<'static>,
}

impl SearchBox {
    pub fn new() -> Self {
        let mut textarea = empty_textarea();
        textarea.set_placeholder_text("搜索…");
        textarea.set_placeholder_style(theme::palette().dim);
        Self { textarea }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SearchAction {
        match key.code {
            KeyCode::Esc => SearchAction::Cancel,
            KeyCode::Enter => SearchAction::Confirm,
            _ => {
                self.textarea.input(key);
                SearchAction::Edited
            }
        }
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(&self.textarea, area);
    }
}

impl Default for SearchBox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::take_width;

    #[test]
    fn take_width_cuts_by_columns_and_trims_the_tail() {
        assert_eq!(take_width("ab cd", 3), "ab");
        assert_eq!(take_width("ab cd", 99), "ab cd");
        assert_eq!(take_width("你好吗", 5), "你好");
        assert_eq!(take_width("abc", 0), "");
    }

    use super::{CURSOR_BLINK, Composer, blink_visible, max_height};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use std::time::{Duration, Instant};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn type_text(composer: &mut Composer, text: &str) {
        for ch in text.chars() {
            composer.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }

    #[test]
    fn enter_submits_and_shift_enter_inserts_newline() {
        let mut composer = Composer::new();
        type_text(&mut composer, "hi");
        assert_eq!(
            composer.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some("hi".to_string())
        );
        assert!(composer.is_empty());

        type_text(&mut composer, "a");
        assert_eq!(
            composer.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            None
        );
        assert!(composer.text().contains('\n'));
    }

    #[test]
    fn every_upstream_newline_chord_inserts_a_line() {
        for (code, modifiers) in [
            (KeyCode::Enter, KeyModifiers::CONTROL),
            (KeyCode::Enter, KeyModifiers::ALT),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            let mut composer = Composer::new();
            type_text(&mut composer, "a");
            assert_eq!(composer.handle_key(key(code, modifiers)), None);
            assert!(
                composer.text().contains('\n'),
                "{code:?}+{modifiers:?} must insert a newline"
            );
        }
    }

    #[test]
    fn empty_enter_does_not_submit() {
        let mut composer = Composer::new();
        assert_eq!(
            composer.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn height_grows_with_lines_and_follows_the_terminal() {
        let mut composer = Composer::new();
        assert_eq!(composer.height(30), 1);
        for _ in 0..20 {
            composer.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        }
        // 21 lines, terminal 30 → max(6, 10) = 10
        assert_eq!(composer.height(30), 10);
        // terminal 9 → max(6, 3) = 6
        assert_eq!(composer.height(9), 6);
        assert_eq!(max_height(1), 6);
        assert_eq!(max_height(99), 33);
    }

    #[test]
    fn submit_keeps_the_placeholder() {
        let mut composer = Composer::new_with_placeholder("Ask anything… \"hi\"");
        type_text(&mut composer, "x");
        composer.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(composer.placeholder(), "Ask anything… \"hi\"");
    }

    #[test]
    fn restore_puts_a_draft_back_and_keeps_the_placeholder() {
        let mut composer = Composer::new_with_placeholder("home");
        composer.restore("retry me");
        assert_eq!(composer.text(), "retry me");
        assert_eq!(composer.placeholder(), "home");
    }

    #[test]
    fn replace_leading_rewrites_the_command_token() {
        let mut composer = Composer::new();
        type_text(&mut composer, "/ne");
        composer.replace_leading(3, "/new ");
        assert_eq!(composer.text(), "/new ");
    }

    #[test]
    fn delete_leading_removes_the_command_token() {
        let mut composer = Composer::new();
        type_text(&mut composer, "/ne");
        composer.delete_leading(3);
        assert!(composer.is_empty());
    }

    #[test]
    fn cursor_is_reported_in_characters() {
        let mut composer = Composer::new();
        type_text(&mut composer, "宽x");
        assert_eq!(composer.cursor(), (0, 2));
    }

    #[test]
    fn clear_empties_the_draft() {
        let mut composer = Composer::new();
        type_text(&mut composer, "hello");
        composer.clear();
        assert!(composer.is_empty());
    }

    fn draw(composer: &mut Composer, width: u16) -> TestBackend {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| composer.render(frame, frame.area()))
            .unwrap();
        terminal.backend().clone()
    }

    #[test]
    fn typed_text_is_not_underlined() {
        let mut composer = Composer::new();
        type_text(&mut composer, "hi");
        let backend = draw(&mut composer, 8);
        for x in 0..4 {
            let cell = &backend.buffer()[(x, 0)];
            assert!(
                !cell.modifier.contains(Modifier::UNDERLINED),
                "cell {x} is underlined: {cell:?}"
            );
        }
    }

    #[test]
    fn the_cursor_blinks_half_a_period_on_and_half_off() {
        assert!(blink_visible(Duration::ZERO));
        assert!(blink_visible(CURSOR_BLINK - Duration::from_millis(1)));
        assert!(!blink_visible(CURSOR_BLINK));
        assert!(!blink_visible(CURSOR_BLINK * 2 - Duration::from_millis(1)));
        assert!(blink_visible(CURSOR_BLINK * 2));
    }

    #[test]
    fn tick_reports_only_phase_changes() {
        let mut composer = Composer::new();
        let start = Instant::now();
        composer.touch(start);
        assert!(!composer.tick(start + Duration::from_millis(1)));
        assert!(composer.tick(start + CURSOR_BLINK), "the off phase starts");
        assert!(!composer.tick(start + CURSOR_BLINK + Duration::from_millis(1)));
        assert!(composer.tick(start + CURSOR_BLINK * 2), "back on");
    }

    #[test]
    fn the_cursor_cell_is_reversed_only_in_the_on_phase() {
        let mut composer = Composer::new();
        type_text(&mut composer, "hi");
        let start = Instant::now();
        composer.touch(start);
        let on = draw(&mut composer, 8);
        assert!(
            on.buffer()[(2, 0)].modifier.contains(Modifier::REVERSED),
            "the cursor after `hi` must be a block: {:?}",
            on.buffer()[(2, 0)]
        );

        composer.tick(start + CURSOR_BLINK);
        let off = draw(&mut composer, 8);
        assert!(
            !off.buffer()[(2, 0)].modifier.contains(Modifier::REVERSED),
            "the off phase must not paint a block: {:?}",
            off.buffer()[(2, 0)]
        );
    }

    #[test]
    fn the_placeholder_cursor_follows_the_blink() {
        let mut composer = Composer::new_with_placeholder("Ask anything");
        let start = Instant::now();
        composer.touch(start);
        let on = draw(&mut composer, 16);
        assert!(on.buffer()[(0, 0)].modifier.contains(Modifier::REVERSED));

        composer.tick(start + CURSOR_BLINK);
        let off = draw(&mut composer, 16);
        assert!(!off.buffer()[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn set_text_places_the_cursor_at_either_end() {
        let mut composer = Composer::new();
        composer.set_text("one\ntwo", false);
        assert!(composer.cursor_at_start());
        assert!(!composer.cursor_at_end());
        composer.set_text("one\ntwo", true);
        assert!(composer.cursor_at_end());
        assert!(!composer.cursor_at_start());
        assert_eq!(composer.text(), "one\ntwo");
    }

    #[test]
    fn row_queries_follow_the_cursor() {
        let mut composer = Composer::new();
        composer.set_text("a\nb", true);
        assert!(composer.on_last_row() && !composer.on_first_row());
        composer.move_to_start();
        assert!(composer.on_first_row() && !composer.on_last_row());
        composer.move_to_end();
        assert!(composer.cursor_at_end());
    }

    #[test]
    fn replace_back_to_swaps_the_token_and_adds_one_space() {
        let mut composer = Composer::new();
        type_text(&mut composer, "see @sr");
        composer.replace_back_to(4, "@src/main.rs");
        assert_eq!(composer.text(), "see @src/main.rs ");
        assert!(composer.cursor_at_end());
    }

    #[test]
    fn replace_back_to_reuses_an_existing_following_space() {
        let mut composer = Composer::new();
        composer.set_text("@sr tail", false);
        composer
            .textarea
            .move_cursor(tui_textarea::CursorMove::Jump(0, 3));
        composer.replace_back_to(0, "@src");
        assert_eq!(composer.text(), "@src tail");
    }
}
