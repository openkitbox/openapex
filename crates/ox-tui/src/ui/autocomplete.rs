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
    /// A leading `/command`.
    Command,
    /// An `@path` mention starting at this column of the cursor's row.
    Mention { col: usize },
}

/// Menu focus state. Visibility, trigger and query are always derived; only the
/// selected row is remembered (reset to the first row whenever they change).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Autocomplete {
    open: Option<(Trigger, String)>,
    selected: usize,
}

impl Autocomplete {
    pub fn visible(&self) -> bool {
        self.open.is_some()
    }

    pub fn query(&self) -> &str {
        self.open.as_ref().map_or("", |(_, query)| query)
    }

    pub fn trigger(&self) -> Option<Trigger> {
        self.open.as_ref().map(|(trigger, _)| *trigger)
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Recompute from the editor. Returns `true` when the query changed, which
    /// is the caller's cue to re-filter and repaint.
    pub fn sync(&mut self, text: &str, cursor: (usize, usize)) -> bool {
        let next = command_query(text, cursor)
            .map(|query| (Trigger::Command, query))
            .or_else(|| {
                mention_query(text, cursor).map(|(col, query)| (Trigger::Mention { col }, query))
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
pub fn step(selected: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let count = count as isize;
    let selected = selected as isize;
    (((selected + delta) % count) + count) as usize % count as usize
}

#[cfg(test)]
mod tests {
    use super::{
        Autocomplete, Trigger, command_query, mention_query, order_files, step, truncate_middle,
    };
    use ox_client::rest::FileEntry;

    fn entry(path: &str, kind: &str) -> FileEntry {
        FileEntry {
            path: path.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn an_at_sign_opens_a_mention_with_the_text_after_it() {
        assert_eq!(mention_query("@", (0, 1)), Some((0, String::new())));
        assert_eq!(mention_query("@src/ma", (0, 7)), Some((0, "src/ma".into())));
        assert_eq!(
            mention_query("look at @fo", (0, 11)),
            Some((8, "fo".into()))
        );
    }

    #[test]
    fn a_mention_needs_a_word_boundary_before_the_at() {
        assert_eq!(mention_query("me@example", (0, 10)), None);
        assert_eq!(mention_query("a @b", (0, 4)), Some((2, "b".into())));
    }

    #[test]
    fn whitespace_after_the_at_closes_the_mention() {
        assert_eq!(mention_query("@foo bar", (0, 8)), None);
        assert_eq!(mention_query("@a\nb", (1, 1)), None, "other line");
    }

    #[test]
    fn the_mention_stops_at_the_cursor_and_uses_the_last_at() {
        assert_eq!(mention_query("@foo", (0, 2)), Some((0, "f".into())));
        assert_eq!(mention_query("@a @b", (0, 5)), Some((3, "b".into())));
        assert_eq!(mention_query("@a @b", (0, 2)), Some((0, "a".into())));
        assert_eq!(mention_query("héllo @ü", (0, 8)), Some((6, "ü".into())));
    }

    #[test]
    fn sync_tracks_the_trigger_kind() {
        let mut auto = Autocomplete::default();
        assert!(auto.sync("see @sr", (0, 7)));
        assert_eq!(auto.trigger(), Some(Trigger::Mention { col: 4 }));
        assert_eq!(auto.query(), "sr");
        assert!(auto.sync("/ne", (0, 3)));
        assert_eq!(auto.trigger(), Some(Trigger::Command));
        assert!(auto.sync("hello", (0, 5)));
        assert_eq!(auto.trigger(), None);
    }

    #[test]
    fn directories_come_first_then_the_better_fuzzy_match() {
        let entries = [
            entry("zz/main.rs", "file"),
            entry("main.rs", "file"),
            entry("crates/domain", "directory"),
            entry("m/a/i/n", "directory"),
            entry("src/main.rs", "file"),
        ];
        let paths: Vec<String> = order_files("main", &entries)
            .into_iter()
            .map(|row| row.path)
            .collect();
        assert_eq!(
            paths,
            [
                "crates/domain",
                "m/a/i/n",
                "main.rs",
                "zz/main.rs",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn truncate_middle_keeps_both_ends() {
        assert_eq!(truncate_middle("short", 10), "short");
        assert_eq!(truncate_middle("abcdefghij", 7), "abc…hij");
        assert_eq!(truncate_middle("abcdefghij", 6), "abc…ij");
        assert_eq!(truncate_middle("abc", 0), "");
        assert!(truncate_middle("中文路径很长很长", 7).len() < "中文路径很长很长".len());
    }

    #[test]
    fn a_leading_slash_opens_an_empty_query() {
        assert_eq!(command_query("/", (0, 1)).as_deref(), Some(""));
        assert_eq!(command_query("/ne", (0, 3)).as_deref(), Some("ne"));
    }

    #[test]
    fn the_query_stops_at_the_cursor() {
        assert_eq!(command_query("/new", (0, 2)).as_deref(), Some("n"));
        assert_eq!(command_query("/new", (0, 0)).as_deref(), Some(""));
    }

    #[test]
    fn a_slash_inside_the_message_does_not_open_the_menu() {
        assert_eq!(command_query("hello /ne", (0, 9)), None);
        assert_eq!(command_query("a\n/ne", (1, 3)), None);
    }

    #[test]
    fn whitespace_or_a_second_slash_closes_it() {
        assert_eq!(command_query("/new foo", (0, 8)), None);
        assert_eq!(command_query("/a/b", (0, 4)), None);
        assert_eq!(command_query("/new", (1, 0)), None);
    }

    #[test]
    fn typing_reopens_and_resets_the_selection() {
        let mut auto = Autocomplete::default();
        assert!(auto.sync("/", (0, 1)));
        assert!(auto.visible());
        auto.move_by(2, 5);
        assert_eq!(auto.selected(), 2);
        assert!(auto.sync("/n", (0, 2)));
        assert_eq!(auto.selected(), 0, "a new query resets the selection");
        assert!(!auto.sync("/n", (0, 2)), "same query is not a change");
    }

    #[test]
    fn deleting_the_trigger_hides_the_menu() {
        let mut auto = Autocomplete::default();
        auto.sync("/n", (0, 2));
        assert!(auto.sync("n", (0, 1)));
        assert!(!auto.visible());
        assert_eq!(auto.query(), "");
    }

    #[test]
    fn hide_keeps_the_text_and_is_idempotent() {
        let mut auto = Autocomplete::default();
        auto.sync("/n", (0, 2));
        auto.hide();
        assert!(!auto.visible());
        auto.hide();
        assert!(!auto.visible());
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(0, 3, 5), 2);
        assert_eq!(step(0, 0, 1), 0);
        assert_eq!(step(4, 3, 0), 1);
    }

    #[test]
    fn clamp_keeps_the_selection_inside_a_shrunk_list() {
        let mut auto = Autocomplete::default();
        auto.sync("/", (0, 1));
        auto.move_by(4, 5);
        assert_eq!(auto.selected(), 4);
        auto.clamp(2);
        assert_eq!(auto.selected(), 1);
        auto.clamp(0);
        assert_eq!(auto.selected(), 0);
    }
}
