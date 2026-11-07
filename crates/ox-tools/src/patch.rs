//! Port of upstream `packages/core/src/tool/plugin/patch.ts` (+ the
//! `@opencode/util/patch` parser): the OpenAI-style patch envelope —
//! `*** Add File:` / `*** Delete File:` / `*** Update File:` hunks with
//! `@@` context markers, `*** Move to:` renames, and `*** End of File`
//! anchors — prepared fully before any byte is written, with the upstream
//! "Completed before failure" reporting.
//!
//! MVP deviations: no formatter pass (ox has none) and no permission/
//! file-diff metadata channel; the model-facing content is identical.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::file_state::Verdict;
use ox_schema::wire::ToolDef;

use super::diff::{self as Diff, DiffStatus};
use super::{fail, object_param, ok_text, param, required_str, resolve_path, resource_of};

pub struct PatchTool;

impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "patch",
            "Use the `patch` tool to edit files. Your patch language is a stripped\u{2011}down, \
             file\u{2011}oriented diff format designed to be easy to parse and safe to apply. You can think \
             of it as a high\u{2011}level envelope:\n\n*** Begin Patch\n[ one or more file sections ]\n*** End \
             Patch\n\nWithin that envelope, you get a sequence of file operations.\nYou MUST include a \
             header to specify the action you are taking.\nEach operation starts with one of three \
             headers:\n\n*** Add File: <path> - create a new file. Every following line is a + line (the \
             initial contents).\n*** Delete File: <path> - remove an existing file. Nothing follows.\n*** \
             Update File: <path> - patch an existing file in place (optionally with a rename).\n\nExample \
             patch:\n\n```\n*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: \
             src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello, \
             world!\")\n*** Delete File: obsolete.txt\n*** End Patch\n```\n\nIt is important to \
             remember:\n\n- You must include a header with your intended action (Add/Delete/Update)\n- You \
             must prefix new lines with `+` even when creating a new file\\n\\n\
             When to use: change several files or several separated regions of one file in a \
             single call, or rename a file while editing it (Move to).\\n\
             When NOT to use: one small change in one file (use edit), creating a file whose \
             full contents you already know (use write), or text you have not read.",
            object_param(
                vec![(
                    "patchText",
                    param(
                        "string",
                        "The full patch text describing add, update, and delete operations",
                    ),
                )],
                &["patchText"],
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

/// One parsed hunk (upstream `Patch.Hunk`).
#[derive(Debug)]
enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<Chunk>,
    },
}

/// One `@@`-delimited update chunk (upstream `UpdateFileChunk`).
#[derive(Debug)]
struct Chunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    context: Option<String>,
    end_of_file: bool,
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let patch_text = match required_str("patch", input, "patchText") {
        Ok(text) => text,
        Err(output) => return output,
    };
    if patch_text.is_empty() {
        return fail("patchText is required".to_owned());
    }
    let hunks = match parse(patch_text) {
        Ok(hunks) => hunks,
        Err(message) => return fail(format!("patch verification failed: {message}")),
    };
    if hunks.is_empty() {
        return fail("patch rejected: empty patch".to_owned());
    }
    apply(ctx, hunks)
}

/// Cause strings matching upstream `errorMessage`.
fn cause_of(error: &std::io::Error, path: &std::path::Path) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "file does not exist".to_owned(),
        _ => {
            if path.is_dir() {
                "path is a directory".to_owned()
            } else {
                error.to_string()
            }
        }
    }
}

/// A prepared change, ready to write (upstream `Prepared`).
struct Prepared {
    kind: &'static str,
    /// Absolute target; for updates with a move this is the original.
    target: PathBuf,
    resource: String,
    /// Bytes to write; `None` for deletes.
    content: Option<String>,
    /// `*** Move to:` destination: absolute path and resource name.
    move_to: Option<(PathBuf, String)>,
    /// The text before the change, for the client-facing diff. `None` when the
    /// file did not exist (an added file).
    before: Option<String>,
}

impl Prepared {
    /// The client-facing diff for this change: added, deleted, or modified.
    fn diff(&self) -> Diff::FileDiff {
        let (resource, after) = match (&self.move_to, self.content.as_deref()) {
            (Some((_, moved)), Some(content)) => (moved.as_str(), content),
            (_, Some(content)) => (self.resource.as_str(), content),
            (_, None) => {
                return Diff::deleted_file(&self.resource, self.before.as_deref().unwrap_or(""));
            }
        };
        match &self.before {
            Some(before) => Diff::file_diff(resource, before, after, DiffStatus::Modified),
            None => Diff::file_diff(resource, "", after, DiffStatus::Added),
        }
    }
}

/// Make a write target's parent directory and reject a target that cannot be
/// written: an existing directory.
///
/// Write permission is not probed here: Unix mode bits alone do not decide it
/// (root and ACLs can write a `444` file, a read-only mount rejects a `644`), so
/// a guessed refusal would be wrong more often than the write's own error.
fn preflight_write(target: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::metadata(target) {
        Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "path is a directory",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Reject a delete target that is missing or is a directory.
fn preflight_delete(target: &std::path::Path) -> std::io::Result<()> {
    match std::fs::metadata(target) {
        Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::IsADirectory,
            "path is a directory",
        )),
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

fn apply(ctx: &ToolContext, hunks: Vec<Hunk>) -> ToolOutput {
    // ---- prepare (no writes) ----
    let mut prepared: Vec<Prepared> = Vec::new();
    let mut updates: HashMap<PathBuf, String> = HashMap::new();
    for hunk in &hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let content = if contents.ends_with('\n') || contents.is_empty() {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                let target = resolve_path(&ctx.directory, path);
                prepared.push(Prepared {
                    kind: "add",
                    resource: resource_of(&ctx.directory, &target),
                    target,
                    content: Some(content),
                    move_to: None,
                    before: None,
                });
            }
            Hunk::Delete { path } => {
                let target = resolve_path(&ctx.directory, path);
                let resource = resource_of(&ctx.directory, &target);
                match std::fs::read_to_string(&target) {
                    Ok(before) => prepared.push(Prepared {
                        kind: "delete",
                        resource,
                        target,
                        content: None,
                        move_to: None,
                        before: Some(before),
                    }),
                    Err(error) => {
                        return fail(format!(
                            "patch verification failed: Failed to delete {resource}: {}",
                            cause_of(&error, &target)
                        ));
                    }
                }
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let target = resolve_path(&ctx.directory, path);
                let resource = resource_of(&ctx.directory, &target);
                let original = match updates.get(&target) {
                    Some(previous) => previous.clone(),
                    None => match std::fs::read_to_string(&target) {
                        Ok(text) => text,
                        Err(error) => {
                            return fail(format!(
                                "patch verification failed: Failed to read file to update {}: {}",
                                target.display(),
                                cause_of(&error, &target)
                            ));
                        }
                    },
                };
                let (content, _bom) = match derive(path, chunks, &original) {
                    Ok(update) => update,
                    Err(message) => {
                        return fail(format!("patch verification failed: {message}"));
                    }
                };
                let move_to = move_path.as_ref().map(|move_path| {
                    let absolute = resolve_path(&ctx.directory, move_path);
                    let resource = resource_of(&ctx.directory, &absolute);
                    (absolute, resource)
                });
                if move_to.is_none() {
                    updates.insert(target.clone(), content.clone());
                }
                prepared.push(Prepared {
                    kind: "update",
                    resource,
                    target,
                    content: Some(content),
                    move_to,
                    before: Some(original),
                });
            }
        }
    }

    // ---- preflight (no file contents written) ----
    // Catch predictable target failures before the first byte is written, so
    // a patch that cannot fully apply does not leave earlier changes behind.
    // Missing parent directories are created here, so a preflight failure can
    // still leave an empty directory. See `preflight_write`.
    //
    // Freshness runs first and covers every path the patch would write,
    // including a move destination, so a target that changed since this session
    // read it is refused before any directory is created for it.
    let stale = |operation: &str, resource: &str, target: &std::path::Path| {
        (ctx.registry.files.check(&ctx.session_id, target) == Verdict::Changed).then(|| {
            fail(format!(
                "Failed to {operation} {resource}: the file changed since your last read of it. Read it again before patching."
            ))
        })
    };
    for change in &prepared {
        let refusal = match change.kind {
            "delete" => stale("delete", &change.resource, &change.target),
            _ => stale("write", &change.resource, &change.target).or_else(|| {
                change
                    .move_to
                    .as_ref()
                    .and_then(|(target, resource)| stale("write", resource, target))
            }),
        };
        if let Some(output) = refusal {
            return output;
        }
    }
    for change in &prepared {
        match change.kind {
            "delete" => {
                if let Err(error) = preflight_delete(&change.target) {
                    return fail(format!(
                        "Failed to delete {}: {}",
                        change.resource,
                        cause_of(&error, &change.target)
                    ));
                }
            }
            "update" => match &change.move_to {
                Some((move_target, move_resource)) => {
                    if let Err(error) = preflight_write(move_target) {
                        return fail(format!(
                            "Failed to write {move_resource}: {}",
                            cause_of(&error, move_target)
                        ));
                    }
                    if let Err(error) = preflight_delete(&change.target) {
                        return fail(format!(
                            "Failed to delete {}: {}",
                            change.resource,
                            cause_of(&error, &change.target)
                        ));
                    }
                }
                None => {
                    if let Err(error) = preflight_write(&change.target) {
                        return fail(format!(
                            "Failed to write {}: {}",
                            change.resource,
                            cause_of(&error, &change.target)
                        ));
                    }
                }
            },
            _ => {
                if let Err(error) = preflight_write(&change.target) {
                    return fail(format!(
                        "Failed to write {}: {}",
                        change.resource,
                        cause_of(&error, &change.target)
                    ));
                }
            }
        }
    }

    // ---- apply in order ----
    let mut applied: Vec<(&'static str, String)> = Vec::new();
    let failure =
        |operation: String, cause: String, applied: &[(&'static str, String)]| -> ToolOutput {
            let completed = applied
                .iter()
                .map(|(_, resource)| resource.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if completed.is_empty() {
                String::new()
            } else {
                format!(". Completed before failure: {completed}")
            };
            fail(format!("{operation}: {cause}{suffix}"))
        };
    for change in &prepared {
        match change.kind {
            "delete" => {
                if let Err(error) = std::fs::remove_file(&change.target) {
                    return failure(
                        format!("Failed to delete {}", change.resource),
                        cause_of(&error, &change.target),
                        &applied,
                    );
                }
                applied.push(("delete", change.resource.clone()));
                // The path is gone: drop its stamp so a recreated file is not
                // compared against content that no longer exists.
                ctx.registry.files.forget(&ctx.session_id, &change.target);
            }
            "update" => match &change.move_to {
                Some((move_target, move_resource)) => {
                    let content = change.content.as_deref().unwrap_or_default();
                    if let Err(error) = std::fs::write(move_target, content.as_bytes()) {
                        return failure(
                            format!("Failed to write {move_resource}"),
                            cause_of(&error, move_target),
                            &applied,
                        );
                    }
                    if let Err(error) = std::fs::remove_file(&change.target) {
                        return failure(
                            format!(
                                "Wrote {move_resource} but failed to remove {}",
                                change.resource
                            ),
                            cause_of(&error, &change.target),
                            &applied,
                        );
                    }
                    // The update moved the content to the destination, so that
                    // is the path whose state this session now knows; the source
                    // no longer exists.
                    ctx.registry.files.record(&ctx.session_id, move_target);
                    ctx.registry.files.forget(&ctx.session_id, &change.target);
                    applied.push(("update", move_resource.clone()));
                }
                None => {
                    if let Err(error) = write_change(change) {
                        return failure(
                            format!("Failed to write {}", change.resource),
                            cause_of(&error, &change.target),
                            &applied,
                        );
                    }
                    ctx.registry.files.record(&ctx.session_id, &change.target);
                    applied.push(("update", change.resource.clone()));
                }
            },
            _ => {
                if let Err(error) = write_change(change) {
                    return failure(
                        format!("Failed to write {}", change.resource),
                        cause_of(&error, &change.target),
                        &applied,
                    );
                }
                ctx.registry.files.record(&ctx.session_id, &change.target);
                applied.push((change.kind, change.resource.clone()));
            }
        }
    }

    // ---- model content (upstream `toModelContent`) ----
    let mut content = vec!["Success. Updated the following files:".to_owned()];
    for (kind, resource) in &applied {
        content.push(format!(
            "{} {resource}",
            match *kind {
                "add" => "A",
                "delete" => "D",
                _ => "M",
            }
        ));
    }
    // Diffs come from the prepared changes, so the client sees the same
    // before/after pair that was written.
    let files: Vec<Diff::FileDiff> = prepared.iter().map(Prepared::diff).collect();
    ok_text(content.join("\n")).with_metadata(json!({ "files": files }))
}

// ---------------------------------------------------------------------------
// Parser (upstream Patch.parse)
// ---------------------------------------------------------------------------

/// The full patch envelope: boundaries, Environment ID, heredoc stripping.
fn parse(patch_text: &str) -> Result<Vec<Hunk>, String> {
    let lines: Vec<String> = strip_heredoc(patch_text.trim())
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    let first = lines.first().map(String::as_str).unwrap_or_default();
    let last = lines.last().map(String::as_str).unwrap_or_default();
    let begin = if first.trim() == "*** Begin Patch" {
        0
    } else {
        -1
    };
    let end = if last.trim() == "*** End Patch" {
        lines.len() as isize - 1
    } else {
        -1
    };
    if begin == -1 {
        return Err("The first line of the patch must be '*** Begin Patch'".to_owned());
    }
    if end == -1 || begin >= end {
        return Err("The last line of the patch must be '*** End Patch'".to_owned());
    }
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut index = begin + 1;
    while index < end {
        let header = lines[index as usize].trim().to_owned();
        if index == begin + 1
            && let Some(rest) = header.strip_prefix("*** Environment ID:")
            && !rest.trim().is_empty()
        {
            index += 1;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let path = path.trim().to_owned();
            let (content, next) = parse_add(&lines, index + 1, end, &path)?;
            hunks.push(Hunk::Add {
                path,
                contents: content,
            });
            index = next;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Delete File: ") {
            let path = path.trim().to_owned();
            if index + 1 < end {
                let next = lines[(index + 1) as usize].trim().to_owned();
                if is_boundary(&next) {
                    return Err(invalid_hunk(&next, index + 2, None));
                }
                return Err(invalid_hunk(
                    &next,
                    index + 2,
                    Some(format!(
                        "Unexpected line after Delete File '{path}': '{next}'. Delete hunks do not contain body lines"
                    )),
                ));
            }
            hunks.push(Hunk::Delete { path });
            index += 1;
            continue;
        }
        if let Some(path) = header.strip_prefix("*** Update File: ") {
            let path = path.trim().to_owned();
            let mut next = index + 1;
            while lines[next as usize].trim_end() == "*** End of File" {
                next += 1;
            }
            let move_line = lines[next as usize].trim_end().to_owned();
            let move_path = if move_line == "*** Move to:" || move_line.starts_with("*** Move to: ")
            {
                let move_path = move_line["*** Move to: ".len()..].trim().to_owned();
                if move_path.is_empty() {
                    return Err(invalid_hunk(
                        lines[next as usize].trim(),
                        next + 1,
                        Some(format!("Move destination for '{path}' must not be empty")),
                    ));
                }
                next += 1;
                Some(move_path)
            } else {
                None
            };
            let (chunks, next) = parse_update(&lines, next, end, &path, index)?;
            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
            index = next;
            continue;
        }
        return Err(invalid_hunk(&header, index + 1, None));
    }
    Ok(hunks)
}

/// Upstream `InvalidHunkError.message`.
fn invalid_hunk(line: &str, line_number: isize, reason: Option<String>) -> String {
    match reason {
        Some(reason) => format!("Invalid hunk at line {line_number}: {reason}"),
        None => format!(
            "Invalid hunk at line {line_number}: '{line}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
        ),
    }
}

/// Create parent directories and write the prepared content.
fn write_change(change: &Prepared) -> std::io::Result<()> {
    if let Some(parent) = change.target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = change.content.as_deref().unwrap_or_default();
    std::fs::write(&change.target, content.as_bytes())
}

fn is_boundary(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn parse_add(
    lines: &[String],
    start: isize,
    end: isize,
    path: &str,
) -> Result<(String, isize), String> {
    let mut content: Vec<&str> = Vec::new();
    let mut index = start;
    while index < end && !is_boundary(lines[index as usize].trim()) {
        let line = &lines[index as usize];
        let Some(plus) = line.strip_prefix('+') else {
            let trimmed = line.trim();
            return Err(invalid_hunk(
                trimmed,
                index + 1,
                Some(format!(
                    "Invalid Add File line for '{path}': expected a line starting with '+', got '{trimmed}'"
                )),
            ));
        };
        content.push(plus);
        index += 1;
    }
    Ok((content.join("\n"), index))
}

fn parse_update(
    lines: &[String],
    start: isize,
    end: isize,
    path: &str,
    hunk: isize,
) -> Result<(Vec<Chunk>, isize), String> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut index = start;
    let mut after_end_of_file = false;
    while index < end {
        let line = &lines[index as usize];
        let update_line = line.trim_end();
        if after_end_of_file {
            if update_line.is_empty() {
                index += 1;
                continue;
            }
            if update_line == "@@" || update_line.starts_with("@@ ") {
                after_end_of_file = false;
            } else if is_boundary(update_line) {
                break;
            } else {
                return Err(invalid_hunk(
                    line,
                    index + 1,
                    Some(format!(
                        "Expected update hunk to start with a @@ context marker, got: '{line}'"
                    )),
                ));
            }
        }
        if update_line == "*** End of File" {
            if let Some(chunk) = chunks.last_mut()
                && chunk.old_lines.is_empty()
                && chunk.new_lines.is_empty()
            {
                return Err(invalid_hunk(
                    update_line,
                    index + 1,
                    Some("Update hunk does not contain any lines".to_owned()),
                ));
            }
            if let Some(chunk) = chunks.last_mut() {
                chunk.end_of_file = true;
                after_end_of_file = true;
            }
            index += 1;
            continue;
        }
        if is_boundary(update_line) {
            break;
        }
        if update_line == "@@" || update_line.starts_with("@@ ") {
            if let Some(chunk) = chunks.last()
                && chunk.old_lines.is_empty()
                && chunk.new_lines.is_empty()
            {
                return Err(invalid_hunk(
                    line,
                    index + 1,
                    Some(format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    )),
                ));
            }
            chunks.push(Chunk {
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                context: (update_line != "@@").then(|| update_line["@@ ".len()..].to_owned()),
                end_of_file: false,
            });
            index += 1;
            continue;
        }
        if chunks.is_empty() {
            chunks.push(Chunk {
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                context: None,
                end_of_file: false,
            });
        }
        let tail = chunks.len() - 1;
        let chunk = &mut chunks[tail];
        if line.is_empty() {
            chunk.old_lines.push(String::new());
            chunk.new_lines.push(String::new());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            chunk.old_lines.push(rest.to_owned());
            chunk.new_lines.push(rest.to_owned());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            chunk.old_lines.push(rest.to_owned());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            chunk.new_lines.push(rest.to_owned());
            index += 1;
            continue;
        }
        let populated = !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty();
        return Err(if populated {
            invalid_hunk(
                line,
                index + 1,
                Some(format!(
                    "Expected update hunk to start with a @@ context marker, got: '{line}'"
                )),
            )
        } else {
            invalid_hunk(
                line,
                index + 1,
                Some(format!(
                    "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                )),
            )
        });
    }
    if chunks.is_empty() {
        return Err(invalid_hunk(
            lines[hunk as usize].trim(),
            hunk + 1,
            Some(format!("Update file hunk for path '{path}' is empty")),
        ));
    }
    if chunks
        .last()
        .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty())
    {
        let line = lines[index as usize].trim().to_owned();
        let reason = if line == "*** End Patch" {
            "Update hunk does not contain any lines".to_owned()
        } else {
            format!(
                "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
            )
        };
        return Err(invalid_hunk(&line, index + 1, Some(reason)));
    }
    Ok((chunks, index))
}

/// Upstream `stripHeredoc`: unwrap a `<<DELIM` / `cat <<DELIM` wrapper.
fn strip_heredoc(input: &str) -> &str {
    let Some((first, rest)) = input.split_once('\n') else {
        return input;
    };
    let after_cat = first
        .strip_prefix("cat")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim_start)
        .unwrap_or(first);
    let Some(opener) = after_cat.strip_prefix("<<") else {
        return input;
    };
    let chars: Vec<char> = opener.chars().collect();
    let mut index = 0;
    let mut quote = '\0';
    if index < chars.len() && (chars[index] == '\'' || chars[index] == '"') {
        quote = chars[index];
        index += 1;
    }
    let word_start = index;
    while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
        index += 1;
    }
    if index == word_start {
        return input;
    }
    let delimiter: String = chars[word_start..index].iter().collect();
    if quote != '\0' {
        if index < chars.len() && chars[index] == quote {
            index += 1;
        } else {
            return input;
        }
    }
    if !chars[index..].iter().all(|c| c.is_whitespace()) {
        return input;
    }
    let Some(last_break) = rest.rfind('\n') else {
        return input;
    };
    let last_line = &rest[last_break + 1..];
    if last_line.trim_end() != delimiter {
        return input;
    }
    &rest[..last_break]
}

// ---------------------------------------------------------------------------
// Derivation (upstream Patch.derive)
// ---------------------------------------------------------------------------

/// Apply the chunks to `original` (upstream `derive`): returns the new text
/// and whether a BOM must be kept.
fn derive(path: &str, chunks: &[Chunk], original: &str) -> Result<(String, bool), String> {
    let (source_bom, source_text) = split_bom(original);
    let mut lines: Vec<String> = source_text.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut replacements = compute_replacements(&lines, path, chunks)?;
    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, remove, insert) in replacements.into_iter().rev() {
        lines.splice(start..start + remove, insert);
    }
    if lines.last().is_some_and(|line| !line.is_empty()) || lines.is_empty() {
        lines.push(String::new());
    }
    let joined = lines.join("\n");
    let (next_bom, next_text) = split_bom(&joined);
    Ok((next_text.to_owned(), source_bom || next_bom))
}

/// Upstream `Bom.split`.
fn split_bom(text: &str) -> (bool, &str) {
    match text.strip_prefix('\u{FEFF}') {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

type Replacement = (usize, usize, Vec<String>);

fn compute_replacements(
    lines: &[String],
    path: &str,
    chunks: &[Chunk],
) -> Result<Vec<Replacement>, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0usize;
    for chunk in chunks {
        if let Some(context) = &chunk.context {
            let found = seek(lines, std::slice::from_ref(context), line_index, false);
            if found == usize::MAX {
                return Err(format!("Failed to find context '{context}' in {path}"));
            }
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let mut old_lines = chunk.old_lines.clone();
        let mut new_lines = chunk.new_lines.clone();
        let mut found = seek(lines, &old_lines, line_index, chunk.end_of_file);
        if found == usize::MAX && chunk.old_lines.last().is_some_and(String::is_empty) {
            old_lines.pop();
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines.pop();
            }
            found = seek(lines, &old_lines, line_index, chunk.end_of_file);
        }
        if found == usize::MAX && chunk.old_lines.iter().all(String::is_empty) {
            let expected = if chunk.old_lines.len() == 1 {
                "an expected blank line".to_owned()
            } else {
                format!("{} consecutive blank lines", chunk.old_lines.len())
            };
            return Err(format!("Failed to find {expected} in {path}"));
        }
        if found == usize::MAX {
            return Err(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ));
        }
        replacements.push((found, old_lines.len(), new_lines));
        line_index = found + old_lines.len();
    }
    Ok(replacements)
}

/// Upstream `seek`; returns `usize::MAX` instead of -1.
fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> usize {
    if pattern.is_empty() {
        return usize::MAX;
    }
    if eof {
        let offset = lines.len().saturating_sub(pattern.len());
        if offset < start {
            return usize::MAX;
        }
        for compare in [exact, rstrip, trim, normalized] {
            if matches(lines, pattern, offset, compare) {
                return offset;
            }
        }
        return usize::MAX;
    }
    for compare in [exact, rstrip, trim, normalized] {
        for offset in start..=lines.len().saturating_sub(pattern.len()) {
            if matches(lines, pattern, offset, compare) {
                return offset;
            }
        }
    }
    usize::MAX
}

fn matches(
    lines: &[String],
    pattern: &[String],
    offset: usize,
    compare: fn(&str, &str) -> bool,
) -> bool {
    pattern
        .iter()
        .enumerate()
        .all(|(index, line)| compare(&lines[offset + index], line))
}

fn exact(left: &str, right: &str) -> bool {
    left == right
}

fn rstrip(left: &str, right: &str) -> bool {
    left.trim_end() == right.trim_end()
}

fn trim(left: &str, right: &str) -> bool {
    left.trim() == right.trim()
}

fn normalized(left: &str, right: &str) -> bool {
    let left: Vec<char> = super::normalize_typographic(&left.trim().chars().collect::<Vec<_>>());
    let right: Vec<char> = super::normalize_typographic(&right.trim().chars().collect::<Vec<_>>());
    left == right
}
