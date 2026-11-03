//! Minimal markdown rendering for assistant transcript text, shaped like
//! upstream's `<markdown conceal>` element: markers are hidden, blocks are
//! separated by one blank row, headings are bold without their `#` (upstream
//! bolds them and does not underline: theme `markdownHeading` is the body
//! colour, and a capture of the same session shows `b` only), list items are
//! `- ` with the dash in the link colour, and fenced code is highlighted by
//! [`super::highlight`] (no fence line, no gutter).
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

/// The colour family a markdown body is drawn in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Assistant text: base text, success-coloured code, highlighted fences.
    Normal,
    /// Reasoning text: every element keeps its modifiers (bold, underline)
    /// but takes the muted foreground, like upstream `generateThinkingSyntax`
    /// (`routes/session/thinking-syntax.ts`).
    Muted,
}

#[must_use]
pub fn render(text: &str, width: u16, indent: usize) -> Vec<Line<'static>> {
    render_toned(text, width, indent, Tone::Normal)
}

/// [`render`] in the given [`Tone`].
#[must_use]
pub fn render_toned(text: &str, width: u16, indent: usize, tone: Tone) -> Vec<Line<'static>> {
    let indent = " ".repeat(indent);
    let palette = theme::palette();
    let muted = match tone {
        Tone::Normal => None,
        Tone::Muted => Some(palette.dim),
    };
    let base = muted.unwrap_or(palette.text);
    let code = muted.unwrap_or(palette.success);
    let link = muted.unwrap_or(palette.link);
    // Upstream bolds a heading and leaves the underline off (verified against
    // `upstream-ses_19f7ed…-30x100`: `b` on the heading, no `u` anywhere).
    let heading = base.add_modifier(Modifier::BOLD);

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
                Some(open) => emit_code(&mut lines, &indent, open, code_width, muted),
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
                &[Span::raw(indent.clone()), Span::styled("- ", link)],
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
        emit_code(&mut lines, &indent, open, code_width, muted);
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
/// the viewport: code is never re-wrapped. A `muted` style replaces every
/// token's foreground and keeps its modifiers.
fn emit_code(
    lines: &mut Vec<Line<'static>>,
    indent: &str,
    fence: Fence<'_>,
    width: usize,
    muted: Option<Style>,
) {
    let code = fence.code.join("\n");
    for mut spans in highlight(fence.lang, &code, theme::palette()) {
        if let Some(muted) = muted {
            for span in &mut spans {
                span.style.fg = muted.fg;
            }
        }
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
