//! A write call drawn as the written file: upstream `Write`
//! (`routes/session/index.tsx:3143-3177`) inside the shared `BlockTool`
//! frame (`block_tool`). Pure: the call's input borrowed in, rows out.
//!
//! Layout: `# Wrote <path>`, the block's gap, then `input.content`
//! highlighted for the path's language behind opentui's `line_number`
//! gutter (`fg text.muted`, `minWidth 3`, `paddingRight 1`): the gutter is
//! `max(3, digits + 2)` wide, the number right-aligned one column short of
//! it and drawn on a line's first visual row only (as the diff gutter,
//! `LineNumberRenderable.ts:199-213, 332-365`).
//!
//! Deviations:
//! - Long lines wrap by display width at whitespace (the diff body's wrap,
//!   `diff::wrap`) instead of opentui's cell wrap; tabs expand to four
//!   spaces.
//! - A trailing newline adds no empty numbered row (`str::lines`).
//! - No Diagnostics block (ox has no LSP).
//! - Calls that are not finished keep the inline `← Write <path>` row
//!   (`tool_text`), as upstream's `InlineTool`.

use std::borrow::Cow;

use crate::theme::Palette;
use ox_schema::support::Record;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::block_tool::{self, BlockSpec};
use super::diff;
use super::highlight;
use super::tool_text;

/// `minWidth={3}`.
const MIN_GUTTER: usize = 3;
/// Tabs expand to this, as in the diff body.
const TAB: &str = "    ";

/// The block for a finished write: the title and the numbered file.
#[must_use]
pub fn render(input: &Record, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let text = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
    let path = text("path").unwrap_or_default();
    let content = text("content").unwrap_or_default();
    let title = format!(
        "# Wrote {}",
        tool_text::format_path(path, tool_text::paths())
    );
    let spec = BlockSpec {
        title: Some(&title),
        ..BlockSpec::default()
    };
    let body = numbered(content, path, block_tool::content_width(width), palette);
    block_tool::lines(&spec, body, width)
}

/// `content` highlighted and wrapped into `width` columns behind the gutter.
fn numbered(content: &str, path: &str, width: usize, palette: &Palette) -> Vec<Line<'static>> {
    let code: Cow<'_, str> = if content.contains('\t') {
        Cow::Owned(content.replace('\t', TAB))
    } else {
        Cow::Borrowed(content)
    };
    let sources: Vec<&str> = code.lines().collect();
    let digits = sources.len().max(1).to_string().len();
    let gutter = MIN_GUTTER.max(digits + 2);
    let room = width.saturating_sub(gutter).max(1);
    let highlighted = match highlight::lang_for_path(path) {
        Some(lang) => highlight::highlight(lang, &code, palette),
        None => Vec::new(),
    };
    let mut rows = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let spans = match highlighted.get(index) {
            Some(spans) => Cow::Borrowed(spans.as_slice()),
            None => Cow::Owned(plain(source, palette)),
        };
        line_rows(index + 1, source, &spans, gutter, room, palette, &mut rows);
    }
    rows
}

fn plain(line: &str, palette: &Palette) -> Vec<Span<'static>> {
    vec![Span::styled(line.to_owned(), palette.text)]
}

/// One source line's visual rows: the number on the first, a blank gutter
/// on the rest, the highlighted runs cut at the wrap points.
fn line_rows(
    number: usize,
    source: &str,
    spans: &[Span<'static>],
    gutter: usize,
    room: usize,
    palette: &Palette,
    out: &mut Vec<Line<'static>>,
) {
    // The highlighted spans cover `source` in order; if their lengths
    // disagree the line is drawn plain rather than cut mid-character.
    let covered: usize = spans.iter().map(|span| span.content.len()).sum();
    let (text, runs): (&str, Vec<(usize, usize, Style)>) = if covered == source.len() {
        let mut at = 0;
        let runs = spans
            .iter()
            .map(|span| {
                let start = at;
                at += span.content.len();
                (start, at, span.style)
            })
            .collect();
        (source, runs)
    } else {
        (source, vec![(0, source.len(), palette.text)])
    };
    let number_width = gutter - 1;
    for (row, range) in diff::wrap(text, room).into_iter().enumerate() {
        let range = diff::fit(text, range, room);
        let mut line = Vec::with_capacity(runs.len() + 1);
        let label = if row == 0 {
            format!("{number:>number_width$} ")
        } else {
            " ".repeat(gutter)
        };
        line.push(Span::styled(label, palette.dim));
        for &(start, end, style) in &runs {
            let from = start.max(range.start);
            let to = end.min(range.end);
            if from < to {
                line.push(Span::styled(text[from..to].to_owned(), style));
            }
        }
        // A single grapheme wider than `room` (CJK at a tiny width) still
        // takes its row, as in the diff pane; the frame clips it.
        out.push(Line::from(line));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ox_schema::support::Record;
    use unicode_width::UnicodeWidthStr;

    use crate::theme;
    use crate::ui::edit_block::tests::texts;

    fn input(path: &str, content: &str) -> Arc<Record> {
        let serde_json::Value::Object(map) = serde_json::json!({"path": path, "content": content})
        else {
            panic!("an object");
        };
        Arc::new(map.into_iter().collect())
    }

    #[test]
    fn r5_a_finished_write_is_the_file_with_line_numbers() {
        let input = input("src/a.rs", "fn main() {\n    run();\n}\n");
        let rows = texts(&super::render(&input, 60, theme::palette()));
        assert!(rows[1].trim_end().ends_with("# Wrote src/a.rs"), "{rows:?}");
        assert_eq!(rows[2].trim(), "", "gap under the title: {rows:?}");
        // Frame padding (2), then the gutter: ` 1 ` (min width 3, padding
        // right 1), then the code.
        assert!(rows[3].starts_with("   1 fn main() {"), "{rows:?}");
        assert!(rows[4].starts_with("   2     run();"), "{rows:?}");
        assert!(rows[5].starts_with("   3 }"), "{rows:?}");
        assert_eq!(rows.len(), 7, "three code rows in the frame: {rows:?}");
        assert!(
            rows.iter().all(|r| r.width() == 59),
            "every row fills the band: {rows:?}"
        );
    }

    #[test]
    fn the_gutter_is_muted_and_the_code_highlighted() {
        let input = input("src/a.rs", "fn main() {}\n");
        let palette = theme::palette();
        let rows = super::render(&input, 60, palette);
        let number = rows[3]
            .spans
            .iter()
            .find(|s| s.content.trim() == "1")
            .expect("line number");
        assert_eq!(number.style.fg, palette.dim.fg, "text.muted gutter");
        let keyword = rows[3]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "fn")
            .expect("keyword span");
        assert_ne!(keyword.style.fg, palette.text.fg, "the keyword is coloured");
    }

    #[test]
    fn the_gutter_grows_with_the_line_count() {
        let content: String = (1..=100).map(|n| format!("l{n}\n")).collect();
        let rows = texts(&super::render(
            &input("a.txt", &content),
            60,
            theme::palette(),
        ));
        assert!(rows[3].starts_with("     1 l1"), "{:?}", rows[3]);
        assert!(rows[102].starts_with("   100 l100"), "{:?}", rows[102]);
    }

    #[test]
    fn long_lines_wrap_by_display_width_with_the_number_once() {
        let input = input(
            "notes.txt",
            "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\n",
        );
        // Width 15: rail 1, frame padding 2, gutter 3 leaves 9 columns, so
        // four wide glyphs per row.
        let rows = texts(&super::render(&input, 15, theme::palette()));
        // The narrow title wraps too, so find the numbered row.
        let first = rows
            .iter()
            .position(|r| r.starts_with("   1 "))
            .expect("numbered row");
        assert_eq!(
            rows[first], "   1 \u{4f60}\u{597d}\u{4e16}\u{754c} ",
            "{rows:?}"
        );
        assert_eq!(
            rows[first + 1],
            "     \u{4f60}\u{597d}\u{4e16}\u{754c} ",
            "{rows:?}"
        );
        assert!(rows.iter().all(|r| r.width() == 14), "{rows:?}");
    }

    #[test]
    fn a_wide_glyph_wider_than_the_room_does_not_panic() {
        let input = input("notes.txt", "\u{4f60}\u{597d}\n");
        for width in 1..=8 {
            let rows = super::render(&input, width, theme::palette());
            assert!(!rows.is_empty(), "width {width} draws rows");
        }
    }
}
