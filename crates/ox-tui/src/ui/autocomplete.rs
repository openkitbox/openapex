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
