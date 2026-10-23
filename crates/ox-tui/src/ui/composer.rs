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
