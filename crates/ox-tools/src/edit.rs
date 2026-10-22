//! Port of upstream `packages/core/src/tool/plugin/edit.ts`: exact-string
//! replacement with three match tiers (exact → typographic-variant →
//! line/trailing-whitespace), uniqueness enforcement, `replaceAll`, and
//! CRLF-preserving region replacement.
use serde_json::{Value, json};

use ox_schema::wire::ToolDef;
use crate::contract::{Tool, ToolContext, ToolOutput};

use super::diff::{self as Diff, DiffStatus};
use super::{
    fail, normalize_typographic, object_param, ok_text, optional_bool, param, required_str,
    resolve_path, resource_of,
};

type Chars = Vec<char>;

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "edit",
            "Edit the contents of a file by finding and replacing exact text. When editing text \
             from Read output, preserve the exact indentation (tabs or spaces) and omit the \
             line-number prefix, such as `1: `. Never include the prefix in oldString or \
             newString. Write line breaks as \\n; the file's existing CRLF or LF line endings \
             are matched and preserved. To change line endings themselves, make oldString and \
             newString differ only in \\r; that edit applies to the exact bytes. If there is no \
             exact match, the edit retries while ignoring typographic quote, dash, and space \
             variants, then trailing whitespace on each line. The edit fails if oldString is \
             not found. By default, oldString must identify a UNIQUE location. Multiple matches \
             FAIL unless replaceAll is true. Add more surrounding context to disambiguate, or \
             set replaceAll to true to replace every occurrence. Use replaceAll when the change \
             should apply to every occurrence, such as renaming a variable.\n\n\
             When to use: make a focused change to an existing file after reading the exact \
             text you are replacing.\n\
             When NOT to use: create a file (use write), rewrite most of a file, or edit text \
             you have not read; do not guess indentation or invent line-number prefixes.",
            object_param(
                vec![
                    ("path", param("string", "File to edit")),
                    (
                        "oldString",
                        param("string", "Exact text to find and replace"),
                    ),
                    (
                        "newString",
                        param(
                            "string",
                            "Text to replace oldString with (must differ from oldString)",
                        ),
                    ),
                    (
                        "replaceAll",
                        param(
                            "boolean",
                            "Whether to replace every occurrence of oldString. When false, oldString must match exactly once. Defaults to false.",
                        ),
                    ),
                ],
                &["path", "oldString", "newString"],
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

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let path = match required_str("edit", input, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let old_string = match required_str("edit", input, "oldString") {
        Ok(text) => text,
        Err(output) => return output,
    };
    let new_string = match required_str("edit", input, "newString") {
        Ok(text) => text,
        Err(output) => return output,
    };
    let replace_all = match optional_bool("edit", input, "replaceAll") {
        Ok(value) => value == Some(true),
        Err(output) => return output,
    };
    if old_string == new_string {
        return fail("No changes to apply: oldString and newString are identical.");
    }
    if old_string.is_empty() {
        return fail("oldString must not be empty. Use write to create or overwrite a file.");
    }
    let absolute = resolve_path(&ctx.directory, path);
    let metadata = match std::fs::metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(_) => return fail(format!("File not found: {path}")),
    };
    if metadata.is_dir() {
        return fail(format!("Path is a directory, not a file: {path}"));
    }
    let bytes = match std::fs::read(&absolute) {
        Ok(bytes) => bytes,
        Err(_) => return fail(format!("Unable to edit {path}")),
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return fail(format!("Unable to edit {path}")),
    };
    let source: Chars = text.chars().collect();

    // Strings that differ only in \r ask to change line endings themselves,
    // so they apply to the exact bytes. Every other edit matches the LF view
    // Read shows the model and keeps the file's endings.
    let endings_only = strip_cr(old_string.chars()) == strip_cr(new_string.chars());
    let (matches, replacement, file_ending) = if endings_only {
        (
            find_occurrences(&source, &old_string.chars().collect::<Chars>()),
            new_string.chars().collect::<Chars>(),
            "\n".to_owned(),
        )
    } else {
        match_view(
            &source,
            &old_string.chars().collect::<Chars>(),
            &new_string.chars().collect::<Chars>(),
        )
    };
    let replacements = matches.len();
    if replacements == 0 {
        return fail(format!(
            "Could not find oldString in {path}. It must match exactly, including whitespace and indentation."
        ));
    }
    if replacements > 1 && !replace_all {
        return fail(format!(
            "Found {replacements} matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."
        ));
    }
    let chosen: Vec<(usize, usize)> = if replace_all {
        matches
    } else {
        matches.into_iter().take(1).collect()
    };
    let mut replaced = source.clone();
    for (start, end) in chosen.iter().rev() {
        let replacement_text = if endings_only {
            replacement.clone()
        } else {
            let region = &source[*start..*end];
            with_region_endings(region, &replacement, &line_ending(region, &file_ending))
        };
        replaced.splice(*start..*end, replacement_text);
    }
    if replaced == source {
        return fail(if endings_only {
            format!("No changes to apply: {path} already has those line endings.")
        } else {
            format!(
                "No changes to apply: the edit leaves {path} unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString."
            )
        });
    }
    let output_text: String = replaced.into_iter().collect();
    if std::fs::write(&absolute, output_text.as_bytes()).is_err() {
        return fail(format!("Unable to edit {path}"));
    }
    let resource = resource_of(&ctx.directory, &absolute);
    // The client renders this diff card; the model reads the text below.
    let diff = Diff::file_diff(&resource, &text, &output_text, DiffStatus::Modified);
    ok_text(format!(
        "Edited {resource} ({replacements} replacement{})",
        if replacements == 1 { "" } else { "s" }
    ))
    .with_metadata(json!({ "files": [diff] }))
}

fn strip_cr(value: impl Iterator<Item = char>) -> Chars {
    value.filter(|&c| c != '\r').collect()
}

/// Non-overlapping occurrences, advancing by the needle length
/// (upstream `findOccurrences`).
fn find_occurrences(haystack: &[char], needle: &[char]) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    if needle.is_empty() {
        return result;
    }
    let mut offset = 0;
    while offset + needle.len() <= haystack.len() {
        if &haystack[offset..offset + needle.len()] == needle {
            result.push((offset, offset + needle.len()));
            offset += needle.len();
        } else {
            offset += 1;
        }
    }
    result
}

/// One `[^\n]*(?:\n|$)` piece of the LF view.
struct Line {
    start: usize,
    end: usize,
    text: Chars,
    content_end: usize,
    newline: bool,
}

fn content_lines(content: &[char]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < content.len() {
        let end = content[start..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(content.len(), |position| start + position + 1);
        let piece = &content[start..end];
        let newline = piece.last() == Some(&'\n');
        let text_len = piece.len() - usize::from(newline);
        let text = &piece[..text_len];
        let content_end = start + text_len - usize::from(text.last() == Some(&'\r'));
        lines.push(Line {
            start,
            end,
            text: text.to_vec(),
            content_end,
            newline,
        });
        start = end;
    }
    lines
}

/// Line-based, trailing-whitespace-insensitive matches
/// (upstream `findLineOccurrences`).
fn find_line_occurrences(content: &[char], search: &[char]) -> Vec<(usize, usize)> {
    let trailing_newline = search.last() == Some(&'\n');
    let mut expected: Vec<Chars> = search.split(|&c| c == '\n').map(<[char]>::to_vec).collect();
    if trailing_newline {
        expected.pop();
    }
    let lines = content_lines(content);
    let key = |line: &Chars| normalize_typographic(js_trim_end(line));
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for index in 0..lines.len() {
        if index + expected.len() > lines.len() {
            break;
        }
        let window = &lines[index..index + expected.len()];
        if window
            .iter()
            .zip(expected.iter())
            .any(|(line, want)| key(&line.text) != key(want))
        {
            continue;
        }
        let Some(last) = window.last() else {
            continue;
        };
        if trailing_newline && !last.newline {
            continue;
        }
        let end = if trailing_newline {
            last.end
        } else {
            last.content_end
        };
        candidates.push((lines[index].start, end));
    }
    // Keep earlier candidates; drop any that overlap an accepted one.
    let mut accepted: Vec<(usize, usize)> = Vec::new();
    for candidate in candidates {
        if accepted
            .iter()
            .any(|(start, end)| *end > candidate.0 && *start < candidate.1)
        {
            continue;
        }
        accepted.push(candidate);
    }
    accepted
}

/// The dominant line ending in `text`, or `fallback` when there is none.
fn line_ending(text: &[char], fallback: &str) -> String {
    let mut crlf = 0usize;
    let mut index = 0;
    while index < text.len() {
        if text[index] == '\r' && text.get(index + 1) == Some(&'\n') {
            crlf += 1;
            index += 2;
        } else {
            index += 1;
        }
    }
    let lf = text.iter().filter(|&&c| c == '\n').count() - crlf;
    if crlf == lf {
        fallback.to_owned()
    } else if crlf > lf {
        "\r\n".to_owned()
    } else {
        "\n".to_owned()
    }
}

/// Number of sorted positions strictly before `offset` (upstream
/// `countBefore`).
fn count_before(positions: &[usize], offset: usize) -> usize {
    positions.partition_point(|&position| position < offset)
}

/// Upstream `matchView`: matches against the LF view Read shows the model,
/// then maps offsets back to the source. Returns the mapped matches, the
/// LF-form replacement text, and the file's fallback ending.
fn match_view(
    source: &[char],
    old_string: &[char],
    new_string: &[char],
) -> (Vec<(usize, usize)>, Chars, String) {
    let mut view: Chars = Vec::with_capacity(source.len());
    let mut removed: Vec<usize> = Vec::new();
    for (index, &c) in source.iter().enumerate() {
        if c == '\r' && source.get(index + 1) == Some(&'\n') {
            removed.push(index);
        } else {
            view.push(c);
        }
    }
    let lf = |value: &[char]| -> Chars {
        let mut out = Chars::new();
        let mut index = 0;
        while index < value.len() {
            if value[index] == '\r' && value.get(index + 1) == Some(&'\n') {
                index += 1;
                continue;
            }
            out.push(value[index]);
            index += 1;
        }
        out
    };
    let file_ending = line_ending(source, "\n");
    let search = lf(old_string);
    let replacement = lf(new_string);
    let exact = find_occurrences(&view, &search);
    // The 1:1 typographic mapping preserves positions in the LF view.
    let unicode = if exact.is_empty() {
        find_occurrences(
            &normalize_typographic(&view),
            &normalize_typographic(&search),
        )
    } else {
        Vec::new()
    };
    let trailing = if exact.is_empty() && unicode.is_empty() {
        find_line_occurrences(&view, &search)
    } else {
        Vec::new()
    };
    let chosen = if !exact.is_empty() {
        exact
    } else if !unicode.is_empty() {
        unicode
    } else {
        trailing
    };
    let matches = chosen
        .into_iter()
        .map(|(start, end)| {
            (
                start + count_before(&removed, start),
                end + count_before(&removed, end),
            )
        })
        .collect();
    (matches, replacement, file_ending)
}

/// Joins the replacement's lines with line endings taken from the matched
/// source region: kept lines reuse their ending, changed lines reuse the
/// ending of the line they replace, extra lines take `fallback`
/// (upstream `withRegionEndings` + `alignLines`).
fn with_region_endings(region: &[char], replacement: &[char], fallback: &str) -> Chars {
    let before: Vec<Chars> = split_region_lines(region);
    let endings: Vec<String> = region_endings(region);
    let after: Vec<Chars> = replacement
        .split(|&c| c == '\n')
        .map(<[char]>::to_vec)
        .collect();
    let key = |line: &Chars| normalize_typographic(js_trim_end(line));
    let before_keys: Vec<Chars> = before.iter().map(key).collect();
    let after_keys: Vec<Chars> = after.iter().map(key).collect();
    let aligned = align_lines(&before_keys, &after_keys);
    let mut out: Chars = Vec::new();
    for (index, line) in after.iter().enumerate() {
        out.extend_from_slice(line);
        if index != after.len() - 1 {
            let ending = aligned[index]
                .and_then(|position| endings.get(position))
                .map(String::as_str)
                .unwrap_or(fallback);
            out.extend(ending.chars());
        }
    }
    out
}

/// Split on `\r?\n`, dropping the endings.
fn split_region_lines(region: &[char]) -> Vec<Chars> {
    let mut lines: Vec<Chars> = Vec::new();
    let mut current: Chars = Vec::new();
    let mut index = 0;
    while index < region.len() {
        let c = region[index];
        if c == '\r' && region.get(index + 1) == Some(&'\n') {
            lines.push(std::mem::take(&mut current));
            index += 2;
        } else if c == '\n' {
            lines.push(std::mem::take(&mut current));
            index += 1;
        } else {
            current.push(c);
            index += 1;
        }
    }
    lines.push(current);
    lines
}

/// The line terminators of `region`, in order.
fn region_endings(region: &[char]) -> Vec<String> {
    let mut endings = Vec::new();
    let mut index = 0;
    while index < region.len() {
        if region[index] == '\r' && region.get(index + 1) == Some(&'\n') {
            endings.push("\r\n".to_owned());
            index += 2;
        } else if region[index] == '\n' {
            endings.push("\n".to_owned());
            index += 1;
        } else {
            index += 1;
        }
    }
    endings
}

/// For each `after` line, the index of the `before` line it keeps (longest
/// common subsequence) or, between kept lines, the line it takes the place
/// of; `None` for lines with no counterpart (upstream `alignLines`).
fn align_lines(before: &[Chars], after: &[Chars]) -> Vec<Option<usize>> {
    let width = after.len() + 1;
    let mut table = vec![0u32; (before.len() + 1) * width];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i * width + j] = if before[i] == after[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut result: Vec<Option<usize>> = vec![None; after.len()];
    let mut gap_before: Vec<usize> = Vec::new();
    let mut gap_after: Vec<usize> = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            flush(&mut result, &mut gap_before, &mut gap_after);
            result[j] = Some(i);
            i += 1;
            j += 1;
            continue;
        }
        if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            gap_before.push(i);
            i += 1;
        } else {
            gap_after.push(j);
            j += 1;
        }
    }
    while i < before.len() {
        gap_before.push(i);
        i += 1;
    }
    while j < after.len() {
        gap_after.push(j);
        j += 1;
    }
    flush(&mut result, &mut gap_before, &mut gap_after);
    result
}

fn flush(result: &mut [Option<usize>], gap_before: &mut Vec<usize>, gap_after: &mut Vec<usize>) {
    for (k, &j) in gap_after.iter().enumerate() {
        // Mirror upstream's JavaScript: a gap whose `after` side has more
        // lines than its `before` side maps the surplus lines to `undefined`
        // (here `None`, which the caller renders as the fallback ending).
        // Indexing `gap_before` directly panics on those surplus lines.
        result[j] = gap_before.get(k).copied();
    }
    gap_before.clear();
    gap_after.clear();
}

/// JS `trimEnd`: Unicode whitespace plus the byte-order mark.
fn js_trim_end(line: &[char]) -> &[char] {
    let mut end = line.len();
    while end > 0 && (line[end - 1].is_whitespace() || line[end - 1] == '\u{FEFF}') {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx(directory: &tempfile::TempDir) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.path().to_path_buf(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    async fn edit(directory: &tempfile::TempDir, input: Value) -> ToolOutput {
        EditTool.execute(ctx(directory), input).await
    }

    #[tokio::test]
    async fn replaces_exactly_once() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "one two one\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "one two", "newString": "three four"}),
        )
        .await;
        assert_eq!(output.content, json!("Edited a.txt (1 replacement)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "three four one\n"
        );
    }

    #[tokio::test]
    async fn multiple_matches_fail_without_replace_all() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "x\nx\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "x", "newString": "y"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some(
                "Found 2 matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."
            )
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "x\nx\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "x", "newString": "y", "replaceAll": true}),
        )
        .await;
        assert_eq!(output.content, json!("Edited a.txt (2 replacements)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "y\ny\n"
        );
    }

    #[tokio::test]
    async fn not_found_and_validation_errors() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "content\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "missing", "newString": "x"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some(
                "Could not find oldString in a.txt. It must match exactly, including whitespace and indentation."
            )
        );
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "same", "newString": "same"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("No changes to apply: oldString and newString are identical.")
        );
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "", "newString": "x"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("oldString must not be empty. Use write to create or overwrite a file.")
        );
        let output = edit(
            &directory,
            json!({"path": "gone.txt", "oldString": "x", "newString": "y"}),
        )
        .await;
        assert_eq!(output.error.as_deref(), Some("File not found: gone.txt"));
    }

    #[tokio::test]
    async fn preserves_crlf_endings_from_region() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("crlf.txt"), "alpha\r\nbeta\r\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "crlf.txt", "oldString": "alpha\nbeta", "newString": "gamma\ndelta"}),
        )
        .await;
        assert_eq!(output.content, json!("Edited crlf.txt (1 replacement)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("crlf.txt")).unwrap(),
            "gamma\r\ndelta\r\n"
        );
    }

    #[tokio::test]
    async fn matches_typographic_variants() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("q.txt"), "don\u{2019}t stop\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "q.txt", "oldString": "don't stop", "newString": "never stop"}),
        )
        .await;
        assert_eq!(output.content, json!("Edited q.txt (1 replacement)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("q.txt")).unwrap(),
            "never stop\n"
        );
    }

    #[tokio::test]
    async fn matches_trailing_whitespace_tier() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("ws.txt"), "foo   \nbar\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "ws.txt", "oldString": "foo\nbar", "newString": "baz\nqux"}),
        )
        .await;
        assert_eq!(output.content, json!("Edited ws.txt (1 replacement)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("ws.txt")).unwrap(),
            "baz\nqux\n"
        );
    }

    #[tokio::test]
    async fn byte_exact_endings_only_edits() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("e.txt"), "a\r\nb").unwrap();
        let output = edit(
            &directory,
            json!({"path": "e.txt", "oldString": "a\r\nb", "newString": "a\nb"}),
        )
        .await;
        assert_eq!(output.content, json!("Edited e.txt (1 replacement)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("e.txt")).unwrap(),
            "a\nb"
        );
    }

    #[tokio::test]
    async fn unchanged_edit_reports_no_changes() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("same.txt"), "foo  \nbar\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "same.txt", "oldString": "foo\nbar", "newString": "foo  \nbar"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some(
                "No changes to apply: the edit leaves same.txt unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString."
            )
        );
    }

    #[tokio::test]
    async fn directory_target_fails() {
        let directory = tempdir().unwrap();
        fs::create_dir_all(directory.path().join("sub")).unwrap();
        let output = edit(
            &directory,
            json!({"path": "sub", "oldString": "x", "newString": "y"}),
        )
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("Path is a directory, not a file: sub")
        );
    }

    /// Regression: a replacement that adds lines may leave a region whose
    /// `after` side has more lines than its `before` side. Upstream's JS maps
    /// those surplus lines to `undefined`; the alignment must not index past
    /// the `before` gap (previously a panic at `edit.rs` flush).
    #[tokio::test]
    async fn replacement_adding_lines_does_not_panic() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "a\nc\n").unwrap();
        let output = edit(
            &directory,
            json!({"path": "a.txt", "oldString": "a\nc", "newString": "a\nx\nc"}),
        )
        .await;
        assert_eq!(output.content, json!("Edited a.txt (1 replacement)"));
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "a\nx\nc\n"
        );
    }
}
