use ratatui::text::{Line, Span};

use crate::theme;

use super::block::Block;

/// Rows of an entry that are layout and not text: the blank margin above every
/// block, and the inner padding above and below a user message.
pub(super) fn text_rows(block: &Block) -> (usize, usize) {
    match block {
        // Margin and top padding above, bottom padding below.
        Block::User { .. } | Block::Shell { .. } => (2, 1),
        Block::Reasoning { .. }
        | Block::Assistant { .. }
        | Block::Switch(_)
        | Block::Notice(_)
        | Block::Error(_)
        | Block::CompactionQueued
        | Block::BackgroundHint
        | Block::Explored(_)
        | Block::Tool { .. }
        | Block::Todo { .. }
        | Block::Compaction { .. }
        | Block::Completion { .. } => (1, 0),
    }
}

/// Paint a whole line with the search highlight: the palette decides the style,
/// so `NO_COLOR` degrades it to reverse video.
pub(super) fn highlight(line: &Line<'static>) -> Line<'static> {
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

pub(super) fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Wrap a tool output line by line: structure (`ls`, `git status`) must survive
/// the wrap, so explicit newlines stay line breaks instead of collapsing.
pub(super) fn wrap_output(output: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut wrapped = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let options = textwrap::Options::new(width)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);
        wrapped.extend(textwrap::wrap(line, options).into_iter().map(String::from));
    }
    wrapped
}

/// Mirrors upstream `Locale.duration`.
pub(super) fn format_duration(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else {
        format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

pub(crate) fn titlecase(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub(super) fn indent_wrapped(text: &str, width: u16, indent: usize) -> Vec<Line<'static>> {
    let pad = " ".repeat(indent);
    let width = width.saturating_sub(indent as u16).max(1) as usize;
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    textwrap::wrap(text, options)
        .into_iter()
        .map(|line| Line::from(format!("{pad}{line}")))
        .collect()
}

/// Wrap `text` to `width` and keep at most `max` lines, marking a cut with `…`.
pub(super) fn bound_wrapped(text: &str, width: u16, max: usize) -> Vec<String> {
    let width = width.max(1) as usize;
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    let wrapped: Vec<String> = textwrap::wrap(text, options)
        .into_iter()
        .map(String::from)
        .collect();
    if wrapped.len() <= max {
        return wrapped;
    }
    let mut kept: Vec<String> = wrapped.into_iter().take(max).collect();
    if let Some(last) = kept.last_mut() {
        last.push('…');
    }
    kept
}
