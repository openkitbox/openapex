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
use ratatui::widgets::{Clear, Paragraph, Widget};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use crate::attachment::Loaded;
use crate::theme;
use crate::ui::autocomplete::{self, Autocomplete, FileRow, Trigger};
use crate::ui::commands::{self, CommandAction, Row};
use crate::ui::composer::{Composer, Submission};
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
#[must_use]
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
#[must_use]
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
#[derive(Debug, Clone, PartialEq)]
pub enum PromptAction {
    /// The widget handled it (typed text, menu navigation, cleared draft).
    Ignored,
    /// The user submitted a non-empty prompt.
    Submit(Submission),
    /// A slash command was chosen; the token was removed from the draft.
    Run(CommandAction),
    /// ctrl+c with an empty draft: the app decides interrupt vs quit.
    CtrlC,
    /// Esc with no menu open: the app decides whether it interrupts a turn.
    Esc,
}

/// What the footer reports as going on: the focused session's own turn.
/// Upstream keys the scanner on that session's status alone
/// (`component/prompt/index.tsx:1913`), so background subagents leave the
/// footer still while the parent waits for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Turn,
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
    /// A modal owns the terminal cursor: the panel still draws, but leaves the
    /// cursor to the modal (or hidden, for one without a text field).
    cursor_yielded: bool,
}

impl Prompt {
    #[must_use]
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
            cursor_yielded: false,
        }
    }

    /// Set before each draw: `true` while a dialog is open, so the prompt does
    /// not claim the terminal cursor (the IME would anchor under the dialog).
    pub fn yield_cursor(&mut self, yielded: bool) {
        self.cursor_yielded = yielded;
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

    /// Advance the footer scanner; `true` when its frame changed and a repaint
    /// is needed. Idle resets it so the next turn starts from frame 0.
    pub fn tick_spinner(&mut self, now: Instant, activity: Activity) -> bool {
        let mut changed = false;
        // An armed Esc only means something while the own turn can be stopped.
        if let Some(armed) = self.interrupt_armed
            && (activity != Activity::Turn
                || now.saturating_duration_since(armed) >= INTERRUPT_WINDOW)
        {
            self.interrupt_armed = None;
            changed = true;
        }
        if activity == Activity::Idle {
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
        let Some(submission) = self.composer.handle_key(key) else {
            self.sync_menu();
            return PromptAction::Ignored;
        };
        self.submitted(&submission.text);
        PromptAction::Submit(submission)
    }

    /// Bracketed paste into the composer. The menu follows the new text the
    /// same way it follows typed keys.
    pub fn handle_paste(&mut self, text: String) {
        self.composer.insert_paste(text);
        self.sync_menu();
    }

    /// Pasted files that were read: each becomes a label in the composer.
    pub fn attach(&mut self, loaded: Vec<Loaded>) {
        for item in loaded {
            self.composer.attach(item);
        }
        self.sync_menu();
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
                if let Some(submission) = self.composer.handle_key(key) {
                    self.submitted(&submission.text);
                    return PromptAction::Submit(submission);
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect, full: Rect, activity: Activity) {
        let running = activity != Activity::Idle;
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
            self.composer.render(frame, text_area, !self.cursor_yielded);
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
            // Upstream `showDetails`: usage and hints stay up during a turn and
            // only yield to an armed interrupt on a narrow terminal
            // (`component/prompt/index.tsx:1614`).
            let details = self.interrupt_armed.is_none() || full.width >= 80;
            let right: Option<Vec<Span<'static>>> = if !details {
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
        // Upstream has no top or bottom border: the popup is exactly as tall
        // as its rows, capped at ten (`component/prompt/autocomplete.tsx`).
        let wanted = rows.len().clamp(1, 10) as u16;
        let available = area.y.saturating_sub(full.y);
        let height = wanted.min(available);
        if height == 0 || area.width < 4 {
            return;
        }
        let rect = Rect {
            x: area.x,
            y: area.y - height,
            width: area.width,
            height,
        };
        // Wipe what is underneath: styling alone keeps the old glyphs.
        Clear.render(rect, frame.buffer_mut());
        let buf = frame.buffer_mut();
        // Only the surface between the rails takes the menu background.
        buf.set_style(
            Rect {
                x: rect.x + 1,
                width: rect.width.saturating_sub(2),
                ..rect
            },
            pal.menu,
        );
        let inner = Rect {
            // One border cell plus one padding cell on either side.
            x: rect.x + 2,
            y: rect.y,
            width: rect.width.saturating_sub(4),
            height: rect.height,
        };
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let visible = inner.height as usize;
        let offset = self
            .auto
            .selected()
            .saturating_sub(visible.saturating_sub(1));
        for index in 0..inner.height {
            let selected = !rows.is_empty() && offset + index as usize == self.auto.selected();
            for (x, left) in [(rect.x, true), (rect.right() - 1, false)] {
                let cell = &mut buf[(x, rect.y + index)];
                cell.set_char('┃');
                // The rails stay transparent like upstream's border; only
                // the content surface uses the raised menu background.
                cell.set_style(if selected && left {
                    pal.accent
                } else {
                    pal.menu_border
                });
            }
        }
        if rows.is_empty() {
            let empty = if mention {
                "No matching files"
            } else {
                "No matching commands"
            };
            Paragraph::new(Line::styled(empty, pal.dim))
                .style(pal.menu)
                .render(inner, buf);
            return;
        }
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
