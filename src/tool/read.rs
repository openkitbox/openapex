//! Port of upstream `packages/core/src/tool/plugin/read.ts` (+ its
//! `read-filesystem.ts` service): line-numbered text reads with paging,
//! directory listings, binary/media detection and not-found suggestions.
//!
//! MVP deviations (documented in `tool/mod.rs`): whole-file reads (upstream
//! chunks at 256 KiB through a virtual Files layer), no AGENTS.md discovery
//! side-effect, no alternate-path Unicode retry, and media files (PNG/JPEG/
//! GIF/WebP/PDF) that upstream ingests as model attachments fail with the
//! binary-file error.

use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};

use super::{
    fail, int_param, object_param, ok_text, optional_nonneg_int, param, required_str, resolve_path,
    resource_of,
};

pub(crate) const MAX_READ_LINES: usize = 2_000;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_LINE_LENGTH: usize = 2_000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "read",
            "Read the contents of a file or directory. Supports text files, images, and PDFs. \
             Images and PDFs are presented directly to the model. Each text line is prefixed by \
             its 1-based line number as <line>: <content>. The prefix is for reference and is \
             not part of the file content. Directory entries are returned one per line. Use \
             offset and limit to read large files or directories in sections. Prefer one larger \
             read over many small slices, and use grep to find specific content in large files.",
            object_param(
                vec![
                    ("path", param("string", "File or directory to read")),
                    (
                        "offset",
                        int_param(
                            0,
                            "The line or directory entry to start reading from (1-based)",
                        ),
                    ),
                    (
                        "limit",
                        int_param(
                            0,
                            "The maximum number of lines or directory entries to read (defaults to and capped at 2000)",
                        ),
                    ),
                ],
                &["path"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(&ctx, &input) })
    }
}

/// Upstream `PageInput` coercion: `0` and absent mean the same as JS `||`.
struct Page {
    offset: usize,
    limit: usize,
    /// Whether the caller passed `offset` or `limit` at all — any explicit
    /// value forces paging upstream (text pages strip `\r`).
    explicit: bool,
}

impl Page {
    fn new(input: &Value) -> Result<Self, ToolOutput> {
        let offset = optional_nonneg_int("read", input, "offset")?;
        let limit = optional_nonneg_int("read", input, "limit")?;
        let raw_offset = offset.unwrap_or(0);
        let raw_limit = limit.unwrap_or(0);
        Ok(Self {
            offset: if raw_offset == 0 {
                1
            } else {
                raw_offset as usize
            },
            limit: if raw_limit == 0 {
                MAX_READ_LINES
            } else {
                (raw_limit as usize).min(MAX_READ_LINES)
            },
            explicit: offset.is_some() || limit.is_some(),
        })
    }
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let path = match required_str("read", input, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let page = match Page::new(input) {
        Ok(page) => page,
        Err(output) => return output,
    };
    read_path(ctx, path, &page)
}

fn read_path(ctx: &ToolContext, path: &str, page: &Page) -> ToolOutput {
    let absolute = resolve_path(&ctx.directory, path);
    let resource = resource_of(&ctx.directory, &absolute);
    let Ok(metadata) = std::fs::metadata(&absolute) else {
        return missing(ctx, path, &absolute);
    };
    if metadata.is_dir() {
        return list_page(ctx, path, &absolute, page);
    }
    if !metadata.is_file() {
        return fail(format!("Path is not a file or directory: {resource}"));
    }
    let Ok(bytes) = std::fs::read(&absolute) else {
        return fail(format!("Unable to read {path}"));
    };
    if is_media(&bytes) || bytes.contains(&0) {
        // Upstream ingests MEDIA_MIMES as model attachments; ox MVP reads
        // text only, so media fails like any other binary.
        return fail(format!("Cannot read binary file: {resource}"));
    }
    let decoded = String::from_utf8_lossy(&bytes);
    let paged = bytes.len() > MAX_READ_BYTES || page.explicit;
    if !paged {
        // Whole-file read: lines keep their `\r` (only pages strip it).
        let content = decoded
            .split('\n')
            .map(clamp_line)
            .collect::<Vec<_>>()
            .join("\n");
        return ok_text(file_model_content(path, &content, 1, false, None));
    }
    let mut available: Vec<&str> = decoded.split('\n').collect();
    if available.last() == Some(&"") {
        available.pop();
    }
    for line in &mut available {
        if let Some(stripped) = line.strip_suffix('\r') {
            *line = stripped;
        }
    }
    let mut entries: Vec<String> = Vec::new();
    let mut size = 0usize;
    let mut next: Option<usize> = None;
    for (index, value) in available.iter().enumerate().skip(page.offset - 1) {
        let line = page.offset + index;
        if entries.len() >= page.limit || size >= MAX_READ_BYTES {
            next = Some(line);
            break;
        }
        let text = clamp_line(value);
        let line_size = text.len() + usize::from(!entries.is_empty());
        if size + line_size > MAX_READ_BYTES {
            next = Some(line);
            break;
        }
        entries.push(text);
        size += line_size;
    }
    if next.is_none()
        && entries.len() >= page.limit
        && page.offset - 1 + entries.len() < available.len()
    {
        next = Some(page.offset + entries.len());
    }
    if entries.is_empty() && page.offset != 1 {
        let lines = available.len();
        return fail(format!(
            "Offset {} is out of range for this file ({} {})",
            page.offset,
            lines,
            if lines == 1 { "line" } else { "lines" }
        ));
    }
    ok_text(file_model_content(
        path,
        &entries.join("\n"),
        page.offset,
        next.is_some(),
        next,
    ))
}

/// Upstream `toModelContent` for text pages / whole files.
fn file_model_content(
    path: &str,
    content: &str,
    start: usize,
    truncated: bool,
    next: Option<usize>,
) -> String {
    let text = content.strip_suffix('\n').unwrap_or(content);
    let lines: Vec<&str> = if content.is_empty() {
        Vec::new()
    } else {
        text.split('\n').collect()
    };
    let mut out = Vec::new();
    if lines.is_empty() {
        out.push(format!("Read file {path}, 0 lines"));
    } else {
        out.push(format!(
            "Read file {path}, lines {}-{}",
            start,
            start + lines.len() - 1
        ));
    }
    for (index, line) in lines.iter().enumerate() {
        out.push(format!("{}: {line}", start + index));
    }
    if truncated && let Some(next) = next {
        out.push(format!(
            "[Output truncated. Continue reading with offset: {next}]"
        ));
    }
    out.join("\n")
}

/// Directory listing page (upstream `list`): directories first, then files,
/// each sorted by path; directories carry a trailing separator.
fn list_page(_ctx: &ToolContext, path: &str, absolute: &Path, page: &Page) -> ToolOutput {
    let Ok(entries) = std::fs::read_dir(absolute) else {
        return fail(format!("Unable to read {path}"));
    };
    let mut visible: Vec<(String, bool)> = Vec::new(); // (display path, is directory)
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            visible.push((format!("{name}/"), true));
        } else if file_type.is_file() || file_type.is_symlink() {
            visible.push((name, false));
        }
    }
    visible.sort_by(|left, right| match (left.1, right.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        // Upstream locale-compares; plain byte order for the ASCII case.
        _ => left.0.cmp(&right.0),
    });
    let start = page.offset.saturating_sub(1);
    let selected: Vec<&(String, bool)> = visible.iter().skip(start).take(page.limit).collect();
    let truncated = start + selected.len() < visible.len();
    let next = truncated.then(|| page.offset + selected.len());
    let mut content = Vec::new();
    if selected.is_empty() {
        content.push(format!("Read directory {path}, 0 entries"));
    } else {
        content.push(format!(
            "Read directory {path}, entries {}-{}",
            page.offset,
            page.offset + selected.len() - 1
        ));
    }
    for (display, _) in &selected {
        content.push(display.clone());
    }
    if truncated && let Some(next) = next {
        content.push(format!(
            "[Output truncated. Continue reading with offset: {next}]"
        ));
    }
    ok_text(content.join("\n"))
}

/// Upstream `missing`: not-found message with sibling-name suggestions.
fn missing(_ctx: &ToolContext, path: &str, absolute: &Path) -> ToolOutput {
    let base = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut suggestions: Vec<String> = Vec::new();
    if let Some(parent) = absolute.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            let candidate = entry.file_name().to_string_lossy().to_lowercase();
            if candidate.contains(&base) || base.contains(&candidate) {
                let display = match Path::new(path).parent() {
                    Some(prefix) => prefix.join(entry.file_name()),
                    None => PathBuf::from(entry.file_name()),
                };
                suggestions.push(display.to_string_lossy().to_string());
                if suggestions.len() == 3 {
                    break;
                }
            }
        }
    }
    if suggestions.is_empty() {
        return fail(format!("File not found: {path}"));
    }
    fail(format!(
        "File not found: {path}\n\nDid you mean one of these?\n{}",
        suggestions.join("\n")
    ))
}

/// Upstream `clampLine`: slice to the character limit and append the suffix.
fn clamp_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        return line.to_owned();
    }
    let mut end = MAX_LINE_LENGTH;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MAX_LINE_SUFFIX}", &line[..end])
}

/// Media detection by magic bytes for `MEDIA_MIMES` (PNG/JPEG/GIF/WebP/PDF).
fn is_media(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
        || bytes.starts_with(b"%PDF-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx(directory: &Path) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.to_path_buf(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    async fn execute(directory: &Path, input: Value) -> ToolOutput {
        ReadTool.execute(ctx(directory), input).await
    }

    #[tokio::test]
    async fn reads_numbered_lines() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let output = execute(directory.path(), json!({"path": "a.txt"})).await;
        assert_eq!(
            output.content,
            json!("Read file a.txt, lines 1-3\n1: one\n2: two\n3: three")
        );
        assert!(output.error.is_none());
    }

    #[tokio::test]
    async fn reads_empty_file() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("e.txt"), "").unwrap();
        let output = execute(directory.path(), json!({"path": "e.txt"})).await;
        assert_eq!(output.content, json!("Read file e.txt, 0 lines"));
    }

    #[tokio::test]
    async fn paginates_with_limit_and_next_marker() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let output = execute(directory.path(), json!({"path": "a.txt", "limit": 2})).await;
        assert_eq!(
            output.content,
            json!(
                "Read file a.txt, lines 1-2\n1: one\n2: two\n[Output truncated. Continue reading with offset: 3]"
            )
        );
    }

    #[tokio::test]
    async fn offset_pages_and_out_of_range() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let output = execute(directory.path(), json!({"path": "a.txt", "offset": 2})).await;
        assert_eq!(
            output.content,
            json!("Read file a.txt, lines 2-3\n2: two\n3: three")
        );
        let output = execute(directory.path(), json!({"path": "a.txt", "offset": 10})).await;
        assert_eq!(
            output.error.as_deref(),
            Some("Offset 10 is out of range for this file (3 lines)")
        );
    }

    #[tokio::test]
    async fn lists_directories_directories_first() {
        let directory = tempdir().unwrap();
        fs::create_dir_all(directory.path().join("sub")).unwrap();
        fs::write(directory.path().join("b.txt"), "x").unwrap();
        fs::write(directory.path().join("a.txt"), "y").unwrap();
        let output = execute(directory.path(), json!({"path": "."})).await;
        assert_eq!(
            output.content,
            json!("Read directory ., entries 1-3\nsub/\na.txt\nb.txt")
        );
    }

    #[tokio::test]
    async fn rejects_binary_and_media() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("bin.dat"), b"ok\x00no").unwrap();
        let output = execute(directory.path(), json!({"path": "bin.dat"})).await;
        assert_eq!(
            output.error.as_deref(),
            Some("Cannot read binary file: bin.dat")
        );
        fs::write(directory.path().join("pic.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
        let output = execute(directory.path(), json!({"path": "pic.png"})).await;
        assert_eq!(
            output.error.as_deref(),
            Some("Cannot read binary file: pic.png")
        );
    }

    #[tokio::test]
    async fn missing_file_suggests_siblings() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), "").unwrap();
        fs::write(directory.path().join("other.txt"), "").unwrap();
        let output = execute(directory.path(), json!({"path": "config.tom"})).await;
        assert_eq!(
            output.error.as_deref(),
            Some("File not found: config.tom\n\nDid you mean one of these?\nconfig.toml")
        );
    }

    #[tokio::test]
    async fn truncates_long_lines_with_suffix() {
        let directory = tempdir().unwrap();
        let long = "x".repeat(2500);
        fs::write(directory.path().join("long.txt"), format!("{long}\n")).unwrap();
        let output = execute(directory.path(), json!({"path": "long.txt"})).await;
        let expected = format!(
            "Read file long.txt, lines 1-1\n1: {}{MAX_LINE_SUFFIX}",
            "x".repeat(MAX_LINE_LENGTH)
        );
        assert_eq!(output.content, json!(expected));
    }
}
