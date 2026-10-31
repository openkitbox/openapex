//! Minimal markdown rendering for assistant transcript text, shaped like
//! upstream's `<markdown conceal>` element: markers are hidden, blocks are
//! separated by one blank row, headings are bold + underlined without their
//! `#`, list items are `- ` with the dash in the link colour, and fenced code
//! is highlighted by [`super::highlight`] (no fence line, no gutter).
//!
//! Homegrown on purpose (no tree-sitter): headings, lists, quotes, rules,
//! fenced code and inline `code` / **bold** cover what an agent emits.
//!
//! Line-based by design: wrapping happens on the raw text (markers count
//! toward the width), then inline spans are applied to the wrapped line.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use textwrap::WordSeparator;
use textwrap::WordSplitter;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;
use crate::ui::highlight::highlight;

/// What kind of block the previous emitted line belonged to; a change of
/// kind (or a blank source line) puts one blank row between blocks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Heading,
    Paragraph,
    List,
    Quote,
    Rule,
    Code,
}

pub fn render(text: &str, width: u16, indent: usize) -> Vec<Line<'static>> {
    let indent = " ".repeat(indent);
    let palette = theme::palette();
    let base = palette.text;
    let code = palette.success;
    let heading = base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut previous: Option<Kind> = None;
    let mut gap = false;
    let mut fence: Option<Fence> = None;
    let code_width = width.saturating_sub(indent.len() as u16).max(1) as usize;

    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            match fence.take() {
                Some(open) => emit_code(&mut lines, &indent, open, code_width, palette),
                None => {
                    separate(&mut lines, &mut previous, &mut gap, Kind::Code);
                    fence = Some(Fence {
                        lang: rest.trim(),
                        code: Vec::new(),
                    });
                }
            }
            continue;
        }
        if let Some(open) = &mut fence {
            open.code.push(raw);
            continue;
        }
        if trimmed.is_empty() {
            gap = true;
            continue;
        }
        if trimmed.starts_with('#') {
            separate(&mut lines, &mut previous, &mut gap, Kind::Heading);
            let content = trimmed.trim_start_matches('#').trim_start();
            lines.extend(wrap_first_with_prefix(
                &[Span::raw(indent.clone())],
                &indent,
                content,
                width,
                heading,
                code,
            ));
        } else if let Some(content) = trimmed.strip_prefix("- ").or(trimmed.strip_prefix("* ")) {
            separate(&mut lines, &mut previous, &mut gap, Kind::List);
            lines.extend(wrap_first_with_prefix(
                &[Span::raw(indent.clone()), Span::styled("- ", palette.link)],
                &format!("{indent}  "),
                content,
                width,
                base,
                code,
            ));
        } else if let Some(content) = trimmed.strip_prefix("> ") {
            separate(&mut lines, &mut previous, &mut gap, Kind::Quote);
            lines.extend(wrap_first_with_prefix(
                &[Span::raw(indent.clone()), Span::styled("▌ ", palette.dim)],
                &format!("{indent}  "),
                content,
                width,
                palette.dim,
                code,
            ));
        } else if trimmed == "---" || trimmed == "***" {
            separate(&mut lines, &mut previous, &mut gap, Kind::Rule);
            lines.push(Line::from(Span::styled(
                format!("{indent}────"),
                palette.dim,
            )));
        } else {
            separate(&mut lines, &mut previous, &mut gap, Kind::Paragraph);
            lines.extend(wrap_first_with_prefix(
                &[Span::raw(indent.clone())],
                &indent,
                raw,
                width,
                base,
                code,
            ));
        }
    }
    // A fence still open at the end is a streaming reply: show what arrived.
    if let Some(open) = fence {
        emit_code(&mut lines, &indent, open, code_width, palette);
    }
    lines
}

/// An open fenced block: its info string and the raw lines seen so far.
struct Fence<'a> {
    lang: &'a str,
    code: Vec<&'a str>,
}

/// Highlight the whole block (lexer state spans lines), then clip each line to
/// the viewport: code is never re-wrapped.
fn emit_code(
    lines: &mut Vec<Line<'static>>,
    indent: &str,
    fence: Fence<'_>,
    width: usize,
    palette: &theme::Palette,
) {
    let code = fence.code.join("\n");
    for spans in highlight(fence.lang, &code, palette) {
        let mut line = vec![Span::raw(indent.to_owned())];
        line.extend(clip(spans, width));
        lines.push(Line::from(line));
    }
}

/// Keep the first `width` display columns of a span list (a wide glyph that
/// would straddle the edge is dropped).
fn clip(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut left = width;
    let mut out = Vec::with_capacity(spans.len());
    for mut span in spans {
        if left == 0 {
            break;
        }
        let columns = span.content.width();
        if columns > left {
            let mut used = 0;
            let cut = span
                .content
                .char_indices()
                .find(|&(_, ch)| {
                    used += ch.width().unwrap_or(0);
                    used > left
                })
                .map_or(span.content.len(), |(at, _)| at);
            span.content.to_mut().truncate(cut);
            left = 0;
        } else {
            left -= columns;
        }
        out.push(span);
    }
    out
}

/// One blank row between blocks: when the kind changes, or when the source
/// had a blank line (and something was already emitted).
fn separate(
    lines: &mut Vec<Line<'static>>,
    previous: &mut Option<Kind>,
    gap: &mut bool,
    kind: Kind,
) {
    if previous.is_some_and(|prev| prev != kind || *gap) && !lines.is_empty() {
        lines.push(Line::default());
    }
    *previous = Some(kind);
    *gap = false;
}

fn wrap_first_with_prefix(
    first: &[Span<'static>],
    rest_prefix: &str,
    text: &str,
    width: u16,
    style: Style,
    code: Style,
) -> Vec<Line<'static>> {
    let first_width: usize = first.iter().map(|span| span.width()).sum();
    let usable = (width as usize).saturating_sub(first_width).max(1);
    let options = textwrap::Options::new(usable)
        .word_separator(WordSeparator::UnicodeBreakProperties)
        .word_splitter(WordSplitter::NoHyphenation);
    textwrap::wrap(text, options)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = if index == 0 {
                first.to_vec()
            } else {
                vec![Span::raw(rest_prefix.to_owned())]
            };
            spans.extend(inline_spans(&line, style, code));
            Line::from(spans)
        })
        .collect()
}

/// Split a plain (already wrapped) line into styled spans: `` `code` `` and
/// `**bold**` are recognized; everything else keeps the base style.
fn inline_spans(text: &str, base: Style, code: Style) -> Vec<Span<'static>> {
    let bold = base.add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        if open > 0 {
            push_bold_aware(&mut spans, &rest[..open], base, bold);
        }
        let after_open = &rest[open + 1..];
        match after_open.find('`') {
            Some(close) => {
                if close > 0 {
                    spans.push(Span::styled(after_open[..close].to_owned(), code));
                }
                rest = &after_open[close + 1..];
            }
            None => {
                // Unbalanced backtick: keep it as plain text.
                spans.push(Span::styled("`".to_owned(), base));
                rest = after_open;
            }
        }
    }
    if !rest.is_empty() {
        push_bold_aware(&mut spans, rest, base, bold);
    }
    spans
}

fn push_bold_aware(spans: &mut Vec<Span<'static>>, text: &str, base: Style, bold: Style) {
    let mut rest = text;
    while let Some(open) = rest.find("**") {
        if open > 0 {
            spans.push(Span::styled(rest[..open].to_owned(), base));
        }
        let after_open = &rest[open + 2..];
        match after_open.find("**") {
            Some(close) => {
                if close > 0 {
                    spans.push(Span::styled(after_open[..close].to_owned(), bold));
                }
                rest = &after_open[close + 2..];
            }
            None => {
                spans.push(Span::styled("**".to_owned(), base));
                rest = after_open;
            }
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_owned(), base));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::{clip, inline_spans, render};

    #[test]
    fn clip_counts_display_columns_not_chars() {
        use ratatui::text::Span;
        use unicode_width::UnicodeWidthStr;

        let clipped = clip(vec![Span::raw("你好世界")], 5);
        let text: String = clipped.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "你好", "two wide glyphs fill 4 of 5 columns");
        assert!(text.width() <= 5);

        let across = clip(vec![Span::raw("ab"), Span::raw("你好")], 4);
        let text: String = across.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "ab你");
    }

    fn strings(lines: &[ratatui::text::Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn fenced_code_has_no_fence_line_or_gutter_and_is_never_rewrapped() {
        let long_code = "x".repeat(80);
        let text = format!("```rust\nlet a = 1;\n{long_code}\n```\nafter");
        let lines = render(&text, 40, 3);
        let rendered = strings(&lines);
        assert_eq!(rendered.len(), 4, "{rendered:?}");
        assert_eq!(rendered[0], "   let a = 1;", "no `── rust` line, no `│`");
        // The 80-char code line clips instead of re-wrapping into two lines.
        assert!(rendered[1].chars().count() <= 3 + 40, "{rendered:?}");
        assert_eq!(rendered[2], "", "one blank row between blocks");
        assert_eq!(rendered[3].trim_end(), "   after");
    }

    #[test]
    fn fenced_code_is_highlighted_and_clipped_by_characters() {
        let palette = crate::theme::Palette::for_deck(crate::theme::Deck::Truecolor);
        let lines = render("```rust\nfn main() {}\n```", 40, 3);
        let first = &lines[0];
        let fn_span = first
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "fn")
            .expect("fn span");
        assert_eq!(fn_span.style.fg, palette.syntax.function.fg);
        // Multi-byte text clips on a character boundary.
        let wide = format!("```text\n{}\n```", "é".repeat(60));
        let rendered = strings(&render(&wide, 20, 3));
        assert_eq!(rendered[0].chars().count(), 20, "{rendered:?}");
    }

    #[test]
    fn an_unterminated_fence_shows_what_has_streamed_in() {
        let rendered = strings(&render("intro\n```rust\nlet a = 1;", 40, 3));
        assert_eq!(rendered, ["   intro", "", "   let a = 1;"]);
    }

    #[test]
    fn headings_bullets_quotes_and_rules_render() {
        let lines = render("# Title\n- first\n- second\n> quoted\n---\nplain", 40, 3);
        let rendered = strings(&lines);
        assert_eq!(
            rendered,
            [
                "   Title",
                "",
                "   - first",
                "   - second",
                "",
                "   ▌ quoted",
                "",
                "   ────",
                "",
                "   plain"
            ],
            "heading without `#`, `- ` bullets, blocks one blank row apart"
        );
    }

    #[test]
    fn inline_code_and_bold_get_distinct_styles() {
        let spans = inline_spans(
            "run `cargo test` and **verify**",
            Style::default(),
            Style::new(),
        );
        let text: Vec<&str> = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text.join(""), "run cargo test and verify");
        let code_span = spans
            .iter()
            .find(|span| span.content.as_ref() == "cargo test")
            .expect("code span");
        let bold_span = spans
            .iter()
            .find(|span| span.content.as_ref() == "verify")
            .expect("bold span");
        assert_ne!(code_span.style, bold_span.style, "code and bold differ");
    }

    #[test]
    fn unbalanced_markers_render_literally() {
        let spans = inline_spans("a ` b ** c", Style::default(), Style::new());
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "a ` b ** c");
    }

    #[test]
    fn paragraphs_wrap_with_a_two_space_indent() {
        let text = format!("{} {}", "word ".repeat(20), "end");
        let lines = render(&text, 20, 3);
        let rendered = strings(&lines);
        assert!(rendered.len() >= 3, "{rendered:?}");
        assert!(rendered.iter().all(|line| line.starts_with("   ")));
    }
}
