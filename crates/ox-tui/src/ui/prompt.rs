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

/// Paint helper used by tests: read a buffer row as text.
#[cfg(test)]
fn row_text(buf: &ratatui::buffer::Buffer, y: u16, from: u16, to: u16) -> String {
    (from..to).map(|x| buf[(x, y)].symbol()).collect()
}

#[cfg(test)]
mod tests {
    use super::{Chrome, Prompt, PromptAction, metadata_line, placeholder_line, row_text};
    use crate::theme::{Deck, Palette};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn pal() -> Palette {
        Palette::for_deck(Deck::Truecolor)
    }

    fn chrome() -> Chrome {
        Chrome {
            agent: Some("Build".into()),
            model: Some("k3-256k".into()),
            provider: Some("Cardinal".into()),
            directory: "~/Developer/openkitbox/ox".into(),
            branch: Some("main".into()),
        }
    }

    fn prompt() -> Prompt {
        Prompt::new(&placeholder_line(0), chrome(), "ctrl+f search")
    }

    fn key(prompt: &mut Prompt, code: KeyCode, modifiers: KeyModifiers) -> PromptAction {
        prompt.handle_key(KeyEvent::new(code, modifiers))
    }

    fn type_text(prompt: &mut Prompt, text: &str) {
        for ch in text.chars() {
            key(prompt, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn draw(prompt: &mut Prompt, width: u16, height: u16) -> TestBackend {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                let h = prompt.height(height).min(height);
                let prompt_area = Rect {
                    x: 0,
                    y: area.height - h,
                    width,
                    height: h,
                };
                prompt.render(frame, prompt_area, frame.area(), false);
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn placeholder_wraps_a_phrase_in_the_upstream_format() {
        assert_eq!(
            placeholder_line(0),
            "Ask anything… \"Fix a TODO in the codebase\""
        );
        assert_eq!(placeholder_line(3), placeholder_line(0));
    }

    #[test]
    fn metadata_joins_agent_model_and_provider() {
        let line = metadata_line(
            Some("Build"),
            Some("k3-256k"),
            Some("Cardinal"),
            70,
            100,
            &pal(),
        );
        assert_eq!(text(&line), "Build · k3-256k Cardinal");
    }

    #[test]
    fn metadata_drops_provider_then_agent_as_width_shrinks() {
        // 18 columns fits "Build · k3-256k" but not the provider.
        let line = metadata_line(
            Some("Build"),
            Some("k3-256k"),
            Some("Cardinal"),
            18,
            100,
            &pal(),
        );
        assert_eq!(text(&line), "Build · k3-256k");
        // 12 columns only fits the model.
        let line = metadata_line(
            Some("Build"),
            Some("k3-256k"),
            Some("Cardinal"),
            12,
            100,
            &pal(),
        );
        assert_eq!(text(&line), "k3-256k");
    }

    #[test]
    fn metadata_truncates_the_model_when_nothing_fits() {
        let line = metadata_line(
            Some("Build"),
            Some("k3-256k-long"),
            Some("Cardinal"),
            6,
            100,
            &pal(),
        );
        assert!(text(&line).ends_with('…'), "{:?}", text(&line));
        assert!(
            line.width() <= 6,
            "text={:?} width={}",
            text(&line),
            line.width()
        );
    }

    #[test]
    fn metadata_hides_agent_on_narrow_terminals() {
        let line = metadata_line(
            Some("Build"),
            Some("k3-256k"),
            Some("Cardinal"),
            70,
            43,
            &pal(),
        );
        assert_eq!(text(&line), "k3-256k");
    }

    #[test]
    fn metadata_is_empty_without_data() {
        let line = metadata_line(None, None, None, 70, 100, &pal());
        assert!(text(&line).is_empty());
    }

    #[test]
    fn slash_opens_the_menu_and_enter_runs_the_command() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/new");
        assert!(prompt.autocomplete_visible());
        let action = key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            action,
            PromptAction::Run(crate::ui::commands::CommandAction::New)
        );
        assert!(prompt.is_empty(), "the token is consumed");
        assert!(!prompt.autocomplete_visible());
    }

    #[test]
    fn tab_also_runs_and_escape_only_hides() {
        let mut running = prompt();
        type_text(&mut running, "/ex");
        assert_eq!(
            key(&mut running, KeyCode::Tab, KeyModifiers::NONE),
            PromptAction::Run(crate::ui::commands::CommandAction::Exit)
        );
        assert!(running.is_empty());

        let mut escaping = prompt();
        type_text(&mut escaping, "/ex");
        key(&mut escaping, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!escaping.autocomplete_visible());
        assert_eq!(escaping.text(), "/ex", "escape keeps the typed token");
    }

    #[test]
    fn arrow_keys_move_the_selection_with_wrap() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/");
        let rows = crate::ui::commands::rows("");
        key(&mut prompt, KeyCode::Up, KeyModifiers::NONE);
        // Up from row 0 wraps to the last row: /quit.
        assert_eq!(
            key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE),
            PromptAction::Run(crate::ui::commands::CommandAction::Exit)
        );
        assert_eq!(rows.last().map(|row| row.name), Some("quit"));
    }

    #[test]
    fn ctrl_c_in_the_menu_drops_the_token_only() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/ne");
        key(&mut prompt, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(prompt.is_empty());
        assert!(!prompt.autocomplete_visible());
    }

    #[test]
    fn ctrl_c_with_a_draft_clears_it_and_without_one_bubbles_up() {
        let mut prompt = prompt();
        type_text(&mut prompt, "hello");
        assert_eq!(
            key(&mut prompt, KeyCode::Char('c'), KeyModifiers::CONTROL),
            PromptAction::Ignored
        );
        assert!(prompt.is_empty());
        assert_eq!(
            key(&mut prompt, KeyCode::Char('c'), KeyModifiers::CONTROL),
            PromptAction::CtrlC
        );
    }

    #[test]
    fn enter_submits_when_the_menu_is_closed() {
        let mut prompt = prompt();
        type_text(&mut prompt, "hi there");
        assert_eq!(
            key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE),
            PromptAction::Submit("hi there".into())
        );
    }

    #[test]
    fn a_slash_inside_the_text_is_just_a_character() {
        let mut prompt = prompt();
        type_text(&mut prompt, "and/or");
        assert!(!prompt.autocomplete_visible());
        assert_eq!(prompt.text(), "and/or");
    }

    #[test]
    fn renders_panel_edge_metadata_and_footer() {
        let mut prompt = prompt();
        let height = 24;
        let backend = draw(&mut prompt, 100, height);
        let buf = backend.buffer();
        let total = prompt.height(height);
        let top = height - total;
        let panel_h = total - 2;

        // Left border on every panel row.
        for y in top..top + panel_h {
            assert_eq!(buf[(0, y)].symbol(), "▌", "row {y}");
        }
        // Panel background reaches the inner padding columns.
        assert_eq!(buf[(2, top)].bg, pal().panel.bg.expect("panel bg"));
        // Metadata row sits two rows under the single text line.
        let meta = row_text(buf, top + 3, 0, 40);
        assert!(meta.contains("Build · k3-256k Cardinal"), "{meta:?}");
        // `╹` + `▀` edge.
        assert_eq!(buf[(0, top + panel_h)].symbol(), "╹");
        assert_eq!(buf[(1, top + panel_h)].symbol(), "▀");
        // Footer: location with a colon and the hint on the right.
        let footer = row_text(buf, top + panel_h + 1, 0, 100);
        assert!(
            footer.contains("~/Developer/openkitbox/ox:main"),
            "{footer:?}"
        );
        assert!(footer.contains("ctrl+f search"), "{footer:?}");
    }

    #[test]
    fn menu_renders_above_the_panel_with_a_selected_row() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/");
        let backend = draw(&mut prompt, 100, 24);
        let buf = backend.buffer();
        let total = prompt.height(24);
        let top = 24 - total;

        // Menu is a box: 5 rows (border + 3 commands) + 2 borders = 7.
        let rows = crate::ui::commands::rows("");
        let menu_h = rows.len() as u16 + 2;
        let menu_top = top - menu_h;
        assert_eq!(buf[(0, menu_top)].symbol(), "┌");
        assert_eq!(buf[(0, menu_top + 1)].bg, pal().menu.bg.expect("menu bg"));
        // First row (clear) is selected and inverted.
        let selected = row_text(buf, menu_top + 1, 1, 12);
        assert!(selected.contains("/clear"), "{selected:?}");
        assert_eq!(
            buf[(1, menu_top + 1)].bg,
            pal().menu_selected.bg.expect("selected bg")
        );
        // Second row is not selected.
        assert_eq!(buf[(1, menu_top + 2)].bg, pal().menu.bg.expect("menu bg"));
    }

    #[test]
    fn menu_shows_an_empty_state_for_an_unmatched_query() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/zzz");
        let backend = draw(&mut prompt, 100, 24);
        let buf = backend.buffer();
        let total = prompt.height(24);
        let top = 24 - total;
        // Empty state still occupies one row inside a 3-row box.
        let text = row_text(buf, top - 2, 1, 30);
        assert!(text.contains("No matching commands"), "{text:?}");
    }

    #[test]
    fn narrow_terminals_use_a_single_column_padding() {
        let mut prompt = prompt();
        let backend = draw(&mut prompt, 40, 24);
        let buf = backend.buffer();
        let total = prompt.height(24);
        let top = 24 - total;
        // pad = 1 → text starts at column 2 (border + 1).
        assert_eq!(buf[(0, top)].symbol(), "▌");
        assert_eq!(buf[(1, top)].bg, pal().panel.bg.expect("panel bg"));
    }

    #[test]
    fn running_replaces_the_location_with_the_interrupt_hint() {
        let mut prompt = prompt();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 100, 24);
                let h = prompt.height(24);
                let prompt_area = Rect {
                    x: 0,
                    y: area.height - h,
                    width: 100,
                    height: h,
                };
                prompt.render(frame, prompt_area, frame.area(), true);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let footer = row_text(buf, 23, 0, 60);
        assert!(footer.contains("esc interrupt"), "{footer:?}");
    }

    #[test]
    fn one_cell_terminal_does_not_panic() {
        let mut prompt = prompt();
        let _ = draw(&mut prompt, 1, 1);
    }
}
