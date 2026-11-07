//! The composer's activity panel: the `Subagents` / `Shell` tabs that take the
//! composer's place.
//!
//! Port of upstream `packages/tui/src/routes/session/composer/subagents-tab.tsx`
//! and `shell-tab.tsx`, with the tab bar and the hint row from
//! `composer/index.tsx` and the tab registry from `composer/context.ts`.
//!
//! What this file owns, as upstream defines it:
//!
//! - One filter at a time, never both lists: `entry.status === "running"` while
//!   the filter is `active` (the default), `!== "running"` while it is not.
//! - The empty state names the filter: `No active subagents` /
//!   `No inactive subagents`, and `No shell commands` on the `Shell` tab.
//! - A subagent row reads `{prefix}{agent}: {title}`, with a right-aligned `Running`
//!   while its subagent still works. `agent` is the session's own agent, else
//!   the one in its `@xxx subagent` title marker, else `Subagent`; the marker is
//!   stripped from the title. The row under the cursor takes the focused fill
//!   and bold text, the row of the session on screen primary text, every other
//!   row body text; only the focused row is filled (upstream's
//!   `background.action.primary` is transparent for `base` and `$selected`).
//! - A shell row reads the first line of the command (`shell.command.split("\n",
//!   1)[0]`), the row under the cursor likewise filled and bold. The rows are
//!   the running shells of the session on screen, sorted by id; that session is
//!   every row here, so no row carries the "current" marking.
//! - `↑` from the first row closes the panel instead of clamping; `↓` wraps to
//!   the first row.
//! - `ctrl+a` flips the filter, puts the selection back on the first row and
//!   with it the window back at its start.
//! - The body is a five-row window (`scrollbox maxHeight={5}`, hidden
//!   scrollbar) over *every* listed entry, so all of them are reachable.
//! - Hints: `interrupt ctrl+d` only while the selected subagent row runs, then
//!   `show inactive ctrl+a` / `show active ctrl+a`; on the `Shell` tab
//!   `output enter` / `kill ctrl+d` only while a row is selected; `tabs ←/→`
//!   belongs to the tab bar, which ox draws in the same row set. Nothing else
//!   is hinted: upstream names no key for what a click does.
//! - The pointer acts on the rows: a cell inside a drawn row picks it
//!   (`onMouseMove`) and acts on it (`onMouseUp`) — navigate to the subagent
//!   (`subagents-tab.tsx:224-228`) or open the shell's output
//!   (`shell-tab.tsx:128-132`). [`row_at`] is this file's half of that, the
//!   geometry alone.
//!
//! The block sits on the composer's rail, in its place, and is not a modal over
//! the transcript: [`rows`], [`lines`] and [`Cursor`] are pure (the family as
//! the server reported it in, the block's rows out) and [`render`] paints them
//! with the same rail as [`super::prompt`].
//!
//! The block is always eleven rows, whatever it lists (upstream
//! `composer/index.tsx:92-97`: padding 1 above and below, `gap={1}` between
//! header, window and hints, `scrollbox maxHeight={5}`). Measured on upstream
//! at 30x100 with one running shell:
//!
//! ```text
//! ┃
//! ┃  Subagents  Shell                                    esc
//! ┃
//! ┃  cd ~/Developer/openkitbox/ox && cargo test
//! ┃
//! ┃
//! ┃
//! ┃
//! ┃
//! ┃  output enter  kill ctrl+d  tabs ←/→
//! ┃
//! ```
//!
//! Ledger — deliberate deviations from the upstream source:
//!
//! - The session on screen is drawn in `accent` without bold, ox's role for the
//!   primary colour (upstream `text.action.primary.$selected` = `primary`).
//! - The listed family is a snapshot: `App::family_state_changed` refetches it
//!   whenever a child the store names starts or finishes its turn (upstream
//!   reads a live memo over the session list), so a known child's `Running`
//!   suffix and both filters follow it. A child no tool result names yet — one
//!   spawned after the panel opened, before its tool row settles — appears only
//!   after the panel is reopened.
//! - The `Shell` tab's rows are derived from `DataStore` every frame
//!   (`shells_of`: id and command of the session's running shells), never held
//!   here: a frame that starts or exits a shell changes the map, and the rows
//!   follow. Upstream reads a live memo over `data.shell` the same way.
//! - `tab` / `shift+tab` switch tabs as well as `←` / `→`: ox binds them to
//!   agent cycling, and the panel owns the keyboard while it is open.
//! - The window is an offset held on [`Cursor`] instead of a scrollbox with its
//!   own scroll state. Upstream holds the same two pieces apart: the keyboard
//!   re-centres the window on every move (`moveTo(_, true)`,
//!   `subagents-tab.tsx:98-116`, called at `:152` and `:162`) while the pointer
//!   only ever sets the selection (`onMouseMove`, `:224`), so hovering a row
//!   cannot scroll the list out from under the pointer. [`Cursor::clamp`] pulls
//!   the offset back onto the rows the frame lists, and [`lines`] hands that
//!   clamped cursor back on [`Block::cursor`] for the caller to store: the
//!   window held between frames is the window on screen, so it cannot drift
//!   from the rows it indexes, and `↑` / `↓` reach every entry.
//! - Agent names go through the transcript's `titlecase`, which uppercases the
//!   first character only, where upstream `Locale.titlecase` uppercases every
//!   word boundary (`/\b\w/`); every built-in agent id is a single word.
//! - One cursor for both tabs: upstream keeps a separate `selected` store per
//!   tab, so a visit to the other tab leaves the first tab's selection where it
//!   was. ox keeps a single index for both: a tab switch keeps it and only
//!   clamps it to the new tab's rows (`App::settle_selection`), so the same
//!   row number is selected on the other tab, or its last row when the list is
//!   shorter, and switching back does not restore the earlier row.

use ox_client::rest::FamilyMember;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;

use super::family;
use super::prompt;
use super::session_dialog::fallback_title;
use super::timeline::titlecase;

/// Body rows the panel shows at once; the rest scroll through the window
/// (upstream `scrollbox maxHeight={5}`).
const VIEWPORT: usize = 5;
/// Rows from the top of the block to the window's first row: padding, header,
/// gap.
const BODY_TOP: u16 = 3;
/// Every row of the block: [`BODY_TOP`], the window, gap, hints, padding.
const BLOCK_ROWS: u16 = BODY_TOP + VIEWPORT as u16 + 3;
/// The key that closes the panel, drawn flush right on the header row.
const CLOSE_KEY: &str = "esc";
/// The right-aligned status of a row whose subagent is still working.
const RUNNING: &str = "Running";
/// The agent a row falls back to when neither its session nor its title names
/// one (upstream `subagents-tab.tsx:55`).
const UNNAMED_AGENT: &str = "Subagent";
/// The tail of upstream's `/@(\w+) subagent/` title marker.
const MARKER_SUFFIX: &str = " subagent";
const SUBAGENTS_EMPTY_ACTIVE: &str = "No active subagents";
const SUBAGENTS_EMPTY_INACTIVE: &str = "No inactive subagents";
const SHELL_EMPTY: &str = "No shell commands";

/// Which tab of the panel is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Subagents,
    Shell,
}

impl Tab {
    /// Every tab, in header order.
    const ALL: [Tab; 2] = [Tab::Subagents, Tab::Shell];

    /// The tab's own title in the header.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Tab::Subagents => "Subagents",
            Tab::Shell => "Shell",
        }
    }

    /// The tab to the right, wrapping (`→`).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Tab::Subagents => Tab::Shell,
            Tab::Shell => Tab::Subagents,
        }
    }

    /// The tab to the left, wrapping (`←`).
    #[must_use]
    pub fn prev(self) -> Self {
        match self {
            Tab::Subagents => Tab::Shell,
            Tab::Shell => Tab::Subagents,
        }
    }
}

/// The panel's cursor: which filter is listed, which row is selected and
/// where the window over the listed rows starts (upstream's in-memory
/// `subagents-filter`, the store's `selected` and the scrollbox's own
/// `scrollTop`). All three outlive the panel, so reopening it keeps them.
///
/// The window is held, not derived from the selection, because upstream holds
/// the two apart: only a keyboard move scrolls (`moveTo(next, true)`,
/// `subagents-tab.tsx:98-116`), while the pointer sets the selection alone
/// (`:224`). Deriving it would scroll the list out from under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// List the running subagents (`true`, the default) or the finished ones.
    /// Exactly one of the two is ever listed.
    pub active: bool,
    /// The selected row among the listed ones.
    pub selected: usize,
    /// The first listed row the window shows. Private: every path that may
    /// move it goes through a method here, and [`Cursor::clamp`] is what keeps
    /// it on the current list.
    window: usize,
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

impl Cursor {
    /// The default filter: running subagents only, on the first row, with the
    /// window at the start of the list.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: true,
            selected: 0,
            window: 0,
        }
    }

    /// Where the window starts among the listed rows. The draw hands its
    /// clamped cursor back on [`Block::cursor`] and the caller stores it, so
    /// this is the row the last frame painted first (upstream's `scrollTop`).
    #[must_use]
    pub fn window(self) -> usize {
        self.window
    }

    /// Keep the selection *and* the window on rows the list still has: the
    /// family, the filter or the tab changed under them. A list with no rows
    /// has only row 0. This is the clamp that stops a held window from
    /// drifting away from the rows it indexes: [`lines`] runs it over the rows
    /// it is about to draw and returns the result on [`Block::cursor`], so the
    /// window held between frames is the one on screen.
    #[must_use]
    pub fn clamp(self, len: usize) -> Self {
        let selected = self.selected.min(len.saturating_sub(1));
        // The window stays full at the end of the list, and the selection
        // stays inside it — upstream's scrollbox clamps the same way and its
        // selection is always a row it has scrolled to. The lower bound is
        // never above the upper one (`selected ≤ len - 1`), so no ordering is
        // assumed here beyond what these two lines establish.
        let last_full = len.saturating_sub(VIEWPORT);
        let first_visible = selected.saturating_sub(VIEWPORT - 1);
        Self {
            selected,
            window: self.window.min(selected.min(last_full)).max(first_visible),
            ..self
        }
    }

    /// Put the selection on `selected` and centre the window on it: upstream's
    /// `moveTo(next, true)` → `scrollToIndex(index, true)`, which scrolls to
    /// `index - floor(viewport / 2)` and lets its scrollbox clamp that to the
    /// last full window (`subagents-tab.tsx:98-116`). Every keyboard move
    /// takes this path (`:152`, `:162`), and so does the panel opening on the
    /// session on screen (`:88-90`).
    #[must_use]
    pub fn centre_on(self, selected: usize, len: usize) -> Self {
        Self {
            selected,
            window: selected
                .saturating_sub(VIEWPORT / 2)
                .min(len.saturating_sub(VIEWPORT)),
            ..self
        }
    }

    /// Back to the first row with the window at its start, the filter kept:
    /// the panel just opened or closed, or the session changed under it
    /// (upstream resets `selected` on close, `subagents-tab.tsx:76-79`).
    #[must_use]
    pub fn reset(self) -> Self {
        Self {
            selected: 0,
            window: 0,
            ..self
        }
    }

    /// `↑` (`config/keybind.ts:249`): the row above, or the panel closing from
    /// the first row — upstream closes the composer there instead of clamping,
    /// and a tab with no rows is always on its first one.
    #[must_use]
    pub fn up(self, len: usize) -> Up {
        if self.selected == 0 || len == 0 {
            Up::Close
        } else {
            Up::Moved(self.centre_on(self.selected - 1, len))
        }
    }

    /// `↓` (`config/keybind.ts:250`): the row below, wrapping to the first. A
    /// list with no rows stays on row 0.
    #[must_use]
    pub fn down(self, len: usize) -> Self {
        if len == 0 {
            return self.reset();
        }
        self.centre_on((self.selected + 1) % len, len)
    }

    /// `ctrl+a` (`composer.subagent.toggle-activity`): the other filter, the
    /// selection back on the first row and the window back at its start
    /// (upstream `setStore("selected", 0)` + `scroll?.scrollTo(0)`,
    /// `subagents-tab.tsx:181-182`).
    #[must_use]
    pub fn toggle(self) -> Self {
        Self {
            active: !self.active,
            ..self.reset()
        }
    }
}

/// Where `↑` sent the cursor: upstream closes the composer from the first row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Up {
    /// The panel closes and the composer comes back.
    Close,
    /// The cursor moved a row up, the window re-centred on it.
    Moved(Cursor),
}

/// One listed row: the subagent a row acts on and the tree prefix the session
/// list draws before its title. The member travels with the row so an action
/// never has to find it again by index.
pub struct Row<'a> {
    /// The session `enter` opens and `ctrl+d` interrupts.
    pub member: &'a FamilyMember,
    /// The tree prefix of the family walk (`"│  └─ "` and the like).
    pub prefix: String,
}

/// The block [`render`] draws: the header, the window of body rows and the
/// hints, with the body rows that are marked, plus the row count the pointer
/// needs and the cursor the block was drawn with.
#[derive(Debug, Default)]
pub struct Block {
    /// The tab bar (`esc` is added flush right when drawn).
    pub header: Line<'static>,
    /// At most [`VIEWPORT`] rows: the listed window, or the empty state.
    pub body: Vec<Line<'static>>,
    /// The hint row.
    pub hints: Line<'static>,
    /// Index into `body` of the selected row, when one is drawn. `None` when
    /// no row is listed.
    pub selected: Option<usize>,
    /// Indices into `body` of the rows that carry the right-aligned
    /// [`RUNNING`] status.
    pub running: Vec<usize>,
    /// How many rows the tab lists in all, which is what [`row_at`] measures
    /// a hit against. Zero for the empty state: it draws a line, not a row.
    pub listed: usize,
    /// The cursor this block was drawn with: the caller's, [`Cursor::clamp`]ed
    /// to `listed`, so its window is the first listed row on screen and its
    /// selection the row that was highlighted. The caller stores it back, so
    /// the window held between frames stays the window that was painted and
    /// the pointer, the paint and the stored cursor are one number, never
    /// computed twice.
    pub cursor: Cursor,
}

impl Block {
    /// Rows the block takes. Upstream's box is the same height whatever it
    /// lists: one padding row, the header, a gap, the five-row window, a gap,
    /// the hints, one padding row (`composer/index.tsx:92-97`, `gap={1}`).
    #[must_use]
    pub fn height(&self) -> u16 {
        BLOCK_ROWS
    }
}

/// The header row: every tab title, the selected one bold.
#[must_use]
pub fn header(tab: Tab) -> Line<'static> {
    let pal = *theme::palette();
    let selected = pal.text.add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(Tab::ALL.len() * 2);
    for (index, each) in Tab::ALL.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if each == tab { selected } else { pal.dim };
        spans.push(Span::styled(each.label(), style));
    }
    Line::from(spans)
}

/// The hint row: each action's title then its key, as the dialogs draw them.
/// `active` is the filter, so the toggle offers the *other* list. `armed` adds
/// the row actions upstream shows conditionally: on `Subagents`
/// `interrupt ctrl+d` while the selected row runs
/// (`subagents-tab.tsx:122-133`), on `Shell` `output enter` and `kill ctrl+d`
/// while a row is selected (`shell-tab.tsx:53-59`,
/// `config/keybind.ts:253-256`). `tabs ←/→` is the composer's own
/// (`composer/index.tsx:143-150`).
#[must_use]
pub fn hints(tab: Tab, active: bool, armed: bool) -> Line<'static> {
    let pal = *theme::palette();
    let toggle = if active {
        "show inactive"
    } else {
        "show active"
    };
    let hints: &[(&str, &str)] = match tab {
        Tab::Subagents if armed => &[("interrupt", "ctrl+d"), (toggle, "ctrl+a"), ("tabs", "←/→")],
        Tab::Subagents => &[(toggle, "ctrl+a"), ("tabs", "←/→")],
        Tab::Shell if armed => &[("output", "enter"), ("kill", "ctrl+d"), ("tabs", "←/→")],
        Tab::Shell => &[("tabs", "←/→")],
    };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(hints.len() * 4);
    for (index, &(title, key)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(title, pal.text.add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(" ", pal.text));
        spans.push(Span::styled(key, pal.dim));
    }
    Line::from(spans)
}

/// The subagent rows of `current`'s family: the running ones while `active`,
/// the finished ones while it is not — never both — in tree order. This is the
/// one list the panel draws and acts on, so no index can outlive the rows.
#[must_use]
pub fn rows<'a>(members: &'a [FamilyMember], current: &str, active: bool) -> Vec<Row<'a>> {
    family::family(members, current)
        .into_iter()
        .filter(|entry| entry.session.running == active)
        .map(|entry| Row {
            member: entry.session,
            prefix: entry.prefix,
        })
        .collect()
}

/// The whole block: header, the window of rows, hints. `shells` is the
/// `Shell` tab's `(id, command)` list (`DataStore::shells_of`).
/// `cursor.selected` indexes the shown tab's rows ([`rows`] or `shells`) and
/// `cursor`'s window says where the window starts; the cursor is clamped to
/// the rows it finds, so a stale one draws a valid window. That clamped
/// cursor comes back on [`Block::cursor`], together with `listed`, so the
/// caller stores the window this frame drew instead of keeping one the draw
/// corrected behind its back.
#[must_use]
pub fn lines(
    members: &[FamilyMember],
    shells: &[(&str, &str)],
    current: &str,
    tab: Tab,
    cursor: Cursor,
) -> Block {
    if tab == Tab::Shell {
        return shell_lines(shells, cursor);
    }
    let listed = rows(members, current, cursor.active);
    if listed.is_empty() {
        let empty_state = if cursor.active {
            SUBAGENTS_EMPTY_ACTIVE
        } else {
            SUBAGENTS_EMPTY_INACTIVE
        };
        return Block {
            header: header(tab),
            body: vec![empty(empty_state)],
            hints: hints(tab, cursor.active, false),
            // No row is drawn, so the stored cursor goes back to the only row
            // an empty list has; the filter is the caller's and stays.
            cursor: cursor.clamp(0),
            ..Block::default()
        };
    }

    let cursor = cursor.clamp(listed.len());
    let offset = cursor.window;
    let selected = cursor.selected;
    let mut block = Block {
        header: header(tab),
        hints: hints(tab, cursor.active, listed[selected].member.running),
        selected: Some(selected - offset),
        listed: listed.len(),
        cursor,
        ..Block::default()
    };
    for (index, row) in listed.iter().skip(offset).take(VIEWPORT).enumerate() {
        let focus = if Some(index) == block.selected {
            Focus::Selected
        } else if row.member.id == current {
            Focus::Current
        } else {
            Focus::None
        };
        block.body.push(listed_row(row, focus));
        if row.member.running {
            block.running.push(index);
        }
    }
    block
}

/// The `Shell` tab's block: one row per running shell, the first line of its
/// command (upstream `shell-tab.tsx:139`), in the same window as the subagent
/// rows.
fn shell_lines(shells: &[(&str, &str)], cursor: Cursor) -> Block {
    let pal = *theme::palette();
    if shells.is_empty() {
        return Block {
            header: header(Tab::Shell),
            body: vec![empty(SHELL_EMPTY)],
            hints: hints(Tab::Shell, cursor.active, false),
            // As on the subagents tab: nothing is drawn, so the stored cursor
            // goes back to row 0 with the window at the start of the list.
            cursor: cursor.clamp(0),
            ..Block::default()
        };
    }
    let cursor = cursor.clamp(shells.len());
    let offset = cursor.window;
    let mut block = Block {
        header: header(Tab::Shell),
        hints: hints(Tab::Shell, cursor.active, true),
        selected: Some(cursor.selected - offset),
        listed: shells.len(),
        cursor,
        ..Block::default()
    };
    let focused = pal.menu_selected.add_modifier(Modifier::BOLD);
    for (index, &(_, command)) in shells.iter().skip(offset).take(VIEWPORT).enumerate() {
        let style = if Some(index) == block.selected {
            focused
        } else {
            pal.text
        };
        let first = command.split('\n').next().unwrap_or_default();
        block
            .body
            .push(Line::from(Span::styled(first.to_owned(), style)));
    }
    block
}

/// How a listed row is marked (upstream's `active()` / `entry.current`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The row under the cursor: filled, bold.
    Selected,
    /// The session on screen: primary text on the panel surface.
    Current,
    None,
}

/// One listed row line: `{prefix}{agent}: {title}`. The focused row is filled
/// and bold, the session on screen in primary text (`text.action.primary`
/// `$selected`), every other one in body text (upstream draws one `<text>`).
fn listed_row(row: &Row<'_>, focus: Focus) -> Line<'static> {
    let pal = *theme::palette();
    let (agent, title) = agent_and_title(row.member);
    let style = match focus {
        Focus::Selected => pal.menu_selected.add_modifier(Modifier::BOLD),
        Focus::Current => pal.accent.remove_modifier(Modifier::BOLD),
        Focus::None => pal.text,
    };
    let label = format!("{}{agent}: {title}", row.prefix);
    Line::from(Span::styled(label, style))
}

/// The row's agent and title. The agent is the session's own, else the one in
/// its `@xxx subagent` title marker, else [`UNNAMED_AGENT`]; the marker is
/// stripped from the title, which stays whole when nothing else is left.
fn agent_and_title(member: &FamilyMember) -> (String, String) {
    let title = fallback_title(member.title.as_deref(), member.time.created);
    let found = marker(&title);
    let agent = match member.agent.as_deref() {
        Some(agent) => titlecase(agent),
        None => found
            .as_ref()
            .map_or_else(|| UNNAMED_AGENT.to_owned(), |found| titlecase(found.agent)),
    };
    let Some(found) = found else {
        return (agent, title);
    };
    let stripped = title.replacen(found.text, "", 1).trim().to_owned();
    let title = if stripped.is_empty() { title } else { stripped };
    (agent, title)
}

/// The `@xxx subagent` marker upstream matches in a title (`/@(\w+) subagent/`),
/// `xxx` being one or more word characters.
struct Marker<'a> {
    /// The whole match, stripped from the title.
    text: &'a str,
    /// The agent name inside it.
    agent: &'a str,
}

fn marker(title: &str) -> Option<Marker<'_>> {
    for (at, _) in title.match_indices('@') {
        let rest = title.get(at + 1..)?;
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end == 0 || !rest.get(end..)?.starts_with(MARKER_SUFFIX) {
            continue;
        }
        let stop = at + 1 + end + MARKER_SUFFIX.len();
        return Some(Marker {
            text: title.get(at..stop)?,
            agent: rest.get(..end)?,
        });
    }
    None
}

/// The block's content box on its first row: one column after the rail,
/// upstream's `paddingLeft={1}` / `paddingRight={2}`
/// (`composer/index.tsx:92-97`). This is the width a row is filled to, so it
/// is also the width a row is clicked in. `None` when the area is too small to
/// draw anything.
fn content_rect(area: Rect) -> Option<Rect> {
    (area.width >= 6 && area.height > 0).then(|| Rect {
        x: area.x + 2,
        width: area.width - 4,
        height: 1,
        y: area.y,
    })
}

/// The body row a screen cell falls in: this file's half of upstream's row
/// pointer handlers, which select the row under the pointer on `onMouseMove`
/// and act on it on `onMouseUp` — navigate to the subagent
/// (`subagents-tab.tsx:224-228`) or open the shell's output
/// (`shell-tab.tsx:128-132`). It owns the geometry alone: which of the `rows`
/// drawn in `area` covers `(x, y)`, counted from the top of the window, or
/// `None` for the header, the hints, the padding, the rail and everything past
/// the last drawn row. It does not know what a row is, so what to do with one
/// stays with the caller.
#[must_use]
pub fn row_at(area: Rect, x: u16, y: u16, rows: usize) -> Option<usize> {
    let content = content_rect(area)?;
    if x < content.x || x >= content.right() || y >= area.bottom() {
        return None;
    }
    let index = usize::from(y.checked_sub(area.y)?.checked_sub(BODY_TOP)?);
    (index < rows.min(VIEWPORT)).then_some(index)
}

/// Draw the block in the composer's place: the prompt's rail on the
/// `raised.base` surface upstream's bordered box fills (rail cell included), the header with `esc` flush right, the window of rows with
/// the selected one filled and `Running` flush right where a row carries one,
/// and the hints. The rail runs to the bottom of the block with no foot, as
/// upstream's left border does.
pub fn render(frame: &mut Frame, area: Rect, block: Block) {
    let Block {
        header,
        body,
        hints,
        selected,
        running,
        listed: _,
        cursor: _,
    } = block;
    // The same box [`row_at`] hit-tests, and the same test `render` used to
    // bail on.
    let Some(content) = content_rect(area) else {
        return;
    };
    let pal = *theme::palette();
    {
        let buf = frame.buffer_mut();
        for y in area.y..area.bottom() {
            for x in area.x + 1..area.right() {
                buf[(x, y)].set_style(pal.raised_base);
            }
            let cell = &mut buf[(area.x, y)];
            cell.set_style(pal.raised_base.patch(pal.panel_border));
            cell.set_char(prompt::PANEL_BORDER_CHAR);
        }
    }
    // Rows fill the whole content box; their text sits one column in
    // (`paddingLeft={1}` on the header, the rows and the hints).
    let text = Rect {
        x: content.x + 1,
        width: content.width - 2,
        ..content
    };
    let row_y = |offset: u16| -> Option<u16> {
        let y = area.y.checked_add(offset)?;
        (y < area.bottom()).then_some(y)
    };

    if let Some(y) = row_y(1) {
        let used = header.width();
        frame.render_widget(Paragraph::new(header), Rect { y, ..text });
        let close_w = u16::try_from(CLOSE_KEY.len()).unwrap_or(u16::MAX);
        if used + CLOSE_KEY.len() + 1 < content.width as usize {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(CLOSE_KEY, pal.dim))),
                Rect {
                    x: content.right() - close_w,
                    y,
                    width: close_w,
                    height: 1,
                },
            );
        }
    }

    let status_w = u16::try_from(RUNNING.len()).unwrap_or(u16::MAX);
    for (index, line) in body.into_iter().take(VIEWPORT).enumerate() {
        let Some(y) = row_y(BODY_TOP + u16::try_from(index).unwrap_or(u16::MAX)) else {
            break;
        };
        let is_selected = Some(index) == selected;
        if is_selected {
            frame
                .buffer_mut()
                .set_style(Rect { y, ..content }, pal.menu_selected);
        }
        // A row with a status gives it the right edge and clips its own text
        // where the status starts (upstream `flexGrow={1} minWidth={0}`).
        let has_status = running.contains(&index) && text.width > status_w;
        let width = if has_status {
            text.width - status_w
        } else {
            text.width
        };
        frame.render_widget(Paragraph::new(line), Rect { y, width, ..text });
        if has_status {
            let style = if is_selected {
                pal.menu_selected
            } else {
                pal.dim
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(RUNNING, style))),
                Rect {
                    x: text.right() - status_w,
                    y,
                    width: status_w,
                    height: 1,
                },
            );
        }
    }

    if let Some(y) = row_y(BLOCK_ROWS - 2) {
        frame.render_widget(Paragraph::new(hints), Rect { y, ..text });
    }
}

fn empty(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), theme::palette().dim))
}
