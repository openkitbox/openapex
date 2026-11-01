// ---------------------------------------------------------------------------
// Shared prompt helpers
// ---------------------------------------------------------------------------

/// The first `max` characters of `text`, cut on a character boundary.
///
/// Borrowed, never copied: a prompt is built once per run from rows the caller
/// already owns. A byte slice would panic on a multi-byte character, which
/// model-written memory text is full of.
pub(super) fn head_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}
