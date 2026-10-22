use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_textarea::TextArea;

use crate::theme;

/// The text editor at the bottom of both routes.
///
/// It owns only the textarea and the placeholder (which must survive a submit:
/// the home screen picks one of three rotating phrases and expects it back).
/// Everything about the surrounding panel lives in [`crate::ui::prompt`].
pub struct Composer {
    textarea: TextArea<'static>,
    placeholder: String,
}

impl Composer {
    pub fn new() -> Self {
        Self::new_with_placeholder("输入消息…")
    }

    pub fn new_with_placeholder(placeholder: &str) -> Self {
        Self {
            textarea: empty_textarea(),
            placeholder: placeholder.to_owned(),
        }
    }

    /// Put a draft back (home route, failed session creation).
    pub fn restore(&mut self, text: &str) {
        self.textarea = empty_textarea();
        self.textarea.insert_str(text);
    }

    pub fn clear(&mut self) {
        self.textarea = empty_textarea();
    }

    /// Returns `Some(text)` when the user submits.
    ///
    /// Enter submits; every newline chord the upstream keymap accepts
    /// (`shift+return,ctrl+return,alt+return,ctrl+j`) inserts a newline instead.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // tui-textarea's own placeholder prepends a cursor cell, which shifts the
        // hint one column right of where typed text lands. Paint it ourselves,
        // after the textarea, with the first glyph standing in for the cursor
        // (upstream draws its block cursor over that glyph).
        frame.render_widget(&self.textarea, area);
        if self.is_empty() {
            let mut glyphs = self.placeholder.chars();
            let first = glyphs.next().unwrap_or(' ').to_string();
            let rest: String = glyphs.collect();
            let dim = theme::palette().dim;
            let line = Line::from(vec![
                Span::styled(first, dim.add_modifier(Modifier::REVERSED)),
                Span::styled(rest, dim),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
    }
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
    TextArea::default()
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
        let mut textarea = TextArea::default();
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
    use super::{Composer, max_height};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
}
