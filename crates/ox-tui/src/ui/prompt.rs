//! The input surface shared by the home and session routes: left-bordered panel,
//! metadata row, `╹`/`▀` bottom edge, footer row, and the slash-command menu.
//!
//! Visual spec and deviations are recorded in
//! `.ox/plans/ox-tui-prompt-surface.md`. Upstream reference:
//! `component/prompt/index.tsx` (panel), `component/prompt/metadata.tsx`
//! (metadata degradation), `component/prompt/autocomplete.tsx` (menu).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use crate::attachment::Loaded;
use crate::graphics::Graphics;
use crate::graphics::Prepared;
use crate::graphics::Slot;
use crate::theme;
use crate::ui::autocomplete::{self, Autocomplete, FileRow, Trigger};
use crate::ui::commands::{self, CommandAction, Row};
use crate::ui::composer::{Composer, Submission};
use crate::ui::history::History;
use crate::ui::media;
use crate::ui::spinner;

/// Whether screen cell (`x`, `y`) falls inside `rect`.
fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    rect.contains(Position::new(x, y))
}

pub const PLACEHOLDERS: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];

/// Upstream `SplitBorder.vertical`: the panel's left edge is a heavy vertical
/// rule. A half block (`▌`) is twice as wide and does not line up with the
/// centred `╹` foot below it, which reads as a notch. The activity panel
/// (`super::activity_panel`), which takes the composer's place, draws the same
/// rail.
pub(crate) const PANEL_BORDER_CHAR: char = '┃';
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

/// How the subagent picker's shortcut reads in the footer and the palette:
/// upstream `session.child.first = down`, formatted as `↓`
/// (`config/keybind.ts:138`, `context/keymap.tsx:501`).
pub const SUBAGENT_SHORTCUT: &str = "↓";

/// The running-subagent / running-shell counts the footer's live chip shows.
/// Zero counts draw nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Live {
    pub subagents: usize,
    pub shells: usize,
    /// The pointer is over the chip: the counts light up.
    pub hovered: bool,
}

/// `1 subagent` / `2 shells`: upstream's `count === 1 ? "" : "s"`.
fn count_label(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// The live chip: `↓ 1 subagent · 2 shells`
/// (`feature-plugins/prompt/footer.tsx:73-92`).
///
/// The shortcut always draws in `text.base`; the counts and their ` · `
/// separator sit in the outer `<text>`, which is `text.muted` until the
/// pointer is over the chip and `text.base` after. A zero count omits its
/// segment, and ` · ` needs both segments; with no counts the chip is empty
/// and draws nothing.
fn live_spans(live: Live, pal: &theme::Palette) -> Vec<Span<'static>> {
    if live.subagents == 0 && live.shells == 0 {
        return Vec::new();
    }
    let counts = if live.hovered { pal.text } else { pal.dim };
    let mut spans = vec![Span::styled(format!("{SUBAGENT_SHORTCUT} "), pal.text)];
    if live.subagents > 0 {
        spans.push(Span::styled(
            count_label(live.subagents, "subagent"),
            counts,
        ));
    }
    if live.subagents > 0 && live.shells > 0 {
        spans.push(Span::styled(" · ", counts));
    }
    if live.shells > 0 {
        spans.push(Span::styled(count_label(live.shells, "shell"), counts));
    }
    spans
}

/// What a key meant to the app.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptAction {
    /// The widget handled it (typed text, menu navigation, cleared draft).
    Ignored,
    /// The key is not the prompt's: a bare `↓` at the end of the history,
    /// which upstream's `prompt.history.next` hands back with `false`
    /// (`component/prompt/index.tsx:1086`) so `session.child.first` reaches
    /// the subagent picker. A bare `↑` at the start of the history declines
    /// the same way (upstream `prompt.history.previous`); ox binds nothing to
    /// it. `App` decides who takes the key.
    Declined,
    /// The user submitted a non-empty prompt.
    Submit(Submission),
    /// A slash command was chosen; the token was removed from the draft.
    Run(CommandAction),
    /// ctrl+c with an empty draft: the app quits.
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

/// What Up/Down did to the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryWalk {
    /// Not a history key (or the caret has rows to travel inside the draft).
    NotMine,
    /// Consumed: moved the caret or replaced the draft from history.
    Consumed,
    /// At the text edge with no further entry: the key belongs to the next
    /// handler, exactly upstream's `prompt.history.next` returning `false`
    /// (`component/prompt/index.tsx:1086`), which lets bare `↓` reach
    /// `session.child.first`. `↑` declines the same way at the start of the
    /// history.
    Declined,
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
    /// The cols the live chip occupied on the last draw, for the pointer.
    live_area: Rect,
    /// The attachment strip as the last draw left it, for the pointer.
    strip: Option<StripState>,
}

/// The image strip above the textarea, as drawn: where its band went and what
/// each cell showed, so a click can open the image under the pointer.
struct StripState {
    strip: media::Strip,
    /// What the band showed: the cells drawn whole and the `+N more` label,
    /// laid out by the draw so hit-testing reads the same answer instead of
    /// recomputing it.
    layout: media::Layout,
    /// The composer generation `images` was derived from: the strip is redrawn
    /// every frame, and rebuilding the list would allocate a key and a name per
    /// image per frame on the draw path.
    revision: u64,
    images: Vec<media::Image>,
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
            strip: None,
            cursor_yielded: false,
            live_area: Rect::default(),
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
        // paddingTop + image strip + textarea + paddingTop + metadata row
        1 + self.strip_rows(terminal_height) + self.composer.height(terminal_height) + 1 + 1
    }

    /// Rows the attachment strip takes above the textarea: the thumbnails plus
    /// the blank under them (upstream
    /// `height={imagePreviewHeight() + 1} ... paddingBottom={1}`,
    /// `component/prompt/index.tsx:1701-1710`). Zero when nothing is attached
    /// or the terminal cannot draw pictures, so no empty band is reserved.
    fn strip_rows(&self, terminal_height: u16) -> u16 {
        if self.composer.image_count() == 0 || !media::supported() {
            return 0;
        }
        media::height(terminal_height) + 1
    }

    /// The images attached to the draft, for the enlarged preview
    /// (`prompt.images.view`). Empty when there are none.
    #[must_use]
    pub fn images(&self) -> Vec<media::Image> {
        self.composer.images().collect()
    }

    /// The image under the pointer and the whole strip it belongs to, for the
    /// preview: `+N more` opens at the first image the strip does not draw.
    #[must_use]
    pub fn image_at(&self, x: u16, y: u16) -> Option<(&[media::Image], usize)> {
        let state = self.strip.as_ref()?;
        let drawn = usize::from(state.strip.images).min(state.images.len());
        let visible = usize::from(state.layout.cells).min(drawn);
        for index in 0..visible {
            let Ok(position) = u16::try_from(index) else {
                continue;
            };
            if rect_contains(state.layout.cell(&state.strip, position), x, y) {
                return Some((state.images.as_slice(), index));
            }
        }
        // `+N more` opens at the first image no cell draws, which is the first
        // one past the cells that fit — not past the strip's image count, or a
        // narrow band would skip the images it could not draw.
        (state
            .layout
            .more
            .is_some_and(|more| rect_contains(more, x, y)))
        .then_some((state.images.as_slice(), visible))
    }

    /// The cols the live chip occupied on the last draw, for the pointer.
    /// Empty when nothing drew (no counts, home footer, narrow terminal).
    #[must_use]
    pub fn live_area(&self) -> Rect {
        self.live_area
    }

    /// Forget the last chip rect: the prompt is not being drawn (a panel holds
    /// the bottom of the screen), so the pointer must not hit a stale one.
    pub fn reset_live_area(&mut self) {
        self.live_area = Rect::default();
        self.strip = None;
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
        if key.modifiers.is_empty() {
            // Upstream's history bindings answer `false` at the end of their
            // list, which lets the same key fall through to `session.child.first`
            // (`component/prompt/index.tsx:1060-1094`).
            match self.walk_history(key.code) {
                HistoryWalk::Consumed => return PromptAction::Ignored,
                HistoryWalk::Declined => return PromptAction::Declined,
                HistoryWalk::NotMine => {}
            }
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
    fn walk_history(&mut self, code: KeyCode) -> HistoryWalk {
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
            _ => return HistoryWalk::NotMine,
        };
        if !text_edge {
            if boundary_row {
                if direction < 0 {
                    self.composer.move_to_start();
                } else {
                    self.composer.move_to_end();
                }
                return HistoryWalk::Consumed;
            }
            return HistoryWalk::NotMine;
        }
        let Some(text) = self.history.step(direction, &self.composer.text()) else {
            return HistoryWalk::Declined;
        };
        self.composer.set_text(&text, direction > 0);
        self.auto.hide();
        self.sync_files();
        HistoryWalk::Consumed
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

    /// Draw the block. `live` is the chip's data; `App` derives it from the
    /// store on every frame instead of caching counts here.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        full: Rect,
        activity: Activity,
        live: Live,
        graphics: &mut Graphics,
    ) -> Vec<media::Request> {
        // The chip and the strip are only hit-testable on a frame that drew them.
        self.live_area = Rect::default();
        self.strip = None;
        let running = activity != Activity::Idle;
        let pal = *theme::palette();
        if area.width == 0 || area.height == 0 {
            return Vec::new();
        }
        let rail = self.rail_style(&pal);
        let pad = if full.width < 44 { 1 } else { 2 };
        let strip_rows = self.strip_rows(full.height);
        let composer_h = self
            .composer
            .height(full.height)
            .min(area.height.saturating_sub(3 + strip_rows).max(1));
        let panel_h = 1 + strip_rows + composer_h + 1 + 1;
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
        // The strip sits between the panel's top padding and the textarea.
        let mut requests = Vec::new();
        if strip_rows > 0 {
            let band = Rect {
                x: inner_x,
                y: panel.y + 1,
                width: inner_w,
                height: strip_rows.saturating_sub(1),
            };
            requests = self.draw_strip(frame, band, graphics, &pal);
        }
        let text_area = Rect {
            x: inner_x,
            y: panel.y + 1 + strip_rows,
            width: inner_w,
            height: composer_h.min(panel.height.saturating_sub(1 + strip_rows)),
        };
        if text_area.height > 0 {
            self.composer.render(frame, text_area, !self.cursor_yielded);
        }

        // Metadata row, one blank row under the textarea.
        let meta_y = panel.y + 1 + strip_rows + composer_h + 1;
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
            // (`component/prompt/index.tsx:1614`). The live chip is a sibling of
            // that block and never takes part in the gate
            // (`feature-plugins/prompt/footer.tsx:71-92`).
            let details = self.interrupt_armed.is_none() || full.width >= 80;
            // The chip leads the right segment and is not charged against the
            // layout budget: upstream's `promptFooterLayout` reserves columns
            // for the usage and the hint only (`footer.tsx:156-166`). It
            // describes a session, so the home route has none to show
            // (`footer.tsx:24,31` read zero without a session id).
            let chip = if self.footer == Footer::Session {
                live_spans(live, &pal)
            } else {
                Vec::new()
            };
            let chip_w = spans_width(&chip).min(area.width as usize) as u16;
            // The usage group and the shortcut hint are separate children of
            // the row: upstream's status `<text>` owns the ` · ` before the
            // usage and the footer container's `gap={2}` spaces the hint off it
            // (`component/prompt/index.tsx:1900`).
            let (usage_part, hint_part): (Vec<Span<'static>>, Vec<Span<'static>>) = if !details {
                (Vec::new(), Vec::new())
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
                        if usage_w > 0 && usage_w <= available {
                            let with_hint = full.width >= 44 && usage_w + 2 + hint_w <= available;
                            let hint = if with_hint { hint } else { Vec::new() };
                            (usage, hint)
                        } else if usage_w == 0 && full.width >= 44 && hint_w <= available {
                            (Vec::new(), hint)
                        } else {
                            (Vec::new(), Vec::new())
                        }
                    }
                    Footer::Home => {
                        let hint = if full.width >= 44 {
                            home_hints(&pal)
                        } else {
                            Vec::new()
                        };
                        (Vec::new(), hint)
                    }
                }
            };
            let mut spans = chip;
            if !usage_part.is_empty() {
                // Upstream writes this separator inside the usage block, so it
                // appears only between the chip and the usage, and it carries
                // the usage's `text.muted` (`footer.tsx:102-105`).
                if !spans.is_empty() {
                    spans.push(Span::styled(" · ", pal.dim));
                }
                spans.extend(usage_part);
            }
            if !hint_part.is_empty() {
                // The container's two-column gap; it needs a column before it
                // only when something already sits to its left.
                if !spans.is_empty() {
                    spans.push(Span::raw("  "));
                }
                spans.extend(hint_part);
            }
            let right: Option<Vec<Span<'static>>> = (!spans.is_empty()).then_some(spans);
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
                let right_w = spans_width(&spans).min(area.width as usize) as u16;
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
                    // The chip is the right segment's first element, so the
                    // segment's left edge is the chip's. Only with nothing after
                    // the chip does its right edge land on `area.right()`.
                    if chip_w > 0 {
                        self.live_area = Rect {
                            x,
                            y: footer_y,
                            width: chip_w,
                            height: 1,
                        };
                    }
                }
            }
        }
        requests
    }

    /// Draw the attachment strip in `band` and report the images whose bytes are
    /// not cached yet.
    ///
    /// Upstream shows at most three thumbnails plus `+N more`
    /// (`component/prompt/index.tsx:1701-1766`); the strip is drawn as pixels by
    /// the graphics layer, so the panel rows it covers stay blank here. What
    /// fits is [`media::Strip::layout`]'s answer, which the hit-test reads back
    /// from the state this draw stores.
    fn draw_strip(
        &mut self,
        frame: &mut Frame,
        band: Rect,
        graphics: &mut Graphics,
        pal: &theme::Palette,
    ) -> Vec<media::Request> {
        // Reuse the list the last draw derived when the attachments have not
        // moved since; only a paste, a removal or a submit rebuilds it.
        let revision = self.composer.attachments_revision();
        let images = match self.strip.take() {
            Some(state) if state.revision == revision => state.images,
            _ => self.composer.images().collect(),
        };
        let Some(strip) = media::Strip::new(0, images.len(), band.height) else {
            return Vec::new();
        };
        let (x, y, width) = (band.x, band.y, band.width);
        let layout = strip.layout(x, y, width);
        let mut requests = Vec::new();
        let mut painted: Vec<Arc<Prepared>> = Vec::new();
        let mut areas = Vec::new();
        for (index, image) in images.iter().enumerate().take(usize::from(layout.cells)) {
            let Ok(position) = u16::try_from(index) else {
                continue;
            };
            let cell = layout.cell(&strip, position);
            match graphics.cached_for(&image.key, strip.cells()) {
                Some(prepared) => {
                    painted.push(prepared);
                    areas.push(cell);
                }
                None => requests.push(media::Request {
                    key: image.key.clone(),
                    bytes: image.bytes.clone(),
                    cells: strip.cells(),
                    class: media::THUMBNAIL,
                }),
            }
        }
        let slots: Vec<Slot<'_>> = painted
            .iter()
            .zip(&areas)
            .map(|(image, area)| Slot { area: *area, image })
            .collect();
        graphics.paint(frame.buffer_mut(), &slots);
        if let Err(err) = graphics.flush() {
            tracing::warn!(%err, "queued graphics escapes not written");
        }
        if let Some(more) = layout.more {
            let label = format!("+{} more", strip.more);
            let label_w = u16::try_from(label.width())
                .unwrap_or(more.width)
                .min(more.width);
            frame.render_widget(
                Paragraph::new(Span::styled(label, pal.dim)),
                Rect {
                    x: more.x + (more.width - label_w) / 2,
                    y,
                    width: label_w,
                    height: 1,
                },
            );
        }
        self.strip = Some(StripState {
            strip,
            layout,
            revision,
            images,
        });
        requests
    }

    /// The rail and its `╹` foot, in the agent colour: upstream takes it from
    /// the selected agent (`component/prompt/index.tsx:1610`; captured in
    /// 2.2.5 as the same magenta `#b294bb` on the home route and on a root
    /// session) and falls back to `border.base` only without an agent colour.
    fn rail_style(&self, pal: &theme::Palette) -> Style {
        pal.agent
            .fg
            .map_or(pal.panel_border, |fg| Style::default().fg(fg))
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
