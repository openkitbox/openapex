//! Minimal markdown rendering for assistant transcript text, shaped like
//! upstream's `<markdown conceal>` element: markers are hidden, blocks are
//! separated by one blank row, headings are bold + underlined without their
//! `#`, list items are `- ` with the dash in the link colour, and fenced code
//! is highlighted by [`super::highlight`] (no fence line, no gutter).
//!
//! Homegrown on purpose (no tree-sitter): headings, lists, quotes, rules,
//! fenced code, GFM pipe tables (drawn by [`super::table`]) and inline
//! `code` / **bold** cover what an agent emits.
//!
//! Line-based by design: wrapping happens on the raw text (markers count
//! toward the width), then inline spans are applied to the wrapped line.
//! Table columns follow the same rule: their natural width is measured on the
//! raw cell text, so a cell that measures as fitting is never split by wrap.

use std::borrow::Cow;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use textwrap::WordSeparator;
use textwrap::WordSplitter;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;
use crate::ui::highlight::highlight;
use crate::ui::table;

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
    Table,
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

    let source: Vec<&str> = text.lines().collect();
    let mut at = 0;
    while at < source.len() {
        let raw = source[at];
        at += 1;
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
        if let Some((rows, used)) = parse_table(trimmed, &source[at..]) {
            separate(&mut lines, &mut previous, &mut gap, Kind::Table);
            let style = TableStyle {
                head: heading,
                body: base,
                code,
                border: palette.dim,
            };
            emit_table(&mut lines, &rows, code_width, &indent, style);
            at += used;
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

/// Styles a table is drawn with: header text, body text, inline code, frame.
#[derive(Clone, Copy)]
struct TableStyle {
    head: Style,
    body: Style,
    code: Style,
    border: Style,
}

type Row<'a> = Vec<Cow<'a, str>>;

/// A GFM table starting at `header`: it needs a pipe, and `rest` must open
/// with a separator row of the same cell count. Returns the rows (header
/// first, each normalised to the header's column count) and how many lines of
/// `rest` were consumed (separator included). Alignment colons are accepted
/// and ignored, as upstream does.
fn parse_table<'a>(header: &'a str, rest: &[&'a str]) -> Option<(Vec<Row<'a>>, usize)> {
    if !header.contains('|') {
        return None;
    }
    let head = split_cells(header);
    if !is_separator(rest.first()?, head.len()) {
        return None;
    }
    let columns = head.len();
    let mut rows = vec![head];
    let mut used = 1;
    for line in &rest[1..] {
        let line = line.trim();
        if !line.contains('|') || line.starts_with("```") {
            break;
        }
        let mut row = split_cells(line);
        row.truncate(columns);
        row.resize_with(columns, || Cow::Borrowed(""));
        rows.push(row);
        used += 1;
    }
    Some((rows, used))
}

/// `|---|:-:|` with exactly `columns` cells, each `:?-+:?`.
fn is_separator(line: &str, columns: usize) -> bool {
    let cells = split_cells(line);
    line.contains('|')
        && cells.len() == columns
        && cells.iter().all(|cell| {
            let dashes = cell.strip_prefix(':').unwrap_or(cell);
            let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
            !dashes.is_empty() && dashes.bytes().all(|b| b == b'-')
        })
}

/// Split a table line on unescaped `|` (one outer pipe each side is
/// dropped), trim every cell and turn `\|` into `|`. Cells borrow from
/// `line` unless an escape forces a copy.
fn split_cells(line: &str) -> Vec<Cow<'_, str>> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = match line.strip_suffix('|') {
        Some(rest) if !rest.ends_with('\\') => rest,
        _ => line,
    };
    let bytes = line.as_bytes();
    let mut cells = Vec::new();
    let mut start = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        if byte == b'|' && (at == 0 || bytes[at - 1] != b'\\') {
            cells.push(unescape(line[start..at].trim()));
            start = at + 1;
        }
    }
    cells.push(unescape(line[start..].trim()));
    cells
}

fn unescape(cell: &str) -> Cow<'_, str> {
    if cell.contains("\\|") {
        Cow::Owned(cell.replace("\\|", "|"))
    } else {
        Cow::Borrowed(cell)
    }
}

/// Draw `rows` (header first) as a grid sized to `avail` display columns
/// after `indent`; when even minimum-width columns do not fit, fall back to
/// one ` · `-joined line per row (deviation D1).
fn emit_table(
    lines: &mut Vec<Line<'static>>,
    rows: &[Row<'_>],
    avail: usize,
    indent: &str,
    style: TableStyle,
) {
    let columns = rows[0].len();
    let avail = avail.max(1);
    if columns * 3 + columns + 1 > avail {
        emit_dotted(lines, rows, avail, indent, style);
        return;
    }
    // Raw cell text is measured and wrapped alike (deviation D6).
    let natural: Vec<usize> = (0..columns)
        .map(|col| {
            rows.iter()
                .map(|row| UnicodeWidthStr::width(row[col].as_ref()) + 2)
                .max()
                .unwrap_or(3)
        })
        .collect();
    let widths = table::column_widths(&natural, avail - (columns + 1));
    // A 2-cell glyph cannot sit in a 1-cell content area, and clipping it
    // would lose text: degrade instead (deviation D7).
    let glyph_overflows = widths.iter().enumerate().any(|(col, &w)| {
        w < 4
            && rows.iter().any(|row| {
                row[col]
                    .chars()
                    .any(|c| UnicodeWidthChar::width(c).is_some_and(|cw| cw >= 2))
            })
    });
    if glyph_overflows {
        emit_dotted(lines, rows, avail, indent, style);
        return;
    }
    let cells: Vec<Vec<Vec<Vec<Span<'static>>>>> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let text = if index == 0 { style.head } else { style.body };
            row.iter()
                .zip(&widths)
                .map(|(cell, &width)| {
                    wrap_cell(cell, width.saturating_sub(2).max(1), text, style.code)
                })
                .collect()
        })
        .collect();
    lines.extend(table::grid_lines(cells, &widths, indent, style.border));
}

/// Narrow fallback (deviation D1): one ` · `-joined line per row, clipped to
/// `avail`.
fn emit_dotted(
    lines: &mut Vec<Line<'static>>,
    rows: &[Row<'_>],
    avail: usize,
    indent: &str,
    style: TableStyle,
) {
    for (index, row) in rows.iter().enumerate() {
        let text = if index == 0 { style.head } else { style.body };
        let mut spans = Vec::new();
        for (col, cell) in row.iter().enumerate() {
            if col > 0 {
                spans.push(Span::styled(" · ", style.border));
            }
            spans.extend(inline_spans(cell, text, style.code));
        }
        let mut line = vec![Span::raw(indent.to_owned())];
        line.extend(clip(spans, avail));
        lines.push(Line::from(line));
    }
}

/// Wrap one cell's raw text to `width` columns and colour each physical line.
fn wrap_cell(cell: &str, width: usize, style: Style, code: Style) -> Vec<Vec<Span<'static>>> {
    if cell.is_empty() {
        return vec![Vec::new()];
    }
    let options = textwrap::Options::new(width)
        .word_separator(WordSeparator::UnicodeBreakProperties)
        .word_splitter(WordSplitter::NoHyphenation);
    textwrap::wrap(cell, options)
        .iter()
        .map(|line| inline_spans(line, style, code))
        .collect()
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

    // ---- tables ----

    fn display_width(line: &str) -> usize {
        unicode_width::UnicodeWidthStr::width(line)
    }

    fn span_of<'a>(
        lines: &'a [ratatui::text::Line<'static>],
        content: &str,
    ) -> &'a ratatui::text::Span<'static> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == content)
            .unwrap_or_else(|| panic!("no span {content:?}"))
    }

    const SMALL: &str = "| a | b |\n|---|---|\n| 1 | 2 |";

    #[test]
    fn a_basic_table_draws_a_grid_without_the_separator_row() {
        let text = "| Name | Role |\n|------|------|\n| ann | dev |\n| bob | ops |";
        let rendered = strings(&render(text, 40, 3));
        let all = rendered.join("\n");
        for glyph in ["┌", "┬", "┐", "├", "┼", "┤", "└", "┴", "┘", "│"] {
            assert!(all.contains(glyph), "missing {glyph}: {rendered:?}");
        }
        assert!(!all.contains("---"), "separator leaked: {rendered:?}");
        assert!(rendered[1].contains("Name") && rendered[1].contains("Role"));
        assert!(rendered[rendered.len() - 2].contains("bob"));
        assert!(rendered[rendered.len() - 2].contains("ops"));
    }

    #[test]
    fn a_small_table_fills_the_line_with_exact_text() {
        // width 23 - indent 3 = 20 avail; borders 3; natural [3, 3] expands
        // to [9, 8] (remainder to the first column).
        let rendered = strings(&render(SMALL, 23, 3));
        assert_eq!(
            rendered,
            [
                "   ┌─────────┬────────┐",
                "   │ a       │ b      │",
                "   ├─────────┼────────┤",
                "   │ 1       │ 2      │",
                "   └─────────┴────────┘",
            ]
        );
        assert!(rendered.iter().all(|l| display_width(l) == 23));
    }

    #[test]
    fn a_table_gets_one_blank_row_on_each_side() {
        let blank = strings(&render(
            "before\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nafter",
            20,
            0,
        ));
        assert_eq!(blank.len(), 9, "{blank:?}");
        assert_eq!(blank[0].trim_end(), "before");
        assert_eq!(blank[1], "");
        assert!(blank[2].starts_with('┌'));
        assert!(blank[6].starts_with('└'));
        assert_eq!(blank[7], "");
        assert_eq!(blank[8].trim_end(), "after");

        // No blank line in the source: the change of block kind adds one.
        let tight = strings(&render(
            "before\n| a | b |\n|---|---|\n| 1 | 2 |\nafter",
            20,
            0,
        ));
        assert_eq!(tight.len(), 9, "{tight:?}");
        assert_eq!(tight[0].trim_end(), "before");
        assert_eq!(tight[8].trim_end(), "after");
        assert_eq!(tight[1], "");
        assert_eq!(tight[7], "");
    }

    #[test]
    fn pipes_inside_a_fence_are_not_a_table() {
        let rendered = strings(&render("```\n| a | b |\n|---|---|\n| 1 | 2 |\n```", 30, 0));
        assert!(!rendered.join("").contains('┌'), "{rendered:?}");
        assert!(rendered.iter().any(|l| l.contains("|---|---|")));
    }

    #[test]
    fn a_header_without_a_separator_yet_is_a_paragraph() {
        assert_eq!(strings(&render("| a | b |", 30, 0)), ["| a | b |"]);
        // A one-cell separator-looking line does not complete a 2-col header.
        let rendered = strings(&render("| a | b |\n|---|", 30, 0));
        assert!(!rendered.join("").contains('┌'), "{rendered:?}");
    }

    #[test]
    fn a_table_with_no_data_rows_is_header_only() {
        let rendered = strings(&render("| a | b |\n|---|---|", 20, 0));
        assert_eq!(rendered.len(), 3, "{rendered:?}");
        assert!(rendered[0].starts_with('┌'));
        assert!(rendered[1].contains('a') && rendered[1].contains('b'));
        assert!(rendered[2].starts_with('└'));
    }

    #[test]
    fn cells_get_inline_styles_and_the_header_is_bold_underlined() {
        use ratatui::style::Modifier;

        let palette = crate::theme::palette();
        let lines = render("| h1 | h2 |\n|---|---|\n| `code` | **bold** |", 40, 0);
        assert_eq!(span_of(&lines, "code").style, palette.success);
        let bold = span_of(&lines, "bold");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert_ne!(bold.style, palette.text);
        let header = span_of(&lines, "h1");
        assert!(header.style.add_modifier.contains(Modifier::BOLD));
        assert!(header.style.add_modifier.contains(Modifier::UNDERLINED));
        let body_plain = render("| h |\n|---|\n| plain |", 40, 0);
        assert!(
            !span_of(&body_plain, "plain")
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn an_escaped_pipe_is_a_literal_pipe_inside_one_cell() {
        let rendered = strings(&render("| a | b |\n|---|---|\n| x\\|y | z |", 30, 0));
        let row = &rendered[3];
        assert!(row.contains("x|y"), "{rendered:?}");
        assert!(!row.contains('\\'), "{rendered:?}");
        assert_eq!(row.matches('│').count(), 3, "two columns: {rendered:?}");
    }

    #[test]
    fn rows_are_normalised_to_the_header_column_count() {
        let text = "| a | b | c |\n|---|---|---|\n| 1 |\n| 1 | 2 | 3 | 4 |";
        let rendered = strings(&render(text, 40, 0));
        assert!(
            !rendered.join("").contains('4'),
            "extra cell kept: {rendered:?}"
        );
        for line in rendered.iter().filter(|l| l.starts_with('│')) {
            assert_eq!(line.matches('│').count(), 4, "{rendered:?}");
        }
        assert_eq!(rendered.iter().filter(|l| l.starts_with('│')).count(), 3);
    }

    #[test]
    fn cjk_cells_keep_every_line_the_same_display_width() {
        let rendered = strings(&render(
            "| 名称 | 说明 |\n|---|---|\n| 苹果 | 水果,红色 |",
            30,
            0,
        ));
        assert!(rendered.len() >= 5, "{rendered:?}");
        assert!(
            rendered.iter().all(|l| display_width(l) == 30),
            "{rendered:?}"
        );
    }

    #[test]
    fn an_over_wide_table_wraps_cells_and_still_fits() {
        let text = "| name | description |\n|---|---|\n| alpha | this is a very long description that must wrap |";
        let rendered = strings(&render(text, 30, 0));
        assert!(rendered.len() > 5, "cells should wrap: {rendered:?}");
        assert!(
            rendered.iter().all(|l| display_width(l) == 30),
            "{rendered:?}"
        );
    }

    #[test]
    fn a_table_too_narrow_for_a_grid_degrades_to_dotted_rows() {
        let text = "a | b | c | d\n|-|-|-|-|\n1 | 2 | 3 | 4";
        let rendered = strings(&render(text, 12, 0));
        let all = rendered.join("\n");
        assert!(!all.contains('┌'), "{rendered:?}");
        assert!(all.contains(" · "), "{rendered:?}");
        assert!(
            rendered.iter().all(|l| display_width(l) <= 12),
            "{rendered:?}"
        );
    }

    #[test]
    fn wide_glyphs_in_one_cell_columns_degrade_instead_of_overflowing() {
        // Three minimum-width columns leave 1 content cell each; a 2-cell glyph
        // cannot fit, so the grid is replaced by the dotted fallback.
        let three = strings(&render(
            "| 中 | 文 | 字 |\n|-|-|-|\n| 中 | 文 | 字 |",
            13,
            0,
        ));
        assert!(!three.join("").contains('┌'), "{three:?}");
        assert!(three.join("\n").contains(" · "), "{three:?}");
        assert!(three.iter().all(|l| display_width(l) <= 13), "{three:?}");

        let two = strings(&render("| 中 | 文 |\n|-|-|\n| 中 | 文 |", 9, 0));
        assert!(!two.join("").contains('┌'), "{two:?}");
        assert!(two.join("\n").contains(" · "), "{two:?}");
        assert!(two.iter().all(|l| display_width(l) <= 9), "{two:?}");
    }

    #[test]
    fn a_wide_glyph_in_a_column_with_room_keeps_the_grid() {
        // Only the first column holds wide glyphs and it is >= 2 content cells.
        let rendered = strings(&render("| 中 | b |\n|-|-|\n| 中 | c |", 12, 0));
        assert!(rendered[0].starts_with('┌'), "{rendered:?}");
        assert!(
            rendered.iter().all(|l| display_width(l) == 12),
            "{rendered:?}"
        );
    }

    #[test]
    fn the_smallest_grid_width_still_draws_and_one_less_falls_back() {
        let text = "| a | b |\n|---|---|\n| 1 | 2 |";
        let grid = strings(&render(text, 9, 0));
        assert!(grid[0].starts_with('┌'), "{grid:?}");
        assert!(grid.iter().all(|l| display_width(l) == 9), "{grid:?}");
        let fallback = strings(&render(text, 8, 0));
        assert!(!fallback.join("").contains('┌'), "{fallback:?}");
        assert!(
            fallback.iter().all(|l| display_width(l) <= 8),
            "{fallback:?}"
        );
    }

    #[test]
    fn tiny_widths_do_not_panic() {
        for width in [0, 1] {
            for text in [SMALL, "| 中 | 文 |\n|-|-|\n| 中 | 文 |"] {
                let rendered = strings(&render(text, width, 0));
                assert!(!rendered.is_empty());
            }
        }
    }

    #[test]
    fn a_separator_without_a_pipe_is_not_a_table() {
        let rendered = strings(&render("foo |\n---", 20, 0));
        assert!(!rendered.join("").contains('┌'), "{rendered:?}");
        let escaped = strings(&render("a\\|b\n---", 20, 0));
        assert!(!escaped.join("").contains('┌'), "{escaped:?}");
    }

    #[test]
    fn a_one_column_table_with_pipes_is_still_a_grid() {
        let rendered = strings(&render("| a |\n|---|\n| 1 |", 20, 0));
        assert!(rendered[0].starts_with('┌'), "{rendered:?}");
        assert_eq!(rendered.len(), 5, "{rendered:?}");
        assert!(
            rendered.iter().all(|l| display_width(l) == 20),
            "{rendered:?}"
        );
    }

    #[test]
    fn a_lone_separator_line_is_not_a_table() {
        assert_eq!(strings(&render("|---|---|", 20, 3)), ["   |---|---|"]);
        assert_eq!(strings(&render("---", 20, 3)), ["   ────"]);
    }
}
