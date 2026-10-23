//! The input surface shared by the home and session routes: left-bordered panel,
//! metadata row, `╹`/`▀` bottom edge, footer row, and the slash-command menu.
//!
//! Visual spec and deviations are recorded in
//! `.ox/plans/ox-tui-prompt-surface.md`. Upstream reference:
//! `component/prompt/index.tsx` (panel), `component/prompt/metadata.tsx`
//! (metadata degradation), `component/prompt/autocomplete.tsx` (menu).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use crate::theme;
use crate::ui::autocomplete::Autocomplete;
use crate::ui::commands::{self, CommandAction, Row};
use crate::ui::composer::Composer;

pub const PLACEHOLDERS: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];

/// Upstream placeholder shape: `Ask anything… "<random phrase>"`.
pub fn placeholder_line(idx: usize) -> String {
    format!(
        "Ask anything… \"{}\"",
        PLACEHOLDERS[idx % PLACEHOLDERS.len()]
    )
}

/// Pick one of the phrases without a random-number dependency.
pub fn pick_placeholder() -> usize {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    nanos % PLACEHOLDERS.len()
}

/// Static decoration shown around the input (built by `bootstrap::chrome`,
/// degrading to empty when an endpoint is unavailable).
#[derive(Debug, Clone, Default)]
pub struct Chrome {
    /// Default agent display name (`Build`).
    pub agent: Option<String>,
    /// Model id as shown in the metadata row (`k3-256k`).
    pub model: Option<String>,
    /// Provider display name (`Cardinal`), a separate segment upstream.
    pub provider: Option<String>,
    /// `~`-abbreviated working directory.
    pub directory: String,
    /// Current git branch.
    pub branch: Option<String>,
}

/// What a key meant to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAction {
    /// The widget handled it (typed text, menu navigation, cleared draft).
    Ignored,
    /// The user submitted a non-empty prompt.
    Submit(String),
    /// A slash command was chosen; the token was removed from the draft.
    Run(CommandAction),
    /// ctrl+c with an empty draft: the app decides interrupt vs quit.
    CtrlC,
}

pub struct Prompt {
    composer: Composer,
    auto: Autocomplete,
    chrome: Chrome,
    hints: &'static str,
}

impl Prompt {
    pub fn new(placeholder: &str, chrome: Chrome, hints: &'static str) -> Self {
        Self {
            composer: Composer::new_with_placeholder(placeholder),
            auto: Autocomplete::default(),
            chrome,
            hints,
        }
    }

    pub fn text(&self) -> String {
        self.composer.text()
    }

    pub fn is_empty(&self) -> bool {
        self.composer.is_empty()
    }

    pub fn restore(&mut self, text: &str) {
        self.composer.restore(text);
    }

    pub fn clear(&mut self) {
        self.composer.clear();
        self.auto.hide();
    }

    pub fn set_placeholder(&mut self, placeholder: &str) {
        self.composer.set_placeholder(placeholder);
        self.auto.hide();
    }

    pub fn set_hints(&mut self, hints: &'static str) {
        self.hints = hints;
    }

    pub fn autocomplete_visible(&self) -> bool {
        self.auto.visible()
    }

    /// Rows the whole block needs: panel, `╹`/`▀` edge and footer row.
    pub fn height(&self, terminal_height: u16) -> u16 {
        self.panel_height(terminal_height) + 2
    }

    fn panel_height(&self, terminal_height: u16) -> u16 {
        // paddingTop + textarea + paddingTop + metadata row
        1 + self.composer.height(terminal_height) + 1 + 1
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PromptAction {
        if self.auto.visible() {
            return self.handle_menu_key(key);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.composer.is_empty() {
                return PromptAction::CtrlC;
            }
            self.composer.clear();
            return PromptAction::Ignored;
        }
        let Some(text) = self.composer.handle_key(key) else {
            self.sync_menu();
            return PromptAction::Ignored;
        };
        self.auto.hide();
        PromptAction::Submit(text)
    }

    fn handle_menu_key(&mut self, key: KeyEvent) -> PromptAction {
        let rows = self.rows();
        let count = rows.len();
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.auto.hide();
                PromptAction::Ignored
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.auto.move_by(-1, count);
                PromptAction::Ignored
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.auto.move_by(1, count);
                PromptAction::Ignored
            }
            (KeyCode::Enter, modifiers) if !modifiers.contains(KeyModifiers::SHIFT) => {
                self.select(&rows)
            }
            (KeyCode::Tab, _) => self.select(&rows),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Upstream `hide(true)`: drop the typed `/token`, keep the rest.
                let (_, column) = self.composer.cursor();
                self.composer.delete_leading(column);
                self.auto.hide();
                PromptAction::Ignored
            }
            _ => {
                if let Some(text) = self.composer.handle_key(key) {
                    self.auto.hide();
                    return PromptAction::Submit(text);
                }
                self.sync_menu();
                PromptAction::Ignored
            }
        }
    }

    fn select(&mut self, rows: &[Row]) -> PromptAction {
        let Some(row) = rows.get(self.auto.selected()) else {
            return PromptAction::Ignored;
        };
        let action = row.action;
        let (_, column) = self.composer.cursor();
        self.composer.delete_leading(column);
        self.auto.hide();
        PromptAction::Run(action)
    }

    /// Re-derive the menu from the editor; cheap enough to run after every key.
    fn sync_menu(&mut self) {
        let (row, column) = self.composer.cursor();
        self.auto.sync(&self.composer.text(), (row, column));
        let count = self.rows().len();
        self.auto.clamp(count);
    }

    fn rows(&self) -> Vec<Row> {
        commands::rows(self.auto.query())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, full: Rect, running: bool) {
        let pal = *theme::palette();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let pad = if full.width < 44 { 1 } else { 2 };
        let composer_h = self
            .composer
            .height(full.height)
            .min(area.height.saturating_sub(3).max(1));
        let panel_h = 1 + composer_h + 1 + 1;
        let panel = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: panel_h.min(area.height),
        };

        // Menu first: it sits above the panel and may overlap the transcript.
        if self.auto.visible() {
            self.render_menu(frame, area, full);
        }

        // Panel background and left border.
        {
            let buf = frame.buffer_mut();
            for y in panel.y..panel.bottom() {
                for x in panel.x..panel.right() {
                    buf[(x, y)].set_style(pal.panel);
                }
                buf[(panel.x, y)].set_style(pal.panel_border);
                buf[(panel.x, y)].set_char('▌');
            }
        }

        let inner_x = panel.x + 1 + pad;
        let inner_w = panel.width.saturating_sub(1 + pad * 2).max(1);
        let text_area = Rect {
            x: inner_x,
            y: panel.y + 1,
            width: inner_w,
            height: composer_h.min(panel.height.saturating_sub(1)),
        };
        if text_area.height > 0 {
            self.composer.render(frame, text_area);
        }

        // Metadata row, one blank row under the textarea.
        let meta_y = panel.y + 1 + composer_h + 1;
        if meta_y < panel.bottom() {
            let line = metadata_line(
                self.chrome.agent.as_deref(),
                self.chrome.model.as_deref(),
                self.chrome.provider.as_deref(),
                inner_w,
                full.width,
                &pal,
            );
            let rect = Rect {
                x: inner_x,
                y: meta_y,
                width: inner_w,
                height: 1,
            };
            frame.render_widget(Paragraph::new(line), rect);
        }

        // `╹` on the border column plus a `▀` shadow line across the width.
        let edge_y = panel.bottom();
        if edge_y < area.bottom() {
            let buf = frame.buffer_mut();
            let corner = &mut buf[(area.x, edge_y)];
            corner.set_char('╹');
            if let Some(fg) = pal.panel_border.fg {
                corner.set_fg(fg);
            }
            if let Some(bg) = pal.panel.bg {
                for x in (area.x + 1)..area.right() {
                    let cell = &mut buf[(x, edge_y)];
                    cell.set_char('▀');
                    cell.set_fg(bg);
                }
            }
        }

        // Footer: location (or the running state) left, hints right.
        let footer_y = edge_y + 1;
        if footer_y < area.bottom() {
            let (left, style) = if running {
                (
                    "● esc interrupt".to_string(),
                    pal.dim.add_modifier(Modifier::BOLD),
                )
            } else {
                (self.location(), pal.dim)
            };
            let left_w = left.width().min(area.width as usize) as u16;
            frame.render_widget(
                Paragraph::new(Line::styled(left, style)),
                Rect {
                    x: area.x,
                    y: footer_y,
                    width: left_w,
                    height: 1,
                },
            );
            if !self.hints.is_empty() {
                let hints_w = (self.hints.width() as u16).min(area.width);
                let x = area.right().saturating_sub(hints_w);
                if x >= area.x + left_w {
                    frame.render_widget(
                        Paragraph::new(Line::styled(self.hints, pal.dim)),
                        Rect {
                            x,
                            y: footer_y,
                            width: hints_w,
                            height: 1,
                        },
                    );
                }
            }
        }
    }

    /// `~/path:branch` (upstream uses a colon, not a dot).
    fn location(&self) -> String {
        match &self.chrome.branch {
            Some(branch) if !branch.is_empty() => {
                format!("{}:{branch}", self.chrome.directory)
            }
            _ => self.chrome.directory.clone(),
        }
    }

    fn render_menu(&mut self, frame: &mut Frame, area: Rect, full: Rect) {
        let pal = *theme::palette();
        let rows = self.rows();
        let wanted = (rows.len().clamp(1, 10) + 2) as u16;
        let available = area.y.saturating_sub(full.y);
        let height = wanted.min(available);
        if height < 3 || area.width < 4 {
            return;
        }
        let rect = Rect {
            x: area.x,
            y: area.y - height,
            width: area.width,
            height,
        };
        Block::bordered()
            .border_style(pal.menu_border)
            .style(pal.menu)
            .render(rect, frame.buffer_mut());

        let inner = Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::styled("No matching commands", pal.dim)).style(pal.menu),
                inner,
            );
            return;
        }
        let visible = inner.height as usize;
        let offset = self.auto.selected().saturating_sub(visible - 1);
        {
            let buf = frame.buffer_mut();
            for (index, row) in rows.iter().skip(offset).take(visible).enumerate() {
                let y = inner.y + index as u16;
                let row_rect = Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                };
                let selected = offset + index == self.auto.selected();
                let description = format!(" {}", row.description);
                let line = if selected {
                    Line::from(vec![Span::raw(row.display.clone()), Span::raw(description)])
                } else {
                    Line::from(vec![
                        Span::styled(row.display.clone(), pal.text),
                        Span::styled(description, pal.dim),
                    ])
                };
                let style = if selected {
                    pal.menu_selected
                } else {
                    pal.menu
                };
                Paragraph::new(line).style(style).render(row_rect, buf);
            }
        }
    }
}

/// `agent · model provider` with upstream's degradation chain.
///
/// Candidates are tried in order until one fits: full, provider's last path
/// segment, no provider, no agent, model only. If none fit, the model is
/// end-truncated with an ellipsis. `terminal_width < 44` drops agent/provider
/// outright (upstream `promptMetadataLayout`).
///
/// Deviations: ox has no permission mode (`auto`) and no model variant
/// selector, so those segments are always absent.
pub fn metadata_line(
    agent: Option<&str>,
    model: Option<&str>,
    provider: Option<&str>,
    inner_width: u16,
    terminal_width: u16,
    pal: &theme::Palette,
) -> Line<'static> {
    let narrow = terminal_width < 44;
    let agent = if narrow {
        None
    } else {
        agent.filter(|value| !value.is_empty())
    };
    let provider = if narrow {
        None
    } else {
        provider.filter(|value| !value.is_empty())
    };
    let model = model.filter(|value| !value.is_empty());
    if agent.is_none() && model.is_none() {
        return Line::default();
    }
    let width = inner_width as usize;
    let provider_last = provider.map(|value| value.rsplit(" / ").next().unwrap_or(value));

    let candidates = [
        (agent, model, provider),
        (agent, model, provider_last),
        (agent, model, None),
        (None, model, provider),
        (None, model, None),
    ];
    for (candidate_agent, candidate_model, candidate_provider) in candidates {
        let line = metadata_spans(candidate_agent, candidate_model, candidate_provider, pal);
        if line.width() <= width {
            return line;
        }
    }
    // Nothing fits: keep the agent prefix only if it leaves room for the model.
    let prefix = match agent {
        Some(agent) if model.is_some() && agent.width() + 2 < width => format!("{agent} · "),
        _ => String::new(),
    };
    let room = width.saturating_sub(prefix.width()).max(1);
    let model = truncate_end(model.unwrap_or_default(), room);
    Line::from(vec![
        Span::styled(prefix, pal.agent),
        Span::styled(model, pal.text),
    ])
}

fn metadata_spans(
    agent: Option<&str>,
    model: Option<&str>,
    provider: Option<&str>,
    pal: &theme::Palette,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(agent) = agent {
        spans.push(Span::styled(agent.to_owned(), pal.agent));
    }
    if let Some(model) = model {
        if agent.is_some() {
            spans.push(Span::styled(" · ", pal.dim));
        }
        spans.push(Span::styled(model.to_owned(), pal.text));
    }
    if let Some(provider) = provider {
        if model.is_some() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(provider.to_owned(), pal.dim));
    }
    Line::from(spans)
}

fn truncate_end(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let next = out.width() + ch.to_string().width();
        if next > width - 1 {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}
