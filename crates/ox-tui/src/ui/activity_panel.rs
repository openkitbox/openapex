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
//!   stripped from the title. The row under the cursor takes the focused fill,
//!   the row of the session on screen a quieter one, every other row the base.
//! - A shell row reads the first line of the command (`shell.command.split("\n",
//!   1)[0]`), the row under the cursor likewise filled and bold. The rows are
//!   the running shells of the session on screen, sorted by id; that session is
//!   every row here, so no row carries the quieter "current" fill.
//! - `↑` from the first row closes the panel instead of clamping; `↓` wraps to
//!   the first row.
//! - `ctrl+a` flips the filter, puts the selection back on the first row and
//!   with it the window back at its start.
//! - The body is a five-row window (`scrollbox maxHeight={5}`, hidden
//!   scrollbar) over *every* listed entry, so all of them are reachable.
//! - Hints: `interrupt ctrl+d` only while the selected subagent row runs, then
//!   `show inactive ctrl+a` / `show active ctrl+a`; on the `Shell` tab
//!   `output enter` / `kill ctrl+d` only while a row is selected; `tabs ←/→`
//!   belongs to the tab bar, which ox draws in the same row set.
//!
//! The block sits on the composer's rail, in its place, and is not a modal over
//! the transcript: [`rows`], [`lines`] and [`Cursor`] are pure (the family as
//! the server reported it in, the block's rows out) and [`render`] paints them
//! with the same rail as [`super::prompt`].
//!
//! Measured at 30x100 with no running child:
//!
//! ```text
//! ┃  Subagents  Shell                                    esc
//! ┃  No active subagents
//! ┃
//! ┃  open enter  show inactive ctrl+a  tabs ←/→
//! ```
//!
//! Ledger — deliberate deviations from the upstream source:
//!
//! - Keys only: upstream selects a row on `onMouseMove` and navigates on
//!   `onMouseUp`; ox's pointer gate belongs to the transcript, so the panel
//!   takes keys and shows `open enter` (upstream's `composer.subagent.select`,
//!   `config/keybind.ts:251`) as the hint for what a click does there.
//! - The session on screen is filled with `raised.base`, not with
//!   `background.action.primary.selected`: ox's palette carries no such token,
//!   and `raised.base` is its one quieter raised surface. The focused row keeps
//!   the picker's accent fill (`menu_selected`), so the two stay distinct.
//!   Only truecolor separates `raised.base` from the `raised.high` panel it sits
//!   on (`theme.rs` `from_tokens`); every 16-colour and `NO_COLOR` palette draws
//!   both as the same surface, so the row of the session on screen goes unmarked
//!   there.
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
//! - The window's offset is derived from the selection (centred, clamped so the
//!   window stays full at the end of the list) instead of a scrollbox with its
//!   own scroll state: nothing can drift, and `↑` / `↓` reach every entry.
//! - Agent names go through the transcript's `titlecase`, which uppercases the
//!   first character only, where upstream `Locale.titlecase` uppercases every
//!   word boundary (`/\b\w/`); every built-in agent id is a single word.
//! - One cursor for both tabs: upstream keeps a separate `selected` store per
//!   tab, so a visit to the other tab leaves the first tab's selection where it
//!   was. ox keeps a single index for both: a tab switch keeps it and only
//!   clamps it to the new tab's rows (`App::settle_selection`), so the same
//!   row number is selected on the other tab, or its last row when the list is
//!   shorter, and switching back does not restore the earlier row.
//! - The block ends with its `╹` foot, and the prompt's footer row below it
//!   (cwd, usage) is not drawn while the panel is open; the status dock under
//!   it still carries the session status.

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

/// The panel's cursor: which filter is listed and which row is selected
/// (upstream's in-memory `subagents-filter` and the store's `selected`). Both
/// outlive the panel, so reopening it keeps them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// List the running subagents (`true`, the default) or the finished ones.
    /// Exactly one of the two is ever listed.
    pub active: bool,
    /// The selected row among the listed ones.
    pub selected: usize,
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

impl Cursor {
    /// The default filter: running subagents only, on the first row.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: true,
            selected: 0,
        }
    }

    /// Keep the selection on a row the list still has: the family, the filter
    /// or the tab changed under it. A list with no rows has only row 0.
    #[must_use]
    pub fn clamp(self, len: usize) -> Self {
        Self {
            selected: self.selected.min(len.saturating_sub(1)),
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
            Up::Row(self.selected - 1)
        }
    }

    /// `↓` (`config/keybind.ts:250`): the row below, wrapping to the first. A
    /// list with no rows stays on row 0.
    #[must_use]
    pub fn down(self, len: usize) -> Self {
        let selected = if len == 0 {
            0
        } else {
            (self.selected + 1) % len
        };
        Self { selected, ..self }
    }

    /// `ctrl+a` (`composer.subagent.toggle-activity`): the other filter, the
    /// selection back on the first row and the window back at its start.
    #[must_use]
    pub fn toggle(self) -> Self {
        Self {
            active: !self.active,
            selected: 0,
        }
    }
}

/// Where `↑` sent the cursor: upstream closes the composer from the first row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Up {
    /// The panel closes and the composer comes back.
    Close,
    /// The selection moves to this row.
    Row(usize),
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

/// The block [`render`] draws: its rows, and which of them are marked.
#[derive(Debug, Default)]
pub struct Block {
    /// Header, body rows, one blank row, hints.
    pub lines: Vec<Line<'static>>,
    /// Index into `lines` of the selected row, when one is drawn. `None` when
    /// no row is listed.
    pub selected: Option<usize>,
    /// Index into `lines` of the row for the session on screen, when it is
    /// listed and inside the window (upstream `entry.current`).
    pub current: Option<usize>,
    /// Indices into `lines` of the rows that carry the right-aligned
    /// [`RUNNING`] status.
    pub running: Vec<usize>,
}

impl Block {
    /// Rows the block takes: its own rows plus the `╹` foot under them.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(1)
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
/// `interrupt ctrl+d` while the selected row runs (ox's `open enter` is the
/// ledger's key stand-in for a click), on `Shell` `output enter` and
/// `kill ctrl+d` while a row is selected (`config/keybind.ts:253-256`).
#[must_use]
pub fn hints(tab: Tab, active: bool, armed: bool) -> Line<'static> {
    let pal = *theme::palette();
    let toggle = if active {
        "show inactive"
    } else {
        "show active"
    };
    let hints: &[(&str, &str)] = match tab {
        Tab::Subagents if armed => &[
            ("open", "enter"),
            ("interrupt", "ctrl+d"),
            (toggle, "ctrl+a"),
            ("tabs", "←/→"),
        ],
        Tab::Subagents => &[("open", "enter"), (toggle, "ctrl+a"), ("tabs", "←/→")],
        Tab::Shell if armed => &[("output", "enter"), ("kill", "ctrl+d"), ("tabs", "←/→")],
        Tab::Shell => &[("tabs", "←/→")],
    };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(hints.len() * 3);
    for (index, &(title, key)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(title, pal.text));
        spans.push(Span::styled(format!(" {key}"), pal.dim));
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

/// The first listed row the window shows: the selection centred, clamped so the
/// window stays full at the end of the list (upstream `scrollToIndex(index,
/// true)` → `scrollTo(index - floor(viewport / 2))`, which its scrollbox then
/// clamps to the last full window).
#[must_use]
pub fn window_offset(selected: usize, len: usize) -> usize {
    selected
        .saturating_sub(VIEWPORT / 2)
        .min(len.saturating_sub(VIEWPORT))
}

/// The whole block interior: header, the window of rows, one blank row, hints.
/// `shells` is the `Shell` tab's `(id, command)` list (`DataStore::shells_of`).
/// `cursor.selected` indexes the shown tab's rows ([`rows`] or `shells`); it is
/// clamped, so a stale index draws the last row.
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
    let mut block = Block {
        lines: vec![header(tab)],
        ..Block::default()
    };
    if listed.is_empty() {
        let empty_state = if cursor.active {
            SUBAGENTS_EMPTY_ACTIVE
        } else {
            SUBAGENTS_EMPTY_INACTIVE
        };
        block.lines.push(empty(empty_state));
        block.lines.push(Line::default());
        block.lines.push(hints(tab, cursor.active, false));
        return block;
    }

    let selected = cursor.selected.min(listed.len() - 1);
    let offset = window_offset(selected, listed.len());
    block.selected = Some(1 + selected - offset);
    if let Some(position) = listed.iter().position(|row| row.member.id == current)
        && (offset..offset + VIEWPORT).contains(&position)
    {
        block.current = Some(1 + position - offset);
    }
    for (index, row) in listed.iter().skip(offset).take(VIEWPORT).enumerate() {
        let line = 1 + index;
        block
            .lines
            .push(listed_row(row, Some(line) == block.selected));
        if row.member.running {
            block.running.push(line);
        }
    }
    block.lines.push(Line::default());
    block
        .lines
        .push(hints(tab, cursor.active, listed[selected].member.running));
    block
}

/// The `Shell` tab's block: one row per running shell, the first line of its
/// command (upstream `shell-tab.tsx:139`), in the same window as the subagent
/// rows.
fn shell_lines(shells: &[(&str, &str)], cursor: Cursor) -> Block {
    let pal = *theme::palette();
    let mut block = Block {
        lines: vec![header(Tab::Shell)],
        ..Block::default()
    };
    if shells.is_empty() {
        block.lines.push(empty(SHELL_EMPTY));
        block.lines.push(Line::default());
        block.lines.push(hints(Tab::Shell, cursor.active, false));
        return block;
    }
    let selected = cursor.selected.min(shells.len() - 1);
    let offset = window_offset(selected, shells.len());
    block.selected = Some(1 + selected - offset);
    let focused = pal.menu_selected.add_modifier(Modifier::BOLD);
    for (index, &(_, command)) in shells.iter().skip(offset).take(VIEWPORT).enumerate() {
        let style = if Some(1 + index) == block.selected {
            focused
        } else {
            pal.text
        };
        let first = command.split('\n').next().unwrap_or_default();
        block
            .lines
            .push(Line::from(Span::styled(first.to_owned(), style)));
    }
    block.lines.push(Line::default());
    block.lines.push(hints(Tab::Shell, cursor.active, true));
    block
}

/// One listed row line: `{prefix}{agent}: {title}`, the focused one filled and
/// bold, every other one in body text (upstream draws one `<text>` either way).
fn listed_row(row: &Row<'_>, focused: bool) -> Line<'static> {
    let pal = *theme::palette();
    let (agent, title) = agent_and_title(row.member);
    let style = if focused {
        pal.menu_selected.add_modifier(Modifier::BOLD)
    } else {
        pal.text
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

/// Draw the block in the composer's place: the prompt's rail and panel fill
/// around `lines`, the current and the selected row filled, `Running` flush
/// right on the rows that carry one, `esc` flush right on the header row, then
/// the `╹` foot.
pub fn render(frame: &mut Frame, area: Rect, block: Block) {
    let Block {
        lines,
        selected,
        current,
        running,
    } = block;
    if area.width < 2 || area.height == 0 || lines.is_empty() {
        return;
    }
    let pal = *theme::palette();
    let box_h = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    {
        let buf = frame.buffer_mut();
        for y in area.y..area.y + box_h {
            for x in area.x + 1..area.right() {
                buf[(x, y)].set_style(pal.panel);
            }
            let cell = &mut buf[(area.x, y)];
            cell.set_style(pal.panel_border);
            cell.set_char(prompt::PANEL_BORDER_CHAR);
        }
    }
    // The current row is the quieter fill, the selected row the focused one, so
    // a row that is both reads as the selected one.
    for (index, fill) in [(current, pal.raised_base), (selected, pal.menu_selected)] {
        let Some(index) = index else {
            continue;
        };
        let y = area.y + u16::try_from(index).unwrap_or(u16::MAX);
        if y < area.y + box_h {
            frame.buffer_mut().set_style(
                Rect {
                    x: area.x + 1,
                    y,
                    width: area.width.saturating_sub(1),
                    height: 1,
                },
                fill,
            );
        }
    }
    let pad = if area.width < 44 { 1 } else { 2 };
    let inner = Rect {
        x: area.x + 1 + pad,
        y: area.y,
        width: area.width.saturating_sub(1 + pad * 2).max(1),
        height: box_h,
    };
    // The header row keeps `esc` where upstream puts it: flush right, on the
    // panel surface, outside the text column.
    let used = lines[0].width();
    let status_w = u16::try_from(RUNNING.len()).unwrap_or(u16::MAX);
    for (index, line) in lines.into_iter().take(box_h as usize).enumerate() {
        let y = area.y + u16::try_from(index).unwrap_or(u16::MAX);
        // A row with a status gives it the right edge and clips its own text
        // where the status starts (upstream `flexGrow={1} minWidth={0}`).
        let has_status = running.contains(&index) && inner.width > status_w;
        let width = if has_status {
            inner.width - status_w
        } else {
            inner.width
        };
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                y,
                width,
                height: 1,
                ..inner
            },
        );
        if has_status {
            let style = if Some(index) == selected {
                pal.menu_selected
            } else {
                pal.dim
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(RUNNING, style))),
                Rect {
                    x: inner.right() - status_w,
                    y,
                    width: status_w,
                    height: 1,
                },
            );
        }
    }
    if used + CLOSE_KEY.len() + 1 < inner.width as usize {
        let rect = Rect {
            x: area.right() - u16::try_from(CLOSE_KEY.len()).unwrap_or(0),
            y: area.y,
            width: u16::try_from(CLOSE_KEY.len()).unwrap_or(0),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(CLOSE_KEY, pal.dim))),
            rect,
        );
    }
    let edge_y = area.y + box_h;
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
}

fn empty(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_owned(), theme::palette().dim))
}
