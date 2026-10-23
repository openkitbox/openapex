use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme;

#[derive(Debug, Clone)]
pub enum Block {
    User {
        text: std::sync::Arc<str>,
    },
    Assistant {
        id: String,
        /// Store-owned version. Equality is `(id, version)`, never text.
        version: u64,
        /// `Arc<String>` mirrors `AssistantItem` so the store→block projection
        /// is a pointer clone — no per-delta deep copy of the growing text.
        text: std::sync::Arc<String>,
        reasoning: std::sync::Arc<String>,
        done: bool,
    },
    System {
        text: std::sync::Arc<str>,
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
            reasoning: std::sync::Arc::new(String::new()),
            done: false,
        }
    }
    pub fn system(text: &str) -> Self {
        Block::System {
            text: std::sync::Arc::from(text),
        }
    }
}

/// O(1) identity comparison: text is shared via `Arc`, assistant blocks compare
/// `(id, version)`. Deep string comparison here would defeat the cache it feeds.
impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Block::User { text: a }, Block::User { text: b }) => std::sync::Arc::ptr_eq(a, b),
            (
                Block::Assistant {
                    id: aid,
                    version: aversion,
                    ..
                },
                Block::Assistant {
                    id: bid,
                    version: bversion,
                    ..
                },
            ) => aid == bid && aversion == bversion,
            (Block::System { text: a }, Block::System { text: b }) => std::sync::Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl Eq for Block {}

struct Entry {
    block: Block,
    generation: u64,
    lines: Vec<Line<'static>>,
    width: u16,
}

impl Entry {
    fn new(block: Block, generation: u64, width: u16) -> Self {
        let lines = render_block(&block, width);
        Self {
            block,
            generation,
            lines,
            width,
        }
    }

    fn lines_for(&mut self, width: u16) -> &[Line<'static>] {
        if self.width != width {
            self.lines = render_block(&self.block, width);
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
        }
    }

    /// Diff incoming blocks against current entries. `changed_from` comes from
    /// `DataStore::claim_changed_from()`: everything before it is never even
    /// compared. Changed blocks get a new generation; unchanged keep both cache
    /// and generation.
    pub fn sync(&mut self, blocks: &[Block], changed_from: usize) {
        let start = changed_from.min(self.entries.len()).min(blocks.len());

        let end = self.entries.len().min(blocks.len());
        for (entry, block) in self.entries[start..end].iter_mut().zip(&blocks[start..end]) {
            if entry.block != *block {
                let generation = entry.generation + 1;
                *entry = Entry::new(block.clone(), generation, self.width);
            }
        }

        if blocks.len() > self.entries.len() {
            for block in &blocks[self.entries.len()..] {
                self.entries.push(Entry::new(block.clone(), 0, self.width));
            }
        }

        self.entries.truncate(blocks.len());
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

    fn max_scroll(&self) -> usize {
        self.total_lines().saturating_sub(self.height as usize)
    }

    fn clamp(&mut self) {
        if self.sticky_bottom {
            self.scroll = self.max_scroll();
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.set_viewport(area.width, area.height);
        self.clamp();

        let mut remaining_skip = self.scroll;
        let mut y = area.y;
        let mut base = 0usize;
        let current_match = self.current_line();

        for entry in &mut self.entries {
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
            if let Some(matched) = current_match {
                let first = base + skip;
                if matched >= first && matched < first + visible.len() {
                    let index = matched - first;
                    visible[index] = highlight(&visible[index]);
                }
            }
            frame.render_widget(Paragraph::new(visible), rect);
            y += height;
            base += count;
            if y >= area.y + area.height {
                break;
            }
        }
    }
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

fn render_block(block: &Block, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    match block {
        Block::User { text } => prefix_lines(
            "▌ you",
            text,
            width,
            palette.accent.add_modifier(Modifier::BOLD),
        ),
        Block::Assistant {
            text,
            reasoning,
            done,
            ..
        } => {
            let mut lines = vec![Line::from(Span::styled(
                if *done {
                    "✻ assistant"
                } else {
                    "✻ assistant …"
                },
                palette.dim.add_modifier(Modifier::BOLD),
            ))];
            if !reasoning.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  ▸ reasoning · {} chars 已折叠", reasoning.chars().count()),
                    palette.dim,
                )));
            }
            lines.extend(indent_wrapped(text, width));
            lines
        }
        Block::System { text } => prefix_lines("⤷", text, width, palette.dim),
    }
}

fn prefix_lines(prefix: &str, text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(prefix.to_owned(), style))];
    lines.extend(indent_wrapped(text, width));
    lines
}

fn indent_wrapped(text: &str, width: u16) -> Vec<Line<'static>> {
    let width = width.saturating_sub(2).max(1) as usize;
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    textwrap::wrap(text, options)
        .into_iter()
        .map(|line| Line::from(format!("  {line}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    use super::{Block, Transcript};

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
        let expected: Vec<usize> = super::render_block(&Block::user(text), 12)
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
}
