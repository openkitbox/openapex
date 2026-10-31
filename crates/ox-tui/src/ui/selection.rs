//! Drag selection over the transcript: a pure state machine plus text
//! extraction. No I/O, no globals, no dependency on `Transcript`; coordinates
//! are the transcript's absolute line index and the display column, so a
//! selection stays on its content while the view scrolls.
//!
//! Upstream: opentui `lib/selection.ts` (anchor/focus, `:5-25`; text joined by
//! `\n` per screen row, rows without a renderable yield nothing, `:120-155`),
//! `text-buffer-view.zig` (cell selection rounds to grapheme boundaries,
//! `:1104-1150`) and opencode `util/selection.ts:copy` (a press without a drag
//! never copies, an empty text never copies).
//!
//! Deviations from upstream:
//! - Word and line behaviour (multi-click) work on the display columns of one
//!   visual line. Upstream's word set is `text-buffer-view.zig`
//!   `default_word_boundaries`; consecutive characters of one class form a
//!   group (so a run of spaces is selectable, `/` and `-` stay inside words,
//!   a CJK run is one word). Upstream's line is the logical source line,
//!   trimmed of space/tab; ox lines carry no source-line information, so a
//!   wrapped paragraph selects one visual row. An all-blank row keeps its
//!   full width. A whitespace-only selection copies nothing (trailing
//!   whitespace is stripped from copied text, see below).
//! - [`ClickTracker`] counts 1/2/3 like `renderer.ts:nextClickBehavior`: 500 ms,
//!   at most one cell apart, same entry; a fourth click stays at 3.
//! - The focus cell is part of the selection (inclusive end), and an
//!   `anchor == focus` selection is empty. Upstream treats the focus as a cell
//!   edge; terminal mouse reports are cell indices, so inclusive is the
//!   natural reading.
//! - Upstream selects per renderable. ox flattens every entry into styled
//!   `Line`s, so the caller says which rows of an entry are not text
//!   ([`Block::skip_head`], [`Block::skip_tail`]: the blank margin row and the
//!   inner padding of a user message).
//! - Upstream copies exactly the selected characters. ox additionally strips
//!   the decoration column ([`gutter_width`]: indent and the `┃` rail) from
//!   the start of each line, and trailing whitespace from its end, so copied
//!   text is the message and not its layout. The decoration is stripped only
//!   inside the selected range. A row without a rail loses exactly three
//!   spaces, so tool bodies (five spaces) keep two and code keeps its indent.
//! - The focus is clamped above/below the content by the caller (it knows the
//!   viewport); `extract` takes whatever coordinates it is given, so a column
//!   past the line end selects to the end of the line.

use std::time::{Duration, Instant};

use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The rail drawn at the start of user rows and reasoning bodies.
const RAIL: char = '\u{2503}';

/// Presses closer together than this, on the same entry and within one cell,
/// continue a multi-click (`CLICK_REPEAT_INTERVAL_MS`).
const REPEAT_WINDOW: Duration = Duration::from_millis(500);

/// `text-buffer-view.zig:default_word_boundaries`. `/` and `-` are not in it.
const WORD_BOUNDARIES: [char; 19] = [
    ' ', '\t', '\'', '"', '\u{2502}', '`', '|', ':', ';', ',', '(', ')', '[', ']', '{', '}', '<',
    '>', '$',
];

/// The text of one absolute transcript line, `None` past the end.
pub(crate) type LineText<'a> = &'a dyn Fn(usize) -> Option<String>;

/// A cell in the transcript: absolute line index and display column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    pub line: usize,
    pub col: u16,
}

/// What a button release means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Pressed and released without moving: an ordinary click.
    Click,
    /// Dragged, and the selection holds text.
    Copy(String),
    /// Dragged, but nothing copyable is selected.
    Nothing,
}

/// How far a selection snaps: the click count picks it (`renderer.ts:
/// nextClickBehavior`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Behavior {
    Cell,
    Word,
    Line,
}

impl Behavior {
    /// One click selects cells, two a word, three (and more) a line.
    pub(crate) fn from_clicks(count: u8) -> Self {
        match count {
            0 | 1 => Self::Cell,
            2 => Self::Word,
            _ => Self::Line,
        }
    }
}

/// Counts consecutive presses. Time is passed in, never read.
#[derive(Debug, Default)]
pub(crate) struct ClickTracker {
    last: Option<Press>,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    count: u8,
    at: Instant,
    pos: Pos,
    entry: usize,
}

impl ClickTracker {
    /// A left press at `pos` (a screen cell, row in `line`) on transcript
    /// entry `entry`. Returns the click count, 1..=3: it grows while presses
    /// come within 500 ms, one cell and the same entry of the previous one
    /// (the window slides with each press), and a fourth click stays at 3.
    pub(crate) fn register(&mut self, now: Instant, pos: Pos, entry: usize) -> u8 {
        let continued = self.last.is_some_and(|last| {
            last.entry == entry
                && now.saturating_duration_since(last.at) <= REPEAT_WINDOW
                && last.pos.line.abs_diff(pos.line) <= 1
                && last.pos.col.abs_diff(pos.col) <= 1
        });
        let count = match self.last {
            Some(last) if continued => (last.count + 1).min(3),
            _ => 1,
        };
        self.last = Some(Press {
            count,
            at: now,
            pos,
            entry,
        });
        count
    }

    /// Forget the history (the selection was cleared by something else).
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }
}

/// An in-progress or finished drag selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// Where the button went down, and where the pointer is now. For word and
    /// line behaviour these are the raw pointer cells; the selected range is
    /// `span`.
    pub anchor: Pos,
    pub focus: Pos,
    /// The button is still down.
    pub dragging: bool,
    /// The pointer left the anchor at least once; dragging back onto it does
    /// not turn the gesture back into a click. Word and line selections hold
    /// text from the press, so they always count as moved.
    moved: bool,
    behavior: Behavior,
    /// Word/line: the expanded inclusive range in reading order, `None` when
    /// there is nothing to select under the pointer.
    span: Option<(Pos, Pos)>,
}

impl Selection {
    /// Button pressed at `pos`. `text` reads a transcript line; word and line
    /// selections need it to find their edges.
    pub(crate) fn start(pos: Pos, behavior: Behavior, text: LineText<'_>) -> Self {
        Self {
            anchor: pos,
            focus: pos,
            dragging: true,
            moved: behavior != Behavior::Cell,
            behavior,
            span: expand(pos, pos, behavior, text),
        }
    }

    /// Pointer moved with the button down.
    pub(crate) fn drag(&mut self, pos: Pos, text: LineText<'_>) {
        self.focus = pos;
        self.moved |= pos != self.anchor;
        self.span = expand(self.anchor, pos, self.behavior, text);
    }

    /// Button released: stop dragging and classify the gesture. `text_of`
    /// extracts the selected text and runs only when the pointer moved.
    pub(crate) fn release(&mut self, text_of: impl FnOnce(&Selection) -> String) -> Outcome {
        self.dragging = false;
        if !self.moved {
            return Outcome::Click;
        }
        let text = text_of(self);
        if text.is_empty() {
            Outcome::Nothing
        } else {
            Outcome::Copy(text)
        }
    }

    /// Nothing is selected: a bare press, or a word/line press with no text
    /// under it.
    pub(crate) fn is_empty(&self) -> bool {
        match self.behavior {
            Behavior::Cell => self.anchor == self.focus,
            _ => self.span.is_none(),
        }
    }

    /// `(start, end)` in reading order, whichever way the drag went.
    pub(crate) fn ordered(&self) -> (Pos, Pos) {
        if let Some(span) = self.span {
            return span;
        }
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

/// The inclusive display columns `(from, to)` selected on `line`, or `None`
/// when the line is outside the selection (or the selection is empty). The
/// first line starts at the start column, the last ends at the end column
/// (the cell under the focus included), lines between are full width. A range
/// can extend past the text or start beyond it; callers clamp or ignore.
pub(crate) fn range_on_line(sel: &Selection, line: usize, line_width: u16) -> Option<(u16, u16)> {
    if sel.is_empty() {
        return None;
    }
    let (start, end) = sel.ordered();
    if line < start.line || line > end.line {
        return None;
    }
    let full_end = line_width.saturating_sub(1);
    let from = if line == start.line { start.col } else { 0 };
    let to = if line == end.line { end.col } else { full_end };
    Some((from, to))
}

/// The inclusive display columns of `line` to paint for `sel`: the selected
/// range minus the decoration column ([`gutter_width`]) and clamped to the
/// text. `abs` is the line's absolute index.
pub(crate) fn highlight_range(line: &Line<'_>, sel: &Selection, abs: usize) -> Option<(u16, u16)> {
    let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
    let (from, to) = range_on_line(sel, abs, width)?;
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let from = from.max(gutter_width(&text));
    let to = to.min(width.saturating_sub(1));
    (width > 0 && from <= to).then_some((from, to))
}

/// One grapheme cluster of a line: its first character and the inclusive
/// display columns it covers.
struct Cell {
    ch: char,
    from: u16,
    to: u16,
}

/// The cells of `row` after the decoration column. Widths are measured per
/// grapheme cluster, as ratatui does, so emoji sequences agree with the
/// rendered buffer. A zero-width cluster belongs to the cell before it and
/// adds none.
fn cells_of(row: &str) -> Vec<Cell> {
    let gutter = gutter_width(row);
    let mut col: u16 = 0;
    let mut cells = Vec::with_capacity(row.len());
    for cluster in row.graphemes(true) {
        let width = cluster.width() as u16;
        let Some(ch) = cluster.chars().next() else {
            continue;
        };
        if width == 0 {
            continue;
        }
        if col >= gutter {
            cells.push(Cell {
                ch,
                from: col,
                to: col + width - 1,
            });
        }
        col = col.saturating_add(width);
    }
    cells
}

/// The range a `behavior` selection covers when pressed at `anchor` and
/// dragged to `focus` (upstream `expandSelectionRange`): the union of what
/// each end expands to. `Cell` has no span.
fn expand(anchor: Pos, focus: Pos, behavior: Behavior, text: LineText<'_>) -> Option<(Pos, Pos)> {
    match behavior {
        Behavior::Cell => None,
        Behavior::Word => union(
            word_between(anchor, focus, text),
            word_between(focus, anchor, text),
        ),
        Behavior::Line => union(
            line_between(anchor.line, focus.line, text),
            line_between(focus.line, anchor.line, text),
        ),
    }
}

fn union(a: Option<(Pos, Pos)>, b: Option<(Pos, Pos)>) -> Option<(Pos, Pos)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
        (a, b) => a.or(b),
    }
}

/// Lines from `from` toward `toward`, inclusive, whichever way that runs.
fn lines_between(from: usize, toward: usize) -> Box<dyn Iterator<Item = usize>> {
    if from <= toward {
        Box::new(from..=toward)
    } else {
        Box::new((toward..=from).rev())
    }
}

/// `selectWordBetween`: the word at the first text cell met walking from
/// `from` toward `toward`, looking only inside the span between the two
/// points. A drag that starts on padding or the decoration therefore snaps to
/// the first word in that span; a press on them alone (empty span of cells)
/// finds no word.
fn word_between(from: Pos, toward: Pos, text: LineText<'_>) -> Option<(Pos, Pos)> {
    let forward = from <= toward;
    let (lo, hi) = if forward {
        (from, toward)
    } else {
        (toward, from)
    };
    for line in lines_between(from.line, toward.line) {
        let Some(row) = text(line) else { continue };
        let cells = cells_of(&row);
        let lo_col = if line == lo.line { lo.col } else { 0 };
        let hi_col = if line == hi.line { hi.col } else { u16::MAX };
        let inside = |c: &Cell| c.to >= lo_col && c.from <= hi_col;
        let hit = if forward {
            cells.iter().position(inside)
        } else {
            cells.iter().rposition(inside)
        };
        if let Some(index) = hit {
            return Some(word_around(&cells, index, line));
        }
    }
    None
}

/// The run of cells around `index` sharing its class (boundary or not).
fn word_around(cells: &[Cell], index: usize, line: usize) -> (Pos, Pos) {
    let class = WORD_BOUNDARIES.contains(&cells[index].ch);
    let same = |c: &Cell| WORD_BOUNDARIES.contains(&c.ch) == class;
    let mut first = index;
    while first > 0 && same(&cells[first - 1]) {
        first -= 1;
    }
    let mut last = index;
    while last + 1 < cells.len() && same(&cells[last + 1]) {
        last += 1;
    }
    (
        Pos {
            line,
            col: cells[first].from,
        },
        Pos {
            line,
            col: cells[last].to,
        },
    )
}

/// `selectLine` on the first text line walking from `from` toward `toward`:
/// the visual line without leading and trailing space/tab; an all-blank line
/// keeps its whole width.
fn line_between(from: usize, toward: usize, text: LineText<'_>) -> Option<(Pos, Pos)> {
    for line in lines_between(from, toward) {
        let Some(row) = text(line) else { continue };
        let cells = cells_of(&row);
        let ink = |c: &&Cell| c.ch != ' ' && c.ch != '\t';
        let (first, last) = match (cells.iter().find(ink), cells.iter().rfind(ink)) {
            (Some(first), Some(last)) => (first, last),
            _ => match (cells.first(), cells.last()) {
                (Some(first), Some(last)) => (first, last),
                _ => continue,
            },
        };
        return Some((
            Pos {
                line,
                col: first.from,
            },
            Pos { line, col: last.to },
        ));
    }
    None
}

/// A run of transcript lines belonging to one entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Block<'a> {
    /// Absolute index of `lines[0]`.
    pub first: usize,
    pub lines: &'a [Line<'static>],
    /// Leading rows that are not text (the blank margin row, user padding).
    pub skip_head: usize,
    /// Trailing rows that are not text (user padding).
    pub skip_tail: usize,
}

/// The selected text. Rows skipped by the blocks produce nothing, so a copy
/// across entries has no blank line between them; blank rows inside a block
/// are kept. Lines are joined by `\n` with no trailing newline. Blocks must be
/// in ascending line order.
pub(crate) fn extract<'a>(blocks: impl IntoIterator<Item = Block<'a>>, sel: &Selection) -> String {
    let (start, end) = sel.ordered();
    let mut out: Vec<String> = Vec::new();
    'blocks: for block in blocks {
        let text_rows = block.lines.len().saturating_sub(block.skip_tail);
        for index in block.skip_head..text_rows {
            let abs = block.first + index;
            if abs > end.line {
                break 'blocks;
            }
            if abs < start.line {
                continue;
            }
            let line = &block.lines[index];
            let range = range_on_line(sel, abs, u16::try_from(line.width()).unwrap_or(u16::MAX));
            out.push(range.map_or_else(String::new, |(from, to)| slice_line(line, from, to)));
        }
    }
    if out.iter().all(String::is_empty) {
        return String::new();
    }
    out.join("\n")
}

/// Width of the decoration at the start of `text`: up to three spaces; and
/// only when a `┃` rail follows, the rail plus up to two spaces. This covers
/// plain rows (`   `), user rows (`┃  `) and reasoning bodies (`   ┃ `).
/// Without a rail nothing beyond the three spaces is touched, so the
/// indentation of code lines (`   ` + code) survives.
pub(crate) fn gutter_width(text: &str) -> u16 {
    let spaces = text.chars().take(3).take_while(|&c| c == ' ').count();
    let rest = &text[spaces..];
    let mut width = spaces as u16;
    if let Some(after) = rest.strip_prefix(RAIL) {
        let pad = after.chars().take(2).take_while(|&c| c == ' ').count();
        width += RAIL.width().unwrap_or(0) as u16 + pad as u16;
    }
    width
}

/// The text of `line` in display columns `[from, to]`, minus the gutter and
/// trailing whitespace. A wide grapheme cluster is taken whole when either of
/// its cells is in range; a zero-width cluster follows the cluster before it.
fn slice_line(line: &Line<'_>, from: u16, to: u16) -> String {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let from = from.max(gutter_width(&text));
    let mut out = String::new();
    let mut col: u32 = 0;
    let mut included = false;
    for cluster in text.graphemes(true) {
        let width = cluster.width() as u32;
        if width > 0 {
            included = col <= u32::from(to) && col + width > u32::from(from);
            col += width;
        }
        if included {
            out.push_str(cluster);
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(rows: &[&str]) -> Vec<Line<'static>> {
        rows.iter().map(|r| Line::from((*r).to_owned())).collect()
    }

    fn no_text(_: usize) -> Option<String> {
        None
    }

    fn pos(line: usize, col: u16) -> Pos {
        Pos { line, col }
    }

    fn sel(a: Pos, b: Pos) -> Selection {
        let mut s = Selection::start(a, Behavior::Cell, &no_text);
        s.drag(b, &no_text);
        s
    }

    fn block<'a>(first: usize, lines: &'a [Line<'static>], head: usize, tail: usize) -> Block<'a> {
        Block {
            first,
            lines,
            skip_head: head,
            skip_tail: tail,
        }
    }

    #[test]
    fn single_line_substring_includes_the_focus_cell() {
        let l = lines(&["hello world"]);
        let s = sel(pos(0, 6), pos(0, 10));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "world");
        let s = sel(pos(0, 0), pos(0, 4));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "hello");
    }

    #[test]
    fn spans_across_two_lines() {
        let l = lines(&["alpha beta", "gamma delta"]);
        let s = sel(pos(0, 6), pos(1, 4));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "beta\ngamma");
    }

    #[test]
    fn middle_lines_are_taken_whole_and_blank_lines_inside_a_block_stay() {
        let l = lines(&["one", "", "three", "four"]);
        let s = sel(pos(0, 1), pos(3, 1));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "ne\n\nthree\nfo");
    }

    #[test]
    fn crossing_blocks_inserts_no_blank_line() {
        let a = lines(&["", "first"]);
        let b = lines(&["", "second"]);
        let s = sel(pos(0, 0), pos(3, 5));
        let blocks = [block(0, &a, 1, 0), block(2, &b, 1, 0)];
        assert_eq!(extract(blocks, &s), "first\nsecond");
    }

    #[test]
    fn skip_head_and_tail_are_honoured() {
        let l = lines(&["", "┃", "┃  hi", "┃"]);
        let s = sel(pos(0, 0), pos(3, 9));
        assert_eq!(extract([block(0, &l, 2, 1)], &s), "hi");
    }

    #[test]
    fn reversed_drag_equals_forward_drag() {
        let a = lines(&["", "alpha beta"]);
        let b = lines(&["", "gamma delta"]);
        let fwd = sel(pos(1, 6), pos(3, 4));
        let rev = sel(pos(3, 4), pos(1, 6));
        let blocks = || [block(0, &a, 1, 0), block(2, &b, 1, 0)];
        assert_eq!(extract(blocks(), &fwd), "beta\ngamma");
        assert_eq!(extract(blocks(), &rev), extract(blocks(), &fwd));
    }

    #[test]
    fn wide_char_is_taken_whole_from_either_half() {
        let l = lines(&["a世b"]); // a=0, 世=1..=2, b=3
        for col in [1, 2] {
            let s = sel(pos(0, col), pos(0, col));
            // anchor == focus is empty; extend one cell to the right instead.
            assert_eq!(extract([block(0, &l, 0, 0)], &s), "");
            let s = sel(pos(0, col), pos(0, 3));
            assert_eq!(extract([block(0, &l, 0, 0)], &s), "世b");
        }
        let s = sel(pos(0, 0), pos(0, 1));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "a世");
        let s = sel(pos(0, 0), pos(0, 2));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "a世");
    }

    #[test]
    fn focus_past_the_line_end_selects_to_the_end() {
        let l = lines(&["abc", "def"]);
        let s = sel(pos(0, 1), pos(1, u16::MAX));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "bc\ndef");
        let s = sel(pos(0, 0), pos(1, 0));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "abc\nd");
    }

    #[test]
    fn trailing_whitespace_is_stripped() {
        let l = lines(&["hi   ", "there  "]);
        let s = sel(pos(0, 0), pos(1, 6));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "hi\nthere");
    }

    #[test]
    fn plain_rows_lose_their_three_space_indent() {
        let l = lines(&["   hello"]);
        let s = sel(pos(0, 0), pos(0, 20));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "hello");
    }

    #[test]
    fn user_rows_lose_the_rail_and_indent() {
        // `user_block`: `┃` then two spaces then text, padded to the width.
        let l = lines(&["┃  hello there         "]);
        let s = sel(pos(0, 0), pos(0, 30));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "hello there");
    }

    #[test]
    fn reasoning_body_rows_lose_the_rail() {
        // `reasoning_block`: three spaces, `┃`, then ` text`.
        let l = lines(&["   ┃ Let me look at the file first."]);
        let s = sel(pos(0, 0), pos(0, 60));
        assert_eq!(
            extract([block(0, &l, 0, 0)], &s),
            "Let me look at the file first."
        );
    }

    #[test]
    fn tool_body_rows_keep_two_spaces() {
        let l = lines(&["     output"]);
        let s = sel(pos(0, 0), pos(0, 20));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "  output");
    }

    #[test]
    fn code_indentation_survives_the_gutter() {
        let l = lines(&["       code", "   x"]);
        let s = sel(pos(0, 0), pos(1, 20));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "    code\nx");
    }

    #[test]
    fn selection_starting_inside_the_gutter_begins_after_it() {
        let l = lines(&["┃  hello"]);
        let s = sel(pos(0, 1), pos(0, 20));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "hello");
    }

    #[test]
    fn selection_inside_the_gutter_is_empty() {
        let l = lines(&["┃  hello"]);
        let s = sel(pos(0, 0), pos(0, 2));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "");
    }

    #[test]
    fn styled_spans_are_flattened() {
        let l = vec![Line::from(vec![
            ratatui::text::Span::raw("   "),
            ratatui::text::Span::raw("foo"),
            ratatui::text::Span::raw("bar"),
        ])];
        let s = sel(pos(0, 4), pos(0, 7));
        assert_eq!(extract([block(0, &l, 0, 0)], &s), "ooba");
    }

    #[test]
    fn gutter_width_covers_the_known_prefixes() {
        assert_eq!(gutter_width("   x"), 3);
        assert_eq!(gutter_width("┃  x"), 3);
        assert_eq!(gutter_width("   ┃ x"), 5);
        assert_eq!(gutter_width("     x"), 3);
        assert_eq!(gutter_width("      x"), 3);
        assert_eq!(gutter_width("  x"), 2);
        assert_eq!(gutter_width("x"), 0);
        assert_eq!(gutter_width(""), 0);
    }

    #[test]
    fn highlight_range_skips_the_gutter_and_clamps_to_the_text() {
        let l = lines(&["┃  hello"]);
        assert_eq!(
            highlight_range(&l[0], &sel(pos(0, 0), pos(0, 5)), 0),
            Some((3, 5))
        );
        assert_eq!(
            highlight_range(&l[0], &sel(pos(0, 0), pos(0, u16::MAX)), 0),
            Some((3, 7))
        );
        assert_eq!(highlight_range(&l[0], &sel(pos(0, 0), pos(0, 2)), 0), None);
        assert_eq!(highlight_range(&l[0], &sel(pos(1, 0), pos(2, 4)), 0), None);
    }

    #[test]
    fn click_without_movement() {
        let mut s = Selection::start(pos(2, 3), Behavior::Cell, &no_text);
        assert!(s.dragging);
        assert_eq!(
            s.release(|_| unreachable!("a click extracts nothing")),
            Outcome::Click
        );
        assert!(!s.dragging);
    }

    #[test]
    fn drag_to_the_same_cell_is_still_a_click() {
        let mut s = Selection::start(pos(2, 3), Behavior::Cell, &no_text);
        s.drag(pos(2, 3), &no_text);
        assert_eq!(s.release(|_| unreachable!()), Outcome::Click);
    }

    #[test]
    fn drag_with_text_copies() {
        let mut s = sel(pos(0, 0), pos(0, 3));
        assert_eq!(
            s.release(|_| "text".to_owned()),
            Outcome::Copy("text".to_owned())
        );
        assert!(!s.dragging);
    }

    #[test]
    fn dragging_back_to_the_start_is_nothing() {
        let l = lines(&["hello"]);
        let mut s = Selection::start(pos(0, 2), Behavior::Cell, &no_text);
        s.drag(pos(0, 4), &no_text);
        s.drag(pos(0, 2), &no_text);
        let outcome = s.release(|s| extract([block(0, &l, 0, 0)], s));
        assert_eq!(outcome, Outcome::Nothing);
    }

    #[test]
    fn dragging_over_only_margin_rows_is_nothing() {
        let l = lines(&["", "text"]);
        let mut s = sel(pos(0, 0), pos(0, 5));
        let outcome = s.release(|s| extract([block(0, &l, 1, 0)], s));
        assert_eq!(outcome, Outcome::Nothing);
    }

    #[test]
    fn dragging_over_only_the_gutter_is_nothing() {
        let l = lines(&["┃  hello"]);
        let mut s = sel(pos(0, 0), pos(0, 2));
        let outcome = s.release(|s| extract([block(0, &l, 0, 0)], s));
        assert_eq!(outcome, Outcome::Nothing);
    }

    #[test]
    fn ordered_normalises_reversed_drags() {
        let s = sel(pos(5, 7), pos(2, 1));
        assert_eq!(s.ordered(), (pos(2, 1), pos(5, 7)));
        let s = sel(pos(2, 9), pos(2, 1));
        assert_eq!(s.ordered(), (pos(2, 1), pos(2, 9)));
    }

    #[test]
    fn range_on_line_first_middle_last() {
        let s = sel(pos(1, 4), pos(3, 6));
        assert_eq!(range_on_line(&s, 0, 20), None);
        assert_eq!(range_on_line(&s, 1, 20), Some((4, 19)));
        assert_eq!(range_on_line(&s, 2, 20), Some((0, 19)));
        assert_eq!(range_on_line(&s, 3, 20), Some((0, 6)));
        assert_eq!(range_on_line(&s, 4, 20), None);
    }

    #[test]
    fn range_on_line_single_line_and_empty_selection() {
        let s = sel(pos(2, 8), pos(2, 3));
        assert_eq!(range_on_line(&s, 2, 20), Some((3, 8)));
        let empty = Selection::start(pos(2, 3), Behavior::Cell, &no_text);
        assert_eq!(range_on_line(&empty, 2, 20), None);
    }

    // ---- multi-click ----

    fn rows_of(rows: &[&str]) -> impl Fn(usize) -> Option<String> {
        let rows: Vec<String> = rows.iter().map(|r| (*r).to_owned()).collect();
        move |i| rows.get(i).cloned()
    }

    /// Press at `a` with `behavior`, drag to `b` (when different), release,
    /// and return the outcome plus the selection.
    fn gesture(rows: &[&str], a: Pos, b: Pos, behavior: Behavior) -> (Outcome, Selection) {
        let text = rows_of(rows);
        let l = lines(rows);
        let mut s = Selection::start(a, behavior, &text);
        if b != a {
            s.drag(b, &text);
        }
        let outcome = s.release(|s| extract([block(0, &l, 0, 0)], s));
        (outcome, s)
    }

    fn word_at(rows: &[&str], col: u16) -> Outcome {
        gesture(rows, pos(0, col), pos(0, col), Behavior::Word).0
    }

    fn copy(text: &str) -> Outcome {
        Outcome::Copy(text.to_owned())
    }

    #[test]
    fn double_click_selects_the_word_under_the_pointer() {
        let row = ["alpha beta gamma"];
        assert_eq!(word_at(&row, 6), copy("beta"));
        assert_eq!(word_at(&row, 9), copy("beta"));
        assert_eq!(word_at(&row, 0), copy("alpha"));
        assert_eq!(word_at(&row, 15), copy("gamma"));
    }

    #[test]
    fn a_word_press_holds_text_at_once_so_release_copies() {
        let s = Selection::start(pos(0, 6), Behavior::Word, &rows_of(&["alpha beta"]));
        assert!(!s.is_empty());
        assert_eq!(s.ordered(), (pos(0, 6), pos(0, 9)));
    }

    #[test]
    fn double_click_on_spaces_selects_the_whole_run() {
        let text = rows_of(&["a   b"]);
        for col in 1..=3 {
            let s = Selection::start(pos(0, col), Behavior::Word, &text);
            assert_eq!(s.ordered(), (pos(0, 1), pos(0, 3)), "col {col}");
        }
        // The run is highlighted, but copied text drops trailing whitespace,
        // so a space-only selection copies nothing (deviation, see module doc).
        assert_eq!(word_at(&["a   b"], 2), Outcome::Nothing);
    }

    #[test]
    fn slash_and_dash_stay_inside_a_word() {
        let row = ["see foo/bar-baz.rs now"];
        assert_eq!(word_at(&row, 5), copy("foo/bar-baz.rs"));
        assert_eq!(word_at(&row, 7), copy("foo/bar-baz.rs"));
        assert_eq!(word_at(&row, 17), copy("foo/bar-baz.rs"));
    }

    #[test]
    fn upstream_punctuation_splits_words() {
        let row = ["f(a,b) \"q\" 'r' `c` x|y: z; <t> {u} [v] $w"];
        for (col, want) in [
            (0u16, "f"),
            (2, "a"),
            (4, "b"),
            (8, "q"),
            (12, "r"),
            (16, "c"),
            (19, "x"),
            (21, "y"),
            (24, "z"),
            (28, "t"),
            (32, "u"),
            (36, "v"),
            (40, "w"),
        ] {
            assert_eq!(word_at(&row, col), copy(want), "col {col}");
        }
    }

    #[test]
    fn a_punctuation_run_is_one_group() {
        // `):` are both boundaries, so they group; so do the spaces.
        let s = Selection::start(pos(0, 3), Behavior::Word, &rows_of(&["a(b): c"]));
        assert_eq!(s.ordered(), (pos(0, 3), pos(0, 5)));
    }

    #[test]
    fn a_cjk_run_is_one_word() {
        let row = ["日本語abc xyz"]; // 日=0-1 本=2-3 語=4-5 abc=6-8
        assert_eq!(word_at(&row, 3), copy("日本語abc"));
        assert_eq!(word_at(&row, 0), copy("日本語abc"));
        assert_eq!(word_at(&row, 8), copy("日本語abc"));
        assert_eq!(word_at(&["say 日本語 now"], 6), copy("日本語"));
    }

    #[test]
    fn the_word_stops_at_the_gutter() {
        // Cols 0..=2 are the rail and indent: not part of any word.
        let row = ["┃  hello"];
        assert_eq!(word_at(&row, 3), copy("hello"));
        // Pressing the rail alone covers no text cell, so it selects nothing.
        assert_eq!(word_at(&row, 1), Outcome::Nothing);
    }

    #[test]
    fn a_word_press_on_padding_selects_nothing() {
        let (outcome, s) = gesture(&["alpha beta"], pos(0, 12), pos(0, 12), Behavior::Word);
        assert_eq!(outcome, Outcome::Nothing);
        assert!(s.is_empty());
    }

    #[test]
    fn dragging_after_a_double_click_extends_by_whole_words() {
        let row = ["alpha beta gamma"];
        let fwd = gesture(&row, pos(0, 6), pos(0, 12), Behavior::Word).0;
        assert_eq!(fwd, copy("beta gamma"));
        let rev = gesture(&row, pos(0, 12), pos(0, 6), Behavior::Word).0;
        assert_eq!(rev, fwd);
        // Ending mid-word still takes the whole word.
        assert_eq!(
            gesture(&row, pos(0, 6), pos(0, 11), Behavior::Word).0,
            copy("beta gamma")
        );
        assert_eq!(
            gesture(&row, pos(0, 7), pos(0, 1), Behavior::Word).0,
            copy("alpha beta")
        );
    }

    #[test]
    fn a_word_drag_onto_padding_snaps_to_the_last_word_toward_the_anchor() {
        // `selectWordBetween(10, 0)` -> beta; `(0, 10)` -> alpha.
        let row = ["alpha beta"];
        assert_eq!(
            gesture(&row, pos(0, 0), pos(0, 20), Behavior::Word).0,
            copy("alpha beta")
        );
        assert_eq!(
            gesture(&row, pos(0, 20), pos(0, 0), Behavior::Word).0,
            copy("alpha beta")
        );
    }

    #[test]
    fn a_word_drag_across_lines() {
        let rows = ["one two", "three four"];
        assert_eq!(
            gesture(&rows, pos(0, 5), pos(1, 2), Behavior::Word).0,
            copy("two\nthree")
        );
    }

    #[test]
    fn triple_click_selects_the_trimmed_line() {
        let row = ["  hello world  "];
        let (outcome, s) = gesture(&row, pos(0, 4), pos(0, 4), Behavior::Line);
        assert_eq!(outcome, copy("hello world"));
        assert_eq!(s.ordered(), (pos(0, 2), pos(0, 12)));
        // Any column on the line, including padding past the text.
        assert_eq!(
            gesture(&row, pos(0, 60), pos(0, 60), Behavior::Line).0,
            copy("hello world")
        );
    }

    #[test]
    fn triple_click_on_a_user_row_skips_the_rail() {
        let (outcome, _) = gesture(&["┃  hello there   "], pos(0, 8), pos(0, 8), Behavior::Line);
        assert_eq!(outcome, copy("hello there"));
    }

    #[test]
    fn triple_click_on_a_blank_row_keeps_its_width_and_copies_nothing() {
        // Cols 0..=2 are the gutter, so the kept span starts at col 3.
        let (outcome, s) = gesture(&["        "], pos(0, 1), pos(0, 1), Behavior::Line);
        assert!(!s.is_empty());
        assert_eq!(s.ordered(), (pos(0, 3), pos(0, 7)));
        assert_eq!(outcome, Outcome::Nothing);
    }

    #[test]
    fn dragging_after_a_triple_click_extends_by_whole_lines() {
        let rows = ["  alpha  ", "beta", "  gamma"];
        let fwd = gesture(&rows, pos(0, 4), pos(1, 1), Behavior::Line).0;
        assert_eq!(fwd, copy("alpha\nbeta"));
        let rev = gesture(&rows, pos(1, 1), pos(0, 4), Behavior::Line).0;
        assert_eq!(rev, fwd);
        assert_eq!(
            gesture(&rows, pos(0, 0), pos(2, 3), Behavior::Line).0,
            copy("alpha\nbeta\ngamma")
        );
    }

    #[test]
    fn a_line_drag_past_the_content_snaps_to_the_last_line() {
        let rows = ["alpha", "beta"];
        assert_eq!(
            gesture(&rows, pos(0, 1), pos(7, 0), Behavior::Line).0,
            copy("alpha\nbeta")
        );
    }

    #[test]
    fn behavior_follows_the_click_count() {
        assert_eq!(Behavior::from_clicks(1), Behavior::Cell);
        assert_eq!(Behavior::from_clicks(2), Behavior::Word);
        assert_eq!(Behavior::from_clicks(3), Behavior::Line);
    }

    #[test]
    fn range_on_line_follows_the_expanded_span() {
        let s = Selection::start(pos(0, 6), Behavior::Word, &rows_of(&["alpha beta gamma"]));
        assert_eq!(range_on_line(&s, 0, 16), Some((6, 9)));
        assert_eq!(range_on_line(&s, 1, 16), None);
    }

    // ---- grapheme clusters ----

    #[test]
    fn emoji_sequences_occupy_the_columns_ratatui_gives_them() {
        // `⚠️` is two cells as a string though its chars sum to one.
        for emoji in ["⚠️", "✔️", "❤️", "👨\u{200d}👩\u{200d}👧"] {
            let row = format!("{emoji}x");
            assert_eq!(Line::from(row.clone()).width(), 3, "{emoji}");
            let cells = cells_of(&row);
            assert_eq!(cells.len(), 2, "{emoji}: one cell per cluster");
            assert_eq!((cells[0].from, cells[0].to), (0, 1), "{emoji}");
            assert_eq!(
                (cells[1].from, cells[1].to),
                (2, 2),
                "{emoji}: x at column 2"
            );
        }
    }

    #[test]
    fn extracting_over_an_emoji_cluster_takes_it_whole() {
        for emoji in ["⚠️", "👨\u{200d}👩\u{200d}👧"] {
            let l = lines(&[&format!("{emoji}x")]);
            // A bare press (anchor == focus) is an empty selection, so the
            // ranges below all span at least two cells.
            let s = sel(pos(0, 0), pos(0, 1));
            assert_eq!(extract([block(0, &l, 0, 0)], &s), emoji, "{emoji}");
            let s = sel(pos(0, 1), pos(0, 2));
            assert_eq!(
                extract([block(0, &l, 0, 0)], &s),
                format!("{emoji}x"),
                "{emoji}: the second cell of the cluster pulls it in whole"
            );
            let l2 = lines(&[&format!("{emoji}xy")]);
            let s = sel(pos(0, 2), pos(0, 3));
            assert_eq!(extract([block(0, &l2, 0, 0)], &s), "xy", "{emoji}");
            let s = sel(pos(0, 0), pos(0, 2));
            assert_eq!(
                extract([block(0, &l, 0, 0)], &s),
                format!("{emoji}x"),
                "{emoji}"
            );
        }
    }

    // ---- ClickTracker ----

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn clicks_within_500ms_count_up_to_three_and_stay() {
        let t0 = Instant::now();
        let mut c = ClickTracker::default();
        assert_eq!(c.register(t0, pos(3, 5), 0), 1);
        assert_eq!(c.register(at(t0, 100), pos(3, 5), 0), 2);
        assert_eq!(c.register(at(t0, 200), pos(3, 5), 0), 3);
        assert_eq!(
            c.register(at(t0, 300), pos(3, 5), 0),
            3,
            "a fourth click stays a line"
        );
    }

    #[test]
    fn the_window_is_500ms_inclusive() {
        let t0 = Instant::now();
        let mut c = ClickTracker::default();
        c.register(t0, pos(0, 0), 0);
        assert_eq!(c.register(at(t0, 499), pos(0, 0), 0), 2);
        let mut c = ClickTracker::default();
        c.register(t0, pos(0, 0), 0);
        assert_eq!(c.register(at(t0, 500), pos(0, 0), 0), 2);
        let mut c = ClickTracker::default();
        c.register(t0, pos(0, 0), 0);
        assert_eq!(c.register(at(t0, 501), pos(0, 0), 0), 1);
    }

    #[test]
    fn the_window_slides_with_each_press() {
        let t0 = Instant::now();
        let mut c = ClickTracker::default();
        c.register(t0, pos(0, 0), 0);
        assert_eq!(c.register(at(t0, 400), pos(0, 0), 0), 2);
        assert_eq!(c.register(at(t0, 800), pos(0, 0), 0), 3);
    }

    #[test]
    fn one_cell_apart_continues_two_cells_apart_restarts() {
        let t0 = Instant::now();
        for (a, b, want) in [
            (pos(2, 5), pos(2, 6), 2),
            (pos(2, 5), pos(3, 4), 2),
            (pos(2, 5), pos(2, 7), 1),
            (pos(2, 5), pos(4, 5), 1),
        ] {
            let mut c = ClickTracker::default();
            c.register(t0, a, 0);
            assert_eq!(c.register(at(t0, 10), b, 0), want, "{a:?} -> {b:?}");
        }
    }

    #[test]
    fn another_entry_restarts_the_count() {
        let t0 = Instant::now();
        let mut c = ClickTracker::default();
        c.register(t0, pos(0, 0), 0);
        assert_eq!(c.register(at(t0, 10), pos(0, 0), 1), 1);
        assert_eq!(c.register(at(t0, 20), pos(0, 0), 1), 2);
    }

    #[test]
    fn reset_forgets_the_history() {
        let t0 = Instant::now();
        let mut c = ClickTracker::default();
        c.register(t0, pos(0, 0), 0);
        c.reset();
        assert_eq!(c.register(at(t0, 10), pos(0, 0), 0), 1);
    }

    #[test]
    fn a_clock_that_runs_backwards_continues_the_click_without_panicking() {
        let t0 = Instant::now();
        let mut c = ClickTracker::default();
        c.register(at(t0, 100), pos(0, 0), 0);
        assert_eq!(c.register(t0, pos(0, 0), 0), 2);
    }
}
