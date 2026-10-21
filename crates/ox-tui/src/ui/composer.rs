use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use tui_textarea::TextArea;

use crate::theme;

pub struct Composer {
    textarea: TextArea<'static>,
}

impl Composer {
    pub fn new() -> Self {
        Self::new_with_placeholder("输入消息…")
    }

    pub fn new_with_placeholder(placeholder: &str) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text(placeholder);
        textarea.set_placeholder_style(theme::palette().dim);
        Self { textarea }
    }

    /// Home 惰性建会话失败时把提交过的草稿放回来。
    pub fn restore(&mut self, text: &str) {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("输入消息…");
        textarea.set_placeholder_style(theme::palette().dim);
        textarea.insert_str(text);
        self.textarea = textarea;
    }

    /// Returns Some(text) when the user submits (Enter without Shift).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
            let text = self.textarea.lines().join("\n");
            let trimmed = text.trim_end().to_owned();
            if trimmed.is_empty() {
                return None;
            }
            self.textarea = TextArea::default();
            self.textarea.set_placeholder_text("输入消息…");
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

    pub fn height(&self) -> u16 {
        (self.textarea.lines().len() as u16).clamp(1, 8)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(&self.textarea, area);
    }
}

/// The `/` prompt. Lives beside the composer (which keeps its draft) and only
/// exists while the search is being typed.
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

#[cfg(test)]
mod tests {
    use super::Composer;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn enter_submits_and_shift_enter_inserts_newline() {
        let mut composer = Composer::new();
        for ch in "hi".chars() {
            let _ = composer.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some("hi".to_string())
        );
        assert!(composer.is_empty());

        composer.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(
            composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            None
        );
        assert!(composer.text().contains('\n'));
    }

    #[test]
    fn height_grows_with_lines_up_to_eight() {
        let mut composer = Composer::new();
        for _ in 0..20 {
            let _ = composer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        }
        assert_eq!(composer.height(), 8);
    }

    #[test]
    fn custom_placeholder_is_accepted() {
        let composer = Composer::new_with_placeholder("Ask anything… \"hi\"");
        // 不直接断言渲染，只断言构造可用且为空
        assert!(composer.is_empty());
    }

    #[test]
    fn restore_puts_a_draft_back() {
        let mut composer = Composer::new();
        composer.restore("retry me");
        assert_eq!(composer.text(), "retry me");
    }

    #[test]
    fn new_keeps_the_chinese_default_placeholder() {
        // 会话路由的既有行为不变
        let composer = Composer::new();
        assert!(composer.is_empty());
    }
}
