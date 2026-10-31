use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_client::store::{CompactionStatus, CompletionSource, ToolState};
use ox_client::todo::Todo;
use ox_schema::support::TokenUsage;

use crate::theme;

use super::selection::{self, Behavior, ClickTracker, Outcome, Pos, Selection};

/// Stats on the line closing a finished turn (upstream `AssistantFooter`,
/// `routes/session/index.tsx:2000`).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnFooter {
    /// Agent that produced the turn (`build`); shown title-cased.
    pub agent: Option<String>,
    /// `providerID/id`.
    pub model: Option<String>,
    /// User prompt creation to the message's completion.
    pub duration_ms: Option<i64>,
    /// Output tokens over provider-active time, across the turn's steps.
    pub tokens_per_second: Option<f64>,
}

/// One tool call folded into an `Explored` group; what the expanded row lists
/// (upstream `group-view.tsx:129-157`, `children("tool")`). Fields mirror
/// `Block::Tool`.
#[derive(Debug, Clone)]
pub struct ExploredCall {
    pub id: String,
    /// `ToolPart::version` — equality is `(id, version)`.
    pub version: u64,
    pub tool: String,
    pub state: ToolState,
    pub summary: String,
}

/// Counts behind one `Explored: 1 read` row (upstream `group-view.tsx`) and
/// the calls they were counted from.
#[derive(Debug, Clone, Default)]
pub struct Explored {
    pub reads: u32,
    pub searches: u32,
    pub fetches: u32,
    /// Some grouped tool has not finished: `Exploring:` instead of `Explored:`.
    pub running: bool,
    /// Id of the group's first call: the key of its expansion state, stable
    /// while later calls join the group (upstream `groupID`).
    pub group_id: String,
    pub calls: Vec<ExploredCall>,
}

/// Identity comparison, like `Block`: counts, `running` and each call's
/// `(id, version)`; never summary text.
impl PartialEq for Explored {
    fn eq(&self, other: &Self) -> bool {
        self.reads == other.reads
            && self.searches == other.searches
            && self.fetches == other.fetches
            && self.running == other.running
            && self.group_id == other.group_id
            && self.calls.len() == other.calls.len()
            && self
                .calls
                .iter()
                .zip(&other.calls)
                .all(|(a, b)| a.id == b.id && a.version == b.version)
    }
}
impl Eq for Explored {}

/// Blank rows kept under the last block (`routes/session/index.tsx` scrollbox
/// content padding).
const BOTTOM_PAD: usize = 1;

#[derive(Debug, Clone)]
pub enum Block {
    User {
        text: std::sync::Arc<str>,
    },
    /// The reasoning header of one assistant message, collapsed by default.
    Reasoning {
        id: String,
        /// Store-owned version, as for `Assistant`.
        version: u64,
        text: std::sync::Arc<String>,
        done: bool,
        /// Reasoning duration when seen live (`reasoning.started` → `ended`).
        ms: Option<u64>,
    },
    /// The answer text of one assistant message and, once the turn has
    /// finished, its footer.
    Assistant {
        id: String,
        /// Store-owned version. Equality is `(id, version)`, never text.
        version: u64,
        /// `Arc<String>` mirrors `AssistantItem` so the store→block projection
        /// is a pointer clone — no per-delta deep copy of the growing text.
        text: std::sync::Arc<String>,
        footer: Option<TurnFooter>,
    },
    System {
        text: std::sync::Arc<str>,
    },
    /// Consecutive read/search/fetch tool calls, folded into one row.
    Explored(Explored),
    /// One inline tool call row (and its collapsed output).
    Tool {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)`.
        version: u64,
        tool: String,
        state: ToolState,
        summary: String,
        output: std::sync::Arc<String>,
    },
    /// A `todowrite` call: the `# Todos` panel once it completed with a
    /// non-empty list, an inline row otherwise (upstream `TodoWrite`).
    Todo {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)`.
        version: u64,
        state: ToolState,
        todos: std::sync::Arc<Vec<Todo>>,
    },
    /// A context-compaction boundary rule with its summary below.
    Compaction {
        id: String,
        /// `CompactionItem::version` — equality is `(id, version)`.
        version: u64,
        status: CompactionStatus,
        text: std::sync::Arc<String>,
        cancelled: bool,
        tokens: Option<TokenUsage>,
    },
    /// A shell / subagent completion notice (`↳ Shell finished · cmd`).
    /// Mirrors `ox_client::store::CompletionItem`; equality is by `id` alone
    /// because the item is immutable once built.
    Completion {
        id: String,
        source: CompletionSource,
        state: Option<std::sync::Arc<str>>,
        description: std::sync::Arc<str>,
        child_id: Option<std::sync::Arc<str>>,
    },
}

impl Block {
    pub fn user(text: &str) -> Self {
        Block::User {
            text: std::sync::Arc::from(text),
        }
    }
    pub fn assistant(id: &str, version: u64, text: &str) -> Self {
        Block::Assistant {
            id: id.to_owned(),
            version,
            text: std::sync::Arc::new(text.to_owned()),
            footer: None,
        }
    }
    pub fn reasoning(id: &str, version: u64, text: &str, done: bool) -> Self {
        Block::Reasoning {
            id: id.to_owned(),
            version,
            text: std::sync::Arc::new(text.to_owned()),
            done,
            ms: None,
        }
    }
    pub fn system(text: &str) -> Self {
        Block::System {
            text: std::sync::Arc::from(text),
        }
    }
}

/// O(1) identity comparison: text is shared via `Arc`, store-backed blocks
/// compare `(id, version)`. Deep string comparison here would defeat the cache
/// it feeds.
impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Block::User { text: a }, Block::User { text: b }) => std::sync::Arc::ptr_eq(a, b),
            (
                Block::Reasoning {
                    id: aid,
                    version: aversion,
                    done: adone,
                    ..
                },
                Block::Reasoning {
                    id: bid,
                    version: bversion,
                    done: bdone,
                    ..
                },
            ) => aid == bid && aversion == bversion && adone == bdone,
            (
                Block::Assistant {
                    id: aid,
                    version: aversion,
                    footer: afooter,
                    ..
                },
                Block::Assistant {
                    id: bid,
                    version: bversion,
                    footer: bfooter,
                    ..
                },
            ) => aid == bid && aversion == bversion && afooter == bfooter,
            (Block::System { text: a }, Block::System { text: b }) => std::sync::Arc::ptr_eq(a, b),
            (Block::Explored(a), Block::Explored(b)) => a == b,
            (
                Block::Tool {
                    id: aid,
                    version: aversion,
                    ..
                },
                Block::Tool {
                    id: bid,
                    version: bversion,
                    ..
                },
            ) => aid == bid && aversion == bversion,
            (
                Block::Todo {
                    id: aid,
                    version: aversion,
                    ..
                },
                Block::Todo {
                    id: bid,
                    version: bversion,
                    ..
                },
            )
            | (
                Block::Compaction {
                    id: aid,
                    version: aversion,
                    ..
                },
                Block::Compaction {
                    id: bid,
                    version: bversion,
                    ..
                },
            ) => aid == bid && aversion == bversion,
            (Block::Completion { id: a, .. }, Block::Completion { id: b, .. }) => a == b,
            _ => false,
        }
    }
}
impl Eq for Block {}

/// A tool row expands to at most this many output lines: a chatty
/// `cargo build` must not turn one row into ten thousand wrapped lines.
pub(crate) const TOOL_OUTPUT_MAX_LINES: usize = 200;

/// Everything the renderer needs beyond the `Block` itself. `Copy`, so `sync`
/// can compute it per block without allocating.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ViewState {
    /// Assistant blocks: the reasoning body is expanded (`r`).
    reasoning_expanded: bool,
    /// Tool blocks: the output body is expanded (`o`).
    tool_expanded: bool,
    /// Tool blocks: the row is the current focus (`[` / `]`).
    tool_focused: bool,
    /// Explored blocks: the per-call tool rows are listed under the title.
    explored_expanded: bool,
    /// The pointer is over this Explored title, Reasoning header, User block
    /// or the title row of a Completion notice that links to a child session.
    hovered: bool,
    /// Reasoning blocks still streaming: the header spinner frame.
    spin: usize,
}

struct Entry {
    block: Block,
    generation: u64,
    lines: Vec<Line<'static>>,
    width: u16,
    view: ViewState,
}

impl Entry {
    fn new(block: Block, generation: u64, width: u16, view: ViewState) -> Self {
        let lines = render_block(&block, width, view);
        Self {
            block,
            generation,
            lines,
            width,
            view,
        }
    }

    fn lines_for(&mut self, width: u16) -> &[Line<'static>] {
        if self.width != width {
            self.lines = render_block(&self.block, width, self.view);
            self.width = width;
        }
        &self.lines
    }
}

pub struct Transcript {
    entries: Vec<Entry>,
    scroll: usize,
    pub sticky_bottom: bool,
    width: u16,
    height: u16,
    search: SearchState,
    /// Done messages whose reasoning body is expanded (`r`).
    expanded: std::collections::HashSet<String>,
    /// Tool parts whose output is expanded (`o`). Keyed by part id, so a
    /// streaming `version` bump keeps the row open.
    tool_expanded: std::collections::HashSet<String>,
    /// Explored groups whose calls are listed, keyed by `Explored::group_id`.
    explored_expanded: std::collections::HashSet<String>,
    /// Entry under the pointer, if it is a hover target (see `hover_target`).
    hovered: Option<usize>,
    /// Drag selection, in absolute line / display column coordinates so it
    /// stays on its content while the view scrolls. Text is read from
    /// `entries` only when it is copied.
    selection: Option<Selection>,
    /// Multi-click history. Lives here because the entry under the pointer is
    /// part of the rule, and `clear_selection` forgets it like upstream's
    /// `clearSelection`.
    clicks: ClickTracker,
    /// Last pointer cell seen by `press`/`drag`, so a wheel scroll can move
    /// the focus under a stationary pointer.
    pointer: Option<(u16, u16)>,
    /// The tool row the expansion keys act on (`[` / `]`).
    focused_tool: Option<String>,
    /// Header spinner of a streaming reasoning block.
    spin_frame: usize,
    spin_epoch: std::time::Instant,
    /// Where the last `render` drew the transcript; the click target map.
    last_area: Rect,
}

/// fx-style search: no cursor row — the viewport jumps to the current match and
/// the line is highlighted for as long as it is the current one.
#[derive(Default, Debug, Clone)]
pub struct SearchState {
    pub query: String,
    /// Absolute line indices into the flattened, wrapped transcript.
    pub matches: Vec<usize>,
    pub current: Option<usize>,
}

impl Transcript {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            entries: Vec::new(),
            scroll: 0,
            sticky_bottom: true,
            width,
            height,
            search: SearchState::default(),
            expanded: std::collections::HashSet::new(),
            tool_expanded: std::collections::HashSet::new(),
            explored_expanded: std::collections::HashSet::new(),
            hovered: None,
            selection: None,
            clicks: ClickTracker::default(),
            pointer: None,
            focused_tool: None,
            spin_frame: 0,
            spin_epoch: std::time::Instant::now(),
            last_area: Rect::default(),
        }
    }

    /// Expand/collapse the body of the last reasoning block. Returns whether
    /// anything changed.
    pub fn toggle_last_reasoning(&mut self) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.block, Block::Reasoning { .. }))
        else {
            return false;
        };
        self.toggle_reasoning_at(index);
        true
    }

    fn toggle_reasoning_at(&mut self, index: usize) {
        let Block::Reasoning { id, .. } = &self.entries[index].block else {
            return;
        };
        let id = id.clone();
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rebuild(index);
    }

    /// Map screen cell (`x`, `y`) to `(entry index, row within the block)`.
    /// `None` outside the last drawn transcript area or below the content.
    fn locate(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self.last_area;
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return None;
        }
        let mut line = self.scroll + usize::from(y - area.y);
        for (index, entry) in self.entries.iter().enumerate() {
            let count = entry.lines.len();
            if line < count {
                return Some((index, line));
            }
            line -= count;
        }
        None
    }

    /// A left click at screen cell (`x`, `y`): on a reasoning header it opens
    /// or closes the body (upstream `ReasoningPart` `onMouseUp`); on an
    /// Explored title it lists or hides the group's calls (upstream
    /// `group-view.tsx:267-284`). Returns whether anything changed.
    pub fn click(&mut self, x: u16, y: u16) -> bool {
        // Row 0 is the block's blank margin; row 1 is the header.
        let Some((index, 1)) = self.locate(x, y) else {
            return false;
        };
        match &self.entries[index].block {
            Block::Reasoning { .. } => self.toggle_reasoning_at(index),
            Block::Explored(explored) => {
                let id = explored.group_id.clone();
                if !self.explored_expanded.remove(&id) {
                    self.explored_expanded.insert(id);
                }
                self.rebuild(index);
            }
            Block::User { .. }
            | Block::Assistant { .. }
            | Block::System { .. }
            | Block::Tool { .. }
            | Block::Todo { .. }
            | Block::Compaction { .. }
            | Block::Completion { .. } => return false,
        }
        true
    }

    /// The child session a click at screen cell (`x`, `y`) opens: the title
    /// row of a Completion notice that carries a `child_id` (upstream
    /// `routes/session/index.tsx:2090-2151`). `None` anywhere else.
    pub fn child_at(&self, x: u16, y: u16) -> Option<&str> {
        // Row 0 is the block's blank margin; row 1 is the title.
        let (index, 1) = self.locate(x, y)? else {
            return None;
        };
        match &self.entries[index].block {
            Block::Completion { child_id, .. } => child_id.as_deref(),
            _ => None,
        }
    }

    /// The entry a pointer at (`x`, `y`) lights up: an Explored or Reasoning
    /// title row, a Completion title row that links to a child session, or
    /// anywhere in a User block (upstream `group-view.tsx:146, 166-177` and
    /// `routes/session/index.tsx:2127-2128, 2379-2383`).
    fn hover_target(&self, x: u16, y: u16) -> Option<usize> {
        let (index, row) = self.locate(x, y)?;
        match &self.entries[index].block {
            Block::Explored(_) | Block::Reasoning { .. } => (row == 1).then_some(index),
            Block::User { .. } => Some(index),
            Block::Completion { child_id, .. } => (row == 1 && child_id.is_some()).then_some(index),
            Block::Assistant { .. }
            | Block::System { .. }
            | Block::Tool { .. }
            | Block::Todo { .. }
            | Block::Compaction { .. } => None,
        }
    }

    /// Point the hover at `target`; only the old and new entries are rebuilt.
    /// Returns whether it moved, i.e. whether a repaint is due.
    fn set_hover(&mut self, target: Option<usize>) -> bool {
        if self.hovered == target {
            return false;
        }
        let previous = std::mem::replace(&mut self.hovered, target);
        for index in [previous, target].into_iter().flatten() {
            if index < self.entries.len() {
                self.rebuild(index);
            }
        }
        true
    }

    /// The pointer moved to screen cell (`x`, `y`). Returns whether the
    /// hovered entry changed.
    pub fn hover(&mut self, x: u16, y: u16) -> bool {
        self.pointer = Some((x, y));
        let target = self.hover_target(x, y);
        self.set_hover(target)
    }

    /// Re-evaluate the hover at the last pointer cell: a wheel scroll moves
    /// the content under a stationary pointer.
    pub fn rehover(&mut self) -> bool {
        match self.pointer {
            Some((x, y)) => self.hover(x, y),
            None => false,
        }
    }

    /// The pointer left the transcript (or a modal took over).
    pub fn clear_hover(&mut self) -> bool {
        self.set_hover(None)
    }

    /// Whether a non-empty range is selected (and therefore drawn).
    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn selection_focus(&self) -> Option<Pos> {
        self.selection.map(|s| s.focus)
    }

    /// Drop the selection. Returns whether a visible range went away.
    pub(crate) fn clear_selection(&mut self) -> bool {
        let visible = self.has_selection();
        self.selection = None;
        self.clicks.reset();
        visible
    }

    /// Left button down at (`x`, `y`) at time `now`: forget the old selection
    /// and, when the cell holds content, start a new one there. `clicks`
    /// counts the press: the second selects a word, the third a line. Returns
    /// whether a visible selection was dropped (a repaint is due).
    pub(crate) fn press(&mut self, x: u16, y: u16, now: Instant) -> bool {
        let dropped = self.has_selection();
        self.pointer = Some((x, y));
        let area = self.last_area;
        let located = self.locate(x, y);
        self.selection = located.map(|(entry, _)| {
            let cell = Pos {
                line: usize::from(y),
                col: x,
            };
            let behavior = Behavior::from_clicks(self.clicks.register(now, cell, entry));
            let pos = Pos {
                line: self.scroll + usize::from(y - area.y),
                col: x - area.x,
            };
            Selection::start(pos, behavior, &|line| selectable_text(&self.entries, line))
        });
        dropped
    }

    /// Pointer moved with the button down. Returns whether the focus moved.
    pub(crate) fn drag(&mut self, x: u16, y: u16) -> bool {
        self.pointer = Some((x, y));
        self.refocus_selection()
    }

    /// Re-aim the focus of a drag in progress at the last pointer cell: the
    /// content under a stationary pointer changes when the wheel scrolls
    /// (upstream `requestSelectionUpdate`). Returns whether the focus moved.
    pub(crate) fn refocus_selection(&mut self) -> bool {
        let Some((x, y)) = self.pointer else {
            return false;
        };
        let Some(focus) = self.focus_at(x, y) else {
            return false;
        };
        match &mut self.selection {
            Some(sel) if sel.dragging && sel.focus != focus => {
                sel.drag(focus, &|line| selectable_text(&self.entries, line));
                true
            }
            _ => false,
        }
    }

    /// Left button up: end the drag and say what it meant. A copy keeps its
    /// highlight; a click or an empty drag drops the selection.
    pub(crate) fn release(&mut self) -> Outcome {
        let Some(mut sel) = self.selection.take() else {
            return Outcome::Nothing;
        };
        let outcome = sel.release(|s| selection::extract(text_blocks(&self.entries), s));
        if matches!(outcome, Outcome::Copy(_)) {
            self.selection = Some(sel);
        }
        outcome
    }

    /// The selection cell a pointer at (`x`, `y`) maps to, clamped to the
    /// content: above the area is the first visible line's start, below the
    /// area or the content is the last line's end (upstream clamps the focus
    /// to the text head/tail, `text-buffer-view.zig:1104-1150`).
    fn focus_at(&self, x: u16, y: u16) -> Option<Pos> {
        let area = self.last_area;
        let total = self.total_lines();
        if total == 0 || area.width == 0 || area.height == 0 {
            return None;
        }
        let last = total - 1;
        if y < area.y {
            return Some(Pos {
                line: self.scroll.min(last),
                col: 0,
            });
        }
        let below = y >= area.y + area.height;
        let row = if below { area.height - 1 } else { y - area.y };
        let line = self.scroll + usize::from(row);
        if below || line > last {
            return Some(Pos {
                line: line.min(last),
                col: u16::MAX,
            });
        }
        Some(Pos {
            line,
            col: x.saturating_sub(area.x).min(area.width - 1),
        })
    }

    /// Lines before entry `index`.
    fn first_line_of(&self, index: usize) -> usize {
        self.entries[..index]
            .iter()
            .map(|entry| entry.lines.len())
            .sum()
    }

    /// Advance the header spinner of streaming reasoning blocks. `true` when a
    /// visible frame changed and a repaint is due.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        let streaming: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry.block, Block::Reasoning { done: false, .. }))
            .map(|(index, _)| index)
            .collect();
        if streaming.is_empty() {
            return false;
        }
        let elapsed = now.saturating_duration_since(self.spin_epoch).as_millis();
        let frame = (elapsed / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        if frame == self.spin_frame {
            return false;
        }
        self.spin_frame = frame;
        for index in streaming {
            self.rebuild(index);
        }
        true
    }

    /// Indices of tool rows whose output can be shown: finished rows with
    /// output. Only these are focusable — a header with nothing behind it
    /// would make `o` a no-op the user cannot see.
    fn expandable_tools(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry_tool_output(&entry.block).is_some())
            .map(|(index, _)| index)
            .collect()
    }

    /// Move the tool focus by one row, wrapping. With nothing focused the
    /// first row takes it going forward, the last going backward.
    pub fn focus_tool(&mut self, forward: bool) -> bool {
        let tools = self.expandable_tools();
        if tools.is_empty() {
            return false;
        }
        let previous = self.focused_tool.as_deref().and_then(|id| {
            tools
                .iter()
                .position(|&index| entry_tool_id(&self.entries[index].block) == Some(id))
        });
        let next = match previous {
            Some(position) if forward => (position + 1) % tools.len(),
            Some(position) => (position + tools.len() - 1) % tools.len(),
            None if forward => 0,
            None => tools.len() - 1,
        };
        let index = tools[next];
        self.focused_tool = entry_tool_id(&self.entries[index].block).map(str::to_owned);
        if let Some(previous) = previous {
            self.rebuild(tools[previous]);
        }
        self.rebuild(index);
        self.reveal(index);
        true
    }

    /// Expand/collapse the focused tool row's output. With nothing focused it
    /// opens the last finished tool row — the one the user just watched run.
    pub fn toggle_tool_output(&mut self) -> bool {
        let tools = self.expandable_tools();
        if tools.is_empty() {
            return false;
        }
        let focused = self.focused_tool.as_deref().and_then(|id| {
            tools
                .iter()
                .copied()
                .find(|&index| entry_tool_id(&self.entries[index].block) == Some(id))
        });
        let index = focused.unwrap_or(tools[tools.len() - 1]);
        let Some(id) = entry_tool_id(&self.entries[index].block).map(str::to_owned) else {
            return false;
        };
        let expanded = !self.tool_expanded.remove(&id);
        if expanded {
            self.tool_expanded.insert(id.clone());
        }
        self.focused_tool = Some(id);
        self.rebuild(index);
        self.reveal(index);
        true
    }

    /// The view flags for one block, derived from the transcript's open sets.
    fn view_of(&self, index: usize, block: &Block) -> ViewState {
        let hovered = self.hovered == Some(index);
        match block {
            Block::Reasoning { id, done, .. } => ViewState {
                reasoning_expanded: self.expanded.contains(id),
                hovered,
                spin: if *done { 0 } else { self.spin_frame },
                ..Default::default()
            },
            Block::Tool { id, output, .. } => ViewState {
                tool_expanded: !output.is_empty() && self.tool_expanded.contains(id),
                tool_focused: self.focused_tool.as_deref() == Some(id.as_str()),
                ..Default::default()
            },
            Block::Explored(explored) => ViewState {
                explored_expanded: self.explored_expanded.contains(&explored.group_id),
                hovered,
                ..Default::default()
            },
            Block::User { .. } => ViewState {
                hovered,
                ..Default::default()
            },
            Block::Completion { .. } => ViewState {
                hovered,
                ..Default::default()
            },
            Block::Assistant { .. }
            | Block::System { .. }
            | Block::Todo { .. }
            | Block::Compaction { .. } => ViewState::default(),
        }
    }

    fn rebuild(&mut self, index: usize) {
        let block = self.entries[index].block.clone();
        let view = self.view_of(index, &block);
        let generation = self.entries[index].generation + 1;
        let rows = self.entries[index].lines.len();
        self.entries[index] = Entry::new(block, generation, self.width, view);
        if let Some(sel) = self.selection
            && self.entries[index].lines.len() != rows
            && self.first_line_of(index) <= sel.ordered().1.line
        {
            self.selection = None;
        }
    }

    /// Scroll the row into view: a row focused while off screen would make the
    /// key look broken.
    fn reveal(&mut self, index: usize) {
        let line = self.entries[..index]
            .iter()
            .map(|entry| entry.lines.len())
            .sum();
        self.jump_to(line);
    }

    /// Diff incoming blocks against current entries. `changed_from` comes from
    /// `DataStore::claim_changed_from()`: everything before it is never even
    /// compared. Changed blocks get a new generation; unchanged keep both cache
    /// and generation.
    pub fn sync(&mut self, blocks: &[Block], changed_from: usize) {
        let start = changed_from.min(self.entries.len()).min(blocks.len());

        let end = self.entries.len().min(blocks.len());
        let views: Vec<ViewState> = blocks[start..end]
            .iter()
            .enumerate()
            .map(|(offset, block)| self.view_of(start + offset, block))
            .collect();
        // A selection is anchored to absolute line numbers, so it survives
        // only while no entry at or above its last line changes its row count.
        let selected_to = self.selection.map(|sel| sel.ordered().1.line);
        let mut base = if selected_to.is_some() {
            self.first_line_of(start)
        } else {
            0
        };
        let mut stale = false;
        for ((entry, block), view) in self.entries[start..end]
            .iter_mut()
            .zip(&blocks[start..end])
            .zip(views)
        {
            let rows = entry.lines.len();
            if entry.block != *block || entry.view != view {
                let generation = entry.generation + 1;
                *entry = Entry::new(block.clone(), generation, self.width, view);
            }
            if let Some(last) = selected_to {
                stale |= entry.lines.len() != rows && base <= last;
                base += entry.lines.len();
            }
        }

        if blocks.len() > self.entries.len() {
            let appended = self.entries.len();
            let views: Vec<ViewState> = blocks[appended..]
                .iter()
                .enumerate()
                .map(|(offset, block)| self.view_of(appended + offset, block))
                .collect();
            for (block, view) in blocks[appended..].iter().zip(views) {
                self.entries
                    .push(Entry::new(block.clone(), 0, self.width, view));
            }
        }

        if let Some(last) = selected_to
            && blocks.len() < self.entries.len()
        {
            stale |= self.first_line_of(blocks.len()) <= last;
        }
        if stale {
            self.selection = None;
        }
        self.entries.truncate(blocks.len());
        if self
            .hovered
            .is_some_and(|index| index >= self.entries.len())
        {
            self.hovered = None;
        }
        if !self.search.query.is_empty() {
            self.recompute_search();
        }
        self.clamp();
    }

    /// Re-run the query over the wrapped lines and jump to the first hit.
    /// Returns the number of matches.
    pub fn set_query(&mut self, query: &str) -> usize {
        self.search.query = query.to_owned();
        self.recompute_search();
        self.search.current = (!self.search.matches.is_empty()).then_some(0);
        if let Some(&line) = self.search.matches.first() {
            self.jump_to(line);
        }
        self.search.matches.len()
    }

    pub fn clear_search(&mut self) {
        self.search = SearchState::default();
        self.sticky_bottom = true;
        self.scroll = self.max_scroll();
    }

    pub fn search(&self) -> &SearchState {
        &self.search
    }

    /// Move to the next/previous match; wraps around. Returns whether the
    /// viewport moved.
    pub fn next_match(&mut self, forward: bool) -> bool {
        let count = self.search.matches.len();
        if count == 0 {
            return false;
        }
        let index = self.search.current.unwrap_or(0);
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        self.search.current = Some(next);
        self.jump_to(self.search.matches[next]);
        true
    }

    /// The absolute line index currently highlighted, if any.
    fn current_line(&self) -> Option<usize> {
        self.search
            .current
            .and_then(|ordinal| self.search.matches.get(ordinal))
            .copied()
    }

    fn recompute_search(&mut self) {
        let needle = self.search.query.to_lowercase();
        let mut matches = Vec::new();
        let mut base = 0;
        for entry in &self.entries {
            for (offset, line) in entry.lines.iter().enumerate() {
                if line_text(line).to_lowercase().contains(&needle) {
                    matches.push(base + offset);
                }
            }
            base += entry.lines.len();
        }
        // Keep the ordinal pointing at a real match; a stale one would panic the
        // render path or silently highlight the wrong line.
        self.search.current = match (matches.len(), self.search.current) {
            (0, _) => None,
            (len, Some(ordinal)) => Some(ordinal.min(len - 1)),
            (_, None) => Some(0),
        };
        self.search.matches = matches;
    }

    fn jump_to(&mut self, line: usize) {
        self.sticky_bottom = false;
        let centered = line.saturating_sub(self.height as usize / 2);
        self.scroll = centered.min(self.max_scroll());
    }

    /// The user messages as `(entry index, text)`, oldest first.
    pub fn user_messages(&self) -> Vec<(usize, &str)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match &entry.block {
                Block::User { text } => Some((index, &**text)),
                _ => None,
            })
            .collect()
    }

    /// Scroll so entry `index` is centred. `false` when it no longer exists.
    pub fn jump_to_entry(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        let line = self.entries[..index].iter().map(|e| e.lines.len()).sum();
        self.jump_to(line);
        true
    }

    pub fn entry_generation(&self, index: usize) -> u64 {
        self.entries[index].generation
    }

    pub fn total_lines(&self) -> usize {
        self.entries.iter().map(|entry| entry.lines.len()).sum()
    }

    pub fn set_viewport(&mut self, width: u16, height: u16) {
        self.height = height;
        if self.width != width {
            self.width = width;
            // Re-wrapping renumbers lines, so a selection no longer points at
            // what it selected.
            self.selection = None;
            // Re-wrap eagerly: `search.matches` holds absolute wrapped-line
            // numbers, and a width change renumbers every line after the first
            // one that now wraps differently. Costs nothing on the frames that
            // keep the width (the guard above), and makes `render`'s own
            // `lines_for` calls no-ops for this pass.
            for entry in &mut self.entries {
                entry.lines_for(width);
            }
            if !self.search.query.is_empty() {
                self.recompute_search();
            }
        }
        self.clamp();
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
        self.sticky_bottom = false;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let max = self.max_scroll();
        self.scroll = (self.scroll + lines).min(max);
        self.sticky_bottom = self.scroll >= max;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.sticky_bottom = true;
        self.scroll = self.max_scroll();
    }

    pub fn scroll_to_top(&mut self) {
        self.sticky_bottom = false;
        self.scroll = 0;
    }

    /// The scroll extent includes one blank row under the last block: when the
    /// content overflows, the gold grids keep that row visible at the bottom.
    fn max_scroll(&self) -> usize {
        (self.total_lines() + BOTTOM_PAD).saturating_sub(self.height as usize)
    }

    fn clamp(&mut self) {
        if self.sticky_bottom {
            self.scroll = self.max_scroll();
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.last_area = area;
        self.set_viewport(area.width, area.height);
        self.clamp();

        let mut remaining_skip = self.scroll;
        let mut y = area.y;
        let mut base = 0usize;
        let current_match = self.current_line();

        let selection = self.selection.filter(|s| !s.is_empty());
        for entry in &mut self.entries {
            let (head, tail) = text_rows(&entry.block);
            let lines = entry.lines_for(area.width);
            let count = lines.len();

            if remaining_skip >= count {
                remaining_skip -= count;
                base += count;
                continue;
            }

            let end = (area.y + area.height).saturating_sub(y) as usize;
            let skip = remaining_skip;
            let visible = &lines[skip..lines.len().min(skip + end)];
            remaining_skip = 0;
            if visible.is_empty() {
                break;
            }

            let height = visible.len() as u16;
            let rect = Rect {
                x: area.x,
                y,
                width: area.width,
                height,
            };
            // ratatui 0.29's `Into<Text<'a>>` has no impl for `&[Line]`, so this clones
            // only the visible window's lines (≤ viewport height) each frame.
            let mut visible = visible.to_vec();
            // Selected columns per visible row, found before `visible` moves
            // into the widget: `(row offset, from, to)`.
            let mut marks: Vec<(u16, u16, u16)> = Vec::new();
            if let Some(sel) = &selection {
                for (offset, line) in visible.iter().enumerate() {
                    let row = skip + offset;
                    if row < head || row >= count.saturating_sub(tail) {
                        continue;
                    }
                    if let Some((from, to)) = selection::highlight_range(line, sel, base + row) {
                        marks.push((offset as u16, from, to));
                    }
                }
            }
            if let Some(matched) = current_match {
                let first = base + skip;
                if matched >= first && matched < first + visible.len() {
                    let index = matched - first;
                    visible[index] = highlight(&visible[index]);
                }
            }
            frame.render_widget(Paragraph::new(visible), rect);
            for (offset, from, to) in marks {
                let cells = Rect {
                    x: area.x + from,
                    y: y + offset,
                    width: to - from + 1,
                    height: 1,
                }
                .intersection(rect);
                frame
                    .buffer_mut()
                    .set_style(cells, theme::palette().highlight);
            }
            y += height;
            base += count;
            if y >= area.y + area.height {
                break;
            }
        }
    }
}

/// Rows of an entry that are layout and not text: the blank margin above every
/// block, and the inner padding above and below a user message.
fn text_rows(block: &Block) -> (usize, usize) {
    match block {
        Block::User { .. } => (2, 1),
        Block::Reasoning { .. }
        | Block::Assistant { .. }
        | Block::System { .. }
        | Block::Explored(_)
        | Block::Tool { .. }
        | Block::Todo { .. }
        | Block::Compaction { .. }
        | Block::Completion { .. } => (1, 0),
    }
}

/// The text of absolute transcript line `abs`; `None` past the end and on
/// rows that are layout (margins, user padding).
fn selectable_text(entries: &[Entry], abs: usize) -> Option<String> {
    let mut first = 0usize;
    for entry in entries {
        let count = entry.lines.len();
        if abs < first + count {
            let (head, tail) = text_rows(&entry.block);
            let row = abs - first;
            if row < head || row >= count.saturating_sub(tail) {
                return None;
            }
            return Some(
                entry.lines[row]
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect(),
            );
        }
        first += count;
    }
    None
}

/// The entries as selectable text blocks, in line order.
fn text_blocks(entries: &[Entry]) -> impl Iterator<Item = selection::Block<'_>> {
    entries.iter().scan(0usize, |first, entry| {
        let (skip_head, skip_tail) = text_rows(&entry.block);
        let block = selection::Block {
            first: *first,
            lines: &entry.lines,
            skip_head,
            skip_tail,
        };
        *first += entry.lines.len();
        Some(block)
    })
}

/// Paint a whole line with the search highlight: the palette decides the style,
/// so `NO_COLOR` degrades it to reverse video.
fn highlight(line: &Line<'static>) -> Line<'static> {
    let style = theme::palette().highlight;
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|span| Span::styled(span.content.clone(), span.style.patch(style)))
        .collect();
    Line {
        spans,
        style: line.style,
        alignment: line.alignment,
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Every block is one upstream row: `marginTop={1}` puts a blank line above it
/// (`routes/session/index.tsx:1670`). A block with nothing to show draws
/// nothing, not a stray blank line.
fn render_block(block: &Block, width: u16, view: ViewState) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let body = match block {
        Block::User { text } => user_block(text, view, width),
        Block::Reasoning { text, done, ms, .. } => reasoning_block(text, *done, *ms, view, width),
        Block::Assistant { text, footer, .. } => assistant_block(text, footer.as_ref(), width),
        Block::System { text } => indent_wrapped(&format!("⤷ {text}"), width, 3)
            .into_iter()
            .map(|line| line.style(palette.dim))
            .collect(),
        Block::Explored(explored) => explored_block(explored, view),
        Block::Tool {
            tool,
            state,
            summary,
            output,
            ..
        } => tool_block(block, tool, *state, summary, output, view, width),
        Block::Todo { state, todos, .. } => todo_block(*state, todos, width),
        Block::Compaction {
            status,
            text,
            cancelled,
            tokens,
            ..
        } => super::compaction::lines(
            &super::compaction::View {
                status: *status,
                text,
                cancelled: *cancelled,
                tokens: tokens.as_ref(),
            },
            width,
        ),
        Block::Completion {
            source,
            state,
            description,
            ..
        } => vec![completion_line(
            *source,
            state.as_deref(),
            description,
            usize::from(width),
            view.hovered,
            palette,
        )],
    };
    if body.is_empty() {
        return body;
    }
    let mut lines = Vec::with_capacity(body.len() + 1);
    lines.push(Line::default());
    lines.extend(body);
    lines
}

/// Columns the notice row is indented, like `Block::System`.
const COMPLETION_INDENT: usize = 3;

/// One completion notice row: `   ↳ Shell finished · description`.
///
/// The icon is `↳` for `completed` and `!` for every other state. Only the
/// title carries a status colour (`error` → error, `cancelled` → warning,
/// anything else → info, or the base text colour while `hovered`; error and
/// cancelled win over the hover, as upstream); the ` · description` suffix is always muted. The
/// suffix is cut by display width to fit `width` and ends with `…`; when even
/// ` · …` does not fit it is dropped, so the row never exceeds `width` unless
/// the title alone does.
fn completion_line(
    source: CompletionSource,
    state: Option<&str>,
    description: &str,
    width: usize,
    hovered: bool,
    palette: &theme::Palette,
) -> Line<'static> {
    use std::borrow::Cow;

    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let icon = if state == Some("completed") {
        '↳'
    } else {
        '!'
    };
    let actor = match source {
        CompletionSource::Shell => "Shell",
        CompletionSource::Subagent => "Subagent",
    };
    let word = match state {
        Some("completed") | None => "finished",
        Some("error") => "failed",
        Some(other) => other,
    };
    let title_style = match state {
        Some("error") => palette.warn,
        Some("cancelled") => palette.warning,
        _ if hovered => palette.text,
        _ => palette.info,
    };
    let title = format!("{:COMPLETION_INDENT$}{icon} {actor} {word}", "");
    let budget = width.saturating_sub(title.width());

    const SEPARATOR: &str = " · ";
    // Control characters (`\n`, `\t`, …) are dropped by the terminal draw and
    // would corrupt copied text, so each one renders as a single space.
    let description: Cow<'_, str> = if description.chars().any(char::is_control) {
        Cow::Owned(
            description
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect(),
        )
    } else {
        Cow::Borrowed(description)
    };
    let cell_width = |ch: char| ch.width().unwrap_or(0);
    let description_cells: usize = description.chars().map(cell_width).sum();
    let separator_cells = SEPARATOR.chars().map(cell_width).sum::<usize>();
    let suffix = if separator_cells + description_cells <= budget {
        Some(format!("{SEPARATOR}{description}"))
    } else if budget > separator_cells {
        // Leave one cell for the ellipsis.
        let mut room = budget - separator_cells - 1;
        let mut cut = String::from(SEPARATOR);
        for ch in description.chars() {
            let cells = cell_width(ch);
            if cells > room {
                break;
            }
            room -= cells;
            cut.push(ch);
        }
        cut.push('…');
        Some(cut)
    } else {
        None
    };

    let mut spans = vec![Span::styled(title, title_style)];
    spans.extend(suffix.map(|suffix| Span::styled(suffix, palette.dim)));
    Line::from(spans)
}

/// Upstream `braille` spinner (`component/spinner-frames.ts`), 80ms a frame.
const SPINNER_FRAMES: [char; 10] = [
    '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280f}',
];
const SPINNER_INTERVAL_MS: u128 = 80;

/// Upstream `reasoningSummary` (`context/thinking.ts`): a leading `**Title**`
/// block is the header's title, the rest is the body.
fn reasoning_summary(text: &str) -> (Option<&str>, &str) {
    let content = text.trim();
    let Some(rest) = content.strip_prefix("**") else {
        return (None, content);
    };
    let Some(end) = rest.find("**") else {
        return (None, content);
    };
    let title = &rest[..end];
    let after = &rest[end + 2..];
    let body = if after.is_empty() {
        Some("")
    } else {
        after
            .strip_prefix("\n\n")
            .or_else(|| after.strip_prefix("\r\n\r\n"))
    };
    match body {
        Some(body) if !title.is_empty() && !title.contains('\n') => {
            (Some(title.trim()), body.trim_end())
        }
        _ => (None, content),
    }
}

/// Header colour: full `warning` when open, or when a finished header is
/// hovered (upstream `group-view.tsx:166-177`); the 60% `thought` tint
/// otherwise.
fn thought_header_style(
    palette: &theme::Palette,
    expanded: bool,
    done: bool,
    hovered: bool,
) -> ratatui::style::Style {
    if expanded || (done && hovered) {
        palette.warning
    } else {
        palette.thought
    }
}

/// Upstream `ReasoningPart` / `ReasoningHeader` (`message-parts.tsx`). The
/// header is `+ Thought` collapsed, `- Thought` open, and a spinner with
/// `Thinking` while the reasoning streams. An open block shows its text on a
/// `┃` rail one blank row below the header. Click or `r` toggles it.
///
/// Observed in upstream 2.2.3 (`gold`): the open header is the plain warning
/// colour and the collapsed one is warning tinted 60% over the background.
/// Deviation: the body is plain wrapped text, not rendered markdown.
fn reasoning_block(
    text: &str,
    done: bool,
    ms: Option<u64>,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let palette = theme::palette();
    if text.trim().is_empty() {
        return Vec::new();
    }
    let expanded = view.reasoning_expanded;
    let (title, body) = reasoning_summary(text);
    let style = thought_header_style(palette, expanded, done, view.hovered);
    let header = if done {
        let duration = ms.map(|ms| format_duration(ms as i64));
        let mut header = format!("   {} Thought", if expanded { '-' } else { '+' });
        match (title, duration) {
            (Some(title), Some(duration)) => {
                header.push_str(&format!(": {title} \u{b7} {duration}"))
            }
            (Some(title), None) => header.push_str(&format!(": {title}")),
            (None, Some(duration)) => header.push_str(&format!(": {duration}")),
            (None, None) => {}
        }
        header
    } else {
        let glyph = SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()];
        match title {
            Some(title) => format!("   {glyph} Thinking: {title}"),
            None => format!("   {glyph} Thinking"),
        }
    };
    let mut lines = vec![Line::from(Span::styled(header, style))];
    if expanded && !body.is_empty() {
        let rail = palette
            .panel
            .bg
            .map_or(palette.dim, |bg| ratatui::style::Style::default().fg(bg));
        lines.push(Line::default());
        lines.extend(indent_wrapped(body, width, 5).into_iter().map(|line| {
            let text = line
                .spans
                .into_iter()
                .map(|span| span.content)
                .collect::<String>();
            let text = text.get(5..).unwrap_or_default().to_owned();
            Line::from(vec![
                Span::raw("   "),
                Span::styled("\u{2503}", rail),
                Span::styled(format!(" {text}"), palette.dim),
            ])
        }));
    }
    lines
}

/// The answer text, then the turn footer (own blank row above it).
fn assistant_block(text: &str, footer: Option<&TurnFooter>, width: u16) -> Vec<Line<'static>> {
    let mut lines = super::markdown::render(text.trim(), width, 3);
    if let Some(footer) = footer {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(footer_line(footer, width));
    }
    lines
}

/// Upstream `AssistantFooter` (`routes/session/index.tsx:2000`): the agent in
/// its colour, then muted ` · model`, ` · duration`, ` · N.N tok/s`. The width
/// gates are upstream's; `width` here is the timeline's, a few columns short
/// of the terminal's, which only matters at the 28/36 boundaries.
fn footer_line(footer: &TurnFooter, width: u16) -> Line<'static> {
    let palette = theme::palette();
    let mut agent_style = palette.agent;
    agent_style.bg = None;
    let mut spans = vec![Span::raw("   ")];
    if let Some(agent) = &footer.agent {
        spans.push(Span::styled(titlecase(agent), agent_style));
    }
    if width >= 28
        && let Some(model) = &footer.model
    {
        spans.push(Span::styled(format!(" \u{b7} {model}"), palette.dim));
    }
    if let Some(ms) = footer.duration_ms.filter(|ms| *ms > 0)
        && !(28..36).contains(&width)
    {
        spans.push(Span::styled(
            format!(" \u{b7} {}", format_duration(ms)),
            palette.dim,
        ));
    }
    if let Some(rate) = footer.tokens_per_second {
        spans.push(Span::styled(
            format!(" \u{b7} {rate:.1} tok/s"),
            palette.dim,
        ));
    }
    Line::from(spans)
}

/// The title row, then (expanded) one tool header row per grouped call —
/// plain inline tool rows, without their output (`group-view.tsx:129-157`).
fn explored_block(explored: &Explored, view: ViewState) -> Vec<Line<'static>> {
    let mut lines = vec![explored_line(explored, view.hovered)];
    if view.explored_expanded {
        lines.extend(
            explored
                .calls
                .iter()
                .map(|call| tool_header(&call.tool, call.state, &call.summary, false)),
        );
    }
    lines
}

/// Upstream `group-view.tsx`: `→ Explored: 2 reads, 1 search`, or
/// `✱ Exploring: …` while a grouped call is still running.
fn explored_line(explored: &Explored, hovered: bool) -> Line<'static> {
    let palette = theme::palette();
    let mut parts = Vec::new();
    for (count, one, many) in [
        (explored.reads, "read", "reads"),
        (explored.searches, "search", "searches"),
        (explored.fetches, "fetch", "fetches"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {}", if count == 1 { one } else { many }));
        }
    }
    let (icon, label) = if explored.running {
        ("\u{2731}", "Exploring:")
    } else {
        ("\u{2192}", "Explored:")
    };
    Line::from(Span::styled(
        format!("   {icon} {label} {}", parts.join(", ")),
        explored_title_style(palette, hovered),
    ))
}

/// Muted at rest, full text colour under the pointer (`group-view.tsx:146`).
fn explored_title_style(palette: &theme::Palette, hovered: bool) -> ratatui::style::Style {
    if hovered { palette.text } else { palette.dim }
}

/// The head line of an inline tool row: paddingLeft 3, a two-column icon, then
/// `tool summary`; bold when focused. Shared by `tool_block` and the expanded
/// Explored group.
fn tool_header(tool: &str, state: ToolState, summary: &str, focused: bool) -> Line<'static> {
    let palette = theme::palette();
    let (icon, style) = match state {
        ToolState::Input | ToolState::Running | ToolState::Progress => ("~", palette.accent),
        ToolState::Success => ("\u{2699}", palette.dim),
        ToolState::Failed => ("\u{2717}", palette.warn),
    };
    let mut label = String::from(tool);
    if !summary.is_empty() {
        label.push(' ');
        label.push_str(summary);
    }
    let style = if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    };
    Line::from(Span::styled(format!("   {icon} {label}"), style))
}

/// Upstream `InlineToolRow` (`message-parts.tsx:202`): the header, then output
/// hanging below only when expanded (`o`) or when the call failed; the
/// focused row is bold instead of advertising its key.
fn tool_block(
    block: &Block,
    tool: &str,
    state: ToolState,
    summary: &str,
    output: &str,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let expandable = entry_tool_output(block).is_some();
    let focused = view.tool_focused && expandable;
    let mut lines = vec![tool_header(tool, state, summary, focused)];
    let failed = matches!(state, ToolState::Failed);
    let body_style = if failed { palette.warn } else { palette.dim };
    if view.tool_expanded && expandable {
        let wrapped = wrap_output(output, width.saturating_sub(5));
        let shown = wrapped.len().min(TOOL_OUTPUT_MAX_LINES);
        for line in wrapped.iter().take(shown) {
            lines.push(Line::from(Span::styled(format!("     {line}"), body_style)));
        }
        if wrapped.len() > shown {
            lines.push(Line::from(Span::styled(
                format!(
                    "     \u{2026} \u{5176}\u{4f59} {} \u{884c}",
                    wrapped.len() - shown
                ),
                body_style,
            )));
        }
    } else if failed && !output.is_empty() {
        for line in bound_wrapped(output, width.saturating_sub(5), 4) {
            lines.push(Line::from(Span::styled(format!("     {line}"), body_style)));
        }
    }
    lines
}

/// Upstream `TodoWrite`: the panel when the call completed with todos, else
/// an `InlineTool` row (`Updating todos…` / `Todo update failed` /
/// `Cleared todos`).
fn todo_block(state: ToolState, todos: &[Todo], width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let (icon, label, style) = match state {
        ToolState::Success if !todos.is_empty() => return super::todo::lines(todos, width),
        ToolState::Input | ToolState::Running | ToolState::Progress => {
            ("~", "Updating todos\u{2026}", palette.accent)
        }
        ToolState::Success => ("\u{2699}", "Cleared todos", palette.dim),
        ToolState::Failed => ("\u{2717}", "Todo update failed", palette.warn),
    };
    vec![Line::from(Span::styled(
        format!("   {icon} {label}"),
        style,
    ))]
}

/// The id of a tool block, for focus/expansion bookkeeping.
fn entry_tool_id(block: &Block) -> Option<&str> {
    match block {
        Block::Tool { id, .. } => Some(id.as_str()),
        _ => None,
    }
}

/// The output of a tool row that can actually be shown.
fn entry_tool_output(block: &Block) -> Option<&str> {
    match block {
        Block::Tool { state, output, .. }
            if !output.is_empty() && matches!(state, ToolState::Success | ToolState::Failed) =>
        {
            Some(output.as_str())
        }
        _ => None,
    }
}

/// Wrap a tool output line by line: structure (`ls`, `git status`) must survive
/// the wrap, so explicit newlines stay line breaks instead of collapsing.
fn wrap_output(output: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut wrapped = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let options = textwrap::Options::new(width)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);
        wrapped.extend(textwrap::wrap(line, options).into_iter().map(String::from));
    }
    wrapped
}

/// The band behind a user message: `raised.base`, one step stronger under the
/// pointer (`routes/session/index.tsx:2281, 2410`, `decrease(raised.base)`).
/// `panel` is the `raised.high` surface (`theme.rs`, `from_tokens`).
fn user_surface(palette: &theme::Palette, hovered: bool) -> ratatui::style::Style {
    if hovered {
        palette.panel
    } else {
        palette.raised_base
    }
}

/// Upstream user message (`routes/session/index.tsx` ~2341): a raised box
/// with an agent-coloured left rule, one blank row above and below the text,
/// and the text two columns in from the rule. The rule sits on the raised
/// surface too, so the whole row is one band.
fn user_block(text: &str, view: ViewState, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let raised = user_surface(palette, view.hovered);
    let rail = palette.agent.patch(raised);
    let body = palette.text.patch(raised);
    let usable = width.saturating_sub(3).max(1) as usize;
    let options = textwrap::Options::new(usable)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let fill = width.saturating_sub(1) as usize;
    let row = |content: &str| -> Line<'static> {
        let used = unicode_width::UnicodeWidthStr::width(content);
        Line::from(vec![
            Span::styled("\u{2503}", rail),
            Span::styled(
                format!("{content}{}", " ".repeat(fill.saturating_sub(used))),
                body,
            ),
        ])
    };
    let mut lines = vec![row("")];
    for line in textwrap::wrap(text, options) {
        lines.push(row(&format!("  {line}")));
    }
    lines.push(row(""));
    lines
}

/// Mirrors upstream `Locale.duration`.
fn format_duration(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else {
        format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

fn titlecase(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn indent_wrapped(text: &str, width: u16, indent: usize) -> Vec<Line<'static>> {
    let pad = " ".repeat(indent);
    let width = width.saturating_sub(indent as u16).max(1) as usize;
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    textwrap::wrap(text, options)
        .into_iter()
        .map(|line| Line::from(format!("{pad}{line}")))
        .collect()
}

/// Wrap `text` to `width` and keep at most `max` lines, marking a cut with `…`.
fn bound_wrapped(text: &str, width: u16, max: usize) -> Vec<String> {
    let width = width.max(1) as usize;
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let wrapped: Vec<String> = textwrap::wrap(text, options)
        .into_iter()
        .map(String::from)
        .collect();
    if wrapped.len() <= max {
        return wrapped;
    }
    let mut kept: Vec<String> = wrapped.into_iter().take(max).collect();
    if let Some(last) = kept.last_mut() {
        last.push('…');
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::TurnFooter;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};
    use std::sync::Arc;

    use super::{Block, Explored, ExploredCall, Transcript, reasoning_summary};
    use ox_client::store::ToolState;

    #[test]
    fn tool_rows_show_name_summary_and_output() {
        let mut t = Transcript::new(60, 12);
        t.sync(
            &[
                Block::Tool {
                    id: "p1".into(),
                    version: 0,
                    tool: "shell".into(),
                    state: ToolState::Running,
                    summary: "git status".into(),
                    output: Arc::default(),
                },
                Block::Tool {
                    id: "p2".into(),
                    version: 0,
                    tool: "read".into(),
                    state: ToolState::Success,
                    summary: "a.txt".into(),
                    output: Arc::new("one\ntwo\nthree".into()),
                },
                Block::Tool {
                    id: "p3".into(),
                    version: 0,
                    tool: "edit".into(),
                    state: ToolState::Failed,
                    summary: "main.rs".into(),
                    output: Arc::new("boom".into()),
                },
            ],
            0,
        );
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = (0..12)
            .map(|y| {
                (0..60)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Upstream inline rows: `icon tool summary`, no `⎿ N lines` stub.
        assert!(text.contains("~ shell git status"), "{text}");
        assert!(text.contains("⚙ read a.txt"), "{text}");
        assert!(!text.contains("lines"), "{text}");
        assert!(
            !text.contains("one"),
            "success output stays collapsed: {text}"
        );
        assert!(text.contains("✗ edit main.rs"), "{text}");
        assert!(
            text.contains("boom"),
            "failures keep their bounded output: {text}"
        );
    }

    #[test]
    fn a_streaming_tool_row_shows_only_the_header() {
        let mut t = Transcript::new(40, 6);
        t.sync(
            &[Block::Tool {
                id: "p1".into(),
                version: 0,
                tool: "grep".into(),
                state: ToolState::Input,
                summary: String::new(),
                output: Arc::default(),
            }],
            0,
        );
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        // Row 0 is the block's marginTop.
        let row: String = (0..40).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(row.trim_end().ends_with("grep"), "{row:?}");
        let row1: String = (0..40).map(|x| buffer[(x, 2)].symbol()).collect();
        assert!(
            row1.trim().is_empty(),
            "no summary and no output yet: header only"
        );
    }

    /// Draws the transcript once and returns the screen as text rows. Wide
    /// glyphs own two buffer cells whose tail is a plain space; walking by
    /// cell width keeps `折叠` from reading as `折 叠`.
    fn screen(t: &mut Transcript, width: u16, height: u16) -> String {
        use ratatui::buffer::CellWidth;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let mut row = String::new();
                let mut tail = 0u16;
                for x in 0..width {
                    if tail > 0 {
                        tail -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    tail = symbol.cell_width().saturating_sub(1);
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_row(id: &str, version: u64, summary: &str, state: ToolState, output: &str) -> Block {
        Block::Tool {
            id: id.into(),
            version,
            tool: "read".into(),
            state,
            summary: summary.into(),
            output: Arc::new(output.into()),
        }
    }

    #[test]
    fn expanding_a_tool_row_shows_every_output_line() {
        let mut t = Transcript::new(60, 10);
        t.sync(
            &[tool_row(
                "p1",
                0,
                "a.txt",
                ToolState::Success,
                "alpha\nbeta\ngamma",
            )],
            0,
        );
        let collapsed = screen(&mut t, 60, 10);
        assert!(collapsed.contains("⚙ read a.txt"), "{collapsed}");
        assert!(!collapsed.contains("beta"), "{collapsed}");

        assert!(t.toggle_tool_output(), "a tool row was expanded");
        let expanded = screen(&mut t, 60, 10);
        assert!(expanded.contains("alpha"), "{expanded}");
        assert!(expanded.contains("beta"), "{expanded}");
        assert!(expanded.contains("gamma"), "{expanded}");
    }

    /// Text of every row whose first glyph is bold: the focused tool row.
    fn bold_rows(t: &mut Transcript, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .filter_map(|y| {
                let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                let first = (0..width).find(|x| buffer[(*x, y)].symbol() != " ")?;
                buffer[(first, y)]
                    .modifier
                    .contains(Modifier::BOLD)
                    .then(|| row.trim().to_owned())
            })
            .collect()
    }

    #[test]
    fn the_focused_tool_row_is_bold_and_o_toggles_its_output() {
        let mut t = Transcript::new(60, 12);
        t.sync(
            &[
                tool_row("p1", 0, "first", ToolState::Success, "one"),
                tool_row("p2", 0, "second", ToolState::Success, "two"),
            ],
            0,
        );
        assert!(bold_rows(&mut t, 60, 12).is_empty(), "nothing focused yet");

        assert!(t.toggle_tool_output());
        let expanded = screen(&mut t, 60, 12);
        assert!(expanded.contains("two"), "{expanded}");
        assert_eq!(bold_rows(&mut t, 60, 12), ["⚙ read second"]);

        assert!(t.toggle_tool_output());
        let collapsed = screen(&mut t, 60, 12);
        assert!(!collapsed.contains("two"), "{collapsed}");
    }

    #[test]
    fn tool_focus_cycles_forward_and_backward() {
        let mut t = Transcript::new(60, 12);
        t.sync(
            &[
                tool_row("p1", 0, "first", ToolState::Success, "one"),
                tool_row("p2", 0, "second", ToolState::Success, "two"),
            ],
            0,
        );
        assert!(
            t.focus_tool(true),
            "focus moves from nothing to the first row"
        );
        assert_eq!(bold_rows(&mut t, 60, 12), ["⚙ read first"]);

        assert!(t.focus_tool(true));
        assert_eq!(bold_rows(&mut t, 60, 12), ["⚙ read second"]);

        assert!(t.focus_tool(true), "wraps to the first row");
        assert_eq!(bold_rows(&mut t, 60, 12), ["⚙ read first"]);

        assert!(t.focus_tool(false), "backwards wraps to the last row");
        assert_eq!(bold_rows(&mut t, 60, 12), ["⚙ read second"]);
    }

    #[test]
    fn expanding_a_huge_output_keeps_at_most_the_cap() {
        let output: Vec<String> = (0..300).map(|i| format!("line{i}")).collect();
        let output = output.join("\n");
        let mut t = Transcript::new(60, 10);
        t.sync(&[tool_row("p1", 0, "big", ToolState::Success, &output)], 0);
        assert!(t.toggle_tool_output());

        assert_eq!(
            t.total_lines(),
            1 + 1 + super::TOOL_OUTPUT_MAX_LINES + 1,
            "marginTop + header + capped body + ellipsis row"
        );
        let text = screen(&mut t, 60, 10);
        assert!(text.contains("line0"), "{text}");
    }

    #[test]
    fn a_tool_row_without_output_is_not_expandable() {
        let mut t = Transcript::new(60, 8);
        t.sync(&[tool_row("p1", 0, "a.txt", ToolState::Running, "")], 0);
        assert!(!t.toggle_tool_output(), "nothing to show");
        assert!(!t.focus_tool(true), "nothing to focus");
        assert_eq!(t.total_lines(), 2, "marginTop and the header alone");
    }

    #[test]
    fn tool_expansion_survives_a_streaming_update() {
        let mut t = Transcript::new(60, 10);
        t.sync(
            &[tool_row("p1", 0, "a.txt", ToolState::Success, "alpha")],
            0,
        );
        assert!(t.toggle_tool_output());

        // The store bumps the part version when more output arrives.
        t.sync(
            &[tool_row(
                "p1",
                1,
                "a.txt",
                ToolState::Success,
                "alpha\nbeta",
            )],
            0,
        );
        let text = screen(&mut t, 60, 10);
        assert!(text.contains("beta"), "expansion keyed by id: {text}");
    }

    fn turn_footer(duration_ms: i64, tokens_per_second: Option<f64>) -> TurnFooter {
        TurnFooter {
            agent: Some("build".into()),
            model: Some("p/m".into()),
            duration_ms: Some(duration_ms),
            tokens_per_second,
        }
    }

    fn footer_block(footer: TurnFooter) -> Block {
        Block::Assistant {
            id: "msg_1".into(),
            version: 1,
            text: Arc::new("done".into()),
            footer: Some(footer),
        }
    }

    #[test]
    fn assistant_footer_shows_turn_duration_and_throughput() {
        let mut t = Transcript::new(60, 12);
        t.sync(&[footer_block(turn_footer(4_200, Some(70.0)))], 0);
        let text = screen(&mut t, 60, 12);
        assert!(text.contains("Build · p/m · 4.2s · 70.0 tok/s"), "{text}");
    }

    #[test]
    fn assistant_footer_omits_throughput_without_a_rate() {
        let mut t = Transcript::new(60, 12);
        t.sync(&[footer_block(turn_footer(4_200, None))], 0);
        let text = screen(&mut t, 60, 12);
        assert!(text.contains("Build · p/m · 4.2s"), "{text}");
        assert!(!text.contains("tok/s"), "{text}");
    }

    #[test]
    fn sync_keeps_tool_row_caches_while_the_text_streams() {
        let tool = Block::Tool {
            id: "p1".into(),
            version: 2,
            tool: "shell".into(),
            state: ToolState::Running,
            summary: "git status".into(),
            output: Arc::default(),
        };
        let mut t = Transcript::new(40, 10);
        t.sync(&[Block::assistant("msg_1", 0, "b"), tool.clone()], 0);
        let tool_gen = t.entry_generation(1);

        // The assistant text streams; the tool part is untouched.
        t.sync(&[Block::assistant("msg_1", 1, "b streaming"), tool], 0);
        assert_eq!(
            t.entry_generation(1),
            tool_gen,
            "unchanged tool part keeps its cache"
        );
    }

    #[test]
    fn done_reasoning_collapses_and_expands_on_toggle() {
        let mut t = Transcript::new(50, 10);
        t.sync(
            &[
                Block::Reasoning {
                    id: "msg_1".into(),
                    version: 1,
                    text: Arc::new("deep hidden thought".into()),
                    done: true,
                    ms: Some(4_000),
                },
                Block::assistant("msg_1", 1, "the answer"),
            ],
            0,
        );
        let text_of = |t: &mut Transcript| -> String {
            let backend = TestBackend::new(50, 10);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| t.render(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..10)
                .flat_map(|y| (0..50).map(move |x| (x, y)))
                .map(|(x, y)| buffer[(x, y)].symbol())
                .collect()
        };
        let collapsed_text = text_of(&mut t);
        assert!(
            collapsed_text.contains("+ Thought: 4.0s"),
            "{collapsed_text}"
        );
        assert!(!collapsed_text.contains("hidden"), "{collapsed_text}");

        assert!(t.toggle_last_reasoning(), "a reasoning block was toggled");
        let expanded_text = text_of(&mut t);
        assert!(expanded_text.contains("hidden"), "{expanded_text}");

        assert!(t.toggle_last_reasoning());
        let recollapsed_text = text_of(&mut t);
        assert!(!recollapsed_text.contains("hidden"), "{recollapsed_text}");
    }

    #[test]
    fn sub_second_thought_shows_milliseconds() {
        let mut t = Transcript::new(50, 6);
        t.sync(
            &[Block::Reasoning {
                id: "msg_1".into(),
                version: 1,
                text: Arc::new("quick".into()),
                done: true,
                ms: Some(800),
            }],
            0,
        );
        let mut terminal = Terminal::new(TestBackend::new(50, 6)).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = (0..6)
            .flat_map(|y| (0..50).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol())
            .collect();
        assert!(text.contains("+ Thought: 800ms"), "{text}");
    }

    #[test]
    fn streaming_reasoning_collapses_to_a_thinking_line_and_expands() {
        let mut t = Transcript::new(50, 10);
        t.sync(
            &[
                Block::reasoning("msg_1", 0, "watching it think", false),
                Block::user("no reasoning here"),
            ],
            0,
        );
        let text_of = |t: &mut Transcript| -> String {
            let backend = TestBackend::new(50, 10);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| t.render(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..10)
                .flat_map(|y| (0..50).map(move |x| (x, y)))
                .map(|(x, y)| buffer[(x, y)].symbol())
                .collect()
        };
        let text = text_of(&mut t);
        assert!(text.contains("Thinking"), "{text}");
        assert!(!text.contains("watching it think"), "{text}");
        // Streaming reasoning is toggleable too (upstream's hide-mode header).
        assert!(t.toggle_last_reasoning());
        let expanded = text_of(&mut t);
        assert!(expanded.contains("watching it think"), "{expanded}");
    }

    #[test]
    fn clicking_the_thought_header_opens_and_closes_the_body() {
        let mut t = Transcript::new(50, 12);
        t.sync(
            &[
                Block::Reasoning {
                    id: "msg_1".into(),
                    version: 1,
                    text: Arc::new("Let me look at the file first.".into()),
                    done: true,
                    ms: None,
                },
                Block::assistant("msg_1", 1, "answer"),
            ],
            0,
        );
        let draw = |t: &mut Transcript| screen(t, 50, 12);
        let closed = draw(&mut t);
        assert!(closed.contains("+ Thought"), "{closed}");
        assert!(!closed.contains("Let me look"), "{closed}");

        // Row 0 is the block margin, row 1 the header.
        assert!(!t.click(6, 0), "the margin row is not a target");
        assert!(!t.click(60, 1), "outside the transcript is not a target");
        assert!(t.click(6, 1));
        let open = draw(&mut t);
        let rows: Vec<&str> = open.lines().collect();
        assert_eq!(rows[1], "   - Thought", "{rows:?}");
        assert_eq!(
            rows[3], "   \u{2503} Let me look at the file first.",
            "{rows:?}"
        );

        assert!(t.click(6, 1));
        assert!(!draw(&mut t).contains("Let me look"));
    }

    fn call(id: &str, version: u64, tool: &str, summary: &str, state: ToolState) -> ExploredCall {
        ExploredCall {
            id: id.into(),
            version,
            tool: tool.into(),
            state,
            summary: summary.into(),
        }
    }

    fn group(calls: Vec<ExploredCall>) -> Block {
        Block::Explored(Explored {
            reads: calls.len() as u32,
            running: calls
                .iter()
                .any(|c| !matches!(c.state, ToolState::Success | ToolState::Failed)),
            group_id: calls[0].id.clone(),
            calls,
            ..Explored::default()
        })
    }

    #[test]
    fn clicking_an_explored_title_lists_its_calls_and_a_second_click_hides_them() {
        let mut t = Transcript::new(50, 12);
        t.sync(
            &[group(vec![
                call("a", 1, "read", "a.txt", ToolState::Success),
                call("b", 1, "grep", "needle", ToolState::Failed),
            ])],
            0,
        );
        let collapsed = screen(&mut t, 50, 12);
        assert!(collapsed.contains("→ Explored: 2 reads"), "{collapsed}");
        assert!(!collapsed.contains("a.txt"), "{collapsed}");
        let lines_before = t.total_lines();

        assert!(!t.click(6, 0), "the margin row is not a target");
        assert!(t.click(6, 1));
        assert_eq!(t.total_lines(), lines_before + 2, "one row per call");
        let open = screen(&mut t, 50, 12);
        let rows: Vec<&str> = open.lines().collect();
        assert_eq!(rows[1], "   → Explored: 2 reads", "{rows:?}");
        assert_eq!(rows[2], "   ⚙ read a.txt", "{rows:?}");
        assert_eq!(rows[3], "   ✗ grep needle", "{rows:?}");

        assert!(!t.click(6, 2), "call rows are not click targets");
        assert!(t.click(6, 1));
        assert_eq!(t.total_lines(), lines_before);
        assert!(!screen(&mut t, 50, 12).contains("a.txt"));
    }

    #[test]
    fn an_expanded_group_stays_open_when_a_call_changes_state() {
        let mut t = Transcript::new(50, 12);
        t.sync(
            &[group(vec![
                call("a", 1, "read", "a.txt", ToolState::Success),
                call("b", 1, "grep", "needle", ToolState::Running),
            ])],
            0,
        );
        screen(&mut t, 50, 12);
        assert!(t.click(6, 1));
        let running = screen(&mut t, 50, 12);
        assert!(running.contains("✱ Exploring: 2 reads"), "{running}");
        assert!(running.contains("~ grep needle"), "{running}");

        t.sync(
            &[group(vec![
                call("a", 1, "read", "a.txt", ToolState::Success),
                call("b", 2, "grep", "needle", ToolState::Success),
            ])],
            0,
        );
        let done = screen(&mut t, 50, 12);
        assert!(done.contains("→ Explored: 2 reads"), "{done}");
        assert!(done.contains("⚙ grep needle"), "{done}");
        assert!(done.contains("⚙ read a.txt"), "{done}");
    }

    fn one_group_then_a_user() -> [Block; 2] {
        [
            group(vec![call("a", 1, "read", "a.txt", ToolState::Success)]),
            Block::user("hello"),
        ]
    }

    /// Cell `(x, y)` of one drawn frame.
    fn cell_at(
        t: &mut Transcript,
        width: u16,
        height: u16,
        x: u16,
        y: u16,
    ) -> ratatui::buffer::Cell {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer()[(x, y)].clone()
    }

    #[test]
    fn hover_reports_only_index_changes_and_targets_titles_and_user_blocks() {
        let mut t = Transcript::new(50, 12);
        t.sync(&one_group_then_a_user(), 0);
        screen(&mut t, 50, 12); // rows: 0 margin, 1 Explored title, 2 margin, 3.. user
        let lines = t.total_lines();

        assert!(!t.hover(6, 0), "a block margin is not a target");
        assert!(t.hover(6, 1), "entering the Explored title is a change");
        assert_eq!(t.hovered, Some(0));
        assert!(!t.hover(9, 1), "same entry again: nothing to redraw");
        assert!(t.hover(6, 4), "any row of a user block hovers it");
        assert_eq!(t.hovered, Some(1));
        assert!(!t.hover(20, 3), "still the same user block");
        assert!(t.hover(60, 4), "leaving the transcript clears the hover");
        assert_eq!(t.hovered, None);
        assert!(!t.hover(60, 4), "already clear");
        assert_eq!(t.total_lines(), lines, "hover never changes the row count");
    }

    #[test]
    fn hover_ignores_call_rows_and_other_blocks_and_clear_hover_reports_a_change() {
        let mut t = Transcript::new(50, 12);
        t.sync(
            &[
                group(vec![call("a", 1, "read", "a.txt", ToolState::Success)]),
                Block::assistant("m", 1, "answer"),
            ],
            0,
        );
        screen(&mut t, 50, 12);
        assert!(t.click(6, 1)); // expand: call row at y=2
        screen(&mut t, 50, 12);
        assert!(!t.hover(6, 2), "call rows are not hover targets");
        assert!(!t.hover(6, 5), "assistant text is not a hover target");
        assert!(t.hover(6, 1));
        assert!(t.clear_hover());
        assert!(!t.clear_hover(), "nothing left to clear");
    }

    #[test]
    fn hover_survives_a_sync_of_the_hovered_entry_and_drops_a_vanished_one() {
        let mut t = Transcript::new(50, 12);
        t.sync(&one_group_then_a_user(), 0);
        screen(&mut t, 50, 12);
        assert!(t.hover(6, 4));
        t.sync(&[Block::user("changed"), Block::user("again")], 0);
        t.sync(&[Block::user("only")], 0);
        assert_eq!(t.hovered, None, "the hovered entry no longer exists");
    }

    #[test]
    fn hover_styles_pick_the_stronger_colour_for_each_block() {
        use super::{explored_title_style, thought_header_style, user_surface};
        use crate::theme::{Deck, Palette};
        let p = Palette::for_deck(Deck::Truecolor);
        assert_eq!(explored_title_style(&p, false), p.dim);
        assert_eq!(explored_title_style(&p, true), p.text);
        // Collapsed and finished: 60% thought -> full warning on hover.
        assert_eq!(thought_header_style(&p, false, true, false), p.thought);
        assert_eq!(thought_header_style(&p, false, true, true), p.warning);
        assert_eq!(thought_header_style(&p, true, true, false), p.warning);
        // A streaming header keeps its colour: upstream only lights finished ones.
        assert_eq!(thought_header_style(&p, false, false, true), p.thought);
        assert_ne!(user_surface(&p, false).bg, user_surface(&p, true).bg);
        assert_eq!(user_surface(&p, false), p.raised_base);
        assert_eq!(user_surface(&p, true), p.panel, "raised.high");
    }

    #[test]
    fn hovering_recolours_the_explored_title_the_thought_header_and_the_user_band() {
        use super::{explored_title_style, thought_header_style, user_surface};
        let p = crate::theme::palette();

        let mut t = Transcript::new(50, 12);
        t.sync(&one_group_then_a_user(), 0);
        let plain_title = cell_at(&mut t, 50, 12, 6, 1);
        assert_eq!(
            plain_title.fg,
            explored_title_style(p, false).fg.unwrap_or(Color::Reset)
        );
        let rows_before = screen(&mut t, 50, 12);
        assert!(t.hover(6, 1));
        let lit_title = cell_at(&mut t, 50, 12, 6, 1);
        assert_eq!(
            lit_title.fg,
            explored_title_style(p, true).fg.unwrap_or(Color::Reset)
        );
        assert_eq!(
            screen(&mut t, 50, 12),
            rows_before,
            "same text, only colour moves"
        );
        assert!(t.hover(6, 4));
        assert_eq!(
            cell_at(&mut t, 50, 12, 6, 1).fg,
            plain_title.fg,
            "the title returns to its resting colour"
        );
        let lit_user = cell_at(&mut t, 50, 12, 10, 4);
        assert_eq!(
            lit_user.bg,
            user_surface(p, true).bg.unwrap_or(Color::Reset)
        );
        assert!(t.hover(60, 4));
        let rest_user = cell_at(&mut t, 50, 12, 10, 4);
        assert_eq!(
            rest_user.bg,
            user_surface(p, false).bg.unwrap_or(Color::Reset)
        );

        let mut t = Transcript::new(50, 6);
        t.sync(&[Block::reasoning("m", 1, "pondering", true)], 0);
        let rest = cell_at(&mut t, 50, 6, 6, 1);
        assert_eq!(
            rest.fg,
            thought_header_style(p, false, true, false)
                .fg
                .unwrap_or(Color::Reset)
        );
        let lines = t.total_lines();
        assert!(t.hover(6, 1));
        let lit = cell_at(&mut t, 50, 6, 6, 1);
        assert_eq!(
            lit.fg,
            thought_header_style(p, false, true, true)
                .fg
                .unwrap_or(Color::Reset)
        );
        assert_eq!(t.total_lines(), lines);
    }

    #[test]
    fn a_running_group_keeps_the_exploring_title_when_collapsed() {
        let mut t = Transcript::new(50, 6);
        t.sync(
            &[group(vec![call(
                "a",
                1,
                "read",
                "a.txt",
                ToolState::Running,
            )])],
            0,
        );
        let text = screen(&mut t, 50, 6);
        assert!(text.contains("✱ Exploring: 1 read"), "{text}");
        assert!(!text.contains("a.txt"), "{text}");
    }

    #[test]
    fn a_streaming_thought_spins_and_a_bold_title_names_it() {
        let mut t = Transcript::new(50, 6);
        t.sync(
            &[Block::reasoning("m", 0, "**Inspecting PR**\n\nbody", false)],
            0,
        );
        let first = screen(&mut t, 50, 6);
        assert!(
            first.contains("\u{280b} Thinking: Inspecting PR"),
            "{first}"
        );

        let later = t.spin_epoch + std::time::Duration::from_millis(80);
        assert!(t.tick(later), "a new frame asks for a repaint");
        assert!(!t.tick(later), "the same frame does not");
        let second = screen(&mut t, 50, 6);
        assert!(
            second.contains("\u{2819} Thinking: Inspecting PR"),
            "{second}"
        );

        // Finished: the spinner stops and the title joins the header.
        t.sync(
            &[Block::reasoning("m", 1, "**Inspecting PR**\n\nbody", true)],
            0,
        );
        assert!(!t.tick(later + std::time::Duration::from_millis(80)));
        assert!(screen(&mut t, 50, 6).contains("+ Thought: Inspecting PR"));
    }

    #[test]
    fn reasoning_summary_needs_a_bold_first_block() {
        assert_eq!(reasoning_summary("**T**\n\nbody"), (Some("T"), "body"));
        assert_eq!(reasoning_summary("**T**"), (Some("T"), ""));
        assert_eq!(
            reasoning_summary("plain **bold** text"),
            (None, "plain **bold** text")
        );
        assert_eq!(reasoning_summary("**T** inline"), (None, "**T** inline"));
    }

    #[test]
    fn tool_rows_and_outputs_start_at_column_three() {
        let mut t = Transcript::new(40, 8);
        t.sync(
            &[Block::Tool {
                id: "p1".into(),
                version: 0,
                tool: "shell".into(),
                state: ToolState::Success,
                summary: "ls".into(),
                output: Arc::new("a".into()),
            }],
            0,
        );
        assert!(t.toggle_tool_output());
        let rows: Vec<String> = screen(&mut t, 40, 8).lines().map(str::to_owned).collect();
        assert_eq!(rows[1], "   ⚙ shell ls", "{rows:?}");
        assert_eq!(rows[2], "     a", "{rows:?}");
    }

    #[test]
    fn search_finds_matches_across_blocks_and_wraps() {
        let mut t = Transcript::new(40, 4);
        t.sync(
            &[
                Block::user("alpha"),
                Block::assistant("msg_1", 0, "beta ALPHA gamma"),
                Block::user("end"),
            ],
            0,
        );
        assert_eq!(t.set_query("alpha"), 2, "case-insensitive across blocks");
        assert!(t.next_match(true), "advance to the second match");
        assert!(t.next_match(true), "wrap around to the first");
        assert!(t.next_match(false), "wrap backwards to the last");
        t.clear_search();
        assert_eq!(t.search().matches.len(), 0);
        assert!(!t.next_match(true), "no query, no navigation");
    }

    #[test]
    fn set_query_jumps_the_viewport_to_the_first_match() {
        let mut t = Transcript::new(20, 3);
        let blocks: Vec<_> = (0..30)
            .map(|i| Block::user(&format!("row {i} needle")))
            .collect();
        t.sync(&blocks, 0);
        t.set_query("needle");
        // sticky was on (tail); a search must take the viewport to the match.
        assert!(!t.sticky_bottom);
        assert!(t.scroll <= 30 - 3, "scroll clamped to the tail");
        t.next_match(true);
        assert!(t.search().current == Some(1));
    }

    #[test]
    fn current_match_line_is_highlighted_in_the_buffer() {
        let mut t = Transcript::new(20, 4);
        t.sync(&[Block::user("hit here"), Block::user("after")], 0);
        t.set_query("hit");
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let hit = (0..4)
            .flat_map(|y| (0..20).map(move |x| (x, y)))
            .find(|(x, y)| buffer[(*x, *y)].symbol() == "h")
            .expect("the hit line is on screen");
        assert!(
            // ratatui 0.29: Cell.bg is `Color`; `Reset` means "no background".
            buffer[hit].bg != Color::Reset || buffer[hit].modifier.contains(Modifier::REVERSED),
            "the current match line must be highlighted (palette-dependent)",
        );
    }

    #[test]
    fn sync_preserves_unchanged_prefix_generations() {
        let mut t = Transcript::new(40, 10);
        let first = Block::user("a");
        t.sync(&[first.clone(), Block::assistant("msg_1", 0, "b")], 0);
        let first_gen = t.entry_generation(0);

        // Only item 1 changed: version 0 → 1. The prefix cache must survive.
        t.sync(&[first, Block::assistant("msg_1", 1, "b streaming…")], 1);
        assert_eq!(
            t.entry_generation(0),
            first_gen,
            "prefix must keep its cache"
        );
        assert_ne!(
            t.entry_generation(1),
            first_gen,
            "changed block bumps generation"
        );
    }

    #[test]
    fn resizing_rewraps_and_rekeys_search_matches() {
        let text = "alpha beta gamma alpha delta epsilon alpha";
        let mut t = Transcript::new(60, 4);
        t.sync(&[Block::user(text)], 0);
        assert_eq!(
            t.set_query("alpha"),
            1,
            "the whole text fits one line at this width, so all three hits share it"
        );
        let wide_lines = t.total_lines();

        t.set_viewport(12, 4);

        assert!(
            t.total_lines() > wide_lines,
            "a narrower viewport must re-wrap the cached block into more lines"
        );
        let expected: Vec<usize> =
            super::render_block(&Block::user(text), 12, super::ViewState::default())
                .iter()
                .enumerate()
                .filter(|(_, line)| super::line_text(line).to_lowercase().contains("alpha"))
                .map(|(index, _)| index)
                .collect();
        assert_eq!(
            t.search().matches,
            expected,
            "match indices are absolute wrapped-line numbers and must follow a re-wrap"
        );
    }

    #[test]
    fn scrolling_disables_sticky_until_bottom() {
        let mut t = Transcript::new(40, 4);
        t.sync(
            &[
                Block::user("a"),
                Block::user("b"),
                Block::user("c"),
                Block::user("d"),
            ],
            0,
        );
        t.scroll_to_bottom();
        assert!(t.sticky_bottom);
        t.scroll_up(2);
        assert!(!t.sticky_bottom);
        t.scroll_to_bottom();
        assert!(t.sticky_bottom);
    }

    #[test]
    fn renders_only_the_visible_window() {
        let mut t = Transcript::new(20, 3);
        let blocks: Vec<_> = (0..10).map(|i| Block::user(&format!("line {i}"))).collect();
        t.sync(&blocks, 0);
        t.scroll_up(1000); // default is sticky: it would show the tail

        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("line 0"));
        assert!(!text.contains("line 9"));
    }

    #[test]
    fn a_completed_todowrite_renders_the_panel_and_other_states_an_inline_row() {
        use ox_client::todo::{Todo, TodoStatus};
        let todos = Arc::new(vec![Todo {
            content: "write tests".into(),
            status: TodoStatus::InProgress,
        }]);
        let block = |state, todos: &Arc<Vec<Todo>>| Block::Todo {
            id: "t1".into(),
            version: 0,
            state,
            todos: Arc::clone(todos),
        };
        let mut t = Transcript::new(40, 12);
        t.sync(&[block(ToolState::Success, &todos)], 0);
        let text = screen(&mut t, 40, 12);
        assert!(text.contains("# Todos"), "{text}");
        assert!(text.contains("[•] write tests"), "{text}");

        let empty = Arc::new(Vec::new());
        for (state, label) in [
            (ToolState::Running, "~ Updating todos…"),
            (ToolState::Success, "⚙ Cleared todos"),
            (ToolState::Failed, "✗ Todo update failed"),
        ] {
            let mut t = Transcript::new(40, 4);
            t.sync(&[block(state, &empty)], 0);
            let text = screen(&mut t, 40, 4);
            assert!(text.contains(label), "{label}: {text}");
            assert!(!text.contains("# Todos"), "{text}");
        }
    }

    #[test]
    fn a_compaction_block_draws_the_rule_and_the_summary() {
        let mut t = Transcript::new(40, 8);
        t.sync(
            &[Block::Compaction {
                id: "c1".into(),
                version: 0,
                status: ox_client::store::CompactionStatus::Completed,
                text: Arc::new("Goal: ship".into()),
                cancelled: false,
                tokens: None,
            }],
            0,
        );
        let text = screen(&mut t, 40, 8);
        assert!(text.contains("── Compaction ──"), "{text}");
        assert!(text.contains("   Goal: ship"), "{text}");
    }

    // ---- drag selection (plan task 4) ----

    use crate::ui::selection::Outcome;

    /// A fresh instant a second after the last one, so consecutive presses
    /// never form a multi-click unless a test says so.
    fn later() -> std::time::Instant {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Instant;
        static BASE: OnceLock<Instant> = OnceLock::new();
        static STEP: AtomicU64 = AtomicU64::new(0);
        *BASE.get_or_init(Instant::now)
            + std::time::Duration::from_secs(STEP.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn users(texts: &[&str]) -> Vec<Block> {
        texts.iter().map(|t| Block::user(t)).collect()
    }

    /// Draw once so `last_area` and the click target map exist.
    fn drawn(t: &mut Transcript, width: u16, height: u16) {
        screen(t, width, height);
    }

    fn is_highlighted(cell: &ratatui::buffer::Cell) -> bool {
        let hl = crate::theme::palette().highlight;
        hl.bg.is_none_or(|bg| cell.bg == bg) && cell.modifier.contains(hl.add_modifier)
    }

    #[test]
    fn drag_over_a_user_line_copies_the_selected_cells() {
        // rows: 0 margin, 1 padding, 2 `┃  hello world`, 3 padding
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        t.press(3, 2, later());
        assert!(t.drag(7, 2));
        assert_eq!(t.release(), Outcome::Copy("hello".to_owned()));
        assert!(t.has_selection(), "a finished copy keeps its highlight");
    }

    fn ms(base: std::time::Instant, ms: u64) -> std::time::Instant {
        base + std::time::Duration::from_millis(ms)
    }

    #[test]
    fn a_double_click_copies_the_word_and_a_triple_click_the_line() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        let t0 = later();
        t.press(10, 2, t0);
        assert_eq!(t.release(), Outcome::Click);
        t.press(10, 2, ms(t0, 100));
        assert!(t.has_selection(), "the word is selected on press");
        assert_eq!(t.release(), Outcome::Copy("world".to_owned()));
        assert!(t.has_selection(), "a copy keeps its highlight");
        t.press(10, 2, ms(t0, 200));
        assert_eq!(t.release(), Outcome::Copy("hello world".to_owned()));
    }

    #[test]
    fn a_double_click_then_a_drag_extends_by_words() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["alpha beta gamma"]), 0);
        drawn(&mut t, 40, 10);
        let t0 = later();
        t.press(10, 2, t0);
        t.release();
        t.press(10, 2, ms(t0, 100)); // `beta` is cols 9..=12 on screen
        assert!(t.drag(16, 2));
        assert_eq!(t.release(), Outcome::Copy("beta gamma".to_owned()));
    }

    #[test]
    fn a_slow_or_distant_second_press_is_not_a_double_click() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["alpha beta gamma"]), 0);
        drawn(&mut t, 40, 10);
        let t0 = later();
        t.press(10, 2, t0);
        t.release();
        t.press(10, 2, ms(t0, 501));
        assert_eq!(t.release(), Outcome::Click);
        t.press(10, 2, ms(t0, 600));
        t.release();
        t.press(13, 2, ms(t0, 700)); // three cells away
        assert_eq!(t.release(), Outcome::Click);
    }

    #[test]
    fn a_double_click_on_a_cjk_run_selects_it_whole() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["日本語abc xyz"]), 0);
        drawn(&mut t, 40, 10);
        let t0 = later();
        t.press(6, 2, t0);
        t.release();
        t.press(6, 2, ms(t0, 50));
        assert_eq!(t.release(), Outcome::Copy("日本語abc".to_owned()));
    }

    #[test]
    fn clearing_the_selection_forgets_the_click_history() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        let t0 = later();
        t.press(10, 2, t0);
        t.release();
        t.clear_selection();
        t.press(10, 2, ms(t0, 50));
        assert_eq!(t.release(), Outcome::Click, "the history was reset");
    }

    #[test]
    fn press_and_release_without_moving_is_a_click_and_leaves_no_selection() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello"]), 0);
        drawn(&mut t, 40, 10);
        t.press(3, 2, later());
        assert_eq!(t.release(), Outcome::Click);
        assert!(!t.has_selection());
    }

    #[test]
    fn a_press_off_the_content_starts_nothing() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello"]), 0);
        drawn(&mut t, 40, 10);
        t.press(3, 9, later()); // below the 4 content rows
        assert!(!t.drag(7, 2));
        assert_eq!(t.release(), Outcome::Nothing);
        t.press(60, 2, later()); // right of the transcript area
        assert_eq!(t.release(), Outcome::Nothing);
    }

    #[test]
    fn dragging_below_the_content_selects_to_the_end_without_blank_lines_between_blocks() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["one", "two"]), 0);
        drawn(&mut t, 40, 10);
        t.press(3, 2, later());
        assert!(t.drag(0, 40), "far below the area");
        assert_eq!(t.release(), Outcome::Copy("one\ntwo".to_owned()));
    }

    #[test]
    fn dragging_above_the_area_selects_from_the_first_visible_row() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["one"]), 0);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let area = ratatui::layout::Rect::new(0, 2, 40, 10);
        terminal.draw(|frame| t.render(frame, area)).unwrap();
        t.press(3, 4, later()); // content line 2, the text row
        assert!(t.drag(0, 0), "above the area");
        assert_eq!(t.release(), Outcome::Copy("o".to_owned()));
    }

    #[test]
    fn a_new_press_clears_the_old_selection() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        t.press(3, 2, later());
        t.drag(7, 2);
        t.release();
        assert!(
            t.press(3, 2, later()),
            "dropping a selected range asks for a repaint"
        );
        assert!(!t.has_selection(), "the new one is empty until it moves");
    }

    #[test]
    fn clear_selection_reports_whether_text_was_selected() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        assert!(!t.clear_selection());
        t.press(3, 2, later());
        assert!(!t.clear_selection(), "a bare press selected nothing");
        t.press(3, 2, later());
        t.drag(7, 2);
        assert!(t.clear_selection());
        assert!(!t.has_selection());
    }

    #[test]
    fn selected_cells_are_highlighted_but_the_gutter_is_not() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        t.press(0, 2, later());
        t.drag(7, 2);
        t.release();
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for x in 0..3 {
            assert!(!is_highlighted(&buffer[(x, 2)]), "gutter column {x}");
        }
        for x in 3..=7 {
            assert!(is_highlighted(&buffer[(x, 2)]), "text column {x}");
        }
        assert!(!is_highlighted(&buffer[(8, 2)]), "past the focus");
        assert!(!is_highlighted(&buffer[(3, 3)]), "other rows");
    }

    #[test]
    fn the_selection_stays_on_its_content_when_the_view_scrolls() {
        let mut t = Transcript::new(40, 8);
        t.sync(&users(&["aaa", "bbb", "ccc", "ddd"]), 0); // 16 rows, 8 visible
        drawn(&mut t, 40, 8);
        // Bottom-pinned: scroll = 16 + 1 - 8 = 9; row 5 shows line 14 (`ddd`'s
        // text row is line 14).
        let text_y = (14 - t.scroll) as u16;
        t.press(3, text_y, later());
        t.drag(5, text_y);
        t.release();
        t.scroll_up(2);
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| t.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(is_highlighted(&buffer[(3, text_y + 2)]));
        assert!(!is_highlighted(&buffer[(3, text_y)]));
    }

    #[test]
    fn a_width_change_clears_the_selection() {
        let mut t = Transcript::new(40, 10);
        t.sync(&users(&["hello world"]), 0);
        drawn(&mut t, 40, 10);
        t.press(3, 2, later());
        t.drag(7, 2);
        t.release();
        t.set_viewport(30, 10);
        assert!(!t.has_selection());
    }

    #[test]
    fn rows_changing_at_or_before_the_selection_clear_it_later_rows_do_not() {
        let mut t = Transcript::new(40, 12);
        t.sync(&users(&["one", "two"]), 0);
        drawn(&mut t, 40, 12);
        t.press(3, 2, later()); // first block
        t.drag(5, 2);
        t.release();

        let mut grown = users(&["one", "two"]);
        grown.push(Block::user("three"));
        t.sync(&grown, 0);
        assert!(
            t.has_selection(),
            "an appended block is below the selection"
        );

        let mut taller = users(&["one", "two", "three"]);
        taller[1] = Block::user(&"word ".repeat(30));
        t.sync(&taller, 0);
        assert!(t.has_selection(), "a later block may grow");

        let mut earlier = taller.clone();
        earlier[0] = Block::user(&"word ".repeat(30));
        t.sync(&earlier, 0);
        assert!(!t.has_selection(), "the selected block changed its rows");
    }

    #[test]
    fn refocus_moves_the_focus_to_the_last_pointer_cell_after_a_scroll() {
        let mut t = Transcript::new(40, 8);
        t.sync(&users(&["aaa", "bbb", "ccc", "ddd"]), 0);
        drawn(&mut t, 40, 8);
        t.press(3, 2, later());
        t.drag(5, 7);
        t.scroll_up(4);
        assert!(
            t.refocus_selection(),
            "the same cell now holds other content"
        );
        assert!(!t.refocus_selection(), "nothing moved");
    }

    #[test]
    fn rehover_follows_the_content_under_a_stationary_pointer() {
        let mut t = Transcript::new(50, 12);
        t.sync(&one_group_then_a_user(), 0);
        screen(&mut t, 50, 12);
        assert!(!t.rehover(), "no pointer seen yet");
        assert!(t.hover(6, 1));
        assert!(!t.rehover(), "same content under the pointer");
        t.sync(&[Block::assistant("m", 1, "answer")], 0);
        screen(&mut t, 50, 12);
        assert!(t.rehover(), "assistant text is not a hover target");
    }

    #[test]
    fn syncing_fewer_entries_than_the_selection_reaches_clears_it() {
        let mut t = Transcript::new(40, 12);
        t.sync(&users(&["one", "two", "three"]), 0);
        drawn(&mut t, 40, 12);
        let y = (t.total_lines() - 2 - t.scroll) as u16; // text row of "three"
        t.press(3, y, later());
        t.drag(5, y);
        assert!(t.has_selection());
        t.sync(&users(&["one"]), 0);
        assert!(!t.has_selection());
        drawn(&mut t, 40, 12);
        assert_eq!(t.release(), Outcome::Nothing);
    }

    mod completion {
        use std::sync::Arc;

        use ox_client::store::CompletionSource::{self, Shell, Subagent};
        use ratatui::text::Line;
        use unicode_width::UnicodeWidthStr;

        use super::super::{Block, Transcript, completion_line};
        use super::screen;
        use crate::theme::{Deck, Palette};

        fn palette() -> Palette {
            Palette::for_deck(Deck::Truecolor)
        }

        fn text(line: &Line<'_>) -> String {
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        }

        fn line(
            source: CompletionSource,
            state: Option<&str>,
            desc: &str,
            width: usize,
        ) -> Line<'static> {
            completion_line(source, state, desc, width, false, &palette())
        }

        #[test]
        fn the_line_text_follows_the_state_table() {
            let cases = [
                (Shell, Some("completed"), "cmd", "   ↳ Shell finished · cmd"),
                (Shell, Some("error"), "cmd", "   ! Shell failed · cmd"),
                (
                    Subagent,
                    Some("cancelled"),
                    "d",
                    "   ! Subagent cancelled · d",
                ),
                (Shell, Some("weird"), "d", "   ! Shell weird · d"),
                (Shell, None, "d", "   ! Shell finished · d"),
                (
                    Subagent,
                    Some("completed"),
                    "d",
                    "   ↳ Subagent finished · d",
                ),
            ];
            for (source, state, desc, want) in cases {
                assert_eq!(text(&line(source, state, desc, 120)), want, "{state:?}");
            }
        }

        #[test]
        fn an_empty_description_still_draws_the_separator() {
            assert_eq!(
                text(&line(Shell, Some("completed"), "", 40)),
                "   ↳ Shell finished · "
            );
        }

        #[test]
        fn long_descriptions_truncate_to_the_width_with_an_ellipsis() {
            let long = "x".repeat(200);
            for width in [40usize, 120] {
                let rendered = text(&line(Shell, Some("completed"), &long, width));
                assert!(rendered.width() <= width, "{width}: {}", rendered.width());
                assert!(rendered.ends_with('…'), "{rendered}");
                assert_eq!(rendered.width(), width, "fills the budget exactly");
            }
        }

        #[test]
        fn cjk_descriptions_truncate_on_display_width() {
            let cjk = "你好世界你好世界你好世界";
            for width in 30..=40usize {
                let rendered = text(&line(Shell, Some("completed"), cjk, width));
                assert!(rendered.width() <= width, "{width}: {rendered}");
                assert!(rendered.ends_with('…'), "{rendered}");
            }
        }

        #[test]
        fn control_characters_in_the_description_render_as_single_spaces() {
            let rendered = text(&line(Subagent, Some("completed"), "a\nb\tc", 120));
            assert_eq!(rendered, "   ↳ Subagent finished · a b c");
            assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
        }

        #[test]
        fn truncation_with_control_characters_stays_within_the_width() {
            use unicode_width::UnicodeWidthChar;

            let desc = "ab\ncd\tef\r\ngh ijkl mnop";
            for width in 24..=44usize {
                let rendered = text(&line(Subagent, Some("completed"), desc, width));
                let cells: usize = rendered.chars().map(|c| c.width().unwrap_or(0)).sum();
                assert!(cells <= width, "{width}: {cells} {rendered:?}");
                assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
            }
            let cut = text(&line(Subagent, Some("completed"), desc, 34));
            assert_eq!(cut, "   ↳ Subagent finished · ab cd ef…");
        }

        #[test]
        fn a_tiny_budget_drops_the_suffix_instead_of_overflowing() {
            for width in 0..24usize {
                let rendered = text(&line(Shell, Some("completed"), "command", width));
                let title = "   ↳ Shell finished";
                assert!(
                    rendered.width() <= width.max(title.width()),
                    "{width}: {rendered}"
                );
                if width < title.width() + 4 {
                    assert_eq!(rendered, title, "{width}");
                }
            }
        }

        #[test]
        fn the_title_colour_follows_the_state_and_the_suffix_is_muted() {
            let p = palette();
            let fg = |state: Option<&str>| {
                let l = completion_line(Shell, state, "cmd", 80, false, &p);
                let title = l
                    .spans
                    .iter()
                    .find(|s| s.content.contains("Shell"))
                    .unwrap();
                let suffix = l.spans.iter().find(|s| s.content.contains("cmd")).unwrap();
                (title.style, suffix.style)
            };
            let (done, done_suffix) = fg(Some("completed"));
            let (error, _) = fg(Some("error"));
            let (cancelled, _) = fg(Some("cancelled"));
            assert_eq!(done.fg, p.info.fg);
            assert_eq!(error.fg, p.warn.fg);
            assert_eq!(cancelled.fg, p.warning.fg);
            assert_ne!(done.fg, error.fg);
            assert_ne!(done.fg, cancelled.fg);
            assert_ne!(error.fg, cancelled.fg);
            assert_eq!(done_suffix.fg, p.dim.fg);
            assert_ne!(done_suffix.fg, done.fg);
        }

        #[test]
        fn hover_recolours_the_title_only_for_neutral_states() {
            let p = palette();
            let title_fg = |state: Option<&str>, hovered: bool| {
                let l = completion_line(Subagent, state, "cmd", 80, hovered, &p);
                (l.spans[0].style.fg, l.spans[1].style.fg)
            };
            for state in [Some("completed"), Some("weird"), None] {
                let (idle, idle_suffix) = title_fg(state, false);
                let (hot, hot_suffix) = title_fg(state, true);
                assert_eq!(idle, p.info.fg, "{state:?}");
                assert_eq!(
                    hot, p.text.fg,
                    "{state:?} hovered uses the base text colour"
                );
                assert_ne!(idle, hot, "{state:?}");
                assert_eq!(idle_suffix, hot_suffix, "the suffix stays muted");
                assert_eq!(hot_suffix, p.dim.fg);
            }
            // error and cancelled keep their own colour under the pointer.
            assert_eq!(title_fg(Some("error"), true).0, p.warn.fg);
            assert_eq!(title_fg(Some("cancelled"), true).0, p.warning.fg);
        }

        fn subagent(id: &str, state: &str, child: Option<&str>) -> Block {
            Block::Completion {
                id: id.into(),
                source: Subagent,
                state: Some(Arc::from(state)),
                description: Arc::from("review"),
                child_id: child.map(Arc::from),
            }
        }

        /// One block per case, each drawn alone in a 40x6 transcript.
        fn lone(block: Block) -> Transcript {
            let mut t = Transcript::new(40, 6);
            t.sync(&[block], 0);
            screen(&mut t, 40, 6);
            t
        }

        #[test]
        fn child_at_names_the_child_on_the_title_row_of_a_subagent_notice() {
            let t = lone(subagent("a", "completed", Some("ses_child")));
            assert_eq!(t.child_at(5, 1), Some("ses_child"));
            assert_eq!(t.child_at(39, 1), Some("ses_child"), "anywhere on the row");
            assert_eq!(t.child_at(5, 0), None, "row 0 is the margin");
            assert_eq!(t.child_at(5, 2), None, "below the block");
            assert_eq!(t.child_at(40, 1), None, "outside the drawn area");
        }

        #[test]
        fn child_at_ignores_rows_without_a_child() {
            assert_eq!(
                lone(notice("a", "completed", "cargo test")).child_at(5, 1),
                None,
                "a shell notice"
            );
            assert_eq!(
                lone(subagent("a", "completed", None)).child_at(5, 1),
                None,
                "a subagent notice without a child id"
            );
            assert_eq!(lone(Block::user("hi")).child_at(5, 1), None);
        }

        fn title_fg(t: &mut Transcript) -> Option<ratatui::style::Color> {
            use ratatui::{Terminal, backend::TestBackend};
            let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
            terminal.draw(|f| t.render(f, f.area())).unwrap();
            // Column 5 is inside `Subagent` on the title row.
            terminal.backend().buffer()[(5, 1)].style().fg
        }

        #[test]
        fn hovering_a_subagent_title_lights_it_up_unless_it_failed_or_was_cancelled() {
            let base = crate::theme::palette();
            let mut t = lone(subagent("a", "completed", Some("ses_child")));
            assert_eq!(title_fg(&mut t), base.info.fg);
            assert!(t.hover(5, 1), "entering the row is a change");
            assert_eq!(t.hovered, Some(0));
            assert_eq!(title_fg(&mut t), base.text.fg);
            assert!(!t.hover(9, 1), "same row again: nothing to redraw");
            assert!(t.hover(5, 0), "the margin is not a target");
            assert_eq!(title_fg(&mut t), base.info.fg);

            let mut t = lone(subagent("a", "error", Some("ses_child")));
            t.hover(5, 1);
            assert_eq!(title_fg(&mut t), base.warn.fg);
            let mut t = lone(subagent("a", "cancelled", Some("ses_child")));
            t.hover(5, 1);
            assert_eq!(title_fg(&mut t), base.warning.fg);
        }

        #[test]
        fn rows_without_a_child_never_hover() {
            let mut t = lone(notice("a", "completed", "cargo test"));
            assert!(!t.hover(5, 1), "a shell row");
            let mut t = lone(subagent("a", "completed", None));
            assert!(!t.hover(5, 1), "no child id");
            assert_eq!(t.hovered, None);
        }

        fn notice(id: &str, state: &str, desc: &str) -> Block {
            Block::Completion {
                id: id.into(),
                source: Shell,
                state: Some(Arc::from(state)),
                description: Arc::from(desc),
                child_id: None,
            }
        }

        #[test]
        fn equality_is_by_id_only() {
            assert_eq!(notice("a", "completed", "x"), notice("a", "error", "y"));
            assert_ne!(notice("a", "completed", "x"), notice("b", "completed", "x"));
        }

        #[test]
        fn a_transcript_draws_the_notice_one_blank_row_below_the_top() {
            let mut t = Transcript::new(40, 4);
            t.sync(&[notice("a", "completed", "cargo test")], 0);
            let rows: Vec<String> = screen(&mut t, 40, 4).lines().map(str::to_owned).collect();
            assert_eq!(rows[0], "", "margin row");
            assert_eq!(rows[1], "   ↳ Shell finished · cargo test");
        }
    }
}
