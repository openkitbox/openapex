//! Fuzzy scoring shared by the slash-command menu and the select dialogs.
//!
//! Pure decision logic. Upstream uses fuzzysort with a prefix bonus
//! (`autocomplete.tsx`, `ui/dialog-select.tsx`); ox reduces it to a
//! deterministic three-step scale so ordering is easy to test.

/// Score `text` against `query`, case-insensitively.
///
/// `None` means no match. Otherwise: `3` prefix, `2` substring, `1`
/// subsequence. An empty query matches everything with `0`.
pub fn score(query: &str, text: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let text = text.to_lowercase();
    if text.starts_with(&query) {
        Some(3)
    } else if text.contains(&query) {
        Some(2)
    } else if is_subsequence(&query, &text) {
        Some(1)
    } else {
        None
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}
