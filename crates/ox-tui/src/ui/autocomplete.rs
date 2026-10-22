//! When the slash-command menu is open, and which row is selected.
//!
//! Derived state only: [`Autocomplete::sync`] recomputes everything from the
//! editor text plus cursor, so there is no second source of truth to drift.
//! Mirrors upstream `prompt/display.ts:33-41` (trigger scan) and
//! `component/prompt/autocomplete.tsx:756-784` (visibility rules).

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

fn take_chars(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Menu focus state. Visibility and query are always derived; only the selected
/// row is remembered (reset to the first row whenever the query changes).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Autocomplete {
    query: Option<String>,
    selected: usize,
}

impl Autocomplete {
    pub fn visible(&self) -> bool {
        self.query.is_some()
    }

    pub fn query(&self) -> &str {
        self.query.as_deref().unwrap_or("")
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Recompute from the editor. Returns `true` when the query changed, which
    /// is the caller's cue to re-filter and repaint.
    pub fn sync(&mut self, text: &str, cursor: (usize, usize)) -> bool {
        let next = command_query(text, cursor);
        if next == self.query {
            return false;
        }
        self.selected = 0;
        self.query = next;
        true
    }

    /// Give up the menu without touching the text (upstream `hide()`).
    pub fn hide(&mut self) {
        self.query = None;
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
    use super::{Autocomplete, command_query, step};

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
