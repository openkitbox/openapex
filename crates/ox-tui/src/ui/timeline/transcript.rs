use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;

use super::block::Block;
use super::entry::{Entry, entry_expandable};
use super::render::{retry_secs, wall_ms_now};
use super::text::{highlight, line_text, text_rows};
use super::view::{
    SPINNER_FRAMES, SPINNER_INTERVAL_MS, ViewState, clickable, entry_tool_id, spins,
    tool_expandable,
};
use crate::ui::selection::{self, Behavior, ClickTracker, Outcome, Pos, Selection};

/// Blank rows kept under the last block (`routes/session/index.tsx` scrollbox
/// content padding).
const BOTTOM_PAD: usize = 1;

/// The `Jump to latest ↓` affordance (upstream `routes/session/index.tsx`
/// `session-jump-to-latest`): drawn over the last row of the scroll area while
/// the view is scrolled away from the bottom.
const JUMP_LABEL: &str = "Jump to latest ↓";

/// Columns kept free between the label and the right edge: upstream pads the
/// box on its left, and the label is right-aligned here instead.
const JUMP_RIGHT_PAD: u16 = 1;

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
    /// Wall clock (Unix ms) retry countdowns are derived from; refreshed on
    /// `sync` and `tick`.
    wall_ms: i64,
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
    #[must_use]
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
            wall_ms: wall_ms_now(),
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

    /// The row the affordance is drawn on while it shows: the last row of the
    /// transcript area. `None` at the bottom (nothing to jump to), before the
    /// first draw, and when the area is too narrow for the label.
    fn jump_row(&self) -> Option<u16> {
        let area = self.last_area;
        if self.scroll >= self.max_scroll()
            || area.width < JUMP_LABEL.chars().count() as u16 + JUMP_RIGHT_PAD
        {
            return None;
        }
        Some(area.y + area.height.checked_sub(1)?)
    }

    /// Screen cells of the affordance: the label right-aligned on `jump_row`,
    /// one column short of the right edge.
    fn jump_rect(&self) -> Option<Rect> {
        // `jump_row` proves the label fits before the subtraction below runs.
        let y = self.jump_row()?;
        let area = self.last_area;
        let width = JUMP_LABEL.chars().count() as u16;
        Some(Rect {
            x: area.x + area.width - width - JUMP_RIGHT_PAD,
            y,
            width,
            height: 1,
        })
    }

    /// Whether screen cell (`x`, `y`) is on the affordance's label, i.e. the
    /// button is drawn there. Only the label's own columns are the button; the
    /// rest of its row is the content drawn underneath.
    fn on_jump_label(&self, x: u16, y: u16) -> bool {
        self.jump_rect().is_some_and(|rect| cell_in(rect, x, y))
    }

    /// Map screen cell (`x`, `y`) to `(entry index, row within the block)`.
    /// `None` outside the last drawn transcript area, below the content, and on
    /// the columns of the `Jump to latest` label. The rest of that row maps to
    /// its content as usual: a drag that ends there still selects it, since
    /// `focus_at` does not consult the label.
    fn locate(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self.last_area;
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return None;
        }
        // The label is a button over the row's last sixteen columns; whatever
        // is drawn beside it keeps its click, hover and child-link behaviour.
        if self.on_jump_label(x, y) {
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
    /// `group-view.tsx:267-284`); on a failed tool or todo row it shows or
    /// hides the error (upstream `InlineTool` `setErrorExpanded`); on a
    /// generic tool row it lists or hides the input and output (upstream
    /// `GenericTool` `onClick`). Returns whether anything changed.
    pub fn click(&mut self, x: u16, y: u16) -> bool {
        let Some((index, row)) = self.locate(x, y) else {
            return false;
        };
        // Row 0 is the block's blank margin. A shell block is one click
        // target (upstream `BlockTool` `onMouseUp`); everything else answers
        // on its header row only.
        if let Block::Shell { .. } = &self.entries[index].block {
            return row > 0 && self.toggle_shell(index);
        }
        if row != 1 {
            return false;
        }
        match &self.entries[index].block {
            Block::Reasoning { .. } => self.toggle_reasoning_at(index),
            Block::Explored(explored) => {
                let id = explored.group_id.clone();
                if !self.explored_expanded.remove(&id) {
                    self.explored_expanded.insert(id);
                }
                self.rebuild(index);
            }
            block @ (Block::Tool { .. } | Block::Todo { .. }) => {
                if !clickable(block) {
                    return false;
                }
                let Some(id) = entry_tool_id(block).map(str::to_owned) else {
                    return false;
                };
                if !self.tool_expanded.remove(&id) {
                    self.tool_expanded.insert(id);
                }
                self.rebuild(index);
            }
            Block::User { .. }
            | Block::Assistant { .. }
            | Block::Switch(_)
            | Block::Notice(_)
            | Block::Error(_)
            | Block::CompactionQueued
            | Block::Compaction { .. }
            | Block::Completion { .. }
            | Block::Shell { .. } => return false,
        }
        true
    }

    /// Open or close an expandable shell block's full command and output.
    fn toggle_shell(&mut self, index: usize) -> bool {
        let entry = &self.entries[index];
        let Block::Shell { id, .. } = &entry.block else {
            return false;
        };
        if !entry.expandable {
            return false;
        }
        if !self.tool_expanded.remove(id) {
            self.tool_expanded.insert(id.clone());
        }
        self.rebuild(index);
        true
    }

    /// The child session a click at screen cell (`x`, `y`) opens: the title
    /// row of a Completion notice that carries a `child_id` (upstream
    /// `routes/session/index.tsx:2090-2151`). `None` anywhere else.
    #[must_use]
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
            Block::Shell { .. } => (row > 0 && self.entries[index].expandable).then_some(index),
            Block::Assistant { .. }
            | Block::Switch(_)
            | Block::Notice(_)
            | Block::Error(_)
            | Block::CompactionQueued
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
    /// hovered entry — or the affordance's highlight — changed.
    pub fn hover(&mut self, x: u16, y: u16) -> bool {
        let was_jump = self.pointer.is_some_and(|(x, y)| self.on_jump_label(x, y));
        self.pointer = Some((x, y));
        let target = self.hover_target(x, y);
        let moved = self.set_hover(target);
        // The affordance highlights from the pointer (see `render_jump`), so a
        // move onto or off its label is a repaint even when no entry changed.
        moved || self.on_jump_label(x, y) != was_jump
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
        // The affordance's highlight is derived from the pointer, so the
        // pointer goes with the hover; `rehover` then has no stale cell to
        // re-evaluate when a wheel arrives.
        self.pointer = None;
        self.set_hover(None)
    }

    /// Whether a non-empty range is selected (and therefore drawn).
    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_empty())
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
    /// whether a repaint is due: a visible selection was dropped, or the
    /// affordance sent the view back to the bottom.
    pub(crate) fn press(&mut self, x: u16, y: u16, now: Instant) -> bool {
        // The affordance is a button on the label's own columns: a press there
        // goes back to the bottom and re-engages the tail instead of starting a
        // selection. Upstream fires it from `onMouseUp={toBottom}`; ox
        // activates on press instead — a deliberate deviation — so a drag
        // cannot carry the activation away from the label.
        if self.on_jump_label(x, y) {
            self.clear_selection();
            // The row under the pointer now holds different content: forget the
            // cell so a later `rehover` does not light a row it is not over.
            self.pointer = None;
            self.scroll_to_bottom();
            return true;
        }
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

    /// Advance the spinner of streaming reasoning blocks, running tool and
    /// todo rows, running Explored groups, and background subagent rows. Only those blocks are
    /// rebuilt. `true` when a visible frame changed and a repaint is due.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        self.tick_at(now, wall_ms_now())
    }

    /// `tick` with the wall clock given: also re-renders retry countdowns
    /// whose displayed second changed (upstream `AssistantRetry` 1s timer).
    fn tick_at(&mut self, now: std::time::Instant, wall_ms: i64) -> bool {
        self.wall_ms = wall_ms;
        // Index loop: no per-tick allocation, and nothing to do when no
        // entry is retrying.
        let mut counted = false;
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            if entry.view.retry_secs.is_some()
                && self.view_of(index, &entry.block).retry_secs != entry.view.retry_secs
            {
                self.rebuild(index);
                counted = true;
            }
        }
        let streaming: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| spins(&entry.block))
            .map(|(index, _)| index)
            .collect();
        if streaming.is_empty() {
            return counted;
        }
        let elapsed = now.saturating_duration_since(self.spin_epoch).as_millis();
        let frame = (elapsed / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        if frame == self.spin_frame {
            return counted;
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
            .filter(|(_, entry)| entry_expandable(entry))
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

    /// Whether tool part `id`'s output is expanded.
    #[must_use]
    pub fn is_tool_expanded(&self, id: &str) -> bool {
        self.tool_expanded.contains(id)
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
            Block::Reasoning { id, .. } => ViewState {
                reasoning_expanded: self.expanded.contains(id),
                hovered,
                spin: if spins(block) { self.spin_frame } else { 0 },
                ..Default::default()
            },
            Block::Tool { id, .. } => ViewState {
                tool_expanded: tool_expandable(block) && self.tool_expanded.contains(id),
                tool_focused: self.focused_tool.as_deref() == Some(id.as_str()),
                spin: if spins(block) { self.spin_frame } else { 0 },
                ..Default::default()
            },
            Block::Shell { id, .. } => ViewState {
                tool_expanded: self.tool_expanded.contains(id),
                tool_focused: self.focused_tool.as_deref() == Some(id.as_str()),
                hovered,
                spin: if spins(block) { self.spin_frame } else { 0 },
                ..Default::default()
            },
            Block::Todo { id, output, .. } => ViewState {
                tool_expanded: !output.is_empty() && self.tool_expanded.contains(id),
                tool_focused: self.focused_tool.as_deref() == Some(id.as_str()),
                spin: if spins(block) { self.spin_frame } else { 0 },
                ..Default::default()
            },
            Block::Explored(explored) => ViewState {
                explored_expanded: self.explored_expanded.contains(&explored.group_id),
                hovered,
                spin: if spins(block) { self.spin_frame } else { 0 },
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
            Block::Assistant { footer, .. } => ViewState {
                retry_secs: footer
                    .as_ref()
                    .and_then(|footer| footer.retry.as_ref())
                    .map(|retry| retry_secs(retry.at, self.wall_ms)),
                ..Default::default()
            },
            Block::Compaction { .. } => ViewState {
                spin: if spins(block) { self.spin_frame } else { 0 },
                ..Default::default()
            },
            Block::Switch(_) | Block::Notice(_) | Block::Error(_) | Block::CompactionQueued => {
                ViewState::default()
            }
        }
    }

    fn rebuild(&mut self, index: usize) {
        let view = self.view_of(index, &self.entries[index].block);
        let generation = self.entries[index].generation + 1;
        if self.entries[index].respin(view) {
            self.entries[index].generation = generation;
            return;
        }
        let block = self.entries[index].block.clone();
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

    /// Diff incoming blocks against current entries. `first_changed_block` is a
    /// BLOCK index — `pipeline::Projection::first_block`, derived from
    /// `DataStore::claim_changed_from()` — not the store's item index: a turn
    /// item draws no block, so the two only coincide by accident. Everything
    /// before it is never even compared. Changed blocks get a new generation;
    /// unchanged keep both cache and generation.
    pub fn sync(&mut self, blocks: &[Block], first_changed_block: usize) {
        self.wall_ms = wall_ms_now();
        let start = first_changed_block
            .min(self.entries.len())
            .min(blocks.len());

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

    #[must_use]
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
    #[must_use]
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

    #[must_use]
    pub fn entry_generation(&self, index: usize) -> u64 {
        self.entries[index].generation
    }

    #[must_use]
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

        if let Some(rect) = self.jump_rect() {
            self.render_jump(frame, rect);
        }
    }

    /// Draw the `Jump to latest ↓` label into `rect`, dim until the pointer
    /// rests on the label and bright after (upstream `text.action.secondary`
    /// base / hovered).
    fn render_jump(&self, frame: &mut Frame, rect: Rect) {
        let hovered = self.pointer.is_some_and(|(x, y)| cell_in(rect, x, y));
        let style = if hovered {
            theme::palette().text
        } else {
            theme::palette().dim
        };
        let line = Line::from(Span::styled(JUMP_LABEL, style));
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Right), rect);
    }
}

/// Whether screen cell (`x`, `y`) falls inside `rect`.
fn cell_in(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pipeline;
    use crate::shell_captures::ShellCaptures;
    use ox_client::store;
    use ox_client::store::DataStore;
    use ox_schema::event::EventEnvelope;
    use ox_schema::event::SessionEventData;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 8;
    /// Narrower than the label plus its right padding.
    const NARROW: u16 = 12;

    /// One frame of `transcript` in a `width` × `height` terminal, returning the
    /// cells it drew.
    fn draw_at(transcript: &mut Transcript, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| transcript.render(frame, frame.area()))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// `draw_at` in the standard fixture viewport.
    fn draw(transcript: &mut Transcript) -> Buffer {
        draw_at(transcript, WIDTH, HEIGHT)
    }

    /// The text of screen row `y`.
    fn row_text(buffer: &Buffer, y: u16) -> String {
        let mut text = String::new();
        for x in 0..buffer.area.width {
            text.push_str(buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()));
        }
        text
    }

    /// Twelve one-line user blocks: taller than `HEIGHT`, so there is room to
    /// scroll away from the bottom.
    fn overflowing() -> Transcript {
        let blocks: Vec<Block> = (0..12)
            .map(|index| Block::user(&format!("message {index}")))
            .collect();
        let mut transcript = Transcript::new(WIDTH, HEIGHT);
        transcript.sync(&blocks, 0);
        transcript
    }

    /// The affordance shows exactly while the view is scrolled away from the
    /// bottom, and activating it returns the scroll to the bottom.
    #[test]
    fn jump_to_latest_shows_away_from_bottom_and_returns_to_it() {
        let bottom_row = HEIGHT - 1;
        let mut transcript = overflowing();
        let max = transcript.max_scroll();
        assert!(
            max > 1,
            "the fixture must overflow the viewport, max was {max}"
        );

        // One row up from the bottom is enough to show it.
        transcript.scroll_up(1);
        assert_eq!(transcript.scroll, max - 1, "one row above the bottom");
        let buffer = draw(&mut transcript);
        assert!(
            row_text(&buffer, bottom_row).contains(JUMP_LABEL),
            "a view scrolled away from the bottom shows the label, bottom row was {:?}",
            row_text(&buffer, bottom_row)
        );
        assert_eq!(
            buffer
                .cell((WIDTH - 1, bottom_row))
                .map(|cell| cell.symbol()),
            Some(" "),
            "the label keeps one column of padding from the right edge"
        );

        // At the bottom there is nothing to jump to.
        transcript.scroll_to_bottom();
        let buffer = draw(&mut transcript);
        assert!(
            !row_text(&buffer, bottom_row).contains(JUMP_LABEL),
            "the label is hidden at the bottom, bottom row was {:?}",
            row_text(&buffer, bottom_row)
        );

        // The label is a button: a press on it returns to the bottom without
        // starting a selection, and the moved view must repaint.
        transcript.scroll_up(2);
        draw(&mut transcript);
        let label_x = WIDTH - JUMP_RIGHT_PAD - JUMP_LABEL.chars().count() as u16 / 2;
        assert!(
            transcript.press(label_x, bottom_row, Instant::now()),
            "the press scrolls away from the label, so a repaint is due"
        );
        assert_eq!(
            transcript.scroll,
            transcript.max_scroll(),
            "the press returns the view to the bottom"
        );
        assert!(
            transcript.selection.is_none(),
            "the press starts no text selection"
        );
    }

    /// Only the label's own columns are the button: the rest of its row is the
    /// content drawn underneath, which keeps its hover and click behaviour.
    #[test]
    fn jump_to_latest_surrenders_only_the_labels_columns() {
        // Twelve one-line user blocks and a collapsed reasoning block whose
        // header is the last line: one row above the bottom, the header sits on
        // the row the label is drawn over.
        let mut blocks: Vec<Block> = (0..12)
            .map(|index| Block::user(&format!("message {index}")))
            .collect();
        blocks.push(Block::reasoning("r1", 0, "looking", true));
        let mut transcript = Transcript::new(WIDTH, HEIGHT);
        transcript.sync(&blocks, 0);
        let max = transcript.max_scroll();
        transcript.scroll_up(1);
        assert_eq!(transcript.scroll, max - 1, "one row above the bottom");

        let bottom_row = HEIGHT - 1;
        let buffer = draw(&mut transcript);
        assert!(
            row_text(&buffer, bottom_row).contains(JUMP_LABEL),
            "the label shows on the last row, bottom row was {:?}",
            row_text(&buffer, bottom_row)
        );
        let label_x = WIDTH - JUMP_RIGHT_PAD - JUMP_LABEL.chars().count() as u16 / 2;
        let content_x = 0;
        let reasoning = Some(12);

        // Left of the label the reasoning header is still the hover target.
        assert!(
            transcript.hover(content_x, bottom_row),
            "the header under the label's row lights up"
        );
        assert_eq!(transcript.hovered, reasoning, "the header, not the label");
        // And a click there still toggles the block.
        assert!(
            transcript.click(content_x, bottom_row),
            "a click beside the label opens the reasoning block"
        );
        assert!(
            transcript.expanded.contains("r1"),
            "the reasoning body is expanded, expanded was {:?}",
            transcript.expanded
        );

        // On the label's own cells neither goes through: it is a button.
        assert!(
            !transcript.click(label_x, bottom_row),
            "the label is not block content"
        );
    }

    /// A viewport too narrow for the label drops it instead of underflowing the
    /// label's own arithmetic.
    #[test]
    fn jump_to_latest_is_dropped_when_the_viewport_is_too_narrow() {
        let mut transcript = overflowing();
        transcript.set_viewport(NARROW, HEIGHT);
        transcript.scroll_up(1);
        let buffer = draw_at(&mut transcript, NARROW, HEIGHT);
        let last = row_text(&buffer, HEIGHT - 1);
        assert!(
            !last.contains("Jump"),
            "no label fits in {NARROW} columns, bottom row was {last:?}"
        );
    }

    /// One raw frame through the socket feed path.
    fn event(frame: serde_json::Value) -> EventEnvelope<SessionEventData> {
        let envelope: EventEnvelope<serde_json::Value> =
            serde_json::from_value(frame).expect("a frame deserializes");
        envelope.parse_typed()
    }

    /// The live answer of the assistant message a prompt opened must keep
    /// streaming into its own block. The turn row renders nothing, so the item
    /// index the store records (2) is one past the block index of the answer
    /// (1): syncing from the item index would skip the answer and freeze it on
    /// its first chunk.
    #[test]
    fn a_changed_answer_after_a_turn_row_reaches_its_block() {
        let mut store = DataStore::default();
        store.push_user("go");
        store::reduce(
            &mut store,
            &event(serde_json::json!({
                "id": "evt_started",
                "created": 1_000,
                "type": "session.execution.started",
                "data": { "sessionID": "ses_1" },
            })),
        );
        let delta = |id: &str, text: &str| {
            event(serde_json::json!({
                "id": id,
                "created": 1_100,
                "type": "session.text.delta",
                "data": {
                    "sessionID": "ses_1",
                    "assistantMessageID": "msg_a",
                    "ordinal": 0,
                    "delta": text,
                },
            }))
        };
        store::reduce(&mut store, &delta("evt_d1", "hel"));

        let mut transcript = Transcript::new(WIDTH, HEIGHT);
        let changed_from = store.claim_changed_from();
        let projection = project(&store, changed_from);
        transcript.sync(&projection.blocks, projection.first_block);
        assert_eq!(
            answer_text(&transcript, 1),
            "hel",
            "the answer's block is index 1: the prompt row is 0 and the turn row draws nothing"
        );

        // The next delta touches only the assistant item (index 2).
        store::reduce(&mut store, &delta("evt_d2", "lo"));
        let changed_from = store.claim_changed_from();
        assert_eq!(changed_from, 2, "the store records ITEM indices");
        let projection = project(&store, changed_from);
        transcript.sync(&projection.blocks, projection.first_block);
        assert_eq!(
            answer_text(&transcript, 1),
            "hello",
            "the delta reaches the answer's block, not past it"
        );
    }

    /// The store's items projected through the real pipeline.
    fn project(store: &DataStore, changed_from: usize) -> pipeline::Projection {
        pipeline::blocks_from_store(store, &ShellCaptures::default(), changed_from)
    }

    /// The text of the assistant block at block index `index`.
    fn answer_text(transcript: &Transcript, index: usize) -> String {
        match &transcript.entries[index].block {
            Block::Assistant { text, .. } => text.to_string(),
            other => panic!("block {index} is not an answer: {other:?}"),
        }
    }
}
