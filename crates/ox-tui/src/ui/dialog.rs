//! Centered modal dialogs (upstream `component/dialog-*.tsx`).
//!
//! Rendering only: the app decides which dialog is open and hands over the data
//! it already has. `/status` mirrors upstream `dialog-status.tsx` — a count line
//! plus one row per MCP server with its live state. Upstream's status dialog has
//! no LSP or permission sections either; those live in the sidebar plugins, and
//! ox has no LSP route at all, so nothing is invented here.

use std::cell::Cell;
use std::cmp::Reverse;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ox_client::rest::{McpRef, McpStatus};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::form::{self, FormOutcome, FormState};
use super::fuzzy;
use crate::theme::{self, Palette};

/// What a key did to the open dialog. The app only needs to know whether to
/// repaint, whether to close, and which value (if any) was picked.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogOutcome {
    /// The key had no effect; nothing to repaint.
    Idle,
    /// The dialog stays open and its content changed.
    Changed,
    /// Enter on a list row: the row's `value`. The dialog is done.
    Selected(String),
    /// Esc (or the info dialog's dismiss keys). The dialog is done.
    Cancelled,
    /// `ctrl+<key>` on a list row that registered it with [`Select::action`]
    /// (after its confirmation, if it asked for one). The dialog stays open.
    Action { key: char, value: String },
    /// Enter in an [`Input`]: its `target` and the text as typed. The dialog is done.
    Submitted { target: String, text: String },
    /// A form passed validation; carries the answer. The dialog is done.
    FormSubmitted(ox_client::form::Answer),
}

/// An open modal. `Info` is the read-only status panel; `Select` is the
/// filterable list every picker (models, sessions, themes…) is built on.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Info(Info),
    Select(Select),
    Input(Input),
    /// A pending form (`ui/form.rs`); drawn over the prompt, not centred.
    Form(Box<FormState>),
}

impl Dialog {
    pub fn title(&self) -> &str {
        match self {
            Self::Info(info) => info.title,
            Self::Select(select) => &select.title,
            Self::Input(input) => &input.title,
            Self::Form(form) => form.title(),
        }
    }

    /// Which caller opened this dialog, so the app can tell apart the
    /// `Selected`/`Submitted` values of different pickers.
    pub fn tag(&self) -> Option<&'static str> {
        match self {
            Self::Info(_) => None,
            Self::Select(select) => select.tag,
            Self::Input(input) => input.tag,
            Self::Form(_) => None,
        }
    }

    /// Feed one key to the dialog. While a dialog is open the app routes
    /// every key here (except `ctrl+c`, which still quits).
    pub fn on_key(&mut self, key: KeyEvent) -> DialogOutcome {
        match self {
            Self::Info(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => DialogOutcome::Cancelled,
                _ => DialogOutcome::Idle,
            },
            Self::Select(select) => select.on_key(key),
            Self::Input(input) => input.on_key(key),
            Self::Form(form) => match form.on_key(key) {
                FormOutcome::Idle => DialogOutcome::Idle,
                FormOutcome::Changed => DialogOutcome::Changed,
                FormOutcome::Submit(answer) => DialogOutcome::FormSubmitted(answer),
                FormOutcome::Cancel => DialogOutcome::Cancelled,
            },
        }
    }
}

/// Colour role of a row's bullet and the semantic state behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Healthy / connected.
    Accent,
    /// Broken / needs attention.
    Warn,
    /// Neutral information.
    Dim,
}

/// One content line: a bullet, a name, and a muted detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub tone: Tone,
    pub label: String,
    pub detail: String,
}

/// A ready-to-render status dialog. Owning `&'static str` for the title keeps
/// the dialog cheap to clone and impossible to confuse with content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub title: &'static str,
    /// Muted line under the title (upstream's `N MCP servers`).
    pub summary: Option<String>,
    pub rows: Vec<Row>,
}

/// Upstream `DialogStatus`: one row per MCP server, the detail being the
/// status text (and the error message when the connection failed).
pub fn status(servers: &[McpRef]) -> Dialog {
    let summary = match servers.len() {
        0 => None,
        1 => Some("1 MCP server".to_owned()),
        n => Some(format!("{n} MCP servers")),
    };
    let rows = servers
        .iter()
        .map(|server| {
            let (tone, detail) = match &server.status {
                McpStatus::Connected => (Tone::Accent, "Connected".to_owned()),
                McpStatus::Pending => (Tone::Dim, "Pending".to_owned()),
                McpStatus::Disabled => (Tone::Dim, "Disabled in configuration".to_owned()),
                McpStatus::Failed { error } if error.is_empty() => {
                    (Tone::Warn, "Failed".to_owned())
                }
                McpStatus::Failed { error } => (Tone::Warn, error.clone()),
            };
            Row {
                tone,
                label: server.name.clone(),
                detail,
            }
        })
        .collect();
    Dialog::Info(Info {
        title: "Status",
        summary,
        rows,
    })
}

/// Draw `dialog` over `area`, clearing what is underneath.
pub fn render(frame: &mut Frame, area: Rect, dialog: &Dialog) {
    match dialog {
        Dialog::Info(info) => render_info(frame, area, info),
        Dialog::Select(select) => {
            let cursor = draw_select(frame.buffer_mut(), area, select, theme::palette());
            frame.set_cursor_position(cursor);
        }
        Dialog::Input(input) => {
            let cursor = draw_input(frame.buffer_mut(), area, input, theme::palette());
            frame.set_cursor_position(cursor);
        }
        Dialog::Form(form) => {
            if let Some(cursor) = form::draw(frame.buffer_mut(), area, form, theme::palette()) {
                frame.set_cursor_position(cursor);
            }
        }
    }
}

/// The status panel: a bordered, centered box.
fn render_info(frame: &mut Frame, area: Rect, dialog: &Info) {
    let pal = *theme::palette();
    let width = area.width.saturating_sub(8).clamp(20, 64).min(area.width);
    let content = dialog.rows.len() + usize::from(dialog.summary.is_some());
    let height = u16::try_from(content + 2)
        .unwrap_or(u16::MAX)
        .max(3)
        .min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pal.panel_border)
        .style(pal.panel)
        .title(Span::styled(
            dialog.title,
            pal.text.add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(summary) = &dialog.summary {
        lines.push(Line::from(Span::styled(summary.clone(), pal.dim)));
    }
    for row in &dialog.rows {
        let dot = match row.tone {
            Tone::Accent => pal.accent,
            Tone::Warn => pal.warn,
            Tone::Dim => pal.dim,
        };
        let mut spans = vec![
            Span::styled("\u{2022} ", dot),
            Span::styled(row.label.clone(), pal.text),
        ];
        if !row.detail.is_empty() {
            spans.push(Span::styled(format!("  {}", row.detail), pal.dim));
        }
        lines.push(Line::from(spans));
    }
    if dialog.rows.is_empty() && dialog.summary.is_none() {
        lines.push(Line::from(Span::styled("No MCP servers", pal.dim)));
    }
    frame.render_widget(Paragraph::new(lines).style(Style::reset()), inner);
}

// ---------------------------------------------------------------------------
// Select: the filterable list every picker is built on
// (upstream `ui/dialog.tsx` + `ui/dialog-select.tsx`).
// ---------------------------------------------------------------------------

/// Panel width (`dialogWidth("medium")`, `ui/dialog.tsx:30-34`).
const PANEL_WIDTH: u16 = 60;
/// `dialogWidth("large")` (`ui/dialog.tsx:30-34`).
const LARGE_PANEL_WIDTH: u16 = 88;
/// Text column inside the panel: `DIALOG_INSET` 3 + the header's 1-cell inset
/// (`dialog.tsx:26-28`, `dialog-select.tsx:696`).
const TEXT_INSET: u16 = 4;
/// Rows above the list: top padding, header, gap, search input, gap
/// (`dialog.tsx:83`, `dialog-select.tsx:696-713`, `DialogLayout` gap 1).
const CHROME_TOP: usize = 5;
/// `dialog.move_selection(±10)` for PageUp/PageDown (`dialog-select.tsx:497-509`).
const PAGE: isize = 10;
/// The backdrop is black at alpha 150 (`dialog.tsx:69`).
const BACKDROP_ALPHA: u16 = 150;

/// One selectable row. `value` is what the dialog hands back on Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub value: String,
    pub title: String,
    /// Muted text after the title.
    pub description: Option<String>,
    /// Right-aligned text (a keybinding, a price, a date…).
    pub footer: Option<String>,
    /// Group header this row lives under (provider, date…).
    pub category: Option<String>,
    /// Shown only while the filter is empty (the palette's `Suggested` copies,
    /// which upstream drops once the user types).
    pub unfiltered_only: bool,
}

impl Item {
    pub fn new(value: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            title: title.into(),
            description: None,
            footer: None,
            category: None,
            unfiltered_only: false,
        }
    }

    pub fn unfiltered_only(mut self) -> Self {
        self.unfiltered_only = true;
        self
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn footer(mut self, text: impl Into<String>) -> Self {
        self.footer = Some(text.into());
        self
    }

    pub fn category(mut self, text: impl Into<String>) -> Self {
        self.category = Some(text.into());
        self
    }
}

/// One footer hint: an action title and its key (`DialogFooterAction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub title: String,
    pub key: String,
}

impl Hint {
    pub fn new(title: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            key: key.into(),
        }
    }
}

/// A title, a filter input and a scrollable, optionally grouped list.
///
/// State is the filter text and the cursor position in the *visible* list;
/// the visible order is derived from `items` + `filter` on demand, so the two
/// cannot drift. `selected` is clamped after every change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    title: String,
    items: Vec<Item>,
    hints: Vec<Hint>,
    /// Value to mark with `●` and to start on.
    current: Option<String>,
    filter: String,
    selected: usize,
    /// First visible list row. Render adjusts it to keep the selection in
    /// view, because only render knows the viewport height.
    offset: Cell<usize>,
    width: u16,
    tag: Option<&'static str>,
    actions: Vec<Action>,
    /// A row that pressed a confirming action once and awaits the second.
    armed: Option<Armed>,
}

/// A `ctrl+<key>` shortcut on the selected row (upstream `actions`). With
/// `confirm`, the first press only arms the row: it is drawn destructive with
/// that text as its title until the cursor moves or the key is pressed again
/// (`dialog-session-list.tsx` `toDelete`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Action {
    key: char,
    confirm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Armed {
    key: char,
    value: String,
}

impl Select {
    pub fn new(title: impl Into<String>, items: Vec<Item>) -> Self {
        Self {
            title: title.into(),
            items,
            hints: Vec::new(),
            current: None,
            filter: String::new(),
            selected: 0,
            offset: Cell::new(0),
            width: PANEL_WIDTH,
            tag: None,
            actions: Vec::new(),
            armed: None,
        }
    }

    /// The wide panel (`setSize("large")`).
    pub fn large(mut self) -> Self {
        self.width = LARGE_PANEL_WIDTH;
        self
    }

    /// Name the caller; read back through [`Dialog::tag`].
    pub fn tag(mut self, tag: &'static str) -> Self {
        self.tag = Some(tag);
        self
    }

    /// Register `ctrl+<key>` as a row action, optionally confirm-on-second-press.
    pub fn action(mut self, key: char, confirm: Option<&str>) -> Self {
        self.actions.push(Action {
            key,
            confirm: confirm.map(str::to_owned),
        });
        self
    }

    /// The row whose value is `value`.
    pub fn item(&self, value: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.value == value)
    }

    /// Drop the row `value` (it was deleted elsewhere); the cursor stays in range.
    pub fn remove(&mut self, value: &str) {
        self.items.retain(|item| item.value != value);
        self.armed = None;
        self.selected = self.selected.min(self.view().len().saturating_sub(1));
    }

    pub fn hints(mut self, hints: Vec<Hint>) -> Self {
        self.hints = hints;
        self
    }

    /// Mark `value` as the current choice and start on it.
    pub fn current(mut self, value: impl Into<String>) -> Self {
        self.current = Some(value.into());
        self.jump_to_current();
        self
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The value under the cursor, if the filtered list is not empty.
    pub fn selected_value(&self) -> Option<&str> {
        let view = self.view();
        let index = *view.get(self.selected.min(view.len().saturating_sub(1)))?;
        Some(&self.items[index].value)
    }

    /// Item indices in display order: filtered, ranked, then grouped by
    /// category in first-seen order (remeda `groupBy`, `dialog-select.tsx:268`).
    fn view(&self) -> Vec<usize> {
        let mut ranked: Vec<(usize, u32)> = if self.filter.is_empty() {
            (0..self.items.len()).map(|index| (index, 0)).collect()
        } else {
            let mut hits: Vec<(usize, u32)> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| !item.unfiltered_only)
                .filter_map(|(index, item)| {
                    let title = fuzzy::score(&self.filter, &item.title);
                    let category = item
                        .category
                        .as_deref()
                        .and_then(|category| fuzzy::score(&self.filter, category));
                    // Title weighs double (`dialog-select.tsx:260`).
                    (title.is_some() || category.is_some())
                        .then(|| (index, title.unwrap_or(0) * 2 + category.unwrap_or(0)))
                })
                .collect();
            hits.sort_by_key(|&(index, score)| (Reverse(score), index));
            hits
        };
        let mut groups: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
        for (index, _) in ranked.drain(..) {
            let category = self.items[index].category.as_deref();
            match groups.iter_mut().find(|(name, _)| *name == category) {
                Some((_, members)) => members.push(index),
                None => groups.push((category, vec![index])),
            }
        }
        groups
            .into_iter()
            .flat_map(|(_, members)| members)
            .collect()
    }

    fn jump_to_current(&mut self) {
        let Some(current) = &self.current else { return };
        if let Some(position) = self
            .view()
            .iter()
            .position(|&index| self.items[index].value == *current)
        {
            self.selected = position;
        }
    }

    fn step(&mut self, delta: isize) -> DialogOutcome {
        let count = self.view().len();
        if count == 0 {
            return DialogOutcome::Idle;
        }
        let next = move_selection(self.selected.min(count - 1), count, delta);
        self.go_to(next)
    }

    fn go_to(&mut self, next: usize) -> DialogOutcome {
        if next == self.selected {
            return DialogOutcome::Idle;
        }
        self.selected = next;
        self.armed = None;
        DialogOutcome::Changed
    }

    /// The filter text changed: restart the list at the best hit (or at the
    /// current value once the filter is cleared).
    fn refilter(&mut self) -> DialogOutcome {
        self.selected = 0;
        self.armed = None;
        self.offset.set(0);
        if self.filter.is_empty() {
            self.jump_to_current();
        }
        DialogOutcome::Changed
    }

    /// `ctrl+<ch>`: the registered action on the selected row, or `None` when
    /// `ch` is not an action (so the key falls through to the list's own keys).
    fn run_action(&mut self, ch: char) -> Option<DialogOutcome> {
        let action = self.actions.iter().find(|action| action.key == ch)?;
        let Some(value) = self.selected_value().map(str::to_owned) else {
            return Some(DialogOutcome::Idle);
        };
        let armed = Armed { key: ch, value };
        if action.confirm.is_some() && self.armed.as_ref() != Some(&armed) {
            self.armed = Some(armed);
            return Some(DialogOutcome::Changed);
        }
        self.armed = None;
        Some(DialogOutcome::Action {
            key: ch,
            value: armed.value,
        })
    }

    /// The confirmation text of the armed row `value`, if it is armed.
    fn confirming(&self, value: &str) -> Option<&str> {
        let armed = self.armed.as_ref().filter(|armed| armed.value == value)?;
        self.actions
            .iter()
            .find(|action| action.key == armed.key)?
            .confirm
            .as_deref()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DialogOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let (true, KeyCode::Char(ch)) = (ctrl && !alt, key.code)
            && let Some(outcome) = self.run_action(ch)
        {
            return outcome;
        }
        match key.code {
            KeyCode::Esc => DialogOutcome::Cancelled,
            KeyCode::Enter => self.selected_value().map_or(DialogOutcome::Idle, |value| {
                DialogOutcome::Selected(value.to_owned())
            }),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('p') if ctrl => self.step(-1),
            KeyCode::Char('n') if ctrl => self.step(1),
            KeyCode::PageUp => self.step(-PAGE),
            KeyCode::PageDown => self.step(PAGE),
            KeyCode::Home => self.go_to(0),
            KeyCode::End => {
                let last = self.view().len().saturating_sub(1);
                self.go_to(last)
            }
            KeyCode::Backspace => {
                if self.filter.pop().is_some() {
                    self.refilter()
                } else {
                    DialogOutcome::Idle
                }
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                self.filter.push(ch);
                self.refilter()
            }
            _ => DialogOutcome::Idle,
        }
    }
}

// ---------------------------------------------------------------------------
// Input: a one-line text prompt (upstream `ui/dialog-prompt.tsx`).
// ---------------------------------------------------------------------------

/// A titled single-line text field. Editing is append/backspace at the end:
/// the only user is renaming, where retyping a title is as fast as moving a
/// cursor, and a text-area widget would pull the composer's keymap into dialogs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    title: String,
    placeholder: String,
    text: String,
    /// What the text is for (a session id…), handed back with the submission.
    target: String,
    tag: Option<&'static str>,
}

impl Input {
    pub fn new(
        title: impl Into<String>,
        placeholder: impl Into<String>,
        target: impl Into<String>,
        initial: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            placeholder: placeholder.into(),
            text: initial.into(),
            target: target.into(),
            tag: None,
        }
    }

    pub fn tag(mut self, tag: &'static str) -> Self {
        self.tag = Some(tag);
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DialogOutcome {
        let blocked = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => DialogOutcome::Cancelled,
            KeyCode::Enter => DialogOutcome::Submitted {
                target: self.target.clone(),
                text: self.text.clone(),
            },
            KeyCode::Backspace => {
                if self.text.pop().is_some() {
                    DialogOutcome::Changed
                } else {
                    DialogOutcome::Idle
                }
            }
            KeyCode::Char(ch) if !blocked => {
                self.text.push(ch);
                DialogOutcome::Changed
            }
            _ => DialogOutcome::Idle,
        }
    }
}

/// Draw an input dialog and return where the text cursor goes. Same chrome as
/// [`draw_select`]: dimmed backdrop, raised panel, title left and `esc` right.
fn draw_input(buf: &mut Buffer, area: Rect, input: &Input, pal: &Palette) -> Position {
    dim_backdrop(buf, area, pal);
    let width = PANEL_WIDTH.min(area.width.saturating_sub(2));
    if width <= TEXT_INSET * 2 || area.height < 8 {
        return area.as_position();
    }
    let top = area.y + ((u32::from(area.height) * 2 + 4) / 8) as u16;
    // Padding, header, gap, input, gap, hints, padding.
    let height = 7u16.min(area.bottom() - top);
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
    let span = right - text_x;
    let esc_x = right - 3;
    put(
        buf,
        panel,
        text_x,
        top + 1,
        esc_x.saturating_sub(text_x + 1),
        &input.title,
        pal.text.add_modifier(Modifier::BOLD),
    );
    put(buf, panel, esc_x, top + 1, 3, "esc", pal.dim);

    let input_y = top + 3;
    let cursor_x = if input.text.is_empty() {
        put(
            buf,
            panel,
            text_x,
            input_y,
            span,
            &input.placeholder,
            pal.dim,
        );
        text_x
    } else {
        put(buf, panel, text_x, input_y, span, &input.text, pal.text);
        (text_x + input.text.width() as u16).min(right.saturating_sub(1))
    };
    let x = put(buf, panel, text_x, top + 5, span, "submit", pal.text);
    put(
        buf,
        panel,
        x,
        top + 5,
        right.saturating_sub(x),
        " enter",
        pal.dim,
    );
    Position::new(cursor_x, input_y)
}

/// Move the cursor by `delta`, wrapping at both ends
/// (`select-controller.ts` `moveSelection`, policy "wrap").
fn move_selection(selected: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let next = selected as isize + delta;
    if next < 0 {
        count - 1
    } else if next >= count as isize {
        0
    } else {
        next as usize
    }
}

/// First list row to draw so that `selected_row` stays visible in a
/// `limit`-row viewport over `total` rows (`dialog-select.tsx:scrollToSelection`
/// without centering). The very first item always scrolls back to the top.
fn reveal_offset(
    offset: usize,
    selected_row: usize,
    first_item: bool,
    limit: usize,
    total: usize,
) -> usize {
    let max = total.saturating_sub(limit);
    let next = if first_item {
        0
    } else if selected_row < offset {
        selected_row
    } else if selected_row >= offset + limit {
        selected_row + 1 - limit
    } else {
        offset
    };
    next.min(max)
}

/// One line of the list viewport.
enum Entry {
    Blank,
    Header(usize),
    Item(usize),
}

/// Lay `view` out as list rows: a header row per category (one blank row
/// before every group but the first), then its items.
fn entries(items: &[Item], view: &[usize]) -> Vec<Entry> {
    let mut out = Vec::with_capacity(view.len() + 4);
    let mut previous: Option<Option<&str>> = None;
    let mut group = 0usize;
    for (position, &index) in view.iter().enumerate() {
        let category = items[index].category.as_deref();
        if previous != Some(category) {
            if previous.is_some() {
                group += 1;
            }
            if category.is_some_and(|name| !name.is_empty()) {
                if group > 0 {
                    out.push(Entry::Blank);
                }
                out.push(Entry::Header(index));
            }
            previous = Some(category);
        }
        out.push(Entry::Item(position));
    }
    out
}

/// Dim everything in `area` the way black at alpha 150 would. `Reset` colours
/// are resolved to the terminal's own colours when the palette knows them;
/// anything that cannot be blended falls back to the DIM modifier.
fn dim_backdrop(buf: &mut Buffer, area: Rect, pal: &Palette) {
    let rgb = |color: Option<Color>| match color {
        Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
        _ => None,
    };
    let terminal_bg = pal.backdrop.map(|c| (c.r, c.g, c.b));
    let terminal_fg = rgb(pal.text.fg);
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if let Some(color) = shade(cell.bg, terminal_bg) {
                cell.bg = color;
            }
            match shade(cell.fg, terminal_fg) {
                Some(color) => cell.fg = color,
                None if cell.symbol() != " " => cell.modifier |= Modifier::DIM,
                None => {}
            }
        }
    }
}

fn shade(color: Color, reset_as: Option<(u8, u8, u8)>) -> Option<Color> {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => reset_as?,
        _ => return None,
    };
    let keep = 255 - BACKDROP_ALPHA;
    let scale = |c: u8| ((u16::from(c) * keep + 127) / 255) as u8;
    Some(Color::Rgb(scale(r), scale(g), scale(b)))
}

/// Write `text` at `(x, y)` inside `clip`, at most `max` cells wide; returns
/// the next free column. Writes outside `clip` are dropped.
fn put(buf: &mut Buffer, clip: Rect, x: u16, y: u16, max: u16, text: &str, style: Style) -> u16 {
    if y < clip.top() || y >= clip.bottom() || x >= clip.right() {
        return x;
    }
    let max = max.min(clip.right() - x);
    buf.set_stringn(x, y, text, usize::from(max), style).0
}

/// Draw a select dialog over `area` and return where the input cursor goes.
fn draw_select(buf: &mut Buffer, area: Rect, select: &Select, pal: &Palette) -> Position {
    dim_backdrop(buf, area, pal);

    let width = select.width.min(area.width.saturating_sub(2));
    if width <= TEXT_INSET * 2 || area.height < 8 {
        return area.as_position();
    }
    let view = select.view();
    let rows = entries(&select.items, &view);
    let trailing = if select.hints.is_empty() { 2 } else { 3 };
    // Upstream rounds `height / 4` (yoga) for the top padding.
    let top = area.y + ((u32::from(area.height) * 2 + 4) / 8) as u16;
    let free = usize::from(area.bottom().saturating_sub(top)).saturating_sub(CHROME_TOP + trailing);
    let limit = (usize::from(area.height) / 2).saturating_sub(6).max(1);
    let body_h = if rows.is_empty() {
        1
    } else {
        rows.len().min(limit).min(free.max(1))
    };
    let height = ((CHROME_TOP + body_h + trailing) as u16).min(area.bottom() - top);
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
    let span = right - text_x;

    // Header: title left, `esc` right.
    let esc_x = right - 3;
    put(
        buf,
        panel,
        text_x,
        top + 1,
        esc_x.saturating_sub(text_x + 1),
        &select.title,
        pal.text.add_modifier(Modifier::BOLD),
    );
    put(buf, panel, esc_x, top + 1, 3, "esc", pal.dim);

    // Search input.
    let search_y = top + 3;
    let cursor_x = if select.filter.is_empty() {
        put(buf, panel, text_x, search_y, span, "Search", pal.dim);
        text_x
    } else {
        put(buf, panel, text_x, search_y, span, &select.filter, pal.text);
        (text_x + select.filter.width() as u16).min(right.saturating_sub(1))
    };

    // List.
    let body_y = top + CHROME_TOP as u16;
    let cursor = select.selected.min(view.len().saturating_sub(1));
    if rows.is_empty() {
        let text = if select.items.is_empty() {
            "No items available"
        } else {
            "No results found"
        };
        put(buf, panel, text_x, body_y, span, text, pal.dim);
    } else {
        let selected_row = rows
            .iter()
            .position(|entry| matches!(entry, Entry::Item(position) if *position == cursor))
            .unwrap_or(0);
        let offset = reveal_offset(
            select.offset.get(),
            selected_row,
            cursor == 0,
            body_h,
            rows.len(),
        );
        select.offset.set(offset);
        for (line, entry) in rows.iter().skip(offset).take(body_h).enumerate() {
            let y = body_y + line as u16;
            match entry {
                Entry::Blank => {}
                Entry::Header(index) => {
                    let name = select.items[*index].category.as_deref().unwrap_or_default();
                    put(buf, panel, text_x, y, span, name, pal.accent);
                }
                Entry::Item(position) => {
                    let item = &select.items[view[*position]];
                    let current = select.current.as_deref() == Some(item.value.as_str());
                    let confirm = select.confirming(&item.value);
                    draw_item(
                        buf,
                        panel,
                        y,
                        item,
                        confirm,
                        *position == cursor,
                        current,
                        pal,
                    );
                }
            }
        }
    }

    // Footer hints, one blank row below the list.
    if !select.hints.is_empty() {
        let y = body_y + body_h as u16 + 1;
        let mut x = text_x;
        for hint in &select.hints {
            x = put(
                buf,
                panel,
                x,
                y,
                right.saturating_sub(x),
                &hint.title,
                pal.text,
            );
            x = put(
                buf,
                panel,
                x,
                y,
                right.saturating_sub(x),
                &format!(" {}", hint.key),
                pal.dim,
            );
            x += 2;
        }
    }

    Position::new(cursor_x, search_y)
}

/// One list row (`DialogSelectRow`, old-design branch: the gold capture
/// shows bold + fill for the active row, `●` for the current value).
#[allow(clippy::too_many_arguments)]
fn draw_item(
    buf: &mut Buffer,
    panel: Rect,
    y: u16,
    item: &Item,
    confirm: Option<&str>,
    active: bool,
    current: bool,
    pal: &Palette,
) {
    let text_x = panel.x + TEXT_INSET;
    let right = panel.right() - TEXT_INSET;
    // An armed destructive action repaints the whole row in the error colour
    // (`background.action.destructive.focused`) with the confirm text as title.
    let destructive = confirm.map(|_| Style {
        fg: pal.menu_selected.fg,
        bg: pal.warn.fg,
        add_modifier: pal.menu_selected.add_modifier,
        ..Style::default()
    });
    let title = confirm.unwrap_or(&item.title);
    let description = item.description.as_deref().filter(|_| confirm.is_none());
    let (title_style, secondary) = if let Some(style) = destructive {
        buf.set_style(
            Rect {
                x: panel.x + 1,
                y,
                width: panel.width - 2,
                height: 1,
            },
            style,
        );
        (style.add_modifier(Modifier::BOLD), style)
    } else if active {
        buf.set_style(
            Rect {
                x: panel.x + 1,
                y,
                width: panel.width - 2,
                height: 1,
            },
            pal.menu_selected,
        );
        (
            pal.menu_selected.add_modifier(Modifier::BOLD),
            pal.menu_selected,
        )
    } else if current {
        (
            Style {
                fg: pal.accent.fg,
                ..Style::default()
            },
            pal.dim,
        )
    } else {
        (pal.text, pal.dim)
    };
    if current {
        put(buf, panel, panel.x + 2, y, 1, "\u{25cf}", title_style);
    }
    // The footer takes its own width from the right; title and description
    // share what is left, minus one separating cell.
    let mut limit = right;
    if let Some(footer) = &item.footer {
        let wanted = footer.width() as u16;
        let x = right.saturating_sub(wanted).max(text_x);
        put(buf, panel, x, y, right - x, footer, secondary);
        limit = x.saturating_sub(1);
    }
    let mut x = put(
        buf,
        panel,
        text_x,
        y,
        limit.saturating_sub(text_x),
        title,
        title_style,
    );
    if let Some(description) = description {
        x = put(buf, panel, x, y, limit.saturating_sub(x), " ", title_style);
        put(
            buf,
            panel,
            x,
            y,
            limit.saturating_sub(x),
            description,
            secondary,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Dialog, DialogOutcome, Hint, Info, Item, Select, Tone, render, status};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ox_client::rest::{McpRef, McpStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn server(name: &str, status: McpStatus) -> McpRef {
        McpRef {
            name: name.to_owned(),
            status,
        }
    }

    fn info(dialog: &Dialog) -> &Info {
        match dialog {
            Dialog::Info(info) => info,
            Dialog::Select(_) | Dialog::Input(_) | Dialog::Form(_) => {
                panic!("expected an info dialog")
            }
        }
    }

    fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn status_lists_every_server_with_its_state() {
        let dialog = status(&[
            server("context7", McpStatus::Connected),
            server(
                "zread",
                McpStatus::Failed {
                    error: "connection refused".into(),
                },
            ),
            server("old", McpStatus::Disabled),
        ]);
        let dialog = info(&dialog);
        assert_eq!(dialog.title, "Status");
        assert_eq!(dialog.summary.as_deref(), Some("3 MCP servers"));
        assert_eq!(dialog.rows.len(), 3);
        assert_eq!(dialog.rows[0].tone, Tone::Accent);
        assert_eq!(dialog.rows[0].detail, "Connected");
        assert_eq!(dialog.rows[1].tone, Tone::Warn);
        assert_eq!(dialog.rows[1].detail, "connection refused");
        assert_eq!(dialog.rows[2].detail, "Disabled in configuration");
    }

    #[test]
    fn status_says_so_when_no_server_is_configured() {
        let dialog = status(&[]);
        let dialog = info(&dialog);
        assert!(dialog.summary.is_none());
        assert!(dialog.rows.is_empty());
    }

    #[test]
    fn render_centers_the_panel_and_draws_every_row() {
        let dialog = status(&[
            server("context7", McpStatus::Connected),
            server("zread", McpStatus::Pending),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), &dialog))
            .expect("draw");
        let buf = terminal.backend().buffer();

        // 80 - 8 = 72 → clamped to 64 wide, so x = 8; the 5-row box starts at
        // y = (24 - 5) / 2 = 9.
        assert_eq!(buf[(8, 9)].symbol(), "\u{250c}", "top-left corner");
        let title = row_text(buf, 9, 80);
        assert!(title.contains("Status"), "{title:?}");
        let body: Vec<String> = (10..14).map(|y| row_text(buf, y, 80)).collect();
        let joined = body.join("\n");
        assert!(joined.contains("2 MCP servers"), "{joined}");
        assert!(joined.contains("context7"), "{joined}");
        assert!(joined.contains("Connected"), "{joined}");
        assert!(joined.contains("zread"), "{joined}");
        assert!(joined.contains("Pending"), "{joined}");
    }

    #[test]
    fn an_empty_status_dialog_still_explains_itself() {
        let dialog = status(&[]);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), &dialog))
            .expect("draw");
        let text: String = (0..12)
            .map(|y| row_text(terminal.backend().buffer(), y, 60))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("No MCP servers"), "{text}");
    }

    #[test]
    fn info_dialog_never_panics_in_a_tiny_area() {
        let dialog = status(&[server("context7", McpStatus::Connected)]);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
        for (width, height) in [(40, 0), (40, 1), (40, 2), (0, 0), (1, 5), (5, 5), (19, 3)] {
            terminal
                .draw(|frame| {
                    let area = ratatui::layout::Rect::new(0, 0, width, height);
                    render(frame, area, &dialog);
                })
                .expect("draw");
        }
    }

    #[test]
    fn an_oversized_dialog_is_clamped_to_the_screen() {
        let dialog = Dialog::Info(Info {
            title: "Status",
            summary: None,
            rows: (0..40)
                .map(|i| super::Row {
                    tone: Tone::Dim,
                    label: format!("s{i}"),
                    detail: String::new(),
                })
                .collect(),
        });
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), &dialog))
            .expect("draw");
        let buf = terminal.backend().buffer();
        // Height clamps to the screen (10), width to 32 → x = 4, and nothing is
        // drawn outside it.
        assert_eq!(buf[(4, 0)].symbol(), "\u{250c}");
        assert_eq!(buf[(4, 9)].symbol(), "\u{2514}");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn type_text(select: &mut Select, text: &str) {
        for ch in text.chars() {
            select.on_key(key(KeyCode::Char(ch)));
        }
    }

    fn models() -> Select {
        Select::new(
            "Select model",
            vec![
                Item::new("a/one", "One").category("Alpha"),
                Item::new("a/two", "Two").category("Alpha"),
                Item::new("b/three", "Three").category("Beta"),
                Item::new("a/four", "Four").category("Alpha"),
            ],
        )
    }

    #[test]
    fn items_are_grouped_by_category_in_first_seen_order() {
        let select = models();
        let order: Vec<&str> = select
            .view()
            .into_iter()
            .map(|i| select.items[i].title.as_str())
            .collect();
        assert_eq!(order, ["One", "Two", "Four", "Three"]);
    }

    #[test]
    fn arrows_and_ctrl_keys_move_and_wrap() {
        let mut select = models();
        assert_eq!(select.selected_value(), Some("a/one"));
        assert_eq!(select.on_key(key(KeyCode::Down)), DialogOutcome::Changed);
        assert_eq!(select.selected_value(), Some("a/two"));
        select.on_key(ctrl('n'));
        assert_eq!(select.selected_value(), Some("a/four"));
        select.on_key(ctrl('p'));
        select.on_key(key(KeyCode::Up));
        assert_eq!(select.selected_value(), Some("a/one"));
        select.on_key(key(KeyCode::Up));
        assert_eq!(select.selected_value(), Some("b/three"), "wraps to the end");
        select.on_key(key(KeyCode::Down));
        assert_eq!(select.selected_value(), Some("a/one"), "wraps to the start");
    }

    #[test]
    fn page_keys_move_ten_and_wrap() {
        let items = (0..25)
            .map(|i| Item::new(format!("v{i}"), format!("Item {i}")))
            .collect();
        let mut select = Select::new("Many", items);
        select.on_key(key(KeyCode::PageDown));
        assert_eq!(select.selected_value(), Some("v10"));
        select.on_key(key(KeyCode::PageDown));
        select.on_key(key(KeyCode::PageDown));
        assert_eq!(select.selected_value(), Some("v0"), "30 wraps past 25");
        select.on_key(key(KeyCode::PageUp));
        assert_eq!(select.selected_value(), Some("v24"));
    }

    #[test]
    fn enter_returns_the_value_and_esc_cancels() {
        let mut select = models();
        select.on_key(key(KeyCode::Down));
        assert_eq!(
            select.on_key(key(KeyCode::Enter)),
            DialogOutcome::Selected("a/two".into())
        );
        assert_eq!(select.on_key(key(KeyCode::Esc)), DialogOutcome::Cancelled);
    }

    #[test]
    fn typing_filters_by_fuzzy_score_and_resets_the_cursor() {
        let mut select = models();
        select.on_key(key(KeyCode::Down));
        type_text(&mut select, "th");
        assert_eq!(select.filter(), "th");
        assert_eq!(select.selected_value(), Some("b/three"));
        select.on_key(key(KeyCode::Backspace));
        select.on_key(key(KeyCode::Backspace));
        assert_eq!(select.selected_value(), Some("a/one"));
        assert_eq!(select.on_key(key(KeyCode::Backspace)), DialogOutcome::Idle);
    }

    #[test]
    fn a_category_match_keeps_the_row_and_titles_rank_first() {
        let mut select = models();
        type_text(&mut select, "beta");
        assert_eq!(select.selected_value(), Some("b/three"));
        let mut select = Select::new(
            "T",
            vec![
                Item::new("c", "xxaxx").category("Cat"),
                Item::new("p", "axx").category("Cat"),
            ],
        );
        type_text(&mut select, "a");
        assert_eq!(select.selected_value(), Some("p"), "prefix beats substring");
    }

    #[test]
    fn an_empty_result_has_nothing_to_select() {
        let mut select = models();
        type_text(&mut select, "zzz");
        assert_eq!(select.selected_value(), None);
        assert_eq!(select.on_key(key(KeyCode::Enter)), DialogOutcome::Idle);
        assert_eq!(select.on_key(key(KeyCode::Down)), DialogOutcome::Idle);
        assert_eq!(select.on_key(key(KeyCode::PageUp)), DialogOutcome::Idle);
    }

    #[test]
    fn the_cursor_stays_in_range_after_the_list_shrinks() {
        let mut select = models();
        select.on_key(key(KeyCode::End));
        assert_eq!(select.selected_value(), Some("b/three"));
        type_text(&mut select, "o");
        assert!(select.selected_value().is_some());
        let n = select.view().len();
        assert!(select.selected < n);
    }

    #[test]
    fn current_value_is_where_the_dialog_starts() {
        let select = models().current("b/three");
        assert_eq!(select.selected_value(), Some("b/three"));
    }

    #[test]
    fn unrelated_keys_do_nothing() {
        let mut select = models();
        assert_eq!(select.on_key(key(KeyCode::F(5))), DialogOutcome::Idle);
        assert_eq!(select.on_key(ctrl('x')), DialogOutcome::Idle);
    }

    #[test]
    fn move_selection_wraps_both_ways() {
        assert_eq!(super::move_selection(0, 3, -1), 2);
        assert_eq!(super::move_selection(2, 3, 1), 0);
        assert_eq!(super::move_selection(1, 3, 1), 2);
        assert_eq!(super::move_selection(0, 0, 1), 0);
    }

    #[test]
    fn reveal_offset_keeps_the_selection_in_view() {
        assert_eq!(super::reveal_offset(0, 12, false, 5, 20), 8);
        assert_eq!(super::reveal_offset(8, 3, false, 5, 20), 3);
        assert_eq!(super::reveal_offset(8, 10, false, 5, 20), 8);
        assert_eq!(super::reveal_offset(8, 0, true, 5, 20), 0);
        assert_eq!(super::reveal_offset(30, 19, false, 5, 20), 15);
    }

    fn draw(select: &Select, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        let dialog = Dialog::Select(select.clone());
        terminal
            .draw(|frame| render(frame, frame.area(), &dialog))
            .expect("draw");
        (0..h)
            .map(|y| row_text(terminal.backend().buffer(), y, w))
            .collect()
    }

    #[test]
    fn select_draws_title_search_groups_and_footers() {
        let select = Select::new(
            "Commands",
            vec![
                Item::new("s", "Switch session")
                    .category("Suggested")
                    .footer("ctrl+x l"),
                Item::new("n", "New session").category("Suggested"),
                Item::new("q", "Queue prompt")
                    .category("Prompt")
                    .description("later"),
            ],
        )
        .hints(vec![Hint::new("Select", "enter")]);
        let screen = draw(&select, 100, 30).join("\n");
        for needle in [
            "Commands",
            "esc",
            "Search",
            "Suggested",
            "Switch session",
            "ctrl+x l",
            "Prompt",
            "Queue prompt later",
            "Select enter",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn an_unfiltered_only_item_hides_once_the_user_types() {
        let mut select = Select::new(
            "Commands",
            vec![
                Item::new("s", "Switch session")
                    .category("Suggested")
                    .unfiltered_only(),
                Item::new("t", "Switch session").category("Session"),
            ],
        );
        let before = draw(&select, 100, 30).join("\n");
        assert!(before.contains("Suggested"), "{before}");
        type_text(&mut select, "switch");
        let after = draw(&select, 100, 30).join("\n");
        assert!(!after.contains("Suggested"), "{after}");
        assert!(after.contains("Switch session"), "{after}");
    }

    #[test]
    fn select_shows_no_results_for_an_empty_filter_hit() {
        let mut select = models();
        type_text(&mut select, "zzz");
        let screen = draw(&select, 100, 30).join("\n");
        assert!(screen.contains("No results found"), "{screen}");
        assert!(screen.contains("zzz"), "{screen}");
    }

    #[test]
    fn select_renders_at_width_40_without_overflow() {
        let select = Select::new(
            "A long dialog title that cannot fit",
            vec![
                Item::new("1", "A rather long item title that must be truncated")
                    .category("Provider")
                    .footer("footer text"),
            ],
        );
        let lines = draw(&select, 40, 20);
        assert!(lines.iter().all(|line| line.chars().count() <= 40));
        assert!(lines.join("\n").contains("Provider"));
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_visible() {
        let items = (0..40)
            .map(|i| Item::new(format!("v{i}"), format!("Item {i}")))
            .collect();
        let mut select = Select::new("Many", items);
        let first = draw(&select, 100, 30).join("\n");
        assert!(first.contains("Item 0") && !first.contains("Item 20"));
        select.on_key(key(KeyCode::End));
        let last = draw(&select, 100, 30).join("\n");
        assert!(
            last.contains("Item 39") && !last.contains("Item 0\n"),
            "{last}"
        );
    }
}
