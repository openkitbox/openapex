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
