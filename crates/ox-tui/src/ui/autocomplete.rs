//! When the slash-command menu is open, and which row is selected.
//!
//! Derived state only: [`Autocomplete::sync`] recomputes everything from the
//! editor text plus cursor, so there is no second source of truth to drift.
//! Mirrors upstream `prompt/display.ts:33-41` (trigger scan) and
//! `component/prompt/autocomplete.tsx:756-784` (visibility rules).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::fuzzy;

/// The text typed after a leading `/`, when the menu should be open.
///
/// A command can only start at the very first character of the prompt (upstream
/// `slashTriggerIndex(...) === 0`). Any whitespace, a second `/`, or a cursor
/// off the first line closes it.
pub fn command_query(text: &str, cursor: (usize, usize)) -> Option<String> {
    let (row, col) = cursor;
    if row != 0 {
        return None;
    }
    let rest = text.strip_prefix('/')?;
    let query = take_chars(rest, col.saturating_sub(1));
    if query.contains('/') || query.chars().any(char::is_whitespace) {
        return None;
    }
    // Text past the cursor is irrelevant, but a slash typed later must not be
    // mistaken for the trigger.
    Some(query.to_owned())
}

/// The `@` token ending at the cursor: its column and the text after the `@`.
///
/// Port of upstream `mentionTriggerIndex` (`prompt/display.ts`): the last `@`
/// before the cursor opens the menu when it starts the line or follows
/// whitespace, and nothing between it and the cursor is whitespace. Only the
/// cursor's own line matters, because a newline is whitespace.
pub fn mention_query(text: &str, cursor: (usize, usize)) -> Option<(usize, String)> {
    let (row, col) = cursor;
    let line = text.split('\n').nth(row)?;
    let before: Vec<char> = line.chars().take(col).collect();
    let at = before.iter().rposition(|&c| c == '@')?;
    let starts_token = at == 0 || before[at - 1].is_whitespace();
    let query: String = before[at + 1..].iter().collect();
    if !starts_token || query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at, query))
}

fn take_chars(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Which token opened the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Directory,
    /// A leading `/command`.
    Command,
    /// An `@path` mention starting at this column of the cursor's row.
    Mention {
        col: usize,
    },
}

/// Menu focus state. Visibility, trigger and query are always derived; only the
/// selected row is remembered (reset to the first row whenever they change).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Autocomplete {
    open: Option<(Trigger, String)>,
    selected: usize,
}

impl Autocomplete {
    #[must_use]
    pub fn visible(&self) -> bool {
        self.open.is_some()
    }

    #[must_use]
    pub fn query(&self) -> &str {
        self.open.as_ref().map_or("", |(_, query)| query)
    }

    #[must_use]
    pub fn trigger(&self) -> Option<Trigger> {
        self.open.as_ref().map(|(trigger, _)| *trigger)
    }

    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Recompute from the editor. Returns `true` when the query changed, which
    /// is the caller's cue to re-filter and repaint.
    pub fn sync(&mut self, text: &str, cursor: (usize, usize)) -> bool {
        let next = (cursor.0 == 0)
            .then(|| take_chars(text, cursor.1))
            .and_then(|text| text.strip_prefix("/cd "))
            .map(|query| (Trigger::Directory, query.to_owned()))
            .or_else(|| {
                command_query(text, cursor)
                    .map(|query| (Trigger::Command, query))
                    .or_else(|| {
                        mention_query(text, cursor)
                            .map(|(col, query)| (Trigger::Mention { col }, query))
                    })
            });
        if next == self.open {
            return false;
        }
        self.selected = 0;
        self.open = next;
        true
    }

    /// Give up the menu without touching the text (upstream `hide()`).
    pub fn hide(&mut self) {
        self.open = None;
        self.selected = 0;
    }

    /// Move the selection with wrap-around (upstream `moveSelection` policy).
    pub fn move_by(&mut self, delta: isize, count: usize) {
        self.selected = step(self.selected, count, delta);
    }

    /// Keep the selection inside a list that shrank under it.
    pub fn clamp(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }
}

/// One row of the `@` menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Path relative to the session directory; inserted verbatim after `@`.
    pub path: String,
    pub is_dir: bool,
}

/// Order the server's matches for the menu: directories first, then the
/// better fuzzy match (prefix, substring, subsequence), then the shorter path,
/// then alphabetically. Matches the fuzzy scale of the slash menu.
#[must_use]
pub fn order_files(query: &str, entries: &[ox_client::rest::FileEntry]) -> Vec<FileRow> {
    let mut scored: Vec<(u32, FileRow)> = entries
        .iter()
        .map(|entry| {
            let score = fuzzy::score(query, &entry.path).unwrap_or(0);
            (
                score,
                FileRow {
                    path: entry.path.clone(),
                    is_dir: entry.is_dir(),
                },
            )
        })
        .collect();
    scored.sort_by(|(a_score, a), (b_score, b)| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| b_score.cmp(a_score))
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
    scored.into_iter().map(|(_, row)| row).collect()
}

/// Shorten `text` to `width` columns by replacing its middle with `…`
/// (upstream `Locale.truncateMiddle`).
#[must_use]
pub fn truncate_middle(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1;
    let head_budget = budget.div_ceil(2);
    let tail_budget = budget - head_budget;
    let mut head = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > head_budget {
            break;
        }
        used += w;
        head.push(ch);
    }
    let mut tail: Vec<char> = Vec::new();
    used = 0;
    for ch in text.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > tail_budget {
            break;
        }
        used += w;
        tail.push(ch);
    }
    tail.reverse();
    format!("{head}…{}", tail.into_iter().collect::<String>())
}

/// Wrap-around index arithmetic shared by the menu (and later any picker).
#[must_use]
pub fn step(selected: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let count = count as isize;
    let selected = selected as isize;
    (((selected + delta) % count) + count) as usize % count as usize
}
