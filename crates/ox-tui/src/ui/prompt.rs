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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use crate::theme;
use crate::ui::autocomplete::{self, Autocomplete, FileRow, Trigger};
use crate::ui::commands::{self, CommandAction, Row};
use crate::ui::composer::Composer;
use crate::ui::history::History;
use crate::ui::spinner;

pub const PLACEHOLDERS: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];

/// Upstream `SplitBorder.vertical`: the panel's left edge is a heavy vertical
/// rule. A half block (`▌`) is twice as wide and does not line up with the
/// centred `╹` foot below it, which reads as a notch.
const PANEL_BORDER_CHAR: char = '┃';
/// How long the first Esc keeps the interrupt armed (upstream: 5s).
const INTERRUPT_WINDOW: Duration = Duration::from_secs(5);

/// Upstream placeholder shape: `Ask anything… "<random phrase>"`.
pub fn placeholder_line(idx: usize) -> String {
    format!(
        "Ask anything… \"{}\"",
        PLACEHOLDERS[idx % PLACEHOLDERS.len()]
    )
}

/// `16.3K · $0.42`: the usage group the session footer shows before the key
/// hint (upstream `footer.tsx` prints the context figure bare, in `text.muted`).
fn usage_spans(ctx: Option<&str>, cost: Option<&str>, pal: &theme::Palette) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(ctx) = ctx {
        spans.push(Span::styled(ctx.to_owned(), pal.dim));
    }
    if let Some(cost) = cost {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", pal.dim));
        }
        spans.push(Span::styled(cost.to_owned(), pal.dim));
    }
    spans
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.width()).sum()
}

/// Upstream `promptFooterLayout`: an 8-column gutter, then a flat reserve of
/// `min(28, half)` for the left side.
fn footer_available(width: u16) -> usize {
    let inner = (width as usize).saturating_sub(8);
    inner.saturating_sub((inner / 2).min(28))
}

/// Pick one of the phrases without a random-number dependency. Upstream
/// seeds `randomIndex(list.length)` (`component/prompt/index.tsx:366`).
/// `OX_TUI_PLACEHOLDER=<n>` pins the index so parity captures are repeatable;
/// tests pass their own index to [`placeholder_line`] instead.
pub fn pick_placeholder() -> usize {
    let pinned = std::env::var("OX_TUI_PLACEHOLDER")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    pinned.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0)
    }) % PLACEHOLDERS.len()
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
    /// Model variant (`high`); `default` is normalised away like upstream.
    pub variant: Option<String>,
    /// `~`-abbreviated working directory.
    pub directory: String,
    /// Current git branch.
    pub branch: Option<String>,
    /// MCP summary for the home footer (`None` hides the segment).
    pub mcp: Option<McpSummary>,
}

/// The MCP summary behind upstream `home_footer`'s `⊙ N MCP` segment:
/// only `connected` entries count, and any failure repaints the dot and
/// switches the label to the failure count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpSummary {
    /// Servers reporting `connected`.
    pub connected: usize,
    /// Servers reporting `failed`.
    pub failed: usize,
}

/// Which route's footer the block paints. Upstream's prompt footer shows the
/// location on the left; the right side carries the route's key hints (the
/// MCP summary is a separate row owned by `ui::home`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Footer {
    /// Home route: location left, `shift+tab agents  ctrl+p commands` right
    /// (`feature-plugins/prompt/footer.tsx:61-120`).
    Home,
    /// Session route: location left, usage and key hints right.
    Session,
}

/// Upstream shows the two shortcut groups from width 44
/// (`feature-plugins/prompt/footer.tsx:64`); each is the key in `text` then its
/// label in `text.muted`, groups separated by two columns.
fn home_hints(pal: &theme::Palette) -> Vec<Span<'static>> {
    vec![
        Span::styled("shift+tab", pal.text),
        Span::styled(" agents", pal.dim),
        Span::raw("  "),
        Span::styled("ctrl+p", pal.text),
        Span::styled(" commands", pal.dim),
    ]
}

/// The session footer's single hint: `ctrl+p` in `text`, label muted.
fn session_hint(pal: &theme::Palette) -> Vec<Span<'static>> {
    vec![
        Span::styled("ctrl+p", pal.text),
        Span::styled(" commands", pal.dim),
    ]
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
    /// Esc with no menu open: the app decides whether it interrupts a turn.
    Esc,
}

pub struct Prompt {
    composer: Composer,
    auto: Autocomplete,
    chrome: Chrome,
    footer: Footer,
    /// Session-route footer token usage (set from the store on refresh).
    ctx_usage: Option<String>,
    /// Session spend (`$0.42`), shown next to the context figure.
    cost: Option<String>,
    /// Submitted prompts, walked with Up/Down.
    history: History,
    /// Latest `@` matches, shown while the next lookup is in flight.
    files: Vec<FileRow>,
    /// Query of the lookup the app has not collected yet.
    file_request: Option<String>,
    /// Query last handed out, so unchanged queries are not re-requested.
    file_requested: Option<String>,
    /// When the footer scanner started (`None` while idle) and its frame.
    spin_started: Option<Instant>,
    spin_frame: usize,
    /// First Esc of the two-press interrupt; the hint reads `again to interrupt`.
    interrupt_armed: Option<Instant>,
}

impl Prompt {
    pub fn new(placeholder: &str, chrome: Chrome, footer: Footer) -> Self {
        Self {
            composer: Composer::new_with_placeholder(placeholder),
            auto: Autocomplete::default(),
            chrome,
            footer,
            ctx_usage: None,
            cost: None,
            history: History::in_memory(),
            files: Vec::new(),
            file_request: None,
            file_requested: None,
            spin_started: None,
            spin_frame: 0,
            interrupt_armed: None,
        }
    }

    /// Swap in the persisted history (the app wires it once at start-up).
    pub fn set_history(&mut self, history: History) {
        self.history = history;
    }

    /// The `@` query the app should look up, once per change. The answer goes
    /// back through [`Prompt::set_files`].
    pub fn take_file_request(&mut self) -> Option<String> {
        self.file_request.take()
    }

    /// Show the matches for `query`; answers for a query the user has since
    /// edited are dropped.
    pub fn set_files(&mut self, query: &str, entries: &[ox_client::rest::FileEntry]) {
        if self.auto.trigger().is_none_or(|t| t == Trigger::Command) || self.auto.query() != query {
            return;
        }
        self.files = autocomplete::order_files(query, entries);
        self.auto.clamp(self.files.len());
    }

    pub fn set_ctx(&mut self, usage: Option<String>) {
        self.ctx_usage = usage;
    }

    #[cfg(test)]
    pub(crate) fn ctx_usage(&self) -> Option<&str> {
        self.ctx_usage.as_deref()
    }

    pub fn set_cost(&mut self, cost: Option<String>) {
        self.cost = cost;
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

    pub fn autocomplete_visible(&self) -> bool {
        self.auto.visible()
    }

    /// Advance the cursor blink; `true` when a repaint is needed.
    pub fn tick(&mut self, now: Instant) -> bool {
        self.composer.tick(now)
    }

    /// Advance the footer scanner; `true` when its frame changed and a repaint
    /// is needed. Idle resets it so the next turn starts from frame 0.
    pub fn tick_spinner(&mut self, now: Instant, running: bool) -> bool {
        let mut changed = false;
        if let Some(armed) = self.interrupt_armed
            && (!running || now.saturating_duration_since(armed) >= INTERRUPT_WINDOW)
        {
            self.interrupt_armed = None;
            changed = true;
        }
        if !running {
            self.spin_frame = 0;
            return self.spin_started.take().is_some() || changed;
        }
        let fresh = self.spin_started.is_none();
        let started = *self.spin_started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started).as_millis();
        let frame = (elapsed / spinner::INTERVAL_MS) as usize % spinner::FRAMES;
        std::mem::replace(&mut self.spin_frame, frame) != frame || fresh || changed
    }

    /// An Esc during a running turn. The first press arms the footer hint; a
    /// second one inside the window confirms. Returns `true` on confirmation.
    pub fn press_interrupt(&mut self, now: Instant) -> bool {
        if self.interrupt_armed.take().is_some() {
            return true;
        }
        self.interrupt_armed = Some(now);
        false
    }

    /// Restart the visible cursor phase (keystroke or focus coming back).
    pub fn touch(&mut self, now: Instant) {
        self.composer.touch(now);
    }

    /// Show a newly selected agent in the metadata row.
    pub fn set_agent(&mut self, agent: String) {
        self.chrome.agent = Some(agent);
    }

    /// Show a newly selected model (id plus provider display name). A new
    /// model starts without a variant.
    pub fn set_model(&mut self, model: String, provider: Option<String>) {
        self.chrome.model = Some(model);
        self.chrome.provider = provider.filter(|name| !name.is_empty());
        self.chrome.variant = None;
    }

    /// Replace agent, model, provider and variant at once (a session's own
    /// selection). Absent parts keep the current value, so a partial answer
    /// never blanks the row.
    pub fn set_selection(
        &mut self,
        agent: Option<String>,
        model: Option<String>,
        provider: Option<String>,
        variant: Option<String>,
    ) {
        if let Some(agent) = agent {
            self.chrome.agent = Some(agent);
        }
        if let Some(model) = model {
            self.chrome.model = Some(model);
            self.chrome.provider = provider.filter(|name| !name.is_empty());
            self.chrome.variant = variant.filter(|name| !name.is_empty());
        }
    }

    /// Switch the footer as the app moves between the home and session routes.
    pub fn set_footer(&mut self, footer: Footer) {
        self.footer = footer;
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
            // Menu navigation never reaches the composer, but it is still the
            // user at the keyboard: keep the cursor in its visible phase.
            self.composer.touch(Instant::now());
            return self.handle_menu_key(key);
        }
        if key.code == KeyCode::Esc {
            return PromptAction::Esc;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.composer.is_empty() {
                return PromptAction::CtrlC;
            }
            self.composer.clear();
            return PromptAction::Ignored;
        }
        if key.modifiers.is_empty() && self.walk_history(key.code) {
            return PromptAction::Ignored;
        }
        let Some(text) = self.composer.handle_key(key) else {
            self.sync_menu();
            return PromptAction::Ignored;
        };
        self.submitted(&text);
        PromptAction::Submit(text)
    }

    /// Record a submitted prompt and close the menu.
    fn submitted(&mut self, text: &str) {
        self.history.append(text);
        self.auto.hide();
        self.sync_files();
    }

    /// Upstream `prompt.history.previous/next`: Up/Down first travel to the
    /// start/end of the text, and only a press there walks the history.
    /// Returns `true` when the key was consumed.
    fn walk_history(&mut self, code: KeyCode) -> bool {
        let (direction, text_edge, boundary_row) = match code {
            KeyCode::Up => (
                -1,
                self.composer.cursor_at_start(),
                self.composer.on_first_row(),
            ),
            KeyCode::Down => (
                1,
                self.composer.cursor_at_end(),
                self.composer.on_last_row(),
            ),
            _ => return false,
        };
        if !text_edge {
            if boundary_row {
                if direction < 0 {
                    self.composer.move_to_start();
                } else {
                    self.composer.move_to_end();
                }
                return true;
            }
            return false;
        }
        if let Some(text) = self.history.step(direction, &self.composer.text()) {
            self.composer.set_text(&text, direction > 0);
            self.auto.hide();
            self.sync_files();
        }
        true
    }

    fn handle_menu_key(&mut self, key: KeyEvent) -> PromptAction {
        let count = self.menu_len();
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
                self.select()
            }
            (KeyCode::Tab, _) => self.select(),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Upstream `hide(true)`: drop the typed `/token`, keep the rest.
                // An `@path` stays in the text; only the menu goes away.
                if self.auto.trigger() == Some(Trigger::Command) {
                    let (_, column) = self.composer.cursor();
                    self.composer.delete_leading(column);
                }
                self.auto.hide();
                self.sync_files();
                PromptAction::Ignored
            }
            _ => {
                if let Some(text) = self.composer.handle_key(key) {
                    self.submitted(&text);
                    return PromptAction::Submit(text);
                }
                self.sync_menu();
                PromptAction::Ignored
            }
        }
    }

    fn select(&mut self) -> PromptAction {
        match self.auto.trigger() {
            Some(Trigger::Mention { col }) => {
                // Attachments are blocked server-side, so a pick is plain text.
                // With nothing to pick, Enter just closes the menu.
                if let Some(row) = self.files.get(self.auto.selected()) {
                    let inserted = format!("@{}", row.path);
                    self.composer.replace_back_to(col, &inserted);
                }
                self.auto.hide();
                self.sync_files();
                PromptAction::Ignored
            }
            _ => {
                let rows = self.rows();
                let Some(row) = rows.get(self.auto.selected()) else {
                    return PromptAction::Ignored;
                };
                let action = row.action;
                let (_, column) = self.composer.cursor();
                self.composer.delete_leading(column);
                self.auto.hide();
                PromptAction::Run(action)
            }
        }
    }

    /// Re-derive the menu from the editor; cheap enough to run after every key.
    fn sync_menu(&mut self) {
        let (row, column) = self.composer.cursor();
        self.auto.sync(&self.composer.text(), (row, column));
        self.sync_files();
        let count = self.menu_len();
        self.auto.clamp(count);
    }

    /// Queue a lookup when an `@` query appeared or changed; forget the
    /// results once no `@` menu is open.
    fn sync_files(&mut self) {
        let wanted = match self.auto.trigger() {
            Some(Trigger::Mention { .. }) => Some(self.auto.query().to_owned()),
            _ => None,
        };
        if wanted.is_none() {
            self.files.clear();
        }
        if wanted != self.file_requested {
            self.file_request = wanted.clone();
            self.file_requested = wanted;
        }
    }

    fn menu_len(&self) -> usize {
        match self.auto.trigger() {
            Some(Trigger::Mention { .. }) => self.files.len(),
            _ => self.rows().len(),
        }
    }

    fn rows(&self) -> Vec<Row> {
        commands::rows(self.auto.query())
    }

    /// `(label, description)` for each menu row, whichever menu is open.
    fn menu_items(&self, width: usize) -> Vec<(String, String)> {
        match self.auto.trigger() {
            Some(Trigger::Mention { .. }) => self
                .files
                .iter()
                .map(|row| {
                    (
                        autocomplete::truncate_middle(&row.path, width),
                        String::new(),
                    )
                })
                .collect(),
            _ => self
                .rows()
                .into_iter()
                .map(|row| (row.display, row.description.to_owned()))
                .collect(),
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, full: Rect, running: bool) {
        let pal = *theme::palette();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rail = self.rail_style(&pal);
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

        // Panel background and left border. Upstream draws the border box
        // *outside* the panel surface, so the panel fill starts one column in
        // and the border cell keeps the terminal background.
        {
            let buf = frame.buffer_mut();
            for y in panel.y..panel.bottom() {
                for x in panel.x + 1..panel.right() {
                    buf[(x, y)].set_style(pal.panel);
                }
                let cell = &mut buf[(panel.x, y)];
                cell.set_style(rail);
                cell.set_char(PANEL_BORDER_CHAR);
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
            // 上游宽度预算：终端宽度减 13（窄终端减 9）。
            let budget = full
                .width
                .saturating_sub(if full.width < 44 { 9 } else { 13 });
            let line = metadata_line(
                &Meta::from(&self.chrome),
                budget.min(inner_w),
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
            if let Some(fg) = rail.fg {
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

        // Footer: location (or the running state) left; the key hints sit flush
        // with the block's right edge.
        let footer_y = edge_y + 1;
        if footer_y < area.bottom() {
            let left: Vec<Span<'static>> = if running {
                // 上游：marginLeft 1 + 扫描条 + 间隔 1 + `esc ` / `interrupt`。
                let mut spans = vec![Span::raw(" ")];
                spans.extend(spinner::spans(self.spin_frame, &pal));
                spans.push(Span::raw(" "));
                if self.interrupt_armed.is_some() {
                    spans.push(Span::styled("esc ", pal.warning));
                    spans.push(Span::styled("again to interrupt", pal.warning));
                } else {
                    spans.push(Span::styled("esc ", pal.text));
                    spans.push(Span::styled("interrupt", pal.dim));
                }
                spans
            } else {
                vec![Span::styled(self.location(), pal.dim)]
            };
            let right: Option<Vec<Span<'static>>> = if running {
                None
            } else {
                match self.footer {
                    Footer::Session => {
                        // Upstream `promptFooterLayout`: usage outranks the
                        // hint, which needs two columns of separation.
                        let available = footer_available(area.width);
                        let usage =
                            usage_spans(self.ctx_usage.as_deref(), self.cost.as_deref(), &pal);
                        let hint = session_hint(&pal);
                        let usage_w = spans_width(&usage);
                        let hint_w = spans_width(&hint);
                        let mut spans: Vec<Span<'static>> = Vec::new();
                        if usage_w > 0 && usage_w <= available {
                            spans.extend(usage);
                            if full.width >= 44 && usage_w + 2 + hint_w <= available {
                                spans.push(Span::raw("  "));
                                spans.extend(hint);
                            }
                        } else if usage_w == 0 && full.width >= 44 && hint_w <= available {
                            spans.extend(hint);
                        }
                        (!spans.is_empty()).then_some(spans)
                    }
                    Footer::Home => (full.width >= 44).then(|| home_hints(&pal)),
                }
            };
            let left_w = left
                .iter()
                .map(|span| span.width())
                .sum::<usize>()
                .min(area.width as usize) as u16;
            frame.render_widget(
                Paragraph::new(Line::from(left)),
                Rect {
                    x: area.x,
                    y: footer_y,
                    width: left_w,
                    height: 1,
                },
            );
            if let Some(spans) = right {
                let right_w = spans
                    .iter()
                    .map(|span| span.width())
                    .sum::<usize>()
                    .min(area.width as usize) as u16;
                let x = area.right().saturating_sub(right_w);
                if x >= area.x + left_w {
                    frame.render_widget(
                        Paragraph::new(Line::from(spans)),
                        Rect {
                            x,
                            y: footer_y,
                            width: right_w,
                            height: 1,
                        },
                    );
                }
            }
        }
    }

    /// The rail and its `╹` foot. Upstream tints the border from the agent
    /// colour (`component/prompt/index.tsx:1610`); on the home route the
    /// prompt is the only surface, so it shows that colour undiluted. The
    /// session route keeps the quieter `panel_border`.
    fn rail_style(&self, pal: &theme::Palette) -> Style {
        match (self.footer, pal.agent.fg) {
            (Footer::Home, Some(fg)) => Style::default().fg(fg),
            _ => pal.panel_border,
        }
    }

    /// The decoration the prompt was built with (the home footer reads the MCP
    /// summary from it).
    pub fn chrome(&self) -> &Chrome {
        &self.chrome
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
        let mention = matches!(self.auto.trigger(), Some(Trigger::Mention { .. }));
        let rows = self.menu_items((area.width as usize).saturating_sub(4));
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
            let empty = if mention {
                "No matching files"
            } else {
                "No matching commands"
            };
            frame.render_widget(
                Paragraph::new(Line::styled(empty, pal.dim)).style(pal.menu),
                inner,
            );
            return;
        }
        let visible = inner.height as usize;
        let offset = self.auto.selected().saturating_sub(visible - 1);
        {
            let buf = frame.buffer_mut();
            for (index, (display, description)) in
                rows.iter().skip(offset).take(visible).enumerate()
            {
                let y = inner.y + index as u16;
                let row_rect = Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                };
                let selected = offset + index == self.auto.selected();
                let description = if description.is_empty() {
                    String::new()
                } else {
                    format!(" {description}")
                };
                let line = if selected {
                    Line::from(vec![Span::raw(display.clone()), Span::raw(description)])
                } else {
                    Line::from(vec![
                        Span::styled(display.clone(), pal.text),
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
    meta: &Meta<'_>,
    width: u16,
    terminal_width: u16,
    pal: &theme::Palette,
) -> Line<'static> {
    let narrow = terminal_width < 44;
    let agent = if narrow {
        None
    } else {
        meta.agent.filter(|value| !value.is_empty())
    };
    let provider = if narrow {
        None
    } else {
        meta.provider.filter(|value| !value.is_empty())
    };
    let model = meta.model.filter(|value| !value.is_empty());
    let variant = meta.variant.filter(|value| !value.is_empty());
    // 上游：没有 agent 标签时整行留空。
    if meta.agent.is_none_or(str::is_empty) || (agent.is_none() && model.is_none()) {
        return Line::default();
    }
    let width = width as usize;
    let provider_last = provider.map(|value| value.rsplit(" / ").next().unwrap_or(value));

    let candidates = [(provider, model), (provider_last, model), (None, model)];
    for (candidate_provider, candidate_model) in candidates {
        let line = metadata_spans(agent, candidate_model, candidate_provider, variant, pal);
        if line.width() <= width {
            return line;
        }
    }
    // 都放不下：保留 `agent · ` 前缀与 ` · variant` 后缀，只截断 model（至少 9 列）。
    let prefix_w = agent.map_or(0, |agent| agent.width() + 3);
    let suffix_w = variant.map_or(0, |variant| variant.width() + 3);
    let room = width.saturating_sub(prefix_w + suffix_w).max(9);
    let model = model.map(|model| truncate_end(model, room));
    metadata_spans(agent, model.as_deref(), None, variant, pal)
}

/// The values behind the metadata row.
pub struct Meta<'a> {
    pub agent: Option<&'a str>,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub variant: Option<&'a str>,
}

impl<'a> From<&'a Chrome> for Meta<'a> {
    fn from(chrome: &'a Chrome) -> Self {
        Self {
            agent: chrome.agent.as_deref(),
            model: chrome.model.as_deref(),
            provider: chrome.provider.as_deref(),
            variant: chrome.variant.as_deref(),
        }
    }
}

fn metadata_spans(
    agent: Option<&str>,
    model: Option<&str>,
    provider: Option<&str>,
    variant: Option<&str>,
    pal: &theme::Palette,
) -> Line<'static> {
    let mut parts: Vec<Span<'static>> = Vec::new();
    if let Some(agent) = agent {
        parts.push(Span::styled(agent.to_owned(), pal.agent));
    }
    if let Some(model) = model {
        if agent.is_some() {
            parts.push(Span::styled("·", pal.dim));
        }
        parts.push(Span::styled(model.to_owned(), pal.text));
    }
    if let Some(provider) = provider {
        parts.push(Span::styled(provider.to_owned(), pal.dim));
    }
    if let Some(variant) = variant {
        parts.push(Span::styled("·", pal.dim));
        parts.push(Span::styled(
            variant.to_owned(),
            pal.warning.add_modifier(Modifier::BOLD),
        ));
    }
    let mut spans = Vec::with_capacity(parts.len() * 2);
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(part);
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
    use super::{
        Chrome, Footer, McpSummary, Meta, Prompt, PromptAction, metadata_line, placeholder_line,
        row_text,
    };
    use crate::theme::{Deck, Palette};
    use crate::ui::composer::CURSOR_BLINK;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use std::time::Instant;

    fn pal() -> Palette {
        Palette::for_deck(Deck::Truecolor)
    }

    fn chrome() -> Chrome {
        Chrome {
            agent: Some("Build".into()),
            model: Some("k3-256k".into()),
            provider: Some("Cardinal".into()),
            variant: None,
            directory: "~/Developer/openkitbox/ox".into(),
            branch: Some("main".into()),
            mcp: None,
        }
    }

    fn prompt() -> Prompt {
        Prompt::new(&placeholder_line(0), chrome(), Footer::Session)
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
    fn placeholder_is_cut_at_the_text_width() {
        // A 30-column area: rail + pad(1) leaves 27 text columns.
        let mut prompt = Prompt::new(
            "Ask anything… \"Fix a TODO in the codebase\"",
            chrome(),
            Footer::Home,
        );
        let backend = draw(&mut prompt, 30, 24);
        let top = 24 - prompt.height(24);
        let row = row_text(backend.buffer(), top + 1, 0, 30);
        assert_eq!(row.trim_end(), "┃ Ask anything… \"Fix a TODO i");
    }

    fn meta<'a>(
        agent: Option<&'a str>,
        model: Option<&'a str>,
        provider: Option<&'a str>,
        variant: Option<&'a str>,
    ) -> Meta<'a> {
        Meta {
            agent,
            model,
            provider,
            variant,
        }
    }

    fn build_meta() -> Meta<'static> {
        meta(Some("Build"), Some("k3-256k"), Some("Cardinal"), None)
    }

    #[test]
    fn metadata_joins_agent_model_and_provider() {
        let line = metadata_line(&build_meta(), 70, 100, &pal());
        assert_eq!(text(&line), "Build · k3-256k Cardinal");
    }

    #[test]
    fn metadata_appends_a_bold_warning_variant() {
        let m = meta(
            Some("Build"),
            Some("k3-256k"),
            Some("Cardinal"),
            Some("high"),
        );
        let line = metadata_line(&m, 70, 100, &pal());
        assert_eq!(text(&line), "Build · k3-256k Cardinal · high");
        let last = line.spans.last().expect("variant span");
        assert_eq!(last.content, "high");
        assert_eq!(last.style.fg, pal().warning.fg);
        assert!(last.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn metadata_drops_the_provider_but_keeps_the_agent_as_width_shrinks() {
        // Upstream drops provider first and never drops the agent on a wide
        // terminal.
        let line = metadata_line(&build_meta(), 18, 100, &pal());
        assert_eq!(text(&line), "Build · k3-256k");
        let line = metadata_line(&build_meta(), 12, 100, &pal());
        assert!(text(&line).starts_with("Build · "), "{:?}", text(&line));
    }

    #[test]
    fn metadata_truncates_the_model_when_nothing_fits() {
        let m = meta(
            Some("Build"),
            Some("k3-256k-very-long-model"),
            None,
            Some("high"),
        );
        let line = metadata_line(&m, 24, 100, &pal());
        let shown = text(&line);
        assert!(shown.starts_with("Build · k3"), "{shown:?}");
        assert!(shown.contains('…'), "{shown:?}");
        assert!(shown.ends_with(" · high"), "{shown:?}");
    }

    #[test]
    fn metadata_hides_agent_and_provider_on_narrow_terminals() {
        let line = metadata_line(&build_meta(), 70, 43, &pal());
        assert_eq!(text(&line), "k3-256k");
    }

    #[test]
    fn metadata_is_empty_without_an_agent() {
        let m = meta(None, Some("k3-256k"), Some("Cardinal"), None);
        assert!(text(&metadata_line(&m, 70, 100, &pal())).is_empty());
        assert!(
            text(&metadata_line(
                &meta(None, None, None, None),
                70,
                100,
                &pal()
            ))
            .is_empty()
        );
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
    fn the_metadata_line_follows_a_switched_model_and_agent() {
        let mut prompt = prompt();
        prompt.set_agent("Plan".into());
        prompt.set_model("k4".into(), Some("Acme".into()));
        let line = metadata_line(&Meta::from(prompt.chrome()), 60, 100, &pal());
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "Plan · k4 Acme");

        // A provider-less model drops the stale provider instead of keeping it.
        prompt.set_model("k5".into(), None);
        assert_eq!(prompt.chrome().provider, None);
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
        // Up from row 0 wraps to the last row: /timeline.
        assert_eq!(
            key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE),
            PromptAction::Run(crate::ui::commands::CommandAction::Timeline)
        );
        assert_eq!(rows.last().map(|row| row.name), Some("timeline"));
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

        // Left border on every panel row: upstream's `SplitBorder.vertical`
        // (`┃`), on the terminal background — the panel surface starts one
        // column in, exactly like upstream's border box.
        for y in top..top + panel_h {
            assert_eq!(buf[(0, y)].symbol(), "┃", "row {y}");
            assert_eq!(buf[(0, y)].fg, pal().panel_border.fg.expect("border fg"));
            assert_eq!(
                buf[(0, y)].bg,
                Color::Reset,
                "row {y}: the border column must not carry the panel background"
            );
        }
        // Panel background reaches the inner padding columns.
        assert_eq!(buf[(2, top)].bg, pal().panel.bg.expect("panel bg"));
        // Upstream renders the metadata row on every route.
        let meta = row_text(buf, top + panel_h - 1, 0, 40);
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
        assert!(footer.contains("ctrl+p commands"), "{footer:?}");
    }

    /// The footer is always the block's last row.
    fn footer_of(prompt: &mut Prompt) -> String {
        let backend = draw(prompt, 100, 24);
        row_text(backend.buffer(), 23, 0, 100)
    }

    fn footer_of_width(prompt: &mut Prompt, width: u16) -> String {
        let backend = draw(prompt, width, 24);
        row_text(backend.buffer(), 23, 0, width)
    }

    #[test]
    fn session_footer_keeps_usage_and_hints_when_they_fit() {
        let mut prompt = Prompt::new(&placeholder_line(0), chrome(), Footer::Session);
        prompt.set_ctx(Some("16.3k".into()));
        prompt.set_cost(Some("$0.42".into()));
        let footer = footer_of_width(&mut prompt, 100);
        assert!(footer.contains("16.3k"), "{footer:?}");
        assert!(footer.contains("$0.42"), "{footer:?}");
        assert!(footer.contains("ctrl+p commands"), "{footer:?}");
    }

    #[test]
    fn session_footer_drops_the_hints_before_the_usage() {
        let mut prompt = Prompt::new(&placeholder_line(0), chrome(), Footer::Session);
        prompt.set_ctx(Some("16.3k".into()));
        prompt.set_cost(Some("$0.42".into()));
        let footer = footer_of_width(&mut prompt, 60);
        assert!(footer.contains("16.3k"), "{footer:?}");
        assert!(footer.contains("$0.42"), "{footer:?}");
        assert!(!footer.contains("ctrl+p"), "{footer:?}");
    }

    #[test]
    fn session_footer_shows_the_hints_alone_without_usage() {
        let mut prompt = Prompt::new(&placeholder_line(0), chrome(), Footer::Session);
        let footer = footer_of_width(&mut prompt, 60);
        assert!(footer.contains("ctrl+p commands"), "{footer:?}");
    }

    #[test]
    fn home_footer_shows_the_shortcut_hints_from_width_44() {
        let mut prompt = Prompt::new(&placeholder_line(0), chrome(), Footer::Home);
        let footer = footer_of_width(&mut prompt, 100);
        assert!(
            footer.ends_with("shift+tab agents  ctrl+p commands"),
            "{footer:?}"
        );
        let footer = footer_of_width(&mut prompt, 43);
        assert!(!footer.contains("shift+tab"), "{footer:?}");
    }

    #[test]
    fn home_footer_no_longer_carries_the_mcp_summary() {
        let mut chrome = chrome();
        chrome.mcp = Some(McpSummary {
            connected: 6,
            failed: 0,
        });
        let mut prompt = Prompt::new(&placeholder_line(0), chrome, Footer::Home);
        assert!(!footer_of(&mut prompt).contains("MCP"));
    }

    #[test]
    fn the_home_rail_takes_the_agent_colour_and_the_session_rail_does_not() {
        let rail_fg = |footer| {
            let mut prompt = Prompt::new(&placeholder_line(0), chrome(), footer);
            let backend = draw(&mut prompt, 100, 24);
            let top = 24 - prompt.height(24);
            let cell = &backend.buffer()[(0, top)];
            (cell.fg, cell.bg)
        };
        let (home, home_bg) = rail_fg(Footer::Home);
        let (session, _) = rail_fg(Footer::Session);
        assert_eq!(home, pal().agent.fg.expect("agent fg"));
        assert_eq!(home_bg, Color::Reset, "the rail sits on the terminal bg");
        assert_eq!(session, pal().panel_border.fg.expect("border fg"));
    }

    #[test]
    fn session_footer_puts_ctx_before_the_key_hints() {
        let mut chrome = chrome();
        chrome.mcp = Some(McpSummary {
            connected: 6,
            failed: 0,
        });
        let mut prompt = Prompt::new(&placeholder_line(0), chrome, Footer::Session);
        let footer = footer_of(&mut prompt);
        assert!(footer.contains("ctrl+p commands"), "{footer:?}");
        assert!(!footer.contains("MCP"), "{footer:?}");

        prompt.set_ctx(Some("16.3k".into()));
        let footer = footer_of(&mut prompt);
        let ctx_at = footer.find("16.3k").expect("ctx segment: {footer:?}");
        let hints_at = footer.find("ctrl+p commands").expect("hints: {footer:?}");
        assert!(ctx_at < hints_at, "ctx before ctrl+p: {footer:?}");
    }

    #[test]
    fn session_footer_shows_cost_between_ctx_and_the_hints() {
        let mut prompt = Prompt::new(&placeholder_line(0), chrome(), Footer::Session);
        prompt.set_ctx(Some("16.3k".into()));
        prompt.set_cost(Some("$0.42".into()));
        let footer = footer_of(&mut prompt);
        let ctx_at = footer.find("16.3k").expect("ctx segment: {footer:?}");
        let cost_at = footer.find("$0.42").expect("cost segment: {footer:?}");
        let hints_at = footer.find("ctrl+p commands").expect("hints: {footer:?}");
        assert!(ctx_at < cost_at, "ctx before cost: {footer:?}");
        assert!(cost_at < hints_at, "cost before hints: {footer:?}");
    }

    #[test]
    fn footer_hides_the_mcp_segment_without_configured_servers() {
        let mut prompt = Prompt::new(&placeholder_line(0), chrome(), Footer::Home);
        let footer = footer_of(&mut prompt);
        assert!(
            footer.contains("~/Developer/openkitbox/ox:main"),
            "{footer:?}"
        );
        assert!(!footer.contains("MCP"), "{footer:?}");
    }

    #[test]
    fn menu_renders_above_the_panel_with_a_selected_row() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/");
        let backend = draw(&mut prompt, 100, 24);
        let buf = backend.buffer();
        let total = prompt.height(24);
        let top = 24 - total;
        // Menu is a box: up to 10 rows plus 2 borders.
        // Menu is a box: 5 rows (border + 3 commands) + 2 borders = 7.
        let rows = crate::ui::commands::rows("");
        let menu_h = rows.len().clamp(1, 10) as u16 + 2;
        let menu_top = top - menu_h;
        assert_eq!(buf[(0, menu_top)].symbol(), "┌");
        assert_eq!(buf[(0, menu_top + 1)].bg, pal().menu.bg.expect("menu bg"));
        // First row (agents) is selected and inverted.
        let selected = row_text(buf, menu_top + 1, 1, 12);
        assert!(selected.contains("/agents"), "{selected:?}");
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
        assert_eq!(buf[(0, top)].symbol(), "┃");
        assert_eq!(buf[(1, top)].bg, pal().panel.bg.expect("panel bg"));
    }

    #[test]
    fn the_cursor_blinks_and_a_keystroke_wakes_it() {
        let mut prompt = prompt();
        type_text(&mut prompt, "hi");
        let start = Instant::now();
        prompt.touch(start);

        // Force the off phase: the drawn frame has no block cursor.
        assert!(prompt.tick(start + CURSOR_BLINK));
        let off = draw(&mut prompt, 100, 24);
        let total = prompt.height(24);
        let text_row = 24 - total + 1;
        assert!(
            !(0..100).any(|x| off.buffer()[(x, text_row)]
                .modifier
                .contains(Modifier::REVERSED)),
            "off phase must not paint a cursor"
        );

        // Any key restores the visible phase.
        key(&mut prompt, KeyCode::Char('x'), KeyModifiers::NONE);
        let on = draw(&mut prompt, 100, 24);
        assert!(
            (0..100).any(|x| on.buffer()[(x, text_row)]
                .modifier
                .contains(Modifier::REVERSED)),
            "on phase must paint the block cursor"
        );
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
    fn esc_interrupt_arms_then_confirms_and_expires() {
        use std::time::Duration;
        let mut prompt = prompt();
        let t0 = Instant::now();
        assert!(!prompt.press_interrupt(t0), "first press only arms");
        assert!(
            prompt.tick_spinner(t0 + Duration::from_secs(5), true),
            "window closes"
        );
        assert!(
            !prompt.press_interrupt(t0 + Duration::from_secs(6)),
            "re-armed, not confirmed"
        );
        assert!(prompt.press_interrupt(t0 + Duration::from_secs(7)));
    }

    #[test]
    fn armed_footer_says_again_to_interrupt() {
        let mut prompt = prompt();
        prompt.press_interrupt(Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| {
                let h = prompt.height(24);
                let area = Rect {
                    x: 0,
                    y: 24 - h,
                    width: 100,
                    height: h,
                };
                prompt.render(frame, area, frame.area(), true);
            })
            .unwrap();
        let footer = row_text(terminal.backend().buffer(), 23, 0, 60);
        assert!(footer.contains("esc again to interrupt"), "{footer:?}");
    }

    #[test]
    fn spinner_advances_with_the_clock_while_running_and_resets_when_idle() {
        use std::time::Duration;
        let mut prompt = prompt();
        let t0 = Instant::now();
        assert!(prompt.tick_spinner(t0, true), "first running tick repaints");
        assert!(!prompt.tick_spinner(t0, true), "same frame: no repaint");
        assert!(prompt.tick_spinner(t0 + Duration::from_millis(40), true));
        assert_eq!(prompt.spin_frame, 1);
        assert!(prompt.tick_spinner(t0 + Duration::from_millis(80), false));
        assert_eq!(prompt.spin_frame, 0);
        assert!(!prompt.tick_spinner(t0 + Duration::from_millis(120), false));
    }

    #[test]
    fn running_footer_shows_the_eight_cell_scanner() {
        let mut prompt = prompt();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let h = prompt.height(24);
                let area = Rect::new(0, 24 - h, 100, h);
                prompt.render(frame, area, frame.area(), true);
            })
            .unwrap();
        let footer = row_text(terminal.backend().buffer(), 23, 0, 60);
        let cells: usize = footer.chars().filter(|c| matches!(c, '■' | '⬝')).count();
        assert_eq!(cells, 8, "{footer:?}");
    }

    #[test]
    fn one_cell_terminal_does_not_panic() {
        let mut prompt = prompt();
        let _ = draw(&mut prompt, 1, 1);
    }

    #[test]
    fn session_footer_renders_in_the_app_shaped_six_row_area() {
        let mut chrome = chrome();
        chrome.mcp = Some(McpSummary {
            connected: 6,
            failed: 0,
        });
        let mut prompt = Prompt::new(&placeholder_line(0), chrome, Footer::Session);
        prompt.set_ctx(Some("16.3k".into()));
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect {
                    x: 2,
                    y: 14,
                    width: 96,
                    height: 6,
                };
                prompt.render(frame, area, frame.area(), false);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let footer: String = (0..100).map(|x| buffer[(x, 19)].symbol()).collect();
        assert!(footer.contains("ctrl+p commands"), "{footer:?}");
        let ctx_at = footer.find("16.3k").expect("ctx: {footer:?}");
        let hints_at = footer.find("ctrl+p commands").expect("hints: {footer:?}");
        assert!(ctx_at < hints_at, "{footer:?}");
    }

    fn submit(prompt: &mut Prompt, text: &str) {
        type_text(prompt, text);
        assert_eq!(
            key(prompt, KeyCode::Enter, KeyModifiers::NONE),
            PromptAction::Submit(text.into())
        );
    }

    fn up(prompt: &mut Prompt) {
        key(prompt, KeyCode::Up, KeyModifiers::NONE);
    }

    fn down(prompt: &mut Prompt) {
        key(prompt, KeyCode::Down, KeyModifiers::NONE);
    }

    #[test]
    fn up_and_down_walk_submitted_prompts_from_an_empty_input() {
        let mut prompt = prompt();
        submit(&mut prompt, "first");
        submit(&mut prompt, "second");
        up(&mut prompt);
        assert_eq!(prompt.text(), "second");
        up(&mut prompt);
        assert_eq!(prompt.text(), "first");
        up(&mut prompt);
        assert_eq!(prompt.text(), "first", "the oldest entry is the end");
        down(&mut prompt); // the cursor sits at the start: Down first moves it to the end
        assert_eq!(prompt.text(), "first");
        down(&mut prompt);
        assert_eq!(prompt.text(), "second");
        down(&mut prompt);
        assert_eq!(
            prompt.text(),
            "",
            "past the newest entry the draft is empty"
        );
    }

    #[test]
    fn up_inside_a_draft_only_moves_the_cursor() {
        let mut prompt = prompt();
        submit(&mut prompt, "old");
        type_text(&mut prompt, "draft");
        up(&mut prompt);
        assert_eq!(prompt.text(), "draft", "typed text is never replaced");
        up(&mut prompt);
        assert_eq!(prompt.text(), "draft");
    }

    #[test]
    fn a_multiline_entry_is_walked_edge_to_edge() {
        let mut prompt = prompt();
        submit(&mut prompt, "x");
        key(&mut prompt, KeyCode::Char('a'), KeyModifiers::NONE);
        key(&mut prompt, KeyCode::Enter, KeyModifiers::SHIFT);
        key(&mut prompt, KeyCode::Char('b'), KeyModifiers::NONE);
        // Cursor is at the end of row 1: Up goes to the start of row 0 first.
        up(&mut prompt);
        assert_eq!(prompt.text(), "a\nb");
        up(&mut prompt);
        assert_eq!(
            prompt.text(),
            "a\nb",
            "second Up lands on the start, still no walk"
        );
    }

    #[test]
    fn walking_back_puts_the_cursor_at_the_start_and_forward_at_the_end() {
        let mut prompt = prompt();
        submit(&mut prompt, "one");
        submit(&mut prompt, "two");
        up(&mut prompt);
        key(&mut prompt, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(prompt.text(), "!two", "the cursor starts at the front");
        key(&mut prompt, KeyCode::Backspace, KeyModifiers::NONE);
        up(&mut prompt);
        down(&mut prompt); // cursor to the end of "one"
        down(&mut prompt); // then forward to "two"
        key(&mut prompt, KeyCode::Char('?'), KeyModifiers::NONE);
        assert_eq!(prompt.text(), "two?", "walking forward ends at the back");
    }

    #[test]
    fn history_survives_a_restart_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut first = prompt();
        first.set_history(crate::ui::history::History::load(path.clone()));
        submit(&mut first, "remember me");

        let mut second = prompt();
        second.set_history(crate::ui::history::History::load(path));
        up(&mut second);
        assert_eq!(second.text(), "remember me");
    }

    #[test]
    fn slash_commands_are_not_recorded_in_the_history() {
        let mut prompt = prompt();
        type_text(&mut prompt, "/new");
        key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE);
        up(&mut prompt);
        assert_eq!(prompt.text(), "");
    }

    fn entries(items: &[(&str, &str)]) -> Vec<ox_client::rest::FileEntry> {
        items
            .iter()
            .map(|(path, kind)| ox_client::rest::FileEntry {
                path: (*path).into(),
                kind: (*kind).into(),
            })
            .collect()
    }

    #[test]
    fn typing_an_at_requests_files_once_per_query_change() {
        let mut prompt = prompt();
        type_text(&mut prompt, "see @");
        assert_eq!(prompt.take_file_request().as_deref(), Some(""));
        assert_eq!(prompt.take_file_request(), None, "taken once");
        type_text(&mut prompt, "sr");
        assert_eq!(
            prompt.take_file_request().as_deref(),
            Some("sr"),
            "requests coalesce to the latest query"
        );
        assert_eq!(prompt.take_file_request(), None);
        assert!(prompt.autocomplete_visible());
        type_text(&mut prompt, " ");
        assert!(!prompt.autocomplete_visible());
    }

    #[test]
    fn a_stale_answer_is_ignored() {
        let mut prompt = prompt();
        type_text(&mut prompt, "@sr");
        prompt.set_files("s", &entries(&[("stale", "file")]));
        let backend = draw(&mut prompt, 100, 24);
        let top = 24 - prompt.height(24);
        let shown = row_text(backend.buffer(), top - 2, 1, 30);
        assert!(shown.contains("No matching files"), "{shown:?}");
    }

    #[test]
    fn the_file_menu_lists_directories_first_and_enter_inserts_the_path() {
        let mut prompt = prompt();
        type_text(&mut prompt, "open @ma");
        prompt.set_files(
            "ma",
            &entries(&[("src/main.rs", "file"), ("main", "directory")]),
        );
        let backend = draw(&mut prompt, 100, 24);
        let buf = backend.buffer();
        let top = 24 - prompt.height(24);
        // Two rows in a four-row box: the first row sits right under the border.
        let first = row_text(buf, top - 3, 1, 30);
        assert!(first.starts_with("main"), "{first:?}");
        assert_eq!(
            buf[(1, top - 3)].bg,
            pal().menu_selected.bg.expect("selected bg")
        );
        let second = row_text(buf, top - 2, 1, 30);
        assert!(second.starts_with("src/main.rs"), "{second:?}");

        key(&mut prompt, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE),
            PromptAction::Ignored,
            "picking a path never submits"
        );
        assert_eq!(prompt.text(), "open @src/main.rs ");
        assert!(!prompt.autocomplete_visible());
    }

    #[test]
    fn escape_and_ctrl_c_close_the_file_menu_without_touching_the_text() {
        let mut escaping = prompt();
        type_text(&mut escaping, "@ab");
        key(&mut escaping, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!escaping.autocomplete_visible());
        assert_eq!(escaping.text(), "@ab");

        let mut cancelling = prompt();
        type_text(&mut cancelling, "@ab");
        key(&mut cancelling, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!cancelling.autocomplete_visible());
        assert_eq!(cancelling.text(), "@ab");
    }

    #[test]
    fn enter_with_no_match_closes_the_file_menu_and_keeps_the_text() {
        let mut prompt = prompt();
        type_text(&mut prompt, "@zzz");
        prompt.set_files("zzz", &[]);
        assert_eq!(
            key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE),
            PromptAction::Ignored
        );
        assert_eq!(prompt.text(), "@zzz");
        assert!(!prompt.autocomplete_visible());
    }

    #[test]
    fn an_email_address_does_not_open_the_file_menu() {
        let mut prompt = prompt();
        type_text(&mut prompt, "me@example");
        assert!(!prompt.autocomplete_visible());
        assert_eq!(prompt.take_file_request(), None);
    }

    #[test]
    fn long_paths_are_cut_in_the_middle_to_fit_the_menu() {
        let mut prompt = prompt();
        type_text(&mut prompt, "@a");
        let long = format!("{}/leaf.rs", "deep".repeat(30));
        prompt.set_files("a", &entries(&[(&long, "file")]));
        let backend = draw(&mut prompt, 40, 24);
        let top = 24 - prompt.height(24);
        let shown = row_text(backend.buffer(), top - 2, 1, 38);
        assert!(
            shown.contains('…') && shown.trim_end().ends_with("leaf.rs"),
            "{shown:?}"
        );
    }
}
