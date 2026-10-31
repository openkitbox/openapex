use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ox_schema::support::FileInput;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_textarea::{CursorRenderMode, TextArea};

use crate::attachment::Loaded;
use crate::theme;
use crate::ui::paste::{self, Pasted};

/// What one submit hands to the app: the prompt text and the files that ride
/// with it. It moves by value from the composer to the HTTP request; the
/// attachments (base64, up to ~27 MB each) are never cloned on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Submission {
    pub text: String,
    pub files: Vec<FileInput>,
}

impl Submission {
    /// A text-only submission.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            files: Vec::new(),
        }
    }
}

/// The text editor at the bottom of both routes.
///
/// It owns only the textarea and the placeholder (which must survive a submit:
/// the home screen picks one of three rotating phrases and expects it back).
/// Everything about the surrounding panel lives in [`crate::ui::prompt`].
pub struct Composer {
    textarea: TextArea<'static>,
    placeholder: String,
    /// Long pastes standing in the textarea as placeholder labels. The text
    /// is authoritative: an entry whose label is no longer in it is ignored
    /// at submit, and every path that replaces the text empties this list.
    pasted: Vec<Pasted>,
    /// Pasted images and PDFs standing in the textarea as labels. Like
    /// `pasted`: the text decides which of them are sent.
    attached: Vec<Attached>,
}

/// An attachment waiting for submit; `label` is its stand-in in the text.
struct Attached {
    label: String,
    input: FileInput,
}

impl Composer {
    pub fn new() -> Self {
        Self::new_with_placeholder("输入消息…")
    }

    pub fn new_with_placeholder(placeholder: &str) -> Self {
        Self {
            textarea: empty_textarea(),
            placeholder: placeholder.to_owned(),
            pasted: Vec::new(),
            attached: Vec::new(),
        }
    }

    /// Put a draft back (home route, failed session creation).
    pub fn restore(&mut self, text: &str) {
        self.reset_text();
        self.textarea.insert_str(text);
    }

    pub fn clear(&mut self) {
        self.reset_text();
    }

    /// Empty textarea and paste registry together: the registry never
    /// outlives the text it belongs to.
    fn reset_text(&mut self) {
        self.textarea = empty_textarea();
        self.pasted.clear();
        self.attached.clear();
    }

    /// Returns `Some(submission)` when the user submits.
    ///
    /// Enter submits; every newline chord the upstream keymap accepts
    /// (`shift+return,ctrl+return,alt+return,ctrl+j`) inserts a newline instead.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Submission> {
        if is_newline(&key) {
            self.textarea.insert_newline();
            return None;
        }
        if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
            let text = self.text();
            let trimmed = text.trim_end();
            if trimmed.is_empty() {
                return None;
            }
            let text = paste::expand(trimmed, &self.pasted);
            let files = self.take_attached(trimmed);
            self.clear();
            return Some(Submission { text, files });
        }
        self.textarea.input(key);
        None
    }

    /// Insert pasted text at the cursor: line breaks normalised, ANSI removed.
    /// A long paste goes in as a `[Pasted ~N lines]` label and is expanded at
    /// submit; pasting the same content again right after its label unfolds it.
    pub fn insert_paste(&mut self, text: String) {
        let mut text = paste::normalize(&text);
        let Some(base) = paste::fold_label(&text) else {
            self.textarea.insert_str(text);
            return;
        };
        paste::trim_in_place(&mut text);
        if let Some(index) = self.expandable(&text) {
            // The entry stays: the text alone decides what is sent, and an undo
            // that brings the label back must still expand.
            let label_chars = self.pasted[index].label.chars().count();
            let (row, col) = self.textarea.cursor();
            let head = col.saturating_sub(label_chars);
            self.textarea
                .move_cursor(tui_textarea::CursorMove::Jump(row as u16, head as u16));
            self.textarea.delete_str(label_chars);
            self.textarea.insert_str(text);
            return;
        }
        let label = self.unique_label(&base);
        self.textarea.insert_str(&label);
        self.pasted.push(Pasted { label, text });
    }

    /// Insert a pasted image or PDF as a label (`[Image n]`, `[PDF: name]`)
    /// followed by a space. The file rides along at submit if the label is
    /// still in the text then.
    pub fn attach_file(&mut self, name: &str, input: FileInput) {
        let images = self
            .attached
            .iter()
            .filter(|a| paste::is_image_uri(&a.input.uri))
            .count();
        let base = paste::attachment_label(&input.uri, name, images);
        let label = self.unique_label(&base);
        self.insert_label(&label);
        self.attached.push(Attached { label, input });
    }

    /// Insert a pasted SVG as a `[SVG: name]` label followed by a space; the
    /// markup replaces the label at submit like a folded paste.
    pub fn attach_svg(&mut self, name: &str, content: String) {
        let label = self.unique_label(&paste::svg_label(name));
        self.insert_label(&label);
        self.pasted.push(Pasted {
            label,
            text: content,
        });
    }

    /// Route one loaded paste item to the matching `attach_*`.
    pub fn attach(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::File { name, input } => self.attach_file(&name, input),
            Loaded::Svg { name, content } => self.attach_svg(&name, content),
        }
    }

    fn insert_label(&mut self, label: &str) {
        self.textarea.insert_str(label);
        self.textarea.insert_char(' ');
    }

    /// `base`, or `base #n`, so that no folded paste or attachment has it yet.
    fn unique_label(&self, base: &str) -> String {
        paste::unique_label(base, |label| {
            self.pasted.iter().any(|p| p.label == label)
                || self.attached.iter().any(|a| a.label == label)
        })
    }

    /// Move out the attachments whose label is still in `text`, in the order
    /// the labels appear; the others are dropped. The files move, never copy.
    fn take_attached(&mut self, text: &str) -> Vec<FileInput> {
        let attached = std::mem::take(&mut self.attached);
        let labels: Vec<&str> = attached.iter().map(|a| a.label.as_str()).collect();
        let order = paste::labels_in_order(text, &labels);
        let mut inputs: Vec<Option<FileInput>> =
            attached.into_iter().map(|a| Some(a.input)).collect();
        order
            .into_iter()
            .filter_map(|index| inputs[index].take())
            .collect()
    }

    /// Index of the entry whose label ends right before the cursor and whose
    /// content equals `text`.
    fn expandable(&self, text: &str) -> Option<usize> {
        let (row, col) = self.textarea.cursor();
        let line = &self.textarea.lines()[row];
        let end = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
        paste::expandable_before(&line[..end], text, &self.pasted)
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
        self.reset_text();
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

    /// Draw the text and, when `own_cursor`, park the terminal's own cursor on
    /// the logical cursor. The cursor is never painted into the buffer: an IME anchors its
    /// preedit string at the terminal cursor, so that must sit on the caret
    /// and stay there across redraws.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, own_cursor: bool) {
        frame.render_widget(&self.textarea, area);
        if self.is_empty() {
            // tui-textarea's own placeholder is not used (it shifts the hint
            // one column right of where typed text lands), so paint ours over
            // the empty textarea. The caret stays at `area`'s origin, on the
            // hint's first glyph.
            // Upstream `Locale.takeWidth(value, width).trimEnd()`
            // (`component/prompt/index.tsx:1628`); the available width there
            // equals this area's width on every terminal size.
            let shown = take_width(&self.placeholder, area.width as usize);
            let line = Line::from(Span::styled(shown, theme::palette().dim));
            frame.render_widget(Paragraph::new(line), area);
        }
        if own_cursor {
            place_cursor(frame, &self.textarea);
        }
    }
}

/// The terminal cursor goes where the textarea just drew its caret; nowhere
/// when the caret is scrolled out of view or the area is empty.
fn place_cursor(frame: &mut Frame, textarea: &TextArea<'_>) {
    if let Some(position) = textarea.rendered_cursor_position() {
        frame.set_cursor_position(position);
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
    // The caret is the terminal's cursor, not a reversed cell in the buffer.
    textarea.set_cursor_render_mode(CursorRenderMode::Hidden);
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

    /// Insert pasted text at the cursor on one line: breaks become spaces.
    pub fn insert_paste(&mut self, text: &str) {
        self.textarea.insert_str(paste::single_line(text));
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Draw the box; `own_cursor` is false while a dialog is open, so the IME
    /// does not anchor under it (the prompt's rule, see [`Composer::render`]).
    pub fn render(&mut self, frame: &mut Frame, area: Rect, own_cursor: bool) {
        frame.render_widget(&self.textarea, area);
        if own_cursor {
            place_cursor(frame, &self.textarea);
        }
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

    use super::{Composer, FileInput, Loaded, SearchBox, Submission, max_height};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::Modifier;

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
            Some(Submission::text("hi"))
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
    fn paste_inserts_multiline_text_at_the_cursor() {
        let mut composer = Composer::new();
        type_text(&mut composer, "ac");
        composer.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        composer.insert_paste("b1\r\nb2".to_owned());
        assert_eq!(composer.text(), "ab1\nb2c");
        assert_eq!(composer.cursor(), (1, 2), "the cursor ends after the paste");
    }

    fn submit(composer: &mut Composer) -> Option<String> {
        let submission = composer.handle_key(key(KeyCode::Enter, KeyModifiers::NONE))?;
        assert!(submission.files.is_empty(), "no attachments exist yet");
        Some(submission.text)
    }

    fn lines(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_short_paste_goes_in_verbatim() {
        let mut composer = Composer::new();
        composer.insert_paste("a\nb".to_owned());
        composer.insert_paste("x".repeat(150));
        assert_eq!(composer.text(), format!("a\nb{}", "x".repeat(150)));
        assert!(composer.pasted.is_empty());
    }

    #[test]
    fn a_long_paste_becomes_a_label_and_submits_as_the_original() {
        let mut composer = Composer::new();
        type_text(&mut composer, "see ");
        composer.insert_paste(lines(12));
        assert_eq!(composer.text(), "see [Pasted ~12 lines]");
        assert_eq!(submit(&mut composer), Some(format!("see {}", lines(12))));
        assert!(composer.pasted.is_empty(), "submit empties the registry");
        assert!(composer.is_empty());
    }

    #[test]
    fn folding_counts_chinese_by_characters() {
        let mut composer = Composer::new();
        composer.insert_paste("中".repeat(150));
        assert_eq!(composer.text(), "中".repeat(150));
        composer.clear();
        composer.insert_paste("中".repeat(151));
        assert_eq!(composer.text(), "[Pasted ~1 lines]");
        assert_eq!(submit(&mut composer), Some("中".repeat(151)));
    }

    #[test]
    fn equal_labels_get_a_numeric_suffix_and_expand_in_text_order() {
        let (first, second) = (lines(12), format!("{}\nextra", lines(11)));
        let mut composer = Composer::new();
        composer.insert_paste(first.clone());
        type_text(&mut composer, " and ");
        composer.insert_paste(second.clone());
        assert_eq!(
            composer.text(),
            "[Pasted ~12 lines] and [Pasted ~12 lines] #2"
        );
        assert_eq!(submit(&mut composer), Some(format!("{first} and {second}")));
    }

    #[test]
    fn a_label_edited_away_drops_its_entry() {
        let mut composer = Composer::new();
        composer.insert_paste(lines(5));
        composer.insert_paste(lines(7));
        // Remove the second label ("[Pasted ~7 lines]", 17 chars) with backspace.
        for _ in 0..17 {
            composer.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        }
        assert_eq!(composer.text(), "[Pasted ~5 lines]");
        assert_eq!(submit(&mut composer), Some(lines(5)));
        assert!(composer.pasted.is_empty());
    }

    #[test]
    fn a_damaged_label_is_sent_as_plain_text() {
        let mut composer = Composer::new();
        composer.insert_paste(lines(5));
        composer.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(submit(&mut composer), Some("[Pasted ~5 lines".to_owned()));
    }

    #[test]
    fn every_text_reset_empties_the_registry() {
        let mut composer = Composer::new();
        for reset in [
            (|c: &mut Composer| c.clear()) as fn(&mut Composer),
            |c| c.restore("draft"),
            |c| c.set_text("draft", true),
            |c| c.set_placeholder("hint"),
        ] {
            composer.insert_paste(lines(4));
            assert_eq!(composer.pasted.len(), 1);
            reset(&mut composer);
            assert!(composer.pasted.is_empty());
        }
    }

    #[test]
    fn pasting_the_same_content_after_its_label_unfolds_it() {
        let mut composer = Composer::new();
        type_text(&mut composer, "a ");
        composer.insert_paste(lines(4));
        composer.insert_paste(format!("{}\n", lines(4)));
        assert_eq!(composer.text(), format!("a {}", lines(4)));
        assert_eq!(composer.pasted.len(), 1, "the entry outlives the unfolding");
        assert_eq!(composer.cursor(), (3, 5), "the cursor ends after the text");
    }

    #[test]
    fn undoing_an_unfolding_brings_back_a_label_that_still_expands() {
        let mut composer = Composer::new();
        composer.insert_paste(lines(4));
        composer.insert_paste(lines(4));
        assert_eq!(composer.text(), lines(4));
        let undo = key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        for _ in 0..4 {
            if composer.text() == "[Pasted ~4 lines]" {
                break;
            }
            composer.handle_key(undo);
        }
        assert_eq!(
            composer.text(),
            "[Pasted ~4 lines]",
            "undo restores the label"
        );
        assert_eq!(submit(&mut composer), Some(lines(4)));
    }

    #[test]
    fn an_unfolded_entry_adds_nothing_at_submit() {
        let mut composer = Composer::new();
        composer.insert_paste(lines(4));
        composer.insert_paste(lines(4));
        assert_eq!(submit(&mut composer), Some(lines(4)), "no duplicated text");
    }

    #[test]
    fn pasting_different_content_or_away_from_the_label_folds_again() {
        let mut composer = Composer::new();
        composer.insert_paste(lines(4));
        composer.insert_paste(lines(5));
        assert_eq!(composer.text(), "[Pasted ~4 lines][Pasted ~5 lines]");
        type_text(&mut composer, " ");
        composer.insert_paste(lines(5));
        assert_eq!(composer.pasted.len(), 3, "a space between: no unfolding");
    }

    #[test]
    fn unfolding_an_entry_keeps_its_neighbours_working() {
        let mut composer = Composer::new();
        composer.insert_paste(lines(4));
        type_text(&mut composer, "|");
        composer.insert_paste(lines(5));
        composer.insert_paste(lines(5));
        assert_eq!(composer.text(), format!("[Pasted ~4 lines]|{}", lines(5)));
        assert_eq!(
            submit(&mut composer),
            Some(format!("{}|{}", lines(4), lines(5)))
        );
    }

    #[test]
    fn paste_keeps_chinese_text_and_strips_ansi() {
        let mut composer = Composer::new();
        composer.insert_paste("\x1b[32m你好\x1b[0m\n世界".to_owned());
        assert_eq!(composer.text(), "你好\n世界");
    }

    #[test]
    fn a_pasted_draft_submits_as_typed_text() {
        let mut composer = Composer::new();
        composer.insert_paste("line one\nline two\n".to_owned());
        let sent = composer.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(sent, Some(Submission::text("line one\nline two")));
    }

    fn png(name: &str) -> FileInput {
        FileInput {
            uri: "data:image/png;base64,AAAA".into(),
            source: None,
            name: Some(name.into()),
            description: None,
            mention: None,
        }
    }

    fn pdf(name: &str) -> FileInput {
        FileInput {
            uri: "data:application/pdf;base64,AAAA".into(),
            ..png(name)
        }
    }

    fn submit_all(composer: &mut Composer) -> Submission {
        composer
            .handle_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .expect("the draft submits")
    }

    #[test]
    fn an_attached_image_shows_a_label_and_rides_along_at_submit() {
        let mut composer = Composer::new();
        type_text(&mut composer, "see ");
        composer.attach_file("a.png", png("a.png"));
        assert_eq!(composer.text(), "see [Image 1] ");
        type_text(&mut composer, "ok");
        let sent = submit_all(&mut composer);
        assert_eq!(sent.text, "see [Image 1] ok", "the label stays in the text");
        assert_eq!(sent.files, vec![png("a.png")]);
        assert!(composer.attached.is_empty() && composer.is_empty());
    }

    #[test]
    fn images_are_numbered_and_pdfs_carry_their_name() {
        let mut composer = Composer::new();
        composer.attach_file("a.png", png("a.png"));
        composer.attach_file("r.pdf", pdf("r.pdf"));
        composer.attach_file("b.png", png("b.png"));
        assert_eq!(composer.text(), "[Image 1] [PDF: r.pdf] [Image 2] ");
        composer.attach_file("r.pdf", pdf("r.pdf"));
        assert!(composer.text().ends_with("[PDF: r.pdf] #2 "));
    }

    #[test]
    fn files_follow_the_order_of_their_labels_in_the_text() {
        let mut composer = Composer::new();
        composer.attach_file("a.png", png("a.png"));
        composer.attach_file("r.pdf", pdf("r.pdf"));
        composer.move_to_start();
        composer.attach_file("b.png", png("b.png"));
        // Text order: [Image 2] [Image 1] [PDF: r.pdf]
        let sent = submit_all(&mut composer);
        let names: Vec<_> = sent.files.iter().map(|f| f.name.clone().unwrap()).collect();
        assert_eq!(names, ["b.png", "a.png", "r.pdf"]);
    }

    #[test]
    fn a_deleted_label_drops_its_file() {
        let mut composer = Composer::new();
        composer.attach_file("a.png", png("a.png"));
        composer.attach_file("r.pdf", pdf("r.pdf"));
        // Remove "[PDF: r.pdf] " (13 chars).
        for _ in 0..13 {
            composer.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        }
        assert_eq!(composer.text(), "[Image 1] ");
        assert_eq!(submit_all(&mut composer).files, vec![png("a.png")]);
    }

    #[test]
    fn an_attachment_with_no_text_left_does_not_submit() {
        let mut composer = Composer::new();
        composer.attach_file("a.png", png("a.png"));
        composer.clear();
        assert!(composer.attached.is_empty());
        assert_eq!(
            composer.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn an_attached_svg_folds_and_expands_at_submit() {
        let mut composer = Composer::new();
        composer.attach_svg("logo.svg", "<svg>中</svg>".into());
        assert_eq!(composer.text(), "[SVG: logo.svg] ");
        type_text(&mut composer, "what is this");
        let sent = submit_all(&mut composer);
        assert_eq!(sent.text, "<svg>中</svg> what is this");
        assert!(sent.files.is_empty(), "an svg is text, not a file");
    }

    #[test]
    fn labels_stay_unique_across_pastes_and_attachments() {
        let mut composer = Composer::new();
        composer.attach_svg("a.svg", "<a/>".into());
        composer.attach_svg("a.svg", "<b/>".into());
        assert_eq!(composer.text(), "[SVG: a.svg] [SVG: a.svg] #2 ");
        assert_eq!(submit_all(&mut composer).text, "<a/> <b/>");
    }

    #[test]
    fn loaded_items_route_to_the_matching_attach() {
        let mut composer = Composer::new();
        composer.attach(Loaded::File {
            name: "a.png".into(),
            input: png("a.png"),
        });
        composer.attach(Loaded::Svg {
            name: "l.svg".into(),
            content: "<svg/>".into(),
        });
        assert_eq!(composer.text(), "[Image 1] [SVG: l.svg] ");
    }

    #[test]
    fn every_text_reset_drops_attachments() {
        let mut composer = Composer::new();
        for reset in [
            (|c: &mut Composer| c.clear()) as fn(&mut Composer),
            |c| c.restore("draft"),
            |c| c.set_text("draft", true),
            |c| c.set_placeholder("hint"),
        ] {
            composer.attach_file("a.png", png("a.png"));
            assert_eq!(composer.attached.len(), 1);
            reset(&mut composer);
            assert!(composer.attached.is_empty());
        }
    }

    #[test]
    fn search_paste_folds_newlines_into_spaces() {
        let mut search = SearchBox::new();
        search.insert_paste("foo\r\nbar\nbaz");
        assert_eq!(search.text(), "foo bar baz");
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
            .draw(|frame| composer.render(frame, frame.area(), true))
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

    /// Where the backend's cursor sits before a draw. It is not (0, 0), the
    /// backend's default, so a cursor that is never placed cannot pass for one
    /// placed at the origin.
    const PARKED: Position = Position::new(9, 0);

    fn cursor_in(composer: &mut Composer, width: u16, height: u16) -> (Position, TestBackend) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.set_cursor_position(PARKED).unwrap();
        terminal
            .draw(|frame| composer.render(frame, frame.area(), true))
            .unwrap();
        let position = terminal.get_cursor_position().unwrap();
        (position, terminal.backend().clone())
    }

    fn no_reversed_cell(backend: &TestBackend) -> bool {
        backend
            .buffer()
            .content()
            .iter()
            .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
    }

    #[test]
    fn the_terminal_cursor_counts_wide_characters_as_two_columns() {
        let mut composer = Composer::new();
        type_text(&mut composer, "你好a");
        let (cursor, backend) = cursor_in(&mut composer, 20, 1);
        assert_eq!(cursor, Position::new(5, 0));
        assert!(no_reversed_cell(&backend));
    }

    #[test]
    fn the_terminal_cursor_sits_on_the_caret_row_of_a_multiline_draft() {
        let mut composer = Composer::new();
        type_text(&mut composer, "one");
        composer.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut composer, "世界");
        let (cursor, _) = cursor_in(&mut composer, 20, 4);
        assert_eq!(cursor, Position::new(4, 1));

        composer.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        let (cursor, _) = cursor_in(&mut composer, 20, 4);
        assert_eq!(cursor.y, 0, "moving the caret moves the cursor");
    }

    #[test]
    fn the_terminal_cursor_stays_inside_a_scrolled_viewport() {
        let mut composer = Composer::new();
        for _ in 0..7 {
            type_text(&mut composer, "x");
            composer.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        }
        type_text(&mut composer, "z");
        // Eight lines in a three-row area: the view follows the caret to the
        // bottom row.
        let (cursor, _) = cursor_in(&mut composer, 20, 3);
        assert_eq!(cursor, Position::new(1, 2));
    }

    #[test]
    fn an_empty_draft_parks_the_cursor_at_the_start_of_the_hint() {
        let mut composer = Composer::new_with_placeholder("Ask anything");
        let (cursor, backend) = cursor_in(&mut composer, 16, 1);
        assert_eq!(cursor, Position::new(0, 0));
        assert_eq!(backend.buffer()[(0, 0)].symbol(), "A");
        assert!(no_reversed_cell(&backend), "the hint has no block cursor");
    }

    #[test]
    fn the_search_box_places_the_terminal_cursor() {
        let mut search = SearchBox::new();
        search.insert_paste("ab");
        let (cursor, _) = search_cursor(&mut search, Rect::new(0, 0, 10, 1));
        assert_eq!(cursor, Position::new(2, 0));
    }

    #[test]
    fn an_empty_search_box_puts_the_cursor_at_the_start_of_its_hint() {
        // tui-textarea reports the area origin as the caret while its own
        // placeholder shows, so the cursor needs no special case. The area is
        // offset so the origin is not the backend's default position either.
        let mut search = SearchBox::new();
        let (cursor, _) = search_cursor(&mut search, Rect::new(3, 1, 10, 1));
        assert_eq!(cursor, Position::new(3, 1));
    }

    #[test]
    fn a_search_box_under_a_dialog_leaves_the_cursor_alone() {
        let mut search = SearchBox::new();
        search.insert_paste("ab");
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.set_cursor_position(PARKED).unwrap();
        terminal
            .draw(|frame| search.render(frame, Rect::new(0, 0, 10, 1), false))
            .unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap(), PARKED);
    }

    fn search_cursor(search: &mut SearchBox, area: Rect) -> (Position, TestBackend) {
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.set_cursor_position(PARKED).unwrap();
        terminal
            .draw(|frame| search.render(frame, area, true))
            .unwrap();
        let position = terminal.get_cursor_position().unwrap();
        (position, terminal.backend().clone())
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
