//! Unified and split diff rows: upstream `PatchDiff`
//! (`packages/tui/src/component/patch-diff.tsx`) over opentui's
//! `DiffRenderable` (`packages/core/src/renderables/Diff.ts`) and its
//! `LineNumberRenderable` gutter. Pure: a patch, a width and a palette in,
//! rows out.
//!
//! Sources:
//! - Parse: jsdiff 9 `parsePatch` (`diff/libesm/patch/parse.js`), first file
//!   only, as `Diff.ts` `parseDiff` keeps `patches[0]`; the parsed text
//!   borrows the patch string.
//! - Gutter: `patch-diff.tsx:46-55` (`minDigits` from every hunk's end lines,
//!   `paddingLeft 1`), `LineNumberRenderable.ts:199-213, 332-365` (number
//!   right-aligned, drawn on a line's first visual row only), `Diff.ts:560-570`
//!   (`paddingRight 1`, a two-cell sign after the number).
//! - Unified rows `Diff.ts:572-674`, split rows and wrap alignment
//!   `Diff.ts:676-1000`, hunk separator rows `patch-diff.tsx:76-86`, error
//!   view `Diff.ts:381-436`, colours `routes/session/index.tsx:3462-3480`.
//! - [`files`]: `parseApplyPatchFiles` (`routes/session/index.tsx:3760-3785`).
//!
//! Deviations:
//! 1. syntect (`ui/highlight.rs`) instead of tree-sitter; colours differ per
//!    scope.
//! 2. No virtualization (upstream uses it only with `scroll`, which the edit
//!    and patch rows never pass).
//! 3. No `diffs.view` / `diffs.wrap` config: always the [`view_for`] width
//!    rule and word wrap.
//! 4. Wrapping is by Unicode display width (CJK-safe), greedy at whitespace,
//!    instead of opentui's cell wrap. Tabs expand to four spaces.
//! 5. No Diagnostics block (ox has no LSP).
//! 6. [`files`] does not require `filePath`: `file` stands in for both paths.
//!    `additions`/`deletions` must be non-negative integers.
//! 7. The ApplyPatch `applied` fallback is not ported (callers keep the
//!    `patchText` fallback).
//! 8. Rows are exact per view instead of upstream's
//!    `minHeight = max(adds, dels)`.
//! 9. Parse errors render the upstream banner in `palette.warn` (the ox
//!    `error` colour) instead of `#ef4444`; the raw diff after it is not
//!    wrapped. A malformed `@@` line reports its own message where jsdiff
//!    would report a count mismatch.
//! 10. No word-level highlight (upstream has none either). `diff.highlight.*`
//!     colours only the `+`/`-` signs, as upstream Edit passes them.

use std::ops::Range;

use ox_schema::support::Record;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme::Palette;

use super::highlight;

/// Above this width Edit and ApplyPatch switch to the split view
/// (`index.tsx:3450`).
const SPLIT_ABOVE: u16 = 120;
/// `Diff.ts` `SIGN_WIDTH`: the `" +"` / `" -"` cell after the number.
const SIGN_WIDTH: usize = 2;
const TAB: &str = "    ";
/// jsdiff ends a file's hunks at this separator line.
const SEPARATOR: &str = "===================================================================";

/// The first file of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch<'a> {
    pub hunks: Vec<Hunk<'a>>,
}

/// One `@@` hunk. The counts are the header's (a missing count is 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk<'a> {
    /// The whole `@@ -a,b +c,d @@ ...` line.
    pub header: &'a str,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Added,
    Removed,
}

/// One body line without its sign; `old`/`new` are its line numbers on each
/// side it exists on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffLine<'a> {
    pub kind: Kind,
    pub text: &'a str,
    pub old: Option<u32>,
    pub new: Option<u32>,
}

/// Why a patch did not parse, worded like jsdiff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Unified,
    Split,
}

/// `parseApplyPatchFiles` `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Add,
    Delete,
    Update,
    Move,
}

/// One entry of a tool's `metadata.files`, borrowed from the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry<'a> {
    pub kind: FileType,
    pub relative_path: &'a str,
    pub file_path: &'a str,
    pub patch: &'a str,
    pub additions: u64,
    pub deletions: u64,
    pub move_path: Option<&'a str>,
}

/// Parse the first file of `patch`, borrowing every line from it.
pub fn parse(patch: &str) -> Result<Patch<'_>, ParseError> {
    let lines: Vec<&str> = patch
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut at = 0;
    while at < lines.len() && !is_file_header(lines[at]) && !is_hunk_header(lines[at]) {
        at += 1;
    }
    let mut old_name = None;
    let mut new_name = None;
    for _ in 0..2 {
        let Some(&line) = lines.get(at) else { break };
        if !is_file_header(line) {
            break;
        }
        let name = line[3..].trim().split('\t').next().unwrap_or("");
        if line.starts_with("---") {
            old_name = Some(name);
        } else {
            new_name = Some(name);
        }
        at += 1;
    }
    match (old_name, new_name) {
        (Some(name), None) => {
            return Err(error(format!("Missing \"+++ ...\" file header for {name}")));
        }
        (None, Some(name)) => {
            return Err(error(format!("Missing \"--- ...\" file header for {name}")));
        }
        _ => {}
    }
    let mut hunks = Vec::new();
    while let Some(&line) = lines.get(at) {
        if is_diff_header(line) || is_file_header(line) || line.starts_with(SEPARATOR) {
            break;
        }
        if is_hunk_header(line) {
            hunks.push(parse_hunk(&lines, &mut at)?);
        } else {
            at += 1;
        }
    }
    Ok(Patch { hunks })
}

/// The split view above 120 columns, unified otherwise.
#[must_use]
pub fn view_for(width: u16) -> View {
    if width > SPLIT_ABOVE {
        View::Split
    } else {
        View::Unified
    }
}

/// Upstream `minDigits`: the digits of the largest hunk end line on either
/// side (`start + count - 1`), so every hunk shares one gutter width.
#[must_use]
pub fn gutter_digits(patch: &Patch<'_>) -> usize {
    let ends = patch.hunks.iter().flat_map(|hunk| {
        [
            i64::from(hunk.old_start) + i64::from(hunk.old_count) - 1,
            i64::from(hunk.new_start) + i64::from(hunk.new_count) - 1,
        ]
    });
    let last = ends.max().unwrap_or(1).max(1);
    digits(u64::try_from(last).unwrap_or(1))
}

/// Every row of `patch` at `width` columns: hunks in order, a header row
/// before each hunk after the first.
#[must_use]
pub fn render(
    patch: &Patch<'_>,
    lang: Option<&str>,
    view: View,
    width: u16,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let numbers = patch.hunks.iter().flat_map(|hunk| hunk.lines.iter());
    let largest = numbers
        .filter_map(|line| line.old.max(line.new))
        .max()
        .unwrap_or(1);
    let digits = gutter_digits(patch).max(digits(u64::from(largest)));
    let mut out = Vec::new();
    for (index, hunk) in patch.hunks.iter().enumerate() {
        if index > 0 {
            out.push(hunk_row(hunk.header, width, palette));
        }
        let pane = Pane {
            lang,
            digits,
            palette,
        };
        match view {
            View::Unified => {
                let lines: Vec<PaneLine<'_>> = hunk.lines.iter().map(unified_line).collect();
                let rows = pane.rows(&lines, width);
                out.extend(rows.into_iter().flatten().map(Line::from));
            }
            View::Split => split_rows(&pane, hunk, width, &mut out),
        }
    }
    out
}

/// Upstream's error view: `Error parsing diff: <message>`, a blank row, then
/// the raw text coloured as a diff.
#[must_use]
pub fn render_error(err: &ParseError, raw: &str, palette: &Palette) -> Vec<Line<'static>> {
    let banner = format!("Error parsing diff: {}", err.message);
    let mut out = vec![
        Line::from(Span::styled(banner, palette.warn)),
        Line::default(),
    ];
    let body = highlight::highlight("diff", raw, palette);
    out.extend(body.into_iter().map(Line::from));
    out
}

/// `parseApplyPatchFiles` over `metadata.files`: entries missing a type
/// (from `type`, else ox's `status`), a path, the patch or the counts are
/// dropped.
#[must_use]
pub fn files(meta: &Record) -> Vec<FileEntry<'_>> {
    let Some(Value::Array(items)) = meta.get("files") else {
        return Vec::new();
    };
    items.iter().filter_map(file_entry).collect()
}

fn file_entry(item: &Value) -> Option<FileEntry<'_>> {
    let entry = item.as_object()?;
    let text = |key: &str| entry.get(key).and_then(Value::as_str);
    let present = |key: &str| text(key).filter(|value| !value.is_empty());
    let kind = match text("type") {
        Some("") => return None,
        Some("add") => FileType::Add,
        Some("delete") => FileType::Delete,
        Some("move") => FileType::Move,
        Some(_) => FileType::Update,
        None => match text("status")? {
            "added" => FileType::Add,
            "deleted" => FileType::Delete,
            "modified" => FileType::Update,
            _ => return None,
        },
    };
    let relative_path = present("file").or_else(|| present("relativePath"))?;
    Some(FileEntry {
        kind,
        relative_path,
        file_path: present("filePath").unwrap_or(relative_path),
        patch: text("patch")?,
        additions: entry.get("additions")?.as_u64()?,
        deletions: entry.get("deletions")?.as_u64()?,
        move_path: text("movePath"),
    })
}

fn error(message: String) -> ParseError {
    ParseError { message }
}

/// `line` starts with `prefix` and then whitespace (`/^prefix\s/`).
fn prefixed(line: &str, prefix: &str) -> bool {
    line.strip_prefix(prefix)
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_whitespace)
}

fn is_file_header(line: &str) -> bool {
    prefixed(line, "---") || prefixed(line, "+++")
}

fn is_hunk_header(line: &str) -> bool {
    prefixed(line, "@@")
}

fn is_diff_header(line: &str) -> bool {
    line.starts_with("diff --git ") || prefixed(line, "Index:") || line.starts_with("diff -r ")
}

/// jsdiff `parseHunk`: the body runs until both counts are met, plus any
/// `\ No newline` markers, which are dropped.
fn parse_hunk<'a>(lines: &[&'a str], at: &mut usize) -> Result<Hunk<'a>, ParseError> {
    let start = *at;
    let header = lines[start];
    let Some(range) = hunk_range(header) else {
        return Err(error(format!(
            "Hunk at line {} has a malformed header {header}",
            start + 1
        )));
    };
    *at += 1;
    let last = lines.len() - 1;
    let (mut old_lines, mut new_lines) = (range.old_count, range.new_count);
    // A zero count names the line before the hunk.
    let mut old = range.old_start.saturating_add(u32::from(old_lines == 0));
    let mut new = range.new_start.saturating_add(u32::from(new_lines == 0));
    let (mut removes, mut adds) = (0, 0);
    let mut body = Vec::new();
    while let Some(&line) = lines.get(*at) {
        if removes >= old_lines && adds >= new_lines && !line.starts_with('\\') {
            break;
        }
        let op = if line.is_empty() && *at != last {
            Some(' ')
        } else {
            line.chars().next()
        };
        let text = line.get(1..).unwrap_or("");
        match op {
            Some(' ') => {
                body.push(DiffLine {
                    kind: Kind::Context,
                    text,
                    old: Some(old),
                    new: Some(new),
                });
                old = old.saturating_add(1);
                new = new.saturating_add(1);
                removes += 1;
                adds += 1;
            }
            Some('-') => {
                body.push(DiffLine {
                    kind: Kind::Removed,
                    text,
                    old: Some(old),
                    new: None,
                });
                old = old.saturating_add(1);
                removes += 1;
            }
            Some('+') => {
                body.push(DiffLine {
                    kind: Kind::Added,
                    text,
                    old: None,
                    new: Some(new),
                });
                new = new.saturating_add(1);
                adds += 1;
            }
            Some('\\') => {}
            _ => {
                return Err(error(format!(
                    "Hunk at line {} contained invalid line {line}",
                    start + 1
                )));
            }
        }
        *at += 1;
    }
    if adds == 0 && new_lines == 1 {
        new_lines = 0;
    }
    if removes == 0 && old_lines == 1 {
        old_lines = 0;
    }
    if adds != new_lines {
        return Err(error(format!(
            "Added line count did not match for hunk at line {}",
            start + 1
        )));
    }
    if removes != old_lines {
        return Err(error(format!(
            "Removed line count did not match for hunk at line {}",
            start + 1
        )));
    }
    if let Some(&next) = lines.get(*at)
        && next.starts_with(['+', ' ', '-'])
        && !is_file_header(next)
    {
        return Err(error(format!(
            "Hunk at line {} has more lines than expected (expected {old_lines} old lines and {new_lines} new lines)",
            start + 1
        )));
    }
    Ok(Hunk {
        header,
        old_start: range.old_start,
        old_count: range.old_count,
        new_start: range.new_start,
        new_count: range.new_count,
        lines: body,
    })
}

struct HunkRange {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
}

/// `@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@`.
fn hunk_range(header: &str) -> Option<HunkRange> {
    let rest = header.strip_prefix("@@ -")?;
    let (old_start, old_count, rest) = side(rest)?;
    let rest = rest.strip_prefix(" +")?;
    let (new_start, new_count, rest) = side(rest)?;
    rest.starts_with(" @@").then_some(HunkRange {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// `start[,count]` and what follows it.
fn side(text: &str) -> Option<(u32, u32, &str)> {
    let (start, rest) = number(text)?;
    match rest.strip_prefix(',') {
        Some(rest) => {
            let (count, rest) = number(rest)?;
            Some((start, count, rest))
        }
        None => Some((start, 1, rest)),
    }
}

fn number(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let value = text[..end].parse().ok()?;
    Some((value, &text[end..]))
}

fn digits(n: u64) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// How a pane row is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Context,
    Added,
    Removed,
    /// A split-view pad: no number, context colours.
    Empty,
}

#[derive(Debug, Clone, Copy)]
struct PaneLine<'a> {
    tone: Tone,
    number: Option<u32>,
    text: &'a str,
}

const EMPTY: PaneLine<'static> = PaneLine {
    tone: Tone::Empty,
    number: None,
    text: "",
};

fn unified_line<'a>(line: &DiffLine<'a>) -> PaneLine<'a> {
    match line.kind {
        Kind::Context => PaneLine {
            tone: Tone::Context,
            number: line.new,
            text: line.text,
        },
        Kind::Added => PaneLine {
            tone: Tone::Added,
            number: line.new,
            text: line.text,
        },
        Kind::Removed => PaneLine {
            tone: Tone::Removed,
            number: line.old,
            text: line.text,
        },
    }
}

/// One visual row as spans, exactly the pane width wide.
type Row = Vec<Span<'static>>;

/// A gutter plus code column, one `CodeRenderable` with its line numbers.
struct Pane<'p> {
    lang: Option<&'p str>,
    digits: usize,
    palette: &'p Palette,
}

impl Pane<'_> {
    /// The visual rows of every logical line, highlighted as one block so
    /// the lexer state runs across lines.
    fn rows(&self, lines: &[PaneLine<'_>], width: usize) -> Vec<Vec<Row>> {
        let highlighted = self.highlight(lines);
        let gutter = self.digits + 2 + SIGN_WIDTH;
        let content = width.saturating_sub(gutter).max(1);
        lines
            .iter()
            .zip(highlighted)
            .map(|(line, spans)| self.line_rows(line, spans, gutter, content))
            .collect()
    }

    fn highlight(&self, lines: &[PaneLine<'_>]) -> Vec<Vec<Span<'static>>> {
        let mut joined = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                joined.push('\n');
            }
            let mut parts = line.text.split('\t');
            joined.push_str(parts.next().unwrap_or(""));
            for part in parts {
                joined.push_str(TAB);
                joined.push_str(part);
            }
        }
        let mut out = match self.lang {
            Some(lang) => highlight::highlight(lang, &joined, self.palette),
            None => joined
                .split('\n')
                .map(|line| plain(line, self.palette.text))
                .collect(),
        };
        // `highlight` drops a trailing empty line.
        out.resize_with(lines.len(), Vec::new);
        out
    }

    fn line_rows(
        &self,
        line: &PaneLine<'_>,
        spans: Vec<Span<'static>>,
        gutter: usize,
        content: usize,
    ) -> Vec<Row> {
        let diff = &self.palette.diff;
        let (gutter_bg, content_bg, sign) = match line.tone {
            Tone::Added => (
                diff.line_number_added_bg,
                diff.added_bg,
                Some((" +", diff.sign_added)),
            ),
            Tone::Removed => (
                diff.line_number_removed_bg,
                diff.removed_bg,
                Some((" -", diff.sign_removed)),
            ),
            Tone::Context | Tone::Empty => (diff.context_bg, diff.context_bg, None),
        };
        let mut text = String::new();
        let mut runs: Vec<(Range<usize>, Style)> = Vec::with_capacity(spans.len());
        for span in &spans {
            let start = text.len();
            text.push_str(&span.content);
            runs.push((
                start..text.len(),
                Style {
                    bg: content_bg.bg,
                    ..span.style
                },
            ));
        }
        let ranges = wrap(&text, content);
        let mut rows = Vec::with_capacity(ranges.len());
        for (index, range) in ranges.into_iter().enumerate() {
            let mut row = Vec::with_capacity(runs.len() + 5);
            if index == 0 {
                let number = line.number.map(|n| n.to_string()).unwrap_or_default();
                let numbered = gutter_bg.patch(diff.line_number);
                row.push(Span::styled(" ", gutter_bg));
                row.push(Span::styled(
                    format!("{number:>width$}", width = self.digits),
                    numbered,
                ));
                // `LineNumberRenderable.ts:368`: the sign sits right after the
                // number, then `paddingRight`.
                match sign {
                    Some((glyph, style)) => row.push(Span::styled(glyph, gutter_bg.patch(style))),
                    None => row.push(Span::styled(" ".repeat(SIGN_WIDTH), gutter_bg)),
                }
                row.push(Span::styled(" ", gutter_bg));
            } else {
                row.push(Span::styled(" ".repeat(gutter), gutter_bg));
            }
            let range = fit(&text, range, content);
            let mut used = 0;
            for (run, style) in &runs {
                let from = run.start.max(range.start);
                let to = run.end.min(range.end);
                if from < to {
                    let piece = &text[from..to];
                    used += piece.width();
                    row.push(Span::styled(piece.to_owned(), *style));
                }
            }
            if used < content {
                row.push(Span::styled(" ".repeat(content - used), content_bg));
            }
            rows.push(row);
        }
        rows
    }

    /// A blank row in the pad colours, for wrap alignment in the split view.
    fn blank(&self, width: usize) -> Row {
        vec![Span::styled(
            " ".repeat(width),
            self.palette.diff.context_bg,
        )]
    }
}

fn plain(line: &str, style: Style) -> Vec<Span<'static>> {
    if line.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(line.to_owned(), style)]
    }
}

/// One hunk's split rows: each `-`/`+` run paired line by line, then every
/// pair padded to the taller side's wrapped height.
fn split_rows(pane: &Pane<'_>, hunk: &Hunk<'_>, width: usize, out: &mut Vec<Line<'static>>) {
    let mut left = Vec::with_capacity(hunk.lines.len());
    let mut right = Vec::with_capacity(hunk.lines.len());
    let mut at = 0;
    while let Some(line) = hunk.lines.get(at) {
        if line.kind == Kind::Context {
            left.push(PaneLine {
                tone: Tone::Context,
                number: line.old,
                text: line.text,
            });
            right.push(PaneLine {
                tone: Tone::Context,
                number: line.new,
                text: line.text,
            });
            at += 1;
            continue;
        }
        let run_end = hunk.lines[at..]
            .iter()
            .position(|line| line.kind == Kind::Context)
            .map_or(hunk.lines.len(), |offset| at + offset);
        let run = &hunk.lines[at..run_end];
        let removes: Vec<PaneLine<'_>> = run
            .iter()
            .filter(|line| line.kind == Kind::Removed)
            .map(unified_line)
            .collect();
        let adds: Vec<PaneLine<'_>> = run
            .iter()
            .filter(|line| line.kind == Kind::Added)
            .map(unified_line)
            .collect();
        for index in 0..removes.len().max(adds.len()) {
            left.push(removes.get(index).copied().unwrap_or(EMPTY));
            right.push(adds.get(index).copied().unwrap_or(EMPTY));
        }
        at = run_end;
    }
    let left_width = width / 2;
    let right_width = width - left_width;
    let left_rows = pane.rows(&left, left_width);
    let right_rows = pane.rows(&right, right_width);
    for (mut l, mut r) in left_rows.into_iter().zip(right_rows) {
        let height = l.len().max(r.len());
        l.resize_with(height, || pane.blank(left_width));
        r.resize_with(height, || pane.blank(right_width));
        for (mut row, tail) in l.into_iter().zip(r) {
            row.extend(tail);
            out.push(Line::from(row));
        }
    }
}

/// `" " + header` in the hunk colour on the context band, full width.
fn hunk_row(header: &str, width: usize, palette: &Palette) -> Line<'static> {
    let band = palette.diff.context_bg;
    let mut text = String::with_capacity(header.len() + 1);
    text.push(' ');
    let mut used = 1;
    for grapheme in header.graphemes(true) {
        let w = grapheme.width();
        if used + w > width {
            break;
        }
        used += w;
        text.push_str(grapheme);
    }
    let mut spans = vec![Span::styled(text, band.patch(palette.diff.hunk))];
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), band));
    }
    Line::from(spans)
}

/// Greedy word wrap by display width: breaks after whitespace, and inside a
/// word only when the word alone is wider than `width`. Byte ranges of
/// `text`, at least one.
#[must_use]
pub(crate) fn wrap(text: &str, width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (at, word) in words(text) {
        let body = word.trim_end();
        let body_width = body.width();
        let space = word.width() - body_width;
        if used + body_width <= width {
            used += body_width + space;
            continue;
        }
        if used > 0 {
            rows.push(start..at);
            start = at;
        }
        if body_width <= width {
            used = body_width + space;
            continue;
        }
        let mut chunk = 0;
        for (offset, grapheme) in body.grapheme_indices(true) {
            let w = grapheme.width();
            if chunk > 0 && chunk + w > width {
                rows.push(start..at + offset);
                start = at + offset;
                chunk = 0;
            }
            chunk += w;
        }
        used = chunk + space;
    }
    rows.push(start..text.len());
    rows
}

/// Words with their trailing whitespace, and their byte offsets.
fn words(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    let mut previous_space = false;
    let mut chars = text.char_indices();
    std::iter::from_fn(move || {
        if start >= text.len() {
            return None;
        }
        for (at, c) in chars.by_ref() {
            let space = c.is_whitespace();
            if previous_space && !space && at > start {
                let word = (start, &text[start..at]);
                start = at;
                previous_space = false;
                return Some(word);
            }
            previous_space = space;
        }
        let word = (start, &text[start..]);
        start = text.len();
        Some(word)
    })
}

/// Drop trailing whitespace that would overflow the row.
#[must_use]
pub(crate) fn fit(text: &str, range: Range<usize>, width: usize) -> Range<usize> {
    let mut end = range.end;
    while text[range.start..end].width() > width {
        match text[range.start..end].chars().next_back() {
            Some(c) if c.is_whitespace() => end -= c.len_utf8(),
            _ => break,
        }
    }
    range.start..end
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;
    use ratatui::text::Line;
    use serde_json::json;
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::theme::{Deck, Palette};

    fn truecolor() -> Palette {
        Palette::for_deck(Deck::Truecolor)
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn cols(line: &Line<'_>) -> usize {
        line.spans.iter().map(|span| span.content.width()).sum()
    }

    /// The span covering display column `col`.
    fn style_at(line: &Line<'_>, col: usize) -> Style {
        let mut at = 0;
        for span in &line.spans {
            let w = span.content.width();
            if col < at + w {
                return span.style;
            }
            at += w;
        }
        panic!("column {col} past the end of {:?}", text(line));
    }

    fn parsed(patch: &str) -> Patch<'_> {
        parse(patch).unwrap_or_else(|err| panic!("parse failed: {}", err.message))
    }

    fn numbers<'a>(patch: &Patch<'a>) -> Vec<(Kind, &'a str, Option<u32>, Option<u32>)> {
        let lines = patch.hunks.iter().flat_map(|hunk| hunk.lines.iter());
        lines.map(|l| (l.kind, l.text, l.old, l.new)).collect()
    }

    const EDIT: &str = "--- a.rs\n+++ a.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";

    #[test]
    fn d1_edit_patch_numbers_both_sides() {
        let patch = parsed(EDIT);
        assert_eq!(patch.hunks.len(), 1, "one hunk");
        let hunk = &patch.hunks[0];
        assert_eq!(hunk.header, "@@ -1,3 +1,3 @@");
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count
            ),
            (1, 3, 1, 3)
        );
        assert_eq!(
            numbers(&patch),
            vec![
                (Kind::Context, "a", Some(1), Some(1)),
                (Kind::Removed, "b", Some(2), None),
                (Kind::Added, "B", None, Some(2)),
                (Kind::Context, "c", Some(3), Some(3)),
            ]
        );
    }

    #[test]
    fn d2_missing_counts_mean_one() {
        let patch = parsed("@@ -5 +5 @@\n-x\n+y\n");
        let hunk = &patch.hunks[0];
        assert_eq!(
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count
            ),
            (5, 1, 5, 1)
        );
        assert_eq!(
            numbers(&patch),
            vec![
                (Kind::Removed, "x", Some(5), None),
                (Kind::Added, "y", None, Some(5)),
            ]
        );
    }

    #[test]
    fn d3_new_file_is_all_added_from_line_one() {
        let patch = parsed("--- /dev/null\n+++ a.rs\n@@ -0,0 +1,3 @@\n+1\n+2\n+3\n");
        assert_eq!(
            numbers(&patch),
            vec![
                (Kind::Added, "1", None, Some(1)),
                (Kind::Added, "2", None, Some(2)),
                (Kind::Added, "3", None, Some(3)),
            ]
        );
        assert_eq!(gutter_digits(&patch), 1);
    }

    #[test]
    fn d4_index_headers_are_skipped_and_no_newline_markers_dropped() {
        let raw = "Index: a\n===================================================================\n\
                   --- a\n+++ a\n@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+y\n\
                   \\ No newline at end of file\n";
        let patch = parsed(raw);
        assert_eq!(
            numbers(&patch),
            vec![
                (Kind::Removed, "x", Some(1), None),
                (Kind::Added, "y", None, Some(1)),
            ]
        );
    }

    #[test]
    fn d4_only_the_first_file_is_parsed() {
        let raw = "--- a\n+++ a\n@@ -1 +1 @@\n-x\n+y\n--- b\n+++ b\n@@ -1 +1 @@\n-p\n+q\n";
        assert_eq!(parsed(raw).hunks.len(), 1, "upstream renders patches[0]");
    }

    #[test]
    fn d5_malformed_hunk_header_is_an_error_with_the_upstream_banner() {
        let raw = "--- a\n+++ a\n@@ -x +1 @@\n+a\n";
        let Err(err) = parse(raw) else {
            panic!("a malformed @@ must not parse");
        };
        let p = truecolor();
        let lines = render_error(&err, raw, &p);
        let first = text(&lines[0]);
        assert!(
            first.starts_with("Error parsing diff: "),
            "banner first, got {first:?}"
        );
        assert_eq!(lines[0].spans[0].style.fg, p.warn.fg, "error colour");
        let body: Vec<String> = lines.iter().map(text).collect();
        assert!(
            body.iter().any(|l| l == "@@ -x +1 @@"),
            "raw diff follows: {body:?}"
        );
    }

    #[test]
    fn d5_hunk_body_shorter_than_its_counts_is_an_error() {
        assert!(
            parse("@@ -1,3 +1,3 @@\n a\n").is_err(),
            "jsdiff count check"
        );
    }

    fn two_hunks() -> String {
        let mut raw = String::from("--- a\n+++ a\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n");
        raw.push_str("@@ -100,21 +100,21 @@\n");
        for n in 0..20 {
            raw.push_str(&format!(" line {n}\n"));
        }
        raw.push_str("-old\n+new\n");
        raw
    }

    #[test]
    fn d6_gutter_width_is_shared_by_every_hunk() {
        let raw = two_hunks();
        let patch = parsed(&raw);
        assert_eq!(gutter_digits(&patch), 3, "end line 120 has 3 digits");
        let p = truecolor();
        let lines = render(&patch, None, View::Unified, 60, &p);
        let first = text(&lines[0]);
        assert_eq!(
            &first[..8],
            "   1   a",
            "hunk 1: pad, `  1`, gap, sign cell"
        );
        let second = text(&lines[5]);
        assert_eq!(&second[..13], " 100   line 0", "hunk 2 shares the gutter");
    }

    #[test]
    fn d7_split_only_above_120_columns() {
        assert_eq!(view_for(120), View::Unified);
        assert_eq!(view_for(121), View::Split);
    }

    const MIXED: &str = "@@ -1,3 +1,4 @@\n a\n-b\n-c\n+B\n+C\n+D\n";

    #[test]
    fn d8_unified_rows_signs_and_numbers() {
        let p = truecolor();
        let lines = render(&parsed(MIXED), None, View::Unified, 40, &p);
        let rows: Vec<String> = lines
            .iter()
            .map(|l| text(l).trim_end().to_owned())
            .collect();
        assert_eq!(
            rows,
            vec![" 1   a", " 2 - b", " 3 - c", " 2 + B", " 3 + C", " 4 + D"],
            "new number on + and context, old on -"
        );
        for line in &lines {
            assert_eq!(cols(line), 40, "row fills the width: {:?}", text(line));
        }
        let d = &p.diff;
        assert_eq!(style_at(&lines[1], 0).bg, d.line_number_removed_bg.bg);
        assert_eq!(style_at(&lines[1], 1).fg, d.line_number.fg, "number");
        assert_eq!(style_at(&lines[1], 3).fg, d.sign_removed.fg, "sign");
        assert_eq!(style_at(&lines[1], 5).bg, d.removed_bg.bg, "content");
        assert_eq!(style_at(&lines[1], 39).bg, d.removed_bg.bg, "fill");
        assert_eq!(style_at(&lines[3], 0).bg, d.line_number_added_bg.bg);
        assert_eq!(style_at(&lines[3], 3).fg, d.sign_added.fg);
        assert_eq!(style_at(&lines[3], 5).bg, d.added_bg.bg);
        assert_eq!(style_at(&lines[0], 0).bg, d.context_bg.bg);
        assert_eq!(style_at(&lines[0], 5).bg, d.context_bg.bg);
        assert_eq!(style_at(&lines[0], 5).fg, p.text.fg, "plain text fg");
    }

    #[test]
    fn d9_split_pairs_runs_and_pads_the_shorter_side() {
        let p = truecolor();
        let lines = render(&parsed(MIXED), None, View::Split, 40, &p);
        let rows: Vec<(String, String)> = lines
            .iter()
            .map(|l| {
                let t = text(l);
                (t[..20].trim_end().to_owned(), t[20..].trim_end().to_owned())
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (" 1   a".to_owned(), " 1   a".to_owned()),
                (" 2 - b".to_owned(), " 2 + B".to_owned()),
                (" 3 - c".to_owned(), " 3 + C".to_owned()),
                (String::new(), " 4 + D".to_owned()),
            ]
        );
        assert_eq!(
            style_at(&lines[3], 0).bg,
            p.diff.context_bg.bg,
            "pad gutter"
        );
        assert_eq!(
            style_at(&lines[3], 10).bg,
            p.diff.context_bg.bg,
            "pad content"
        );
        for line in &lines {
            assert_eq!(cols(line), 40, "both panes fill: {:?}", text(line));
        }
    }

    #[test]
    fn d10_split_wrapped_side_pads_the_other_to_equal_height() {
        let raw = "@@ -1,2 +1,2 @@\n-one two three four five six seven eight\n+short\n same\n";
        let p = truecolor();
        let lines = render(&parsed(raw), None, View::Split, 40, &p);
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert!(lines.len() >= 4, "left line wraps: {rows:?}");
        let last = lines.len() - 1;
        assert_eq!(rows[last][..20].trim_end(), " 2   same");
        assert_eq!(
            rows[last][20..].trim_end(),
            " 2   same",
            "context realigned"
        );
        for row in &rows[1..last] {
            assert!(row[20..].trim().is_empty(), "right pads: {row:?}");
        }
        assert!(rows[0][20..].starts_with(" 1 + short"));
        for line in &lines {
            assert_eq!(cols(line), 40, "{:?}", text(line));
        }
    }

    #[test]
    fn d11_wrapped_unified_line_numbers_only_the_first_row() {
        let raw = "@@ -0,0 +1 @@\n+alpha beta gamma delta epsilon\n";
        let p = truecolor();
        let lines = render(&parsed(raw), None, View::Unified, 20, &p);
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert!(rows.len() >= 2, "wraps at 15 content columns: {rows:?}");
        assert!(rows[0].starts_with(" 1 + alpha"), "{rows:?}");
        assert!(rows[1].starts_with("     "), "blank gutter: {rows:?}");
        assert_ne!(&rows[1][5..6], " ", "words resume at the content column");
        assert_eq!(style_at(&lines[1], 0).bg, p.diff.line_number_added_bg.bg);
        assert_eq!(style_at(&lines[1], 5).bg, p.diff.added_bg.bg);
        for line in &lines {
            assert_eq!(cols(line), 20, "{:?}", text(line));
        }
    }

    #[test]
    fn d12_hunk_header_rows_separate_later_hunks() {
        let raw = two_hunks();
        let p = truecolor();
        let lines = render(&parsed(&raw), None, View::Unified, 60, &p);
        let headers: Vec<usize> = (0..lines.len())
            .filter(|&i| text(&lines[i]).starts_with(" @@"))
            .collect();
        assert_eq!(headers, vec![4], "only between hunks");
        let header = &lines[4];
        assert_eq!(text(header).trim_end(), " @@ -100,21 +100,21 @@");
        assert_eq!(style_at(header, 1).fg, p.diff.hunk.fg);
        assert_eq!(style_at(header, 1).bg, p.diff.context_bg.bg);
        assert_eq!(cols(header), 60, "full width");
    }

    #[test]
    fn d13_wide_glyphs_wrap_by_display_width() {
        let raw = "@@ -0,0 +1 @@\n+汉字汉字汉字汉字汉字汉字汉字\n";
        let p = truecolor();
        let lines = render(&parsed(raw), None, View::Unified, 16, &p);
        assert!(lines.len() > 1, "28 columns of CJK wrap in 11");
        for line in &lines {
            assert!(cols(line) <= 16, "row too wide: {:?}", text(line));
        }
        let joined: String = lines
            .iter()
            .map(|l| text(l)[5..].trim_end().to_owned())
            .collect();
        assert_eq!(joined, "汉字汉字汉字汉字汉字汉字汉字", "nothing lost");
    }

    #[test]
    fn d14_files_derive_the_type_from_ox_status() {
        let meta: Record = serde_json::from_value(json!({
            "files": [
                {"file": "new.rs", "patch": "p1", "additions": 3, "deletions": 0, "status": "added"},
                {"file": "old.rs", "patch": "p2", "additions": 0, "deletions": 2, "status": "deleted"},
                {"file": "mod.rs", "patch": "p3", "additions": 1, "deletions": 1, "status": "modified"},
                {"file": "mv.rs", "patch": "p4", "additions": 1, "deletions": 1, "type": "move",
                 "movePath": "to.rs", "filePath": "/abs/mv.rs"},
                {"file": "bad.rs", "patch": "p5", "additions": 1, "status": "modified"},
                {"patch": "p6", "additions": 1, "deletions": 1, "status": "modified"},
                {"file": "odd.rs", "patch": "p7", "additions": 1, "deletions": 1, "status": "renamed"},
            ]
        }))
        .expect("record");
        let entry = |kind, path, patch, additions, deletions| FileEntry {
            kind,
            relative_path: path,
            file_path: path,
            patch,
            additions,
            deletions,
            move_path: None,
        };
        assert_eq!(
            files(&meta),
            vec![
                entry(FileType::Add, "new.rs", "p1", 3, 0),
                entry(FileType::Delete, "old.rs", "p2", 0, 2),
                entry(FileType::Update, "mod.rs", "p3", 1, 1),
                FileEntry {
                    file_path: "/abs/mv.rs",
                    move_path: Some("to.rs"),
                    ..entry(FileType::Move, "mv.rs", "p4", 1, 1)
                },
            ],
            "incomplete entries and unknown statuses are dropped"
        );
        let empty: Record = Record::new();
        assert!(files(&empty).is_empty(), "no files key");
    }

    #[test]
    fn d15_parsed_text_borrows_the_input() {
        let patch = parsed(EDIT);
        let range = EDIT.as_bytes().as_ptr_range();
        let lines = patch.hunks.iter().flat_map(|hunk| hunk.lines.iter());
        for line in lines {
            let ptr = line.text.as_ptr();
            assert!(range.contains(&ptr), "{:?} is a copy", line.text);
        }
        assert!(
            range.contains(&patch.hunks[0].header.as_ptr()),
            "header borrows"
        );
    }

    #[test]
    fn crlf_patches_strip_the_carriage_return() {
        let patch = parsed("@@ -1 +1 @@\r\n-x\r\n+y\r\n");
        assert_eq!(patch.hunks[0].lines[0].text, "x");
        assert_eq!(patch.hunks[0].header, "@@ -1 +1 @@");
    }

    #[test]
    fn content_takes_the_highlighter_fg_on_the_row_bg() {
        let p = truecolor();
        let raw = "@@ -0,0 +1 @@\n+fn main() {}\n";
        let lines = render(&parsed(raw), Some("rust"), View::Unified, 40, &p);
        let fn_style = style_at(&lines[0], 5);
        assert_eq!(
            fn_style.fg, p.syntax.function.fg,
            "`fn` keeps its syntax colour"
        );
        assert_eq!(fn_style.bg, p.diff.added_bg.bg, "on the added bg");
    }

    #[test]
    fn tabs_expand_so_rows_keep_their_width() {
        let p = truecolor();
        let lines = render(
            &parsed("@@ -0,0 +1 @@\n+\tx\n"),
            None,
            View::Unified,
            20,
            &p,
        );
        assert_eq!(cols(&lines[0]), 20);
        assert!(!text(&lines[0]).contains('\t'));
    }

    #[test]
    fn colourless_decks_keep_signs_without_backgrounds() {
        for deck in [Deck::Ansi16, Deck::NoColor] {
            let p = Palette::for_deck(deck);
            let lines = render(&parsed(MIXED), Some("rust"), View::Split, 40, &p);
            assert!(text(&lines[1]).contains("- b"), "{deck:?} keeps the - sign");
            assert!(text(&lines[1]).contains("+ B"), "{deck:?} keeps the + sign");
            let spans = lines.iter().flat_map(|l| l.spans.iter());
            for span in spans {
                assert_eq!(span.style.bg, None, "{deck:?}: no bg on {:?}", span.content);
            }
        }
    }

    #[test]
    fn empty_patch_renders_nothing() {
        let p = truecolor();
        assert!(render(&parsed(""), None, View::Unified, 40, &p).is_empty());
    }

    #[test]
    fn d16_gutter_is_pad_number_sign_pad() {
        // `LineNumberRenderable.ts:359,368`: number ends at
        // `width - after - paddingRight`, the `" -"` sign follows it, then
        // `paddingRight`; `paddingLeft 1` comes first.
        let p = truecolor();
        let lines = render(&parsed(MIXED), None, View::Unified, 20, &p);
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(rows[0], format!("{:<20}", " 1   a"), "context");
        assert_eq!(rows[1], format!("{:<20}", " 2 - b"), "removed");
        assert_eq!(rows[3], format!("{:<20}", " 2 + B"), "added");
    }

    #[test]
    fn d17_huge_hunk_starts_do_not_overflow() {
        let raw = "@@ -4294967295,0 +4294967295,0 @@\n";
        let patch = parsed(raw);
        assert_eq!(patch.hunks.len(), 1, "parsed");
        let raw = "@@ -4294967295,2 +4294967295,2 @@\n a\n b\n";
        let p = truecolor();
        let lines = render(&parsed(raw), None, View::Unified, 40, &p);
        assert_eq!(lines.len(), 2, "two context rows");
    }

    /// `metadata.files` from the real ox diff: generated by
    /// `ox_tools::diff::{file_diff, deleted_file}` (a throwaway ox-tools
    /// test writing `serde_json::json!({"files": [...]})`) for a two-hunk
    /// edit of `line 1..30`, a new file, a deletion, and a 1001-line
    /// rewrite past `MAX_EDIT_DISTANCE` (full-replacement fallback).
    /// Pasted rather than a dev-dependency: ox-tools pulls ox-core/ox-db.
    const OX_FILES: &str = include_str!("../../fixtures/ox-diff-files.json");

    #[test]
    fn d18_real_ox_diff_output_parses_and_numbers() {
        let meta: Record = serde_json::from_str(OX_FILES).expect("fixture record");
        let entries = files(&meta);
        assert_eq!(entries.len(), 4, "every ox file entry is accepted");
        let p = truecolor();

        let multi = parsed(entries[0].patch);
        assert_eq!(entries[0].kind, FileType::Update);
        assert_eq!(multi.hunks.len(), 2, "two hunks");
        let nums = numbers(&multi);
        assert_eq!(nums[0], (Kind::Context, "line 1", Some(1), Some(1)));
        assert_eq!(
            nums[nums.len() - 1],
            (Kind::Context, "line 28", Some(28), Some(29))
        );
        let rows: Vec<String> = render(&multi, None, View::Unified, 40, &p)
            .iter()
            .map(|l| text(l).trim_end().to_owned())
            .collect();
        assert!(rows.contains(&"  3 - line 3".to_owned()), "{rows:?}");
        assert!(rows.contains(&"  3 + line three".to_owned()), "{rows:?}");
        assert!(rows.contains(&" 26 + extra".to_owned()), "{rows:?}");
        assert!(rows.contains(&" @@ -22,7 +22,8 @@".to_owned()), "{rows:?}");

        let added = parsed(entries[1].patch);
        assert_eq!(entries[1].kind, FileType::Add);
        assert_eq!(
            numbers(&added),
            vec![
                (Kind::Added, "fn new() {}", None, Some(1)),
                (Kind::Added, "fn two() {}", None, Some(2)),
            ]
        );

        let deleted = parsed(entries[2].patch);
        assert_eq!(entries[2].kind, FileType::Delete);
        assert_eq!(
            numbers(&deleted),
            vec![
                (Kind::Removed, "old", Some(1), None),
                (Kind::Removed, "gone", Some(2), None),
            ]
        );

        let big = parsed(entries[3].patch);
        assert_eq!(big.hunks.len(), 1, "one full-replacement hunk");
        let nums = numbers(&big);
        assert_eq!(nums.len(), 2002);
        assert_eq!(nums[1000], (Kind::Removed, "a1000", Some(1001), None));
        assert_eq!(nums[1001], (Kind::Added, "b0", None, Some(1)));
        assert_eq!(nums[2001], (Kind::Added, "b1000", None, Some(1001)));
        let lines = render(&big, None, View::Unified, 40, &p);
        assert_eq!(lines.len(), 2002, "one row per line");
        assert_eq!(text(&lines[2001]).trim_end(), " 1001 + b1000");
    }
}
