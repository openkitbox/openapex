//! A patch call drawn as one block per file: upstream `ApplyPatch`
//! (`routes/session/index.tsx:3503-3621`). Pure: the call borrowed in, rows
//! out.
//!
//! A finished call with files in `metadata.files` draws one `BlockTool` per
//! file, one unbanded row apart (`gap={1}`): `# Created <path>` and
//! `← Patched <path>` over the edit block's diff body
//! ([`edit_block::diff_body`]), `# Deleted <path>` over `-N line(s)` in the
//! removed-diff colour. A failed call draws `# Patch failed`, naming the file
//! when `patchText` targets exactly one, with the header in the error colour
//! and the error text muted.
//!
//! Deviations:
//! - A running call (and a finished one without files) keeps the inline
//!   `Patching` row (`tool_text`); upstream draws a title-only `BlockTool`
//!   with a spinner.
//! - The `metadata.applied` branch is not ported: ox never reports it.
//! - Upstream's `path` titles truncate via `FilePath`; ox wraps the title.

use ox_client::store::ToolState;
use ox_schema::support::Record;
use ratatui::text::{Line, Span};

use crate::theme::Palette;

use super::block_tool::{self, BlockSpec};
use super::diff::{self, FileEntry, FileType};
use super::edit_block;
use super::tool_text;

/// What one patch call carries, borrowed from its block.
#[derive(Debug, Clone, Copy)]
pub struct PatchCall<'a> {
    pub state: ToolState,
    pub input: &'a Record,
    pub metadata: Option<&'a Record>,
    /// The tool result: the failure message once failed.
    pub output: &'a str,
}

/// The blocks for a finished call with files or a failed call, else `None`
/// (the caller keeps the inline row).
#[must_use]
pub fn render(call: &PatchCall<'_>, width: u16, palette: &Palette) -> Option<Vec<Line<'static>>> {
    match call.state {
        ToolState::Success => {
            let files = diff::files(call.metadata?);
            if files.is_empty() {
                return None;
            }
            let mut rows = Vec::new();
            for (index, file) in files.iter().enumerate() {
                if index > 0 {
                    rows.push(Line::default());
                }
                rows.extend(file_block(file, width, palette));
            }
            Some(rows)
        }
        ToolState::Failed => Some(failed_block(call, width, palette)),
        _ => None,
    }
}

/// One file's block.
fn file_block(file: &FileEntry<'_>, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let label = match file.kind {
        FileType::Add => "# Created",
        FileType::Delete => "# Deleted",
        FileType::Update | FileType::Move => "\u{2190} Patched",
    };
    let title = format!(
        "{label} {}",
        tool_text::format_path(file.relative_path, tool_text::paths())
    );
    let body = match file.kind {
        FileType::Delete => {
            let plural = if file.deletions == 1 { "" } else { "s" };
            let count = format!("-{} line{plural}", file.deletions);
            vec![Line::from(Span::styled(count, palette.diff.removed))]
        }
        _ => edit_block::diff_body(file.patch, file.relative_path, width, palette),
    };
    let spec = BlockSpec {
        title: Some(&title),
        ..BlockSpec::default()
    };
    block_tool::lines(&spec, body, width)
}

/// `# Patch failed[ <path>]` over the muted error.
fn failed_block(call: &PatchCall<'_>, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let patch_text = call
        .input
        .get("patchText")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let title = match targets(patch_text).as_slice() {
        [one] => format!(
            "# Patch failed {}",
            tool_text::format_path(one, tool_text::paths())
        ),
        _ => String::from("# Patch failed"),
    };
    let spec = BlockSpec {
        title: Some(&title),
        error: Some(call.output),
        title_fg: palette.warn.fg,
        error_fg: palette.dim.fg,
        ..BlockSpec::default()
    };
    block_tool::lines(&spec, Vec::new(), width)
}

/// The files `patchText` names: upstream's
/// `/\*\*\* (?:Add|Update|Delete) File: ([^\r\n]+)/g`, each trimmed.
#[must_use]
pub fn targets(patch_text: &str) -> Vec<&str> {
    const MARKERS: [&str; 3] = ["*** Add File: ", "*** Update File: ", "*** Delete File: "];
    let mut found = Vec::new();
    let mut rest = patch_text;
    while let Some(start) = rest.find("*** ") {
        let tail = &rest[start..];
        let marker = MARKERS.iter().find(|marker| tail.starts_with(**marker));
        let Some(marker) = marker else {
            rest = &tail[4..];
            continue;
        };
        let value = &tail[marker.len()..];
        let end = value.find(['\r', '\n']).unwrap_or(value.len());
        // `[^\r\n]+` needs at least one character.
        if end > 0 {
            found.push(value[..end].trim());
        }
        rest = &value[end..];
    }
    found
}
