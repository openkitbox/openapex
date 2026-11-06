use std::sync::Arc;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::graphics::Graphics;
use crate::graphics::Prepared;
use crate::graphics::Slot;
use crate::theme;
use crate::ui::media;

use super::block::Block;
use super::entry::{Entry, entry_expandable};
use super::render::{self, retry_secs, wall_ms_now};
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
    /// Rows one attachment thumbnail is tall, derived from `height`
    /// (`media::height`); part of the layout key, so a change re-lays blocks
    /// out like a width change.
    thumb_height: u16,
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
            thumb_height: media::height(height),
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
            | Block::BackgroundHint
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
            | Block::BackgroundHint
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
            Block::Switch(_)
            | Block::Notice(_)
            | Block::Error(_)
            | Block::CompactionQueued
            | Block::BackgroundHint => ViewState::default(),
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
        let metrics = self.metrics();
        self.entries[index] = Entry::new(block, generation, metrics, view);
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
        let metrics = self.metrics();
        for ((entry, block), view) in self.entries[start..end]
            .iter_mut()
            .zip(&blocks[start..end])
            .zip(views)
        {
            let rows = entry.lines.len();
            if entry.block != *block || entry.view != view {
                let generation = entry.generation + 1;
                *entry = Entry::new(block.clone(), generation, metrics, view);
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
                    .push(Entry::new(block.clone(), 0, metrics, view));
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
                Block::User { text, .. } => Some((index, &**text)),
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
        let thumb_height = media::height(height);
        if self.width == width && self.thumb_height == thumb_height {
            self.clamp();
            return;
        }
        // A thumbnail-height change renumbers lines too: a block's strip rows
        // are part of its line list, so a taller band pushes every later match
        // down even at an unchanged width.
        self.width = width;
        self.thumb_height = thumb_height;
        // Re-wrapping renumbers lines, so a selection no longer points at
        // what it selected.
        self.selection = None;
        // Re-wrap eagerly: `search.matches` holds absolute wrapped-line
        // numbers, and a width change renumbers every line after the first
        // one that now wraps differently. Costs nothing on the frames that
        // keep the width (the guard above), and makes `render`'s own
        // `lines_for` calls no-ops for this pass.
        let metrics = self.metrics();
        for entry in &mut self.entries {
            entry.lines_for(metrics);
        }
        if !self.search.query.is_empty() {
            self.recompute_search();
        }
        self.clamp();
    }

    /// The frame every block is laid out for: the columns it may use and the
    /// rows one attachment thumbnail is tall.
    fn metrics(&self) -> render::Metrics {
        render::Metrics {
            width: self.width,
            thumb_height: self.thumb_height,
        }
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

    /// Draw the transcript into `area`, painting the attachment thumbnails the
    /// graphics layer already holds.
    ///
    /// The escape sequences a kitty transmission needs are written from here,
    /// before the frame's cells: a placeholder cell can only show an image the
    /// terminal has, and the frame that first names one is the frame this
    /// queues. Returns the thumbnails still to fetch, which the caller asks for
    /// once the frame is done.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        graphics: &mut Graphics,
    ) -> Vec<media::Request> {
        self.last_area = area;
        self.set_viewport(area.width, area.height);
        self.clamp();

        let metrics = self.metrics();
        let mut remaining_skip = self.scroll;
        let mut y = area.y;
        let mut base = 0usize;
        let current_match = self.current_line();
        // Where each visible thumbnail strip starts, by entry index: its rows
        // are drawn with the text, the pixels after it.
        let mut strips: Vec<(usize, i32)> = Vec::new();

        let selection = self.selection.filter(|s| !s.is_empty());
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let (head, tail) = text_rows(&entry.block);
            // The strip's geometry is a value; the images stay behind the
            // borrow `lines_for` needs.
            let strip = entry.thumbs.as_ref().map(|thumbs| thumbs.strip);
            let lines = entry.lines_for(metrics);
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
            if let Some(strip) = strip {
                // The screen row the strip's first image row lands on; it may
                // be above the viewport when the entry is scrolled partly off.
                let top = i32::from(y) + strip.at as i32 - skip as i32;
                strips.push((index, top));
            }
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
        self.paint_thumbs(frame, area, &strips, graphics)
    }

    /// The thumbnail a click at screen cell (`x`, `y`) landed on: the strip it
    /// belongs to (every image it draws, in strip order) and which of them was
    /// hit.
    ///
    /// Geometry mirrors [`Self::paint_thumbs`] rather than re-rendering: an
    /// entry's rows only move when `sync`, `rebuild` or a resize re-lays them
    /// out, and a click follows the frame that was just drawn.
    #[must_use]
    pub fn image_at(&self, x: u16, y: u16) -> Option<(&[media::Image], usize)> {
        let area = self.last_area;
        if !cell_in(area, x, y) {
            return None;
        }
        let mut base = 0usize;
        for entry in &self.entries {
            let count = entry.lines.len();
            let Some(thumbs) = &entry.thumbs else {
                base += count;
                continue;
            };
            let top = i32::from(area.y) + (base + thumbs.strip.at) as i32 - self.scroll as i32;
            let Some(layout) = strip_layout(area, thumbs.strip, top) else {
                base += count;
                continue;
            };
            for position in 0..usize::from(layout.cells) {
                let Ok(position) = u16::try_from(position) else {
                    break;
                };
                if cell_in(layout.cell(&thumbs.strip, position), x, y) {
                    return Some((&thumbs.images, usize::from(position)));
                }
            }
            // `+N more` opens at the first image the band does not draw.
            if layout.more.is_some_and(|more| cell_in(more, x, y)) {
                return Some((&thumbs.images, layout.cells.into()));
            }
            base += count;
        }
        None
    }

    /// Paint the cached thumbnails of `strips` into the frame and report the
    /// ones whose bytes are still missing.
    ///
    /// Only a thumbnail whose cells are all inside `area` is drawn: a kitty
    /// placeholder names its row and column inside the image, so a clipped
    /// corner would have to be drawn with offsets the encoder does not carry.
    /// Half-visible images stay blank until the scroll brings them in.
    fn paint_thumbs(
        &self,
        frame: &mut Frame,
        area: Rect,
        strips: &[(usize, i32)],
        graphics: &mut Graphics,
    ) -> Vec<media::Request> {
        let mut prepared: Vec<Arc<Prepared>> = Vec::new();
        let mut areas: Vec<Rect> = Vec::new();
        let mut requests: Vec<media::Request> = Vec::new();
        for (index, top) in strips {
            let Some(thumbs) = &self.entries[*index].thumbs else {
                continue;
            };
            let Some(layout) = strip_layout(area, thumbs.strip, *top) else {
                continue;
            };
            for position in 0..usize::from(layout.cells) {
                let Ok(position) = u16::try_from(position) else {
                    break;
                };
                let Some(image) = thumbs.images.get(usize::from(position)) else {
                    break;
                };
                let cell = layout.cell(&thumbs.strip, position);
                match graphics.cached_for(&image.key, thumbs.strip.cells()) {
                    Some(image) => {
                        prepared.push(image);
                        areas.push(cell);
                    }
                    None => requests.push(media::Request {
                        key: image.key.clone(),
                        bytes: image.bytes.clone(),
                        cells: thumbs.strip.cells(),
                        class: media::THUMBNAIL,
                    }),
                }
            }
            if let Some(more) = layout.more {
                draw_more(frame, more, thumbs.strip.more);
            }
        }
        let slots: Vec<Slot<'_>> = prepared
            .iter()
            .zip(&areas)
            .map(|(image, area)| Slot { area: *area, image })
            .collect();
        graphics.paint(frame.buffer_mut(), &slots);
        if let Err(err) = graphics.flush() {
            tracing::warn!(%err, "queued graphics escapes not written");
        }
        requests
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

/// What a strip whose first image row lands on screen row `top` shows, or
/// `None` when the band is not wholly inside `area` (scrolled partly out of
/// view): a kitty placeholder names its row and column inside the image, so a
/// clipped corner cannot be drawn with the offsets the encoder carries.
fn strip_layout(area: Rect, strip: media::Strip, top: i32) -> Option<media::Layout> {
    if top < i32::from(area.y) {
        return None;
    }
    let y = u16::try_from(top).ok()?;
    (y.saturating_add(strip.height) <= area.bottom()).then(|| strip.layout_in(area, y))
}

/// Upstream's `+N more` label, dim and centred in its box
/// (`routes/session/index.tsx:2676`).
fn draw_more(frame: &mut Frame, more: Rect, remaining: usize) {
    let label = format!("+{remaining} more");
    let width = u16::try_from(label.width())
        .unwrap_or(more.width)
        .min(more.width);
    frame.render_widget(
        Paragraph::new(Span::styled(label, theme::palette().dim)),
        Rect {
            x: more.x + (more.width - width) / 2,
            y: more.y,
            width,
            height: 1,
        },
    );
}

/// Whether screen cell (`x`, `y`) falls inside `rect`.
fn cell_in(rect: Rect, x: u16, y: u16) -> bool {
    rect.contains(Position::new(x, y))
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
