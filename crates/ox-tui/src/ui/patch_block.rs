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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ox_client::store::ToolState;
    use ox_schema::support::Record;

    use super::PatchCall;
    use crate::theme;
    use crate::ui::edit_block::tests::texts;

    fn rec(value: serde_json::Value) -> Arc<Record> {
        match value {
            serde_json::Value::Object(map) => Arc::new(map.into_iter().collect()),
            other => panic!("not an object: {other}"),
        }
    }

    fn files() -> Arc<Record> {
        rec(serde_json::json!({"files": [
            {"file": "new.rs", "status": "added", "additions": 1, "deletions": 0,
             "patch": "--- /dev/null\n+++ new.rs\n@@ -0,0 +1 @@\n+fn new() {}\n"},
            {"file": "lib.rs", "status": "modified", "additions": 1, "deletions": 1,
             "patch": "--- lib.rs\n+++ lib.rs\n@@ -1 +1 @@\n-old\n+new\n"},
            {"file": "gone.rs", "status": "deleted", "additions": 0, "deletions": 3,
             "patch": "--- gone.rs\n+++ gone.rs\n@@ -1,3 +0,0 @@\n-a\n-b\n-c\n"}
        ]}))
    }

    fn draw(call: &PatchCall<'_>) -> Option<Vec<String>> {
        super::render(call, 80, theme::palette()).map(|rows| texts(&rows))
    }

    #[test]
    fn r6_one_block_per_file_with_a_gap_between() {
        let input = Record::new();
        let meta = files();
        let call = PatchCall {
            state: ToolState::Success,
            input: &input,
            metadata: Some(&meta),
            output: "",
        };
        let rows = draw(&call).expect("files draw blocks");
        let titles: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains(" new.rs") || r.contains(" lib.rs") || r.contains(" gone.rs"))
            .collect();
        assert_eq!(titles.len(), 3, "{rows:?}");
        assert!(titles[0].trim().ends_with("# Created new.rs"), "{rows:?}");
        assert!(
            titles[1].trim().ends_with("\u{2190} Patched lib.rs"),
            "{rows:?}"
        );
        assert!(titles[2].trim().ends_with("# Deleted gone.rs"), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("1 + fn new() {}")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("1 - old")), "{rows:?}");
        assert!(rows.iter().any(|r| r.trim() == "-3 lines"), "{rows:?}");
        assert!(
            !rows.iter().any(|r| r.contains("-a")),
            "a delete shows no diff: {rows:?}"
        );
    }

    #[test]
    fn the_gap_between_blocks_is_one_unbanded_row() {
        let input = Record::new();
        let meta = files();
        let call = PatchCall {
            state: ToolState::Success,
            input: &input,
            metadata: Some(&meta),
            output: "",
        };
        let rows = super::render(&call, 80, theme::palette()).expect("blocks");
        let gaps: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, line)| line.spans.is_empty())
            .map(|(index, _)| index)
            .collect();
        assert_eq!(gaps.len(), 2, "one gap between each pair: {rows:?}");
        assert!(
            rows.first().is_some_and(|l| !l.spans.is_empty()),
            "no gap above the first"
        );
        assert!(
            rows.last().is_some_and(|l| !l.spans.is_empty()),
            "no gap below the last"
        );
    }

    #[test]
    fn r7_a_one_line_delete_is_singular() {
        let input = Record::new();
        let meta = rec(serde_json::json!({"files": [
            {"file": "x.rs", "status": "deleted", "additions": 0, "deletions": 1,
             "patch": "--- x.rs\n+++ x.rs\n@@ -1 +0,0 @@\n-a\n"}
        ]}));
        let call = PatchCall {
            state: ToolState::Success,
            input: &input,
            metadata: Some(&meta),
            output: "",
        };
        let rows = super::render(&call, 80, theme::palette()).expect("block");
        let body = rows
            .iter()
            .find(|line| texts(std::slice::from_ref(line))[0].trim() == "-1 line")
            .expect("singular count");
        let span = body
            .spans
            .iter()
            .find(|s| s.content.contains("-1"))
            .expect("count span");
        assert_eq!(
            span.style.fg,
            theme::palette().diff.removed.fg,
            "removed colour"
        );
    }

    fn failed(patch_text: &str) -> Vec<ratatui::text::Line<'static>> {
        let input = rec(serde_json::json!({"patchText": patch_text}));
        let call = PatchCall {
            state: ToolState::Failed,
            input: &input,
            metadata: None,
            output: "hunk 1 did not apply",
        };
        super::render(&call, 80, theme::palette()).expect("a failure is a block")
    }

    #[test]
    fn r8_a_failed_patch_with_one_target_names_it_in_the_error_colour() {
        let rows = failed("*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** End Patch");
        let text = texts(&rows);
        assert!(
            text[1].trim().ends_with("# Patch failed src/a.rs"),
            "{text:?}"
        );
        let palette = theme::palette();
        let title = rows[1]
            .spans
            .iter()
            .find(|s| s.content.contains("Patch"))
            .expect("title");
        assert_eq!(
            title.style.fg, palette.warn.fg,
            "header in the error colour"
        );
        let error = rows
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|s| s.content.contains("did not apply"))
            .expect("error text");
        assert_eq!(error.style.fg, palette.dim.fg, "error text muted");
    }

    #[test]
    fn r9_a_failed_patch_with_two_targets_names_none() {
        let rows = failed("*** Add File: a.rs\n+x\n*** Delete File: b.rs\n");
        let text = texts(&rows);
        assert_eq!(text[1].trim(), "# Patch failed", "{text:?}");
    }

    #[test]
    fn r12_a_success_without_files_and_a_running_call_keep_the_inline_row() {
        let input = Record::new();
        let empty = rec(serde_json::json!({"files": []}));
        for (state, metadata) in [
            (ToolState::Success, Some(&*empty)),
            (ToolState::Success, None),
            (ToolState::Running, None),
            (ToolState::Input, None),
        ] {
            let call = PatchCall {
                state,
                input: &input,
                metadata,
                output: "",
            };
            assert!(draw(&call).is_none(), "{state:?} {metadata:?}");
        }
    }

    #[test]
    fn targets_follow_the_upstream_regex() {
        assert_eq!(
            super::targets(
                "*** Add File:  a.rs \r\n*** Update File: b c.rs\n*** Move to: d.rs\n*** Delete File:"
            ),
            ["a.rs", "b c.rs"]
        );
        assert!(super::targets("").is_empty());
    }
}
