//! Port of upstream `packages/core/src/tool/plugin/grep.ts`.
//!
//! Matching and searching use ripgrep's own library crates —
//! `grep-regex` (the regex matcher) and `grep-searcher` (the line searcher)
//! — and directory traversal uses the `ignore` crate. Together these are the
//! exact stack ripgrep itself is built from, so match semantics, line
//! splitting, binary detection, and encoding handling all come from
//! ripgrep's engines rather than a reimplementation.
//!
//! The tool's observable contract is unchanged: the upstream name,
//! model-facing description, parameter schema, error strings, and the
//! `FileSystem.Match`-shaped `toModelContent` output are byte-for-byte what
//! the previous line-scanner produced. One presentation quirk is kept
//! verbatim: upstream keeps rg's line text with its trailing newline, so
//! each match line renders followed by a blank line.

use globset::{Glob, GlobSetBuilder};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;

use super::glob::{DEFAULT_SEARCH_LIMIT, DEFAULT_SEARCH_TIMEOUT};
use super::{
    fail, int_param, object_param, ok_text, optional_bool, optional_nonneg_int,
    optional_positive_int, optional_str, param, required_str, resolve_path,
};

/// Upstream truncates ripgrep line text at 2 000 characters.
const MAX_LINE_TEXT: usize = 2_000;
/// Context is a reading aid, not a paging mechanism: cap it so one match
/// cannot pull a whole file into the result.
const MAX_CONTEXT_LINES: usize = 10;

/// What the caller wants back (upstream `FileSystem.GrepInput.output_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    /// Matching lines with line numbers.
    Content,
    /// One path per file that matched.
    Files,
    /// One `path:count` line per file that matched.
    Count,
}

fn output_mode(input: &Value) -> Result<OutputMode, ToolOutput> {
    match input.get("output_mode") {
        None | Some(Value::Null) => Ok(OutputMode::Content),
        Some(Value::String(mode)) => match mode.as_str() {
            "content" => Ok(OutputMode::Content),
            "files" => Ok(OutputMode::Files),
            "count" => Ok(OutputMode::Count),
            other => Err(super::invalid_arguments(
                "grep",
                &[(
                    "output_mode",
                    format!("Expected one of \"content\" | \"files\" | \"count\", got \"{other}\""),
                )],
                input,
            )),
        },
        Some(other) => Err(super::invalid_arguments(
            "grep",
            &[(
                "output_mode",
                format!("Expected one of \"content\" | \"files\" | \"count\", got {other}"),
            )],
            input,
        )),
    }
}

struct GrepMatch {
    /// Absolute path, as the model content resolves against the session
    /// directory.
    path: String,
    line: usize,
    /// Line text including the trailing newline (rg `lines.text`).
    text: String,
    /// A context line (`before_context`/`after_context`), not a match. Only
    /// counted separately so truncation and counts never include it.
    is_context: bool,
}

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "grep",
            "Search file contents using ripgrep's regular expression syntax or literal text \
             matching. Use it to locate specific code, symbols, or text patterns, and narrow \
             searches with `path` or `include`. Returns matching file paths, line numbers, and \
             line previews.\n\n\
             When to use: find an exact symbol, string, TODO, or usage site.\n\
             When NOT to use: regex is not supported with literal=true; avoid unknown-concept \
             exploration, filename lookup (use glob), reading a known path (use read), and \
             shell grep or rg. Do not repeat an equivalent search after one only found a \
             definition: the callers may simply not exist, so report that instead.",
            object_param(
                vec![
                    (
                        "pattern",
                        param(
                            "string",
                            "Regular expression or literal text to match in file contents.",
                        ),
                    ),
                    (
                        "path",
                        param(
                            "string",
                            "File or directory to search. Defaults to the current working directory.",
                        ),
                    ),
                    (
                        "include",
                        param(
                            "string",
                            "Glob pattern to filter files (for example, \"*.js\" or \"*.{ts,tsx}\")",
                        ),
                    ),
                    (
                        "literal",
                        param(
                            "boolean",
                            "Treat `pattern` as exact text instead of a regular expression (default: false).",
                        ),
                    ),
                    (
                        "caseSensitive",
                        param("boolean", "Use case-sensitive matching (default: true)."),
                    ),
                    (
                        "output_mode",
                        json!({
                            "type": "string",
                            "enum": ["content", "files", "count"],
                            "description": "content shows matching lines (default); files lists only the matching paths; count reports matches per file.",
                        }),
                    ),
                    (
                        "context_lines",
                        int_param(
                            0,
                            "Unchanged lines to show before and after each match, 0-10 (default: 0). Ignored unless output_mode=content.",
                        ),
                    ),
                    (
                        "limit",
                        int_param(1, "Maximum number of results to return (default: 100)"),
                    ),
                    (
                        "offset",
                        int_param(
                            0,
                            "Results to skip before returning any, for paging (default: 0)",
                        ),
                    ),
                ],
                &["pattern"],
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
    let pattern = match required_str("grep", input, "pattern") {
        Ok(pattern) => pattern,
        Err(output) => return output,
    };
    if pattern.is_empty() {
        return super::invalid_arguments(
            "grep",
            &[("pattern", "Pattern must not be empty".to_owned())],
            input,
        );
    }
    let path = match optional_str("grep", input, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let include = match optional_str("grep", input, "include") {
        Ok(include) => include,
        Err(output) => return output,
    };
    let literal = match optional_bool("grep", input, "literal") {
        Ok(value) => value.unwrap_or(false),
        Err(output) => return output,
    };
    let case_sensitive = match optional_bool("grep", input, "caseSensitive") {
        Ok(value) => value.unwrap_or(true),
        Err(output) => return output,
    };
    let limit = match optional_positive_int("grep", input, "limit") {
        Ok(value) => value.unwrap_or(DEFAULT_SEARCH_LIMIT) as usize,
        Err(output) => return output,
    };
    let offset = match optional_nonneg_int("grep", input, "offset") {
        Ok(value) => value.unwrap_or(0) as usize,
        Err(output) => return output,
    };
    let mode = match output_mode(input) {
        Ok(mode) => mode,
        Err(output) => return output,
    };
    let context_lines = match optional_nonneg_int("grep", input, "context_lines") {
        Ok(value) => (value.unwrap_or(0) as usize).min(MAX_CONTEXT_LINES),
        Err(output) => return output,
    };
    let target = resolve_path(&ctx.directory, path.unwrap_or("."));
    let Ok(metadata) = std::fs::metadata(&target) else {
        return fail(format!(
            "Search path does not exist: {}",
            path.unwrap_or(".")
        ));
    };
    let is_file = metadata.is_file();
    if !is_file && !metadata.is_dir() {
        return fail(format!(
            "Search path does not exist: {}",
            path.unwrap_or(".")
        ));
    }
    // rg runs with `cwd` = the directory and the file argument when a file
    // is targeted, so result paths stay relative to the directory.
    let cwd = if is_file {
        target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        target.clone()
    };
    // `grep-regex` is ripgrep's own matcher: single-line mode is selected by
    // setting the line terminator, `fixed_strings` is `--fixed-strings`, and
    // case folding follows the same builder as `-i`.
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .fixed_strings(literal)
        .line_terminator(Some(b'\n'))
        .build(pattern)
    {
        Ok(matcher) => matcher,
        Err(error) => return fail(format!("Invalid regex pattern: {error}")),
    };
    let include_set = match include {
        Some(include) => match Glob::new(include) {
            Ok(glob) => match GlobSetBuilder::new().add(glob).build() {
                Ok(set) => Some(set),
                Err(_) => return fail(format!("Unable to grep for {pattern}")),
            },
            Err(_) => return fail(format!("Unable to grep for {pattern}")),
        },
        None => None,
    };
    // `grep-searcher` with line numbers on and binary detection that quits at
    // the first NUL — the same defaults rg uses for `--json` line matches.
    // Context needs both a before/after width and line numbers to render.
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .before_context(if mode == OutputMode::Content {
            context_lines
        } else {
            0
        })
        .after_context(if mode == OutputMode::Content {
            context_lines
        } else {
            0
        })
        .build();
    let mut matches: Vec<GrepMatch> = Vec::new();
    let mut timed_out = false;
    let started = Instant::now();
    // Collect `offset + limit + 1` so the page can start at `offset` and the
    // result still knows whether anything follows it.
    let collect = offset.saturating_add(limit).saturating_add(1);
    if is_file {
        grep_file(
            &mut searcher,
            &matcher,
            &target,
            &cwd,
            collect,
            &mut matches,
        );
    } else {
        // rg grep passes `--hidden` unconditionally; `.git` stays excluded.
        for entry in WalkBuilder::new(&target)
            .hidden(false)
            .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != OsStr::new(".git"))
            .build()
        {
            if started.elapsed() > DEFAULT_SEARCH_TIMEOUT {
                timed_out = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            if let Some(include_set) = &include_set {
                let Ok(relative) = entry.path().strip_prefix(&target) else {
                    continue;
                };
                if !include_set.is_match(relative) {
                    continue;
                }
            }
            grep_file(
                &mut searcher,
                &matcher,
                entry.path(),
                &cwd,
                collect,
                &mut matches,
            );
            if matches.len() > collect {
                break;
            }
        }
    }
    if timed_out && matches.len() <= collect {
        return fail(
            "Search timed out after 30 seconds. Consider using a more specific path or pattern.",
        );
    }
    let truncated = matches.len() > offset.saturating_add(limit);
    matches.truncate(offset.saturating_add(limit).min(matches.len()));
    let page: Vec<GrepMatch> = matches.into_iter().skip(offset).collect();
    // `Grep`'s count is what makes the client title read "(N matches)".
    let count = page.len();
    let text = match mode {
        OutputMode::Content => model_content(&page, truncated),
        OutputMode::Files => files_content(&page, truncated),
        OutputMode::Count => count_content(&page, truncated),
    };
    ok_text(text).with_metadata(json!({
        "matches": count,
        "truncated": truncated,
        "mode": match mode {
            OutputMode::Content => "content",
            OutputMode::Files => "files",
            OutputMode::Count => "count",
        },
    }))
}

/// `output_mode=files`: one path per matching file, in first-match order.
fn files_content(matches: &[GrepMatch], truncated: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current: &str = "";
    for matcher in matches {
        if current != matcher.path.as_str() {
            current = &matcher.path;
            lines.push(matcher.path.clone());
        }
    }
    if lines.is_empty() {
        return "No matches found".to_owned();
    }
    if truncated {
        lines.push(String::new());
        lines.push(
            "(Results are truncated. Consider using a more specific path or pattern.)".to_owned(),
        );
    }
    lines.join("\n")
}

/// `output_mode=count`: `path:count` per matching file.
fn count_content(matches: &[GrepMatch], truncated: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current: Option<(&str, usize)> = None;
    for matcher in matches {
        match current.as_mut() {
            Some((path, count)) if *path == matcher.path.as_str() => *count += 1,
            _ => {
                if let Some((path, count)) = current.take() {
                    lines.push(format!("{path}:{count}"));
                }
                current = Some((matcher.path.as_str(), 1));
            }
        }
    }
    if let Some((path, count)) = current {
        lines.push(format!("{path}:{count}"));
    }
    if lines.is_empty() {
        return "No matches found".to_owned();
    }
    if truncated {
        lines.push(String::new());
        lines.push(
            "(Results are truncated. Consider using a more specific path or pattern.)".to_owned(),
        );
    }
    lines.join("\n")
}

/// Collects one file's matching lines through ripgrep's searcher.
struct CollectSink<'a> {
    /// Absolute path recorded on every match for this file.
    absolute: String,
    /// Stop once this many matches have been collected (+1 for truncation
    /// detection).
    limit: usize,
    matches: &'a mut Vec<GrepMatch>,
}

impl Sink for CollectSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let line = mat.line_number().unwrap_or(0) as usize;
        self.matches.push(GrepMatch {
            path: self.absolute.clone(),
            line,
            text: render_line(mat.bytes()),
            is_context: false,
        });
        // Keep searching while at or below the limit; stop after the first
        // match past it (so `truncated` can be reported).
        Ok(self.matches.len() <= self.limit)
    }

    /// One context line the searcher decided to include. It never counts as a
    /// match, so the limit is unaffected.
    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &grep_searcher::SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let line = context.line_number().unwrap_or(0) as usize;
        self.matches.push(GrepMatch {
            path: self.absolute.clone(),
            line,
            text: render_line(context.bytes()),
            is_context: true,
        });
        Ok(true)
    }
}

/// Run one file through the searcher. `cwd` mirrors the rg invocation's
/// working directory for path resolution; I/O and decode errors skip the
/// file (matching rg's `--no-messages` silence).
fn grep_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
    cwd: &Path,
    limit: usize,
    matches: &mut Vec<GrepMatch>,
) {
    let absolute = cwd.join(path);
    let mut sink = CollectSink {
        absolute: absolute.to_string_lossy().into_owned(),
        limit,
        matches,
    };
    let _ = searcher.search_path(matcher, path, &mut sink);
}

/// Render one matched line's bytes as model text, capped at
/// [`MAX_LINE_TEXT`] characters with a trailing `...` when truncated.
fn render_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.chars().count() > MAX_LINE_TEXT {
        let truncated: String = text.chars().take(MAX_LINE_TEXT).collect();
        format!("{truncated}...")
    } else {
        text.into_owned()
    }
}

/// Upstream `toModelContent`: file-grouped, line-numbered previews. Context
/// lines are already interleaved by the searcher (they arrive as their own
/// matches with `is_context` set), so this only chooses the prefix.
fn model_content(matches: &[GrepMatch], truncated: bool) -> String {
    let mut lines: Vec<String> = if matches.is_empty() {
        vec!["No matches found".to_owned()]
    } else {
        vec![format!("Found {} matches", matches.len())]
    };
    let mut current = String::new();
    for matcher in matches {
        if current != matcher.path {
            if !current.is_empty() {
                lines.push(String::new());
            }
            current = matcher.path.clone();
            lines.push(format!("{}:", matcher.path));
        }
        // Match lines keep upstream's exact prefix; context lines add a `-`
        // so a reader can tell them apart from matches.
        if matcher.is_context {
            lines.push(format!("  Line {}: -{}", matcher.line, matcher.text));
        } else {
            lines.push(format!("  Line {}: {}", matcher.line, matcher.text));
        }
    }
    if truncated {
        lines.push(String::new());
        lines.push(format!(
            "(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)",
            matches.len()
        ));
    }
    lines.join("\n")
}
