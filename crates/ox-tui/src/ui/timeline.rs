use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

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
}

impl Transcript {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            entries: Vec::new(),
            scroll: 0,
            sticky_bottom: true,
            width,
            height,
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
        self.clamp();
    }

    pub fn entry_generation(&self, index: usize) -> u64 {
        self.entries[index].generation
    }

    pub fn total_lines(&self) -> usize {
        self.entries.iter().map(|entry| entry.lines.len()).sum()
    }

    pub fn set_viewport(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
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

        for entry in &mut self.entries {
            let lines = entry.lines_for(area.width);
            let count = lines.len();

            if remaining_skip >= count {
                remaining_skip -= count;
                continue;
            }

            let end = (area.y + area.height).saturating_sub(y) as usize;
            let visible = &lines[remaining_skip..lines.len().min(remaining_skip + end)];
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
            frame.render_widget(Paragraph::new(visible.to_vec()), rect);
            y += height;
            if y >= area.y + area.height {
                break;
            }
        }
    }
}

fn render_block(block: &Block, width: u16) -> Vec<Line<'static>> {
    match block {
        Block::User { text } => prefix_lines(
            "▌ you",
            text,
            width,
            Style::default().add_modifier(Modifier::BOLD),
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
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if !reasoning.is_empty() {
                lines.push(Line::from(format!(
                    "  ▸ reasoning · {} chars 已折叠",
                    reasoning.chars().count()
                )));
            }
            lines.extend(indent_wrapped(text, width));
            lines
        }
        Block::System { text } => prefix_lines("⤷", text, width, Style::default()),
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

    use super::{Block, Transcript};

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
