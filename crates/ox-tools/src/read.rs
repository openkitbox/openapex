//! Port of upstream `packages/core/src/tool/plugin/read.ts` (+ its
//! `read-filesystem.ts` service): line-numbered text reads with paging,
//! directory listings, binary/media detection and not-found suggestions.
//!
//! Two deviations remain from upstream, both deliberate:
//!
//! - Text is read in bounded windows ([`WINDOW_BYTES`]) instead of whole-file,
//!   so a large file costs a window rather than its full size. A window is
//!   only decoded when it is the page the caller asked for.
//! - Media (PNG/JPEG/GIF/WebP/PDF) becomes a `Content::File` attachment with a
//!   `data:` URI, which is how upstream hands an image to the model. PDFs are
//!   attached the same way; the model reads their text from the attachment.
//!
//! Not ported: the AGENTS.md discovery side effect and the alternate-path
//! Unicode retry.

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;

use super::{
    fail, int_param, object_param, ok_text, optional_nonneg_int, param, required_str, resolve_path,
    resource_of,
};

pub(crate) const MAX_READ_LINES: usize = 2_000;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_LINE_LENGTH: usize = 2_000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
/// Bytes read from disk at a time when a page needs to be located. Large
/// enough that ordinary files are one read, small enough that a huge file is
/// not fully resident.
const WINDOW_BYTES: usize = 256 * 1024;
/// Upstream `MAX_MEDIA_INGEST_BYTES`: an attachment larger than this is refused
/// rather than base64-inflated into the request.
const MAX_MEDIA_BYTES: usize = 20 * 1024 * 1024;

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
             read over many small slices, and use grep to find specific content in large files.\n\n\
             When to use: inspect an exact known path before editing or explaining it, view a \
             directory's entries, or look at an image or PDF the user supplied.\n\
             When NOT to use: locate a file by name (use glob), search file contents (use grep), \
             read a path you have not resolved yet, or re-read a range you already have in \
             context.",
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
        crate::contract::blocking(ctx, input, run)
    }
}

/// Upstream `PageInput` coercion: `0` and absent mean the same as JS `||`.
struct Page {
    offset: usize,
    limit: usize,
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
        return list_page(path, &absolute, page);
    }
    if !metadata.is_file() {
        return fail(format!("Path is not a file or directory: {resource}"));
    }
    // Media is attached, not decoded: one bounded read, then a base64 payload
    // the model sees as an image.
    if let Some(mime) = media_mime_for(&absolute) {
        return media_output(path, &absolute, &resource, mime, metadata.len());
    }
    let output = read_text(path, &absolute, &resource, page);
    // Only a served text page is remembered: it is the content a later edit
    // would replace. Media is attached rather than edited, and a directory has
    // no stamp to compare. The stamp comes from the metadata this call already
    // read, so it costs no extra syscall; a write that lands between this stat
    // and the bytes being served shows up as a changed stamp on the next write,
    // which only ever refuses, never overlooks.
    if output.error.is_none() {
        let stamp = crate::file_state::Stamp::of(&metadata);
        ctx.registry
            .files
            .record_stamp(&ctx.session_id, &absolute, stamp);
    }
    output
}

/// Attach a media file as a model-visible `file` content part.
fn media_output(
    path: &str,
    absolute: &Path,
    resource: &str,
    mime: &'static str,
    size: u64,
) -> ToolOutput {
    if size as usize > MAX_MEDIA_BYTES {
        return fail(format!(
            "Media exceeds {MAX_MEDIA_BYTES} byte ingestion limit: {resource}"
        ));
    }
    let Ok(bytes) = std::fs::read(absolute) else {
        return fail(format!("Unable to read {path}"));
    };
    if !is_media(&bytes) {
        // The extension lied about the bytes; fail like any other binary.
        return fail(format!("Cannot read binary file: {resource}"));
    }
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let kind = if mime == "application/pdf" {
        "PDF"
    } else {
        "Image"
    };
    ok_text(format!("{kind} read successfully"))
        .with_attachments(vec![super::Attachment {
            uri: format!("data:{mime};base64,{encoded}"),
            mime: mime.to_owned(),
            name: Some(path.to_owned()),
        }])
        .with_metadata(serde_json::json!({
            "truncated": false,
            "loaded": [resource],
        }))
}

/// Read a text page. Only the window that contains the requested page is
/// decoded, so the cost follows the page rather than the file.
fn read_text(path: &str, absolute: &Path, resource: &str, page: &Page) -> ToolOutput {
    let Ok(file) = std::fs::File::open(absolute) else {
        return fail(format!("Unable to read {path}"));
    };
    let mut reader = std::io::BufReader::with_capacity(WINDOW_BYTES, file);
    let mut carry: Vec<u8> = Vec::new();
    let mut line_number = 1usize;
    let mut entries: Vec<String> = Vec::new();
    let mut size = 0usize;
    let mut next: Option<usize> = None;
    let mut saw_any_line = false;
    let mut binary = false;

    loop {
        let mut window = vec![0u8; WINDOW_BYTES];
        let read = match reader.read(&mut window) {
            Ok(read) => read,
            Err(_) => return fail(format!("Unable to read {path}")),
        };
        let eof = read == 0;
        window.truncate(read);
        if !eof && carry.is_empty() && window.contains(&0) {
            binary = true;
            break;
        }
        carry.extend_from_slice(&window);
        if eof {
            // Final chunk: the trailing empty piece is not a line.
            if carry.last() == Some(&b'\n') {
                carry.pop();
            }
        }
        // Only the final piece can be incomplete; everything before a newline
        // is a whole line.
        let mut consumed = 0usize;
        while let Some(position) = carry[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + position;
            let line = &carry[consumed..end];
            if handle_line(
                line,
                line_number,
                path,
                page,
                &mut entries,
                &mut size,
                &mut next,
            ) {
                return finish_page(path, page, &entries, next, binary);
            }
            saw_any_line = true;
            line_number += 1;
            consumed = end + 1;
            if next.is_some() {
                return finish_page(path, page, &entries, next, binary);
            }
        }
        carry.drain(..consumed);
        if eof {
            if !carry.is_empty() {
                let _ = handle_line(
                    &carry,
                    line_number,
                    path,
                    page,
                    &mut entries,
                    &mut size,
                    &mut next,
                );
                saw_any_line = true;
            }
            break;
        }
        if carry.len() > MAX_LINE_LENGTH * 4 {
            // A single pathologically long line: keep the head so the page can
            // still be produced instead of buffering the whole file.
            let line = std::mem::take(&mut carry);
            if handle_line(
                &line,
                line_number,
                path,
                page,
                &mut entries,
                &mut size,
                &mut next,
            ) {
                return finish_page(path, page, &entries, next, binary);
            }
            saw_any_line = true;
            line_number += 1;
            if next.is_some() {
                return finish_page(path, page, &entries, next, binary);
            }
        }
    }

    if binary {
        return fail(format!("Cannot read binary file: {resource}"));
    }
    if next.is_none() && saw_any_line && entries.is_empty() && page.offset != 1 {
        return fail(format!(
            "Offset {} is out of range for this file ({} {})",
            page.offset,
            line_number - 1,
            if line_number == 2 { "line" } else { "lines" }
        ));
    }
    finish_page(path, page, &entries, next, binary)
}

/// Consider one line for the page. Returns true when the page is complete.
fn handle_line(
    raw: &[u8],
    line_number: usize,
    _path: &str,
    page: &Page,
    entries: &mut Vec<String>,
    size: &mut usize,
    next: &mut Option<usize>,
) -> bool {
    if line_number < page.offset {
        return false;
    }
    if entries.len() >= page.limit || *size >= MAX_READ_BYTES {
        *next = Some(line_number);
        return true;
    }
    let decoded = String::from_utf8_lossy(raw);
    let value = decoded.strip_suffix('\r').unwrap_or(&decoded);
    let text = clamp_line(value);
    let line_size = text.len() + usize::from(!entries.is_empty());
    if *size + line_size > MAX_READ_BYTES {
        *next = Some(line_number);
        return true;
    }
    entries.push(text);
    *size += line_size;
    false
}

fn finish_page(
    path: &str,
    page: &Page,
    entries: &[String],
    next: Option<usize>,
    binary: bool,
) -> ToolOutput {
    if binary {
        return fail(format!("Cannot read binary file: {path}"));
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
fn list_page(path: &str, absolute: &Path, page: &Page) -> ToolOutput {
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
    let start = page.offset.saturating_sub(1).min(visible.len());
    let end = start.saturating_add(page.limit).min(visible.len());
    let selected_len = end - start;
    let truncated = end < visible.len();
    let next = truncated.then(|| page.offset + selected_len);
    let selected: Vec<(String, bool)> = visible.drain(start..end).collect();
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
    for (display, _) in selected {
        content.push(display);
    }
    if truncated && let Some(next) = next {
        content.push(format!(
            "[Output truncated. Continue reading with offset: {next}]"
        ));
    }
    ok_text(content.join("\n")).with_metadata(serde_json::json!({ "truncated": truncated }))
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

/// The media MIME for a path, by extension, when upstream would ingest it.
fn media_mime_for(absolute: &Path) -> Option<&'static str> {
    let extension = absolute.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

/// Media MIME by magic bytes for `MEDIA_MIMES` (PNG/JPEG/GIF/WebP/PDF).
#[must_use]
pub fn sniff_media(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF") {
        Some("application/pdf")
    } else {
        None
    }
}

fn is_media(bytes: &[u8]) -> bool {
    sniff_media(bytes).is_some()
}
