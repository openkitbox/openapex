use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;

use ox_schema::support::FileInput;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_textarea::{CursorMove, CursorRenderMode, TextArea, WrapMode};

use crate::attachment::Loaded;
use crate::theme;
use crate::ui::media;
use crate::ui::paste::{self, Pasted};

/// What one submit hands to the app: the prompt text, the files that ride with
/// it, and the one-turn logical model override the app attaches (`/slim`;
/// `None` for an ordinary prompt). It moves by value from the composer to the
/// HTTP request; the attachments (base64, up to ~27 MB each) are never cloned
/// on the way, and `model` is a tiny id the app fills in at submit — never a
/// resolved provider/model.
#[derive(Debug, Clone, PartialEq)]
pub struct Submission {
    pub text: String,
    pub files: Vec<FileInput>,
    /// Logical model id for this prompt's turn only (`ox_schema::support::SLIM`
    /// for `/slim`); the engine resolves it and leaves the session model alone.
    pub model: Option<String>,
}

impl Submission {
    /// A text-only submission.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            files: Vec::new(),
            model: None,
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
    /// Serial behind the cache key of each new attachment, so two pastes of the
    /// same image stay apart and a removed attachment's key is never reused.
    next_key: u64,
    /// Bumped on every change to `attached`. The prompt's strip caches what it
    /// derives from the attachments and rebuilds it when this moves, so the
    /// draw path does not allocate the same image list every frame.
    attachments: u64,
}

/// An attachment waiting for submit; `label` is its stand-in in the text.
///
/// `key` names the picture in the graphics cache for as long as the attachment
/// is held, and `bytes` is its decoded payload — PNG, JPEG, GIF or WebP as it
/// was pasted, and whichever of those it is, it still has to be decoded once
/// here: the preview strip redraws every frame the app is dirty, so it must not
/// decode the 20 MiB of base64 the `data:` URI carries more than once.
struct Attached {
    key: String,
    label: String,
    input: FileInput,
    bytes: Option<Arc<[u8]>>,
}

impl Composer {
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_placeholder("输入消息…")
    }

    #[must_use]
    pub fn new_with_placeholder(placeholder: &str) -> Self {
        Self {
            textarea: wrapping_textarea(),
            placeholder: placeholder.to_owned(),
            pasted: Vec::new(),
            attached: Vec::new(),
            next_key: 0,
            attachments: 0,
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
        self.textarea = wrapping_textarea();
        self.pasted.clear();
        self.attached.clear();
        self.attachments += 1;
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
            return Some(Submission {
                text,
                files,
                // The composer knows nothing about models: the app fills this
                // in at submit from its one-shot `/slim` intent.
                model: None,
            });
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
        let key = format!("c{}", self.next_key);
        self.next_key += 1;
        self.attachments += 1;
        let bytes = crate::attachment::image_uri_bytes(&input.uri).map(Arc::from);
        self.attached.push(Attached {
            key,
            label,
            input,
            bytes,
        });
    }

    /// The image attachments and their bytes, in the order they were pasted:
    /// what the preview strip draws and the enlarged dialog pages through. An
    /// attachment whose bytes did not decode is left out — there is nothing to
    /// show, and its label still names it.
    fn image_attachments(&self) -> impl Iterator<Item = (&Attached, &Arc<[u8]>)> + '_ {
        self.attached
            .iter()
            .filter_map(|attached| attached.bytes.as_ref().map(|bytes| (attached, bytes)))
    }

    /// How many previewable images are attached, for the strip's height.
    #[must_use]
    pub fn image_count(&self) -> usize {
        self.image_attachments().count()
    }

    /// Which generation of attachments [`Self::images`] describes: a caller
    /// that stores what it derived from them compares this to know when to
    /// rebuild.
    #[must_use]
    pub fn attachments_revision(&self) -> u64 {
        self.attachments
    }

    /// The previewable attachments as the graphics layer wants them.
    pub fn images(&self) -> impl Iterator<Item = media::Image> + '_ {
        self.image_attachments()
            .map(|(attached, bytes)| media::Image {
                key: attached.key.clone(),
                bytes: media::Bytes::Inline(Arc::clone(bytes)),
                name: attached.input.name.as_deref().map(Arc::from),
            })
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
        self.attachments += 1;
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

    /// Visible rows when drawn `width` columns wide: the soft-wrapped row
    /// count, clamped to `1..=max(6, terminal_height / 3)` (upstream
    /// `minHeight`/`maxHeight`). Measuring caches per width, hence `&mut`.
    pub fn height(&mut self, terminal_height: u16, width: u16) -> u16 {
        let max = max_height(terminal_height);
        self.textarea.measure(width).content_rows.clamp(1, max)
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

    /// Move one screen (wrapped) row up; on the first screen row, jump to the
    /// start instead (upstream `prompt.history.previous` with text before the
    /// cursor).
    pub fn move_up_or_start(&mut self) {
        let before = self.textarea.cursor();
        self.textarea.move_cursor(CursorMove::Up);
        if self.textarea.cursor() == before {
            self.move_to_start();
        }
    }

    /// Move one screen row down; on the last screen row, jump to the end.
    pub fn move_down_or_end(&mut self) {
        let before = self.textarea.cursor();
        self.textarea.move_cursor(CursorMove::Down);
        if self.textarea.cursor() == before {
            self.move_to_end();
        }
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
#[must_use]
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

/// The prompt's editor soft-wraps at word boundaries like upstream's
/// textarea; the single-line search box keeps horizontal scrolling.
fn wrapping_textarea() -> TextArea<'static> {
    let mut textarea = empty_textarea();
    textarea.set_wrap_mode(WrapMode::Word);
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
    #[must_use]
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
