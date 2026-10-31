//! An edit call drawn as a diff block: upstream `Edit`
//! (`routes/session/index.tsx:3439-3501`) inside the shared `BlockTool`
//! frame (`block_tool`). Pure: the call's result metadata borrowed in, rows
//! out. [`diff_body`] is shared with `patch_block`, whose update
//! bodies are the same box (`index.tsx:3561-3583`).
//!
//! Layout: `← Edit <path>`, the block's gap, then the diff inside a box with
//! `paddingLeft 1` / `paddingRight 3`. The view (split above 120 columns) is
//! chosen from the timeline width, as upstream compares `ctx.width`, while
//! the diff itself is drawn at the box's inner width.
//!
//! Deviations:
//! - Without a file in `metadata.files` (still running, failed, or a result
//!   without metadata) upstream draws a title-only `BlockTool`
//!   (`← Edit <path>` / `# Preparing edit…`); ox keeps the inline row
//!   (`tool_text`), which also lists a failure's error under it.
//! - No Diagnostics block (ox has no LSP).
//! - The path is `relativePath` (ox `file`), formatted against the TUI's
//!   directory like every tool path (`tool_text::format_path`).

use ox_schema::support::Record;
use ratatui::text::{Line, Span};

use crate::theme::Palette;

use super::block_tool::{self, BlockSpec};
use super::diff;
use super::highlight;
use super::tool_text;

/// Body padding inside the block (`paddingLeft={1}`, `paddingRight={3}`).
const PAD_LEFT: usize = 1;
const PAD_RIGHT: usize = 3;

/// The block for a call whose metadata names an edited file, else `None`
/// (the caller keeps the inline row).
#[must_use]
pub fn render(
    metadata: Option<&Record>,
    width: u16,
    palette: &Palette,
) -> Option<Vec<Line<'static>>> {
    let files = diff::files(metadata?);
    let file = files.first()?;
    let title = format!(
        "\u{2190} Edit {}",
        tool_text::format_path(file.relative_path, tool_text::paths())
    );
    let spec = BlockSpec {
        title: Some(&title),
        ..BlockSpec::default()
    };
    let body = diff_body(file.patch, file.relative_path, width, palette);
    Some(block_tool::lines(&spec, body, width))
}

/// The padded diff of `patch` for a block `width` columns wide: the parsed
/// rows in the view the width picks, or upstream's parse-error view.
#[must_use]
pub fn diff_body(patch: &str, path: &str, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let inner = block_tool::content_width(width).saturating_sub(PAD_LEFT + PAD_RIGHT);
    let inner = u16::try_from(inner.max(1)).unwrap_or(u16::MAX);
    let rows = match diff::parse(patch) {
        Ok(parsed) => diff::render(
            &parsed,
            highlight::lang_for_path(path),
            diff::view_for(width),
            inner,
            palette,
        ),
        Err(err) => diff::render_error(&err, patch, palette),
    };
    rows.into_iter().map(pad).collect()
}

/// One diff row inside the body box's padding.
fn pad(line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    spans.push(Span::raw(" ".repeat(PAD_LEFT)));
    spans.extend(line.spans);
    spans.push(Span::raw(" ".repeat(PAD_RIGHT)));
    Line::from(spans)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use ox_schema::support::Record;
    use ratatui::text::Line;

    use crate::theme;

    pub(crate) const PATCH: &str =
        "--- src/a.rs\n+++ src/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+zwei\n three\n";

    fn rec(value: serde_json::Value) -> Arc<Record> {
        match value {
            serde_json::Value::Object(map) => Arc::new(map.into_iter().collect()),
            other => panic!("not an object: {other}"),
        }
    }

    fn meta(patch: &str) -> Arc<Record> {
        rec(serde_json::json!({"files": [{
            "file": "src/a.rs", "patch": patch, "additions": 1, "deletions": 1,
            "status": "modified"
        }]}))
    }

    /// Each row without its rail cell (`┃`, or a blank without a known
    /// backdrop colour).
    pub(crate) fn texts(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .skip(1)
                    .map(|s| s.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn draw(patch: &str, width: u16) -> Vec<String> {
        let rows = super::render(Some(&meta(patch)), width, theme::palette())
            .expect("an edit with a file draws a block");
        texts(&rows)
    }

    #[test]
    fn r1_a_finished_edit_is_a_titled_unified_diff_block() {
        let rows = draw(PATCH, 100);
        assert!(
            rows[1].trim_end().ends_with("\u{2190} Edit src/a.rs"),
            "title row after the frame's top row: {rows:?}"
        );
        assert_eq!(rows[2].trim(), "", "gap under the title: {rows:?}");
        // Rail, two cells of frame padding, one of body padding, then the
        // diff gutter (` 2`), the sign cell (` -`) and its right padding.
        assert!(rows[4].starts_with("    2 - two"), "{rows:?}");
        assert!(rows[5].starts_with("    2 + zwei"), "{rows:?}");
        assert!(
            rows.iter()
                .all(|r| unicode_width::UnicodeWidthStr::width(r.as_str()) == 99)
        );
    }

    #[test]
    fn the_body_leaves_three_columns_free_on_the_right() {
        let rows = super::render(Some(&meta(PATCH)), 100, theme::palette()).expect("block");
        let row = &rows[4];
        let palette = theme::palette();
        let tail: Vec<_> = row.spans.iter().rev().take(1).collect();
        assert_eq!(tail[0].content.as_ref(), "   ", "right padding: {row:?}");
        assert_eq!(
            tail[0].style.bg, palette.raised_base.bg,
            "the padding sits on the block band, not the diff background"
        );
    }

    #[test]
    fn r2_a_wide_timeline_draws_the_split_view() {
        let rows = draw(PATCH, 140);
        let pair = rows
            .iter()
            .find(|r| r.contains("- two"))
            .expect("removed row");
        assert!(
            pair.contains("+ zwei"),
            "old and new side by side: {pair:?}"
        );
    }

    #[test]
    fn a_malformed_patch_shows_the_parse_error() {
        let rows = draw("@@ -x +y @@\n-a\n", 100);
        assert!(
            rows.iter().any(|r| r.contains("Error parsing diff:")),
            "{rows:?}"
        );
    }

    #[test]
    fn no_files_means_no_block() {
        let empty = rec(serde_json::json!({"files": []}));
        assert!(super::render(Some(&empty), 100, theme::palette()).is_none());
        assert!(super::render(None, 100, theme::palette()).is_none());
    }
}
