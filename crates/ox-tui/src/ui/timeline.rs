use std::borrow::Cow;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_client::store::{
    CompactionStatus, CompletionSource, ErrorInfo, ModelChoice, Notice, RetryInfo, Switch,
    ToolState,
};
use ox_client::todo::Todo;
use ox_schema::support::{Record, TokenUsage};

use crate::theme;

use super::block_tool;
use super::patch_block::PatchCall;
use super::selection::{self, Behavior, ClickTracker, Outcome, Pos, Selection};
use super::tool_row::{self, InlineSpec, RowView};
use super::tool_text::{self, ToolText};

/// The rows closing a finished (or retrying) turn (upstream `AssistantFooter`,
/// `routes/session/index.tsx:2000`): the step error, the retry countdown and
/// the stats line.
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
    /// Why the message's last step failed. `Step interrupted` is not shown as
    /// an error; it marks the stats line ` · interrupted`.
    pub error: Option<ErrorInfo>,
    /// A scheduled retry; its countdown is derived from `at` when drawn.
    pub retry: Option<RetryInfo>,
}

/// One tool call folded into an `Explored` group; what the expanded row lists
/// (upstream `group-view.tsx:129-157`, `children("tool")`). Fields mirror
/// `Block::Tool`; `input`/`metadata` are shared with the `ToolPart`.
#[derive(Debug, Clone)]
pub struct ExploredCall {
    pub id: String,
    /// `ToolPart::version` — equality is `(id, version)`.
    pub version: u64,
    pub tool: String,
    pub state: ToolState,
    pub input: std::sync::Arc<Record>,
    pub metadata: Option<std::sync::Arc<Record>>,
}

/// One `2 reads` entry of an Explored header: the singular noun upstream
/// counts under (`read`, `search`, `fetch`) and how many calls had it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExploredCount {
    pub noun: &'static str,
    pub count: u32,
}

/// Counts behind one `Explored: 1 read` row (upstream `group-view.tsx`) and
/// the calls they were counted from.
#[derive(Debug, Clone, Default)]
pub struct Explored {
    /// Per-noun counts in order of first appearance in the group.
    pub counts: Vec<ExploredCount>,
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
        self.counts == other.counts
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
    /// An agent / model / location change (upstream `SessionSwitchMessageV2`).
    Switch(Switch),
    /// A system notice or a loaded skill (upstream `SessionNoticeMessageV2`,
    /// `SessionSkillMessage`).
    Notice(Notice),
    /// A run failure no step or compaction already shows: `Error: message`.
    Error(ErrorInfo),
    /// A compaction waiting in the inbox (upstream `CompactionQueued`).
    CompactionQueued,
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
        /// A subagent call whose child session is still running. Not part of
        /// the part, so a child starting or ending does not bump `version`:
        /// equality compares it too.
        background: bool,
        /// The parsed call input, shared with the `ToolPart` (per-tool
        /// texts read their fields from it).
        input: std::sync::Arc<Record>,
        /// The result metadata, shared with the `ToolPart`.
        metadata: Option<std::sync::Arc<Record>>,
    },
    /// A `shell` call, drawn as a block (`super::shell_block`, upstream
    /// `Shell`). `input`/`output` are shared with the `ToolPart`.
    Shell {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)` plus the two
        /// store-derived flags below.
        version: u64,
        state: ToolState,
        /// The parsed call input (`command`, `workdir`).
        input: std::sync::Arc<Record>,
        /// The text shown: the tool result (output or failure message), or
        /// the bound shell's fetched output (`crate::shell_captures`).
        /// Equality compares the pointer: a new capture is a new `Arc`.
        output: std::sync::Arc<String>,
        /// `output` lacks the shell's earliest bytes.
        omitted: bool,
        /// The result names a `shellID` (upstream `Boolean(shellID)`).
        shell: bool,
        /// The call returned while its shell kept running
        /// (`metadata.status == "running"`).
        background: bool,
        /// The bound shell is alive (`DataStore::is_shell_running`). Not part
        /// of the part, so equality compares it.
        shell_running: bool,
    },
    /// A `todowrite` call: the `# Todos` panel once it completed with a
    /// non-empty list, an inline row otherwise (upstream `TodoWrite`).
    Todo {
        id: String,
        /// `ToolPart::version` — equality is `(id, version)`.
        version: u64,
        state: ToolState,
        todos: std::sync::Arc<Vec<Todo>>,
        /// The failure message of a failed call, shared with the `ToolPart`.
        output: std::sync::Arc<String>,
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
    #[must_use]
    pub fn user(text: &str) -> Self {
        Block::User {
            text: std::sync::Arc::from(text),
        }
    }
    #[must_use]
    pub fn assistant(id: &str, version: u64, text: &str) -> Self {
        Block::Assistant {
            id: id.to_owned(),
            version,
            text: std::sync::Arc::new(text.to_owned()),
            footer: None,
        }
    }
    #[must_use]
    pub fn reasoning(id: &str, version: u64, text: &str, done: bool) -> Self {
        Block::Reasoning {
            id: id.to_owned(),
            version,
            text: std::sync::Arc::new(text.to_owned()),
            done,
            ms: None,
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
            // Short `Arc<str>` fields; immutable once built.
            (Block::Switch(a), Block::Switch(b)) => a == b,
            (Block::Notice(a), Block::Notice(b)) => a == b,
            (Block::Error(a), Block::Error(b)) => {
                std::sync::Arc::ptr_eq(&a.message, &b.message) || a == b
            }
            (Block::CompactionQueued, Block::CompactionQueued) => true,
            (Block::Explored(a), Block::Explored(b)) => a == b,
            (
                Block::Tool {
                    id: aid,
                    version: aversion,
                    background: abackground,
                    ..
                },
                Block::Tool {
                    id: bid,
                    version: bversion,
                    background: bbackground,
                    ..
                },
            ) => aid == bid && aversion == bversion && abackground == bbackground,
            (
                Block::Shell {
                    id: aid,
                    version: aversion,
                    output: aoutput,
                    omitted: aomitted,
                    background: abackground,
                    shell_running: arunning,
                    ..
                },
                Block::Shell {
                    id: bid,
                    version: bversion,
                    output: boutput,
                    omitted: bomitted,
                    background: bbackground,
                    shell_running: brunning,
                    ..
                },
            ) => {
                aid == bid
                    && aversion == bversion
                    && std::sync::Arc::ptr_eq(aoutput, boutput)
                    && aomitted == bomitted
                    && abackground == bbackground
                    && arunning == brunning
            }
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
    /// The spinner frame of a block that spins (see `spins`).
    spin: usize,
    /// Assistant blocks with a scheduled retry: whole seconds until it fires,
    /// from the transcript's wall clock. Part of the view so a tick rebuilds
    /// the block only when the displayed second changes.
    retry_secs: Option<i64>,
}

struct Entry {
    block: Block,
    generation: u64,
    lines: Vec<Line<'static>>,
    width: u16,
    view: ViewState,
    /// A click or `o` toggles the block (see `entry_expandable`). Computed
    /// with `lines`, because a shell block's answer depends on the width its
    /// collapse ran at; pointer moves read it instead of re-rendering.
    expandable: bool,
    /// The row of `lines` holding a shell block's spinner lead (see
    /// `shell_block::Rendered::spinner_row`), rendered with `lines`.
    spinner_row: Option<usize>,
}

impl Entry {
    fn new(block: Block, generation: u64, width: u16, view: ViewState) -> Self {
        let (lines, expandable, spinner_row) = render_entry(&block, width, view);
        Self {
            block,
            generation,
            lines,
            width,
            view,
            expandable,
            spinner_row,
        }
    }

    fn lines_for(&mut self, width: u16) -> &[Line<'static>] {
        if self.width != width {
            (self.lines, self.expandable, self.spinner_row) =
                render_entry(&self.block, width, self.view);
            self.width = width;
        }
        &self.lines
    }

    /// Move a shell block to spinner frame `view.spin` by rewriting its one
    /// spinner span, when that is the only change: the expanded output of a
    /// running shell (up to the capture cap) is not cleaned and re-wrapped
    /// 12 times a second. The rows stay exactly what `render_entry` would
    /// draw, since the glyph is the only thing the frame index selects.
    fn respin(&mut self, view: ViewState) -> bool {
        let Some(row) = self.spinner_row else {
            return false;
        };
        let unchanged = ViewState {
            spin: view.spin,
            ..self.view
        };
        if unchanged != view {
            return false;
        }
        let Some(span) = self
            .lines
            .get_mut(row)
            .and_then(|line| line.spans.get_mut(super::shell_block::SPINNER_SPAN))
        else {
            return false;
        };
        let glyph = SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()];
        span.content = super::shell_block::spinner_lead(glyph).into();
        self.view = view;
        true
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

    /// Diff incoming blocks against current entries. `changed_from` comes from
    /// `DataStore::claim_changed_from()`: everything before it is never even
    /// compared. Changed blocks get a new generation; unchanged keep both cache
    /// and generation.
    pub fn sync(&mut self, blocks: &[Block], changed_from: usize) {
        self.wall_ms = wall_ms_now();
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
    }
}

/// Rows of an entry that are layout and not text: the blank margin above every
/// block, and the inner padding above and below a user message.
fn text_rows(block: &Block) -> (usize, usize) {
    match block {
        // Margin and top padding above, bottom padding below.
        Block::User { .. } | Block::Shell { .. } => (2, 1),
        Block::Reasoning { .. }
        | Block::Assistant { .. }
        | Block::Switch(_)
        | Block::Notice(_)
        | Block::Error(_)
        | Block::CompactionQueued
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
/// A block's rows, and whether a click or `o` toggles it: a shell block
/// learns that from the same collapse its rows came from.
/// A shell block also reports the row of its spinner lead.
fn render_entry(
    block: &Block,
    width: u16,
    view: ViewState,
) -> (Vec<Line<'static>>, bool, Option<usize>) {
    let palette = theme::palette();
    let mut expandable = tool_expandable(block);
    let mut spinner_row = None;
    let body = match block {
        Block::Shell {
            state,
            input,
            output,
            omitted,
            shell,
            background,
            shell_running,
            ..
        } => {
            let workdir = input
                .get("workdir")
                .and_then(serde_json::Value::as_str)
                .map(|dir| tool_text::format_path(dir, tool_text::paths()))
                .unwrap_or_default();
            let spec = super::shell_block::ShellSpec {
                command: input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                workdir: &workdir,
                state: *state,
                output,
                omitted: *omitted,
                has_shell: *shell,
                background: *background,
                shell_running: *shell_running,
                expanded: view.tool_expanded,
                hovered: view.hovered || view.tool_focused,
                spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
            };
            let rendered = super::shell_block::render(&spec, width);
            expandable = rendered.expandable;
            spinner_row = rendered.spinner_row;
            rendered.lines
        }
        Block::User { text } => user_block(text, view, width),
        Block::Reasoning { text, done, ms, .. } => reasoning_block(text, *done, *ms, view, width),
        Block::Assistant { text, footer, .. } => {
            assistant_block(text, footer.as_ref(), view.retry_secs, width)
        }
        Block::Switch(switch) => vec![switch_line(switch, width)],
        Block::Notice(notice) => notice_lines(notice, width),
        Block::Error(error) => error_lines(&error.message, width),
        Block::CompactionQueued => vec![super::compaction::queued_line(width)],
        Block::Explored(explored) => explored_block(explored, view, width),
        Block::Tool {
            tool,
            state,
            summary,
            output,
            input,
            metadata,
            ..
        } => match own_block(
            tool,
            &PatchCall {
                state: *state,
                input,
                metadata: metadata.as_deref(),
                output,
            },
            width,
            palette,
        ) {
            Some(lines) => lines,
            None => {
                match tool_text::text(tool, input, metadata.as_deref(), *state, tool_text::paths())
                {
                    Some(text) => text_tool_block(block, &text, input, output, view, width),
                    None => tool_block(block, tool, *state, summary, output, view, width),
                }
            }
        },
        Block::Todo {
            state,
            todos,
            output,
            ..
        } => todo_block(*state, todos, output, view, width),
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
                spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
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
        return (body, expandable, None);
    }
    let mut lines = Vec::with_capacity(body.len() + 1);
    lines.push(Line::default());
    lines.extend(body);
    // The margin row above shifts the block's own rows down by one.
    (lines, expandable, spinner_row.map(|row| row + 1))
}

/// Columns every session-level row is indented (`paddingLeft={3}`).
const COMPLETION_INDENT: usize = 3;

/// A call drawn as its tool's own block (upstream `Edit`, `ApplyPatch`,
/// `Write`), or
/// `None` to keep the inline row. Such blocks are not expandable
/// ([`draws_block`] keeps `tool_expandable` and `clickable` false).
fn own_block(
    tool: &str,
    call: &PatchCall<'_>,
    width: u16,
    palette: &theme::Palette,
) -> Option<Vec<Line<'static>>> {
    if !draws_block(tool, call.state) {
        return None;
    }
    match tool {
        "edit" => super::edit_block::render(call.metadata, width, palette),
        "patch" | "apply_patch" => super::patch_block::render(call, width, palette),
        "write" => Some(super::write_block::render(call.input, width, palette)),
        "question" => super::question_block::render(call.input, call.metadata, width, palette),
        _ => None,
    }
}

/// Whether `tool` in `state` is drawn by [`own_block`] (when its data
/// allows). Those calls, and their inline fallbacks in the same state, have
/// nothing to open: the block shows everything, the fallback has no body.
fn draws_block(tool: &str, state: ToolState) -> bool {
    match tool {
        "edit" | "write" | "question" => matches!(state, ToolState::Success),
        "patch" | "apply_patch" => matches!(state, ToolState::Success | ToolState::Failed),
        _ => false,
    }
}

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
/// The body is markdown in the muted tone (upstream `ReasoningPart` with
/// `generateThinkingSyntax`, `message-parts.tsx:43`).
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
        let rendered = super::markdown::render_toned(
            body,
            width.saturating_sub(5),
            0,
            super::markdown::Tone::Muted,
        );
        lines.extend(rendered.into_iter().map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 3);
            spans.push(Span::raw("   "));
            spans.push(Span::styled("\u{2503}", rail));
            spans.push(Span::raw(" "));
            spans.extend(line.spans);
            Line::from(spans)
        }));
    }
    lines
}

/// The answer text, then the turn footer (own blank row above it).
fn assistant_block(
    text: &str,
    footer: Option<&TurnFooter>,
    retry_secs: Option<i64>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = super::markdown::render(text.trim(), width, 3);
    if let Some(footer) = footer {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(footer_lines(footer, retry_secs, width));
    }
    lines
}

/// The step error the engine records for a cancelled step (upstream
/// `AssistantFooter` `interrupted`).
const STEP_INTERRUPTED: &str = "Step interrupted";

/// Upstream `AssistantFooter` (`routes/session/index.tsx:2022-2058`): the
/// error (unless interrupted or retrying), the retry countdown
/// (`AssistantRetry`, :2528), a blank row when either showed, then the stats
/// line. `retry_secs` is the countdown `Transcript` derived from the wall clock.
fn footer_lines(footer: &TurnFooter, retry_secs: Option<i64>, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let interrupted = footer
        .error
        .as_ref()
        .is_some_and(|error| &*error.message == STEP_INTERRUPTED);
    let mut lines = Vec::new();
    if let Some(error) = footer.error.as_ref()
        && !interrupted
        && footer.retry.is_none()
    {
        lines.extend(error_lines(&error.message, width));
    }
    if let Some(retry) = &footer.retry {
        let seconds = retry_secs.unwrap_or(0);
        let when = if seconds > 0 {
            format!("Retrying in {seconds}s")
        } else {
            "Retry due".to_owned()
        };
        let text = format!(
            "\u{26a0} {when} \u{b7} attempt {} \u{b7} {}",
            retry.attempt, retry.message
        );
        lines.extend(
            indent_wrapped(&text, width, COMPLETION_INDENT)
                .into_iter()
                .map(|line| line.style(palette.warning)),
        );
    }
    if footer.retry.is_some() || (footer.error.is_some() && !interrupted) {
        lines.push(Line::default());
    }
    let mut stats = footer_line(footer, width);
    if interrupted {
        stats
            .spans
            .push(Span::styled(" \u{b7} interrupted", palette.dim));
    }
    lines.push(stats);
    lines
}

/// `Error: message` in the error colour, indented three columns (upstream
/// `AssistantFooter` fallback and the session-level error row).
fn error_lines(message: &str, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    indent_wrapped(&format!("Error: {message}"), width, COMPLETION_INDENT)
        .into_iter()
        .map(|line| line.style(palette.warn))
        .collect()
}

/// Upstream `AssistantRetry`: `max(0, ceil((at − now) / 1000))`.
fn retry_secs(at: i64, now_ms: i64) -> i64 {
    let left = at.saturating_sub(now_ms);
    if left <= 0 {
        0
    } else {
        left.saturating_add(999) / 1000
    }
}

/// Milliseconds since the Unix epoch; 0 if the clock is before it.
fn wall_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Upstream `SessionSwitchMessageV2` (`routes/session/index.tsx:2060-2088`).
fn switch_line(switch: &Switch, width: u16) -> Line<'static> {
    let palette = theme::palette();
    let pad = " ".repeat(COMPLETION_INDENT);
    let text = match switch {
        Switch::Location { directory } => {
            return Line::from(vec![
                Span::raw(pad),
                Span::styled("\u{21b3} Moved to ", palette.dim),
                Span::styled(directory.to_string(), palette.info),
            ]);
        }
        Switch::Agent { agent, previous } => match previous {
            Some(previous) if previous != agent => format!(
                "Switched agent from {} to {}",
                titlecase(previous),
                titlecase(agent)
            ),
            _ => format!("Switched agent to {}", titlecase(agent)),
        },
        Switch::Model { model, previous } => switch_label(model, previous.as_ref()),
    };
    let line = indent_wrapped(&text, width, COMPLETION_INDENT)
        .into_iter()
        .next()
        .unwrap_or_default();
    line.style(palette.dim)
}

/// Upstream `switchLabel` (`util/model.ts:10`) without the catalogue's
/// display names: the same provider and id is a variant switch, anything else
/// names the model by `formatRef` (`provider/id[/variant]`).
fn switch_label(model: &ModelChoice, previous: Option<&ModelChoice>) -> String {
    if let Some(previous) = previous
        && previous.provider_id == model.provider_id
        && previous.id == model.id
    {
        return format!(
            "Switched variant to {}",
            model.variant.as_deref().unwrap_or("default")
        );
    }
    match &model.variant {
        Some(variant) => format!(
            "Switched model to {}/{}/{variant}",
            model.provider_id, model.id
        ),
        None => format!("Switched model to {}/{}", model.provider_id, model.id),
    }
}

/// Upstream `SessionNoticeMessageV2` (`◈ description`, default
/// `Instructions updated`) and `SessionSkillMessage` (`→ Skill name`): muted
/// complete inline tool rows.
fn notice_lines(notice: &Notice, width: u16) -> Vec<Line<'static>> {
    let (icon, pending, text) = match notice {
        Notice::System { description } => (
            "\u{25c8}",
            "Notice",
            Cow::Borrowed(description.as_deref().unwrap_or("Instructions updated")),
        ),
        Notice::Skill { name } => ("\u{2192}", "Skill", Cow::Owned(format!("Skill {name}"))),
    };
    let spec = InlineSpec {
        icon,
        pending,
        complete: Some(&text),
        error: None,
    };
    let view = RowView {
        state: ToolState::Success,
        spinner: SPINNER_FRAMES[0],
        hovered: false,
        error_expanded: false,
    };
    tool_row::lines(&spec, view, width)
}

/// Upstream `StatusBadge raised` (`routes/session/index.tsx:2860`): the
/// label padded by one space each side, muted on the raised background.
fn push_background_badge(header: &mut Line<'static>) {
    let palette = theme::palette();
    let mut badge = palette.dim;
    badge.bg = palette.panel.bg;
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(" Background ", badge));
}

/// Upstream `AssistantFooter` (`routes/session/index.tsx:2000`): the agent in
/// its colour, then muted ` · model`, ` · duration`, ` · N.N tok/s`. The width
/// gates are upstream's; `width` here is the timeline's, a few columns short
/// of the terminal's, which only matters at the 28/36 boundaries.
fn footer_line(footer: &TurnFooter, width: u16) -> Line<'static> {
    let palette = theme::palette();
    // A failed message's agent is muted (`AssistantFooter`).
    let mut agent_style = if footer.error.is_some() {
        palette.dim
    } else {
        palette.agent
    };
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

/// The title row, then (expanded) one inline tool row per grouped call, each
/// with its per-tool text (`group-view.tsx:129-157`, `children("tool")`).
fn explored_block(explored: &Explored, view: ViewState, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![explored_line(explored, view)];
    if view.explored_expanded {
        for call in &explored.calls {
            let Some(text) = tool_text::text(
                &call.tool,
                &call.input,
                call.metadata.as_deref(),
                call.state,
                tool_text::paths(),
            ) else {
                continue;
            };
            lines.extend(text_row(&text, call.state, false, None, view, width));
            lines.extend(loaded_lines(&text));
        }
    }
    lines
}

/// Upstream `group-view.tsx:120-151`: `→ Explored: 2 reads, 1 search` once
/// every call finished, otherwise the spinner and `Exploring: …`.
fn explored_line(explored: &Explored, view: ViewState) -> Line<'static> {
    let palette = theme::palette();
    let mut parts = Vec::with_capacity(explored.counts.len());
    for entry in &explored.counts {
        let ExploredCount { noun, count } = *entry;
        let suffix = match (count, noun) {
            (1, _) => "",
            (_, "search" | "fetch") => "es",
            _ => "s",
        };
        parts.push(format!("{count} {noun}{suffix}"));
    }
    let (icon, label) = if explored.running {
        (
            SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
            "Exploring:",
        )
    } else {
        ('\u{2192}', "Explored:")
    };
    Line::from(Span::styled(
        format!("   {icon} {label} {}", parts.join(", ")),
        explored_title_style(palette, view.hovered),
    ))
}

/// Muted at rest, full text colour under the pointer (`group-view.tsx:146`).
fn explored_title_style(palette: &theme::Palette, hovered: bool) -> ratatui::style::Style {
    if hovered { palette.text } else { palette.dim }
}

/// The head line of a legacy inline tool row (tools without `tool_text`
/// texts), drawn by `tool_row`: the label
/// is `tool summary` both while running and once done; `⚙` on success, `✗`
/// on failure; bold when focused.
fn tool_header(tool: &str, summary: &str, row: RowView, focused: bool) -> Line<'static> {
    let label = if summary.is_empty() {
        Cow::Borrowed(tool)
    } else {
        Cow::Owned(format!("{tool} {summary}"))
    };
    let icon = if matches!(row.state, ToolState::Failed) {
        "\u{2717}"
    } else {
        "\u{2699}"
    };
    let spec = InlineSpec {
        icon,
        pending: &label,
        complete: Some(&label),
        error: None,
    };
    // The header alone: the output body below stays `tool_block`'s, so the
    // row is not cut here (the paragraph clips at the edge as before).
    let mut header = tool_row::lines(&spec, row, u16::MAX)
        .into_iter()
        .next()
        .unwrap_or_default();
    if focused {
        header = header.patch_style(Modifier::BOLD);
    }
    header
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
    let background = matches!(
        block,
        Block::Tool {
            background: true,
            ..
        }
    );
    let expandable = tool_expandable(block);
    let focused = view.tool_focused && expandable;
    // Upstream `Subagent` row (`routes/session/index.tsx:3289`): while the
    // child runs the call itself is already done, so the spinner replaces the
    // icon and a dim `Background` badge follows the label.
    let row = RowView {
        state: if background {
            ToolState::Running
        } else {
            state
        },
        spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
        hovered: false,
        error_expanded: false,
    };
    let mut header = tool_header(tool, summary, row, focused);
    if background {
        push_background_badge(&mut header);
    }
    let mut lines = vec![header];
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

/// An inline tool row with per-tool texts (`tool_text`), drawn by
/// `tool_row`: Read's `↳ Loaded` rows below it, the error of a failed row
/// once opened, and a generic row's `key: value` list once expanded
/// (upstream `GenericTool`, `index.tsx:2741-2767`). Bold when focused.
///
/// Deviation: an expanded generic body keeps at most
/// [`TOOL_OUTPUT_MAX_LINES`] rows, like a shell row, where upstream lists
/// everything.
fn text_tool_block(
    block: &Block,
    text: &ToolText<'_>,
    input: &Record,
    output: &str,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let Block::Tool {
        state, background, ..
    } = block
    else {
        return Vec::new();
    };
    let state = *state;
    let failed = matches!(state, ToolState::Failed);
    let error = (failed && !output.is_empty()).then_some(output);
    let mut lines = text_row(text, state, *background, error, view, width);
    if let Some(header) = lines.first_mut() {
        if *background {
            push_background_badge(header);
        }
        if view.tool_focused && tool_expandable(block) {
            *header = std::mem::take(header).patch_style(Modifier::BOLD);
        }
    }
    lines.extend(loaded_lines(text));
    if text.generic && view.tool_expanded && !failed {
        let rows = tool_text::generic_details(input, output);
        lines.extend(detail_lines(&rows, width));
    }
    lines
}

/// One `tool_text` row drawn by `tool_row` (upstream `InlineToolRow`): a
/// spinner row shows the label next to the spinner; otherwise icon + label
/// once complete or failed, else the spinner + pending text. A background
/// subagent spins with its badge.
fn text_row(
    text: &ToolText<'_>,
    state: ToolState,
    background: bool,
    error: Option<&str>,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let failed = matches!(state, ToolState::Failed);
    let spinning = text.spinner || background;
    let spec = InlineSpec {
        icon: text.icon,
        pending: if spinning { &text.label } else { text.pending },
        complete: (!spinning && (text.complete || failed)).then_some(text.label.as_str()),
        error,
    };
    let row = RowView {
        state: if spinning { ToolState::Running } else { state },
        spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
        hovered: false,
        error_expanded: view.tool_expanded,
    };
    tool_row::lines(&spec, row, width)
}

/// Read's `↳ Loaded <file>` rows under its tool row.
fn loaded_lines<'a>(text: &'a ToolText<'_>) -> impl Iterator<Item = Line<'static>> + 'a {
    let palette = theme::palette();
    text.loaded.iter().map(move |file| {
        Line::from(Span::styled(
            format!("      \u{21b3} Loaded {file}"),
            palette.dim,
        ))
    })
}

/// Upstream `GenericTool`'s expanded body: per row a muted `key: ` and the
/// value word-wrapped beside it, everything indented past the icon.
fn detail_lines(rows: &[(&str, String)], width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    const PAD: usize = 5;
    let palette = theme::palette();
    let mut out = Vec::new();
    for (key, value) in rows {
        let key = format!("{key}: ");
        let hang = PAD + key.width();
        let usable = usize::from(width).saturating_sub(hang).max(1);
        let options = textwrap::Options::new(usable)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);
        let mut first = true;
        for line in value.split('\n') {
            let parts = textwrap::wrap(line, &options);
            let parts = if parts.is_empty() {
                vec![Cow::Borrowed("")]
            } else {
                parts
            };
            for part in parts {
                if out.len() >= TOOL_OUTPUT_MAX_LINES {
                    out.push(Line::from(Span::styled(
                        format!("{:PAD$}\u{2026}", ""),
                        palette.dim,
                    )));
                    return out;
                }
                let lead = if first {
                    Span::styled(format!("{:PAD$}{key}", ""), palette.dim)
                } else {
                    Span::raw(" ".repeat(hang))
                };
                first = false;
                out.push(Line::from(vec![
                    lead,
                    Span::styled(part.into_owned(), palette.text),
                ]));
            }
        }
    }
    out
}

/// Upstream `TodoWrite`: the panel when the call completed with todos, else
/// an `InlineTool` row (`index.tsx:3623-3647`): `⚙`, pending `Updating
/// todos…`, complete `Todo update failed` on error and `Cleared todos`
/// otherwise; a click on a failed row lists the error.
fn todo_block(
    state: ToolState,
    todos: &[Todo],
    output: &str,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    if matches!(state, ToolState::Success) && !todos.is_empty() {
        return super::todo::lines(todos, width);
    }
    let failed = matches!(state, ToolState::Failed);
    let spec = InlineSpec {
        icon: "\u{2699}",
        pending: "Updating todos\u{2026}",
        complete: Some(if failed {
            "Todo update failed"
        } else {
            "Cleared todos"
        }),
        error: (!output.is_empty()).then_some(output),
    };
    let row = RowView {
        state,
        spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
        hovered: false,
        error_expanded: view.tool_expanded,
    };
    let mut lines = tool_row::lines(&spec, row, width);
    // The `[` / `]` focus reads as bold, like a tool row.
    if view.tool_focused
        && failed
        && let Some(header) = lines.first_mut()
    {
        *header = std::mem::take(header).patch_style(Modifier::BOLD);
    }
    lines
}

/// Blocks whose spinner moves with the clock: streaming reasoning, running
/// tool and todo rows, and background subagent rows.
fn spins(block: &Block) -> bool {
    let running = |state: &ToolState| {
        matches!(
            state,
            ToolState::Input | ToolState::Running | ToolState::Progress
        )
    };
    match block {
        Block::Reasoning { done, .. } => !done,
        Block::Tool {
            state, background, ..
        } => *background || running(state),
        Block::Todo { state, .. } => running(state),
        Block::Shell {
            state,
            shell_running,
            ..
        } => *shell_running || running(state),
        Block::Explored(explored) => explored.running,
        _ => false,
    }
}

/// A failed tool or todo row with a message to show: the click target that
/// opens the error.
fn failed_with_error(block: &Block) -> bool {
    match block {
        Block::Tool { state, output, .. } | Block::Todo { state, output, .. } => {
            matches!(state, ToolState::Failed) && !output.is_empty()
        }
        _ => false,
    }
}

/// A tool or todo row a click opens: a failed row with an error, or a
/// generic row with something to list. Legacy rows (`shell`, …) are not
/// clickable unless failed, as before.
fn clickable(block: &Block) -> bool {
    match block {
        Block::Tool { tool, state, .. } if draws_block(tool, *state) => false,
        Block::Tool { tool, .. } if !failed_with_error(block) => {
            tool_text::is_generic(tool) && tool_expandable(block)
        }
        _ => failed_with_error(block),
    }
}

/// The id of a tool block, for focus/expansion bookkeeping.
fn entry_tool_id(block: &Block) -> Option<&str> {
    match block {
        Block::Tool { id, .. } | Block::Todo { id, .. } | Block::Shell { id, .. } => {
            Some(id.as_str())
        }
        _ => None,
    }
}

/// Whether a click or `o` toggles the entry: cached at render for a shell
/// block, derived from the block for tool and todo rows.
fn entry_expandable(entry: &Entry) -> bool {
    match entry.block {
        Block::Shell { .. } => entry.expandable,
        _ => tool_expandable(&entry.block),
    }
}

/// Whether a tool or todo row has something behind its header: a failed
/// row's error, a generic row's input and output (upstream `GenericTool`
/// `expandable`), or a finished legacy row's output.
fn tool_expandable(block: &Block) -> bool {
    match block {
        Block::Tool {
            tool,
            state,
            output,
            input,
            ..
        } => {
            if draws_block(tool, *state) {
                false
            } else if failed_with_error(block) {
                true
            } else if tool_text::own_row(tool) {
                !output.is_empty() && matches!(state, ToolState::Success)
            } else {
                tool_text::is_generic(tool) && tool_text::generic_expandable(input, output)
            }
        }
        // A failed todo row opens its error with `o` like a tool's output.
        Block::Todo { .. } => failed_with_error(block),
        _ => false,
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

/// Upstream user message (`routes/session/index.tsx` ~2370): the block-tool
/// frame with an agent-coloured rail, on `raised.base` (`raised.high` under
/// the pointer, `block_tool::surface`).
fn user_block(text: &str, view: ViewState, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let band = block_tool::surface(palette, view.hovered);
    let rail = Span::styled(block_tool::RAIL, palette.agent.patch(band));
    let rows: Vec<Line<'static>> = block_tool::wrap(text, block_tool::content_width(width))
        .into_iter()
        .map(|row| Line::from(Span::styled(row, palette.text)))
        .collect();
    block_tool::frame(rows, &rail, band, width)
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

pub(crate) fn titlecase(word: &str) -> String {
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
