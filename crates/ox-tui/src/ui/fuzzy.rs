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

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn empty_query_matches_everything_with_zero() {
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("", ""), Some(0));
    }

    #[test]
    fn prefix_beats_substring_beats_subsequence() {
        assert_eq!(score("ex", "exit"), Some(3));
        assert_eq!(score("xi", "exit"), Some(2));
        assert_eq!(score("et", "exit"), Some(1));
        assert_eq!(score("zz", "exit"), None);
    }

    #[test]
    fn matching_ignores_case_on_both_sides() {
        assert_eq!(score("EX", "exit"), Some(3));
        assert_eq!(score("ex", "EXIT"), Some(3));
        assert_eq!(score("Xt", "eXiT"), Some(1));
    }

    #[test]
    fn subsequence_keeps_character_order() {
        assert_eq!(score("ti", "exit"), None);
        assert_eq!(score("eit", "exit"), Some(1));
    }

    #[test]
    fn query_longer_than_text_never_matches() {
        assert_eq!(score("exits", "exit"), None);
    }
}
