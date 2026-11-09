//! Port of upstream `packages/core/src/tool/plugin/edit.ts`: exact-string
//! replacement with three match tiers (exact → typographic-variant →
//! line/trailing-whitespace), uniqueness enforcement, `replaceAll`, and
//! CRLF-preserving region replacement.
use std::collections::HashMap;

use serde_json::{Value, json};

use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::file_state::Verdict;
use ox_schema::wire::ToolDef;

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
        crate::contract::blocking(ctx, input, run)
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
    // Refuse to write over content that changed since this session read it: the
    // replaced text came from a stale view, so the edit would clobber whatever
    // replaced it.
    if ctx.registry.files.check(&ctx.session_id, &absolute) == Verdict::Changed {
        return fail(format!(
            "Failed to edit {path}: the file changed since your last read of it. Read it again before editing."
        ));
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
        return fail(miss_message(path, &text, old_string, new_string));
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
    // Commit entry: after this gate cancellation must not skip the stamp/result.
    if ctx.cancellation.is_cancelled() {
        return fail("Tool execution interrupted");
    }
    if std::fs::write(&absolute, output_text.as_bytes()).is_err() {
        return fail(format!("Unable to edit {path}"));
    }
    ctx.registry.files.record(&ctx.session_id, &absolute);
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

/// The LF view Read shows the model: CRLF pairs collapse to LF, a lone `\r`
/// stays (upstream `replaceAll(crlf, "\n")`).
fn lf_view(chars: impl Iterator<Item = char>) -> Chars {
    let mut view: Chars = Vec::with_capacity(chars.size_hint().0);
    let mut iter = chars.peekable();
    while let Some(c) = iter.next() {
        if c == '\r' && iter.peek() == Some(&'\n') {
            continue;
        }
        view.push(c);
    }
    view
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
    let file_ending = line_ending(source, "\n");
    let search = lf_view(old_string.iter().copied());
    let replacement = lf_view(new_string.iter().copied());
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

/// JS whitespace: Unicode whitespace plus the byte-order mark.
fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

/// JS `trimEnd`: JS whitespace without the leading side.
fn js_trim_end(line: &[char]) -> &[char] {
    let mut end = line.len();
    while end > 0 && is_js_space(line[end - 1]) {
        end -= 1;
    }
    &line[..end]
}

/// JS `trim`: JS whitespace on both ends.
fn js_trim(line: &[char]) -> &[char] {
    let mut start = 0;
    let mut end = line.len();
    while start < end && is_js_space(line[start]) {
        start += 1;
    }
    while end > start && is_js_space(line[end - 1]) {
        end -= 1;
    }
    &line[start..end]
}

/// Upstream `missHint`'s display budget: at most this many lines of the file,
/// each clipped to this many characters (upstream counts UTF-16 code units,
/// this port counts characters, like the rest of the offset handling here).
const HINT_LINES: usize = 12;
const HINT_LINE_LENGTH: usize = 200;
/// Upper bound on the (oldString line, file line) votes one hint may cast, so a
/// file that repeats those lines cannot turn the diagnostic into a quadratic
/// scan. Over budget the hint is dropped; the failure message is unaffected.
const VOTE_BUDGET: usize = 1 << 20;

/// The 0-match failure message: upstream's sentence, plus the miss hint when
/// one is found (upstream appends `\n\n${hint}`). Without a hint the wording
/// stays exactly what it was before the hint existed.
fn miss_message(path: &str, source: &str, old_string: &str, new_string: &str) -> String {
    let base = format!(
        "Could not find oldString in {path}. It must match exactly, including whitespace and indentation."
    );
    match miss_hint(source, old_string, new_string) {
        Some(hint) => format!("{base}\n\n{hint}"),
        None => base,
    }
}

/// A hint for an oldString miss, so the next attempt can succeed without
/// rereading the file: either newString is already there (often an edit that
/// already applied), or the lines closest to oldString numbered like Read
/// output. Lines vote for where oldString starts when they match ignoring all
/// whitespace; at least half of oldString's content lines must agree. Only the
/// span from the first to the last line that matched is shown, so padding
/// oldString cannot reveal the lines after it (upstream `missHint`; its
/// `readable` permission gate is omitted because ox has no permission system).
fn miss_hint(source: &str, old_string: &str, new_string: &str) -> Option<String> {
    let view = lf_view(source.chars());
    let replacement = lf_view(new_string.chars());
    if js_trim(&replacement).len() >= 20
        && !find_occurrences(
            &normalize_typographic(&view),
            &normalize_typographic(&replacement),
        )
        .is_empty()
    {
        return Some(
            "newString is already in the file, so this edit may already be applied.".to_owned(),
        );
    }
    // Borrow the lines out of `view`: the hint path runs once per failed edit,
    // and copying every line again would double the file's transient footprint.
    let lines: Vec<&[char]> = view.split(|&c| c == '\n').collect();
    let search = lf_view(old_string.chars());
    let keys: Vec<Chars> = search.split(|&c| c == '\n').map(hint_key).collect();
    let mut positions: HashMap<Chars, Vec<usize>> = HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        let value = hint_key(line);
        if hint_distinct(&value) {
            positions.entry(value).or_default().push(index);
        }
    }
    let mut votes: Vec<(isize, usize)> = Vec::new();
    let mut voted: HashMap<isize, usize> = HashMap::new();
    let mut wanted = 0usize;
    let mut budget = VOTE_BUDGET;
    for (offset, value) in keys.iter().enumerate() {
        if !hint_distinct(value) {
            continue;
        }
        wanted += 1;
        let Some(found) = positions.get(value) else {
            continue;
        };
        for &index in found {
            // A file that repeats oldString's content lines many times makes the
            // voting pass quadratic. The hint is best effort, so drop it instead
            // of stalling a failed edit on a pathological file.
            if budget == 0 {
                return None;
            }
            budget -= 1;
            let start = index as isize - offset as isize;
            match voted.get(&start) {
                Some(&position) => votes[position].1 += 1,
                None => {
                    voted.insert(start, votes.len());
                    votes.push((start, 1));
                }
            }
        }
    }
    // Ties keep the first start that collected the highest vote count.
    let mut best = (0isize, 0usize);
    for &(start, count) in &votes {
        if count > best.1 {
            best = (start, count);
        }
    }
    let (start, count) = best;
    if count == 0 || count * 2 < wanted {
        return None;
    }
    let mut matched: Vec<usize> = Vec::new();
    for (offset, value) in keys.iter().enumerate() {
        let Ok(index) = usize::try_from(start + offset as isize) else {
            continue;
        };
        if index < lines.len() && !value.is_empty() && &hint_key(lines[index]) == value {
            matched.push(index);
        }
    }
    let from = *matched.first()?;
    let to = (matched.last()? + 1).min(from + HINT_LINES);
    let mut hint = vec!["The closest lines in the file:".to_owned()];
    for (offset, line) in lines[from..to].iter().enumerate() {
        let mut text: String = line[..line.len().min(HINT_LINE_LENGTH)].iter().collect();
        if line.len() > HINT_LINE_LENGTH {
            text.push('…');
        }
        hint.push(format!("{}: {text}", from + offset + 1));
    }
    Some(hint.join("\n"))
}

/// Upstream `missHint`'s line `key`: typographic variants folded, every run of
/// whitespace collapsed to one space, then trimmed.
fn hint_key(line: &[char]) -> Chars {
    let mut key: Chars = Vec::with_capacity(line.len());
    let mut pending_space = false;
    for c in normalize_typographic(line) {
        if is_js_space(c) {
            pending_space = !key.is_empty();
            continue;
        }
        if pending_space {
            key.push(' ');
            pending_space = false;
        }
        key.push(c);
    }
    key
}

/// Upstream `missHint`'s `distinct`: the line carries at least one Unicode
/// letter or digit, so whitespace- and punctuation-only lines neither vote nor
/// count towards the threshold (`\p{L}\p{N}` approximated by
/// `char::is_alphanumeric`).
fn hint_distinct(key: &[char]) -> bool {
    key.iter().any(|c| c.is_alphanumeric())
}
