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
