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
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};

use super::glob::{DEFAULT_SEARCH_LIMIT, DEFAULT_SEARCH_TIMEOUT};
use super::{
    fail, int_param, object_param, ok_text, optional_bool, optional_positive_int, optional_str,
    param, required_str, resolve_path,
};

/// Upstream truncates ripgrep line text at 2 000 characters.
const MAX_LINE_TEXT: usize = 2_000;

struct GrepMatch {
    /// Absolute path, as the model content resolves against the session
    /// directory.
    path: String,
    line: usize,
    /// Line text including the trailing newline (rg `lines.text`).
    text: String,
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
             line previews.",
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
                        "limit",
                        int_param(
                            1,
                            "Maximum number of matching lines to return (default: 100)",
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
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut matches: Vec<GrepMatch> = Vec::new();
    let mut timed_out = false;
    let started = Instant::now();
    if is_file {
        grep_file(&mut searcher, &matcher, &target, &cwd, limit, &mut matches);
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
                limit,
                &mut matches,
            );
            if matches.len() > limit {
                break;
            }
        }
    }
    if timed_out && matches.len() <= limit {
        return fail(
            "Search timed out after 30 seconds. Consider using a more specific path or pattern.",
        );
    }
    let truncated = matches.len() > limit;
    matches.truncate(limit);
    ok_text(model_content(&matches, truncated))
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
        });
        // Keep searching while at or below the limit; stop after the first
        // match past it (so `truncated` can be reported).
        Ok(self.matches.len() <= self.limit)
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

/// Upstream `toModelContent`: file-grouped, line-numbered previews. The
/// line text keeps rg's trailing newline (hence the blank line after each).
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
        lines.push(format!("  Line {}: {}", matcher.line, matcher.text));
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

    async fn grep_tool(directory: &tempfile::TempDir, input: Value) -> ToolOutput {
        GrepTool.execute(ctx(directory), input).await
    }

    #[tokio::test]
    async fn finds_matches_with_line_numbers() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("a.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\n",
        )
        .unwrap();
        let output = grep_tool(&directory, json!({"pattern": "fn two"})).await;
        let path = directory.path().join("a.rs");
        assert_eq!(
            output.content,
            json!(format!(
                "Found 1 matches\n{}:\n  Line 2: fn two() {{}}\n",
                path.display()
            ))
        );
    }

    #[tokio::test]
    async fn include_filter_and_case_sensitivity() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.ts"), "const VALUE = 1\n").unwrap();
        fs::write(directory.path().join("b.rs"), "let VALUE = 1\n").unwrap();
        let output = grep_tool(
            &directory,
            json!({"pattern": "value", "include": "*.ts", "caseSensitive": false}),
        )
        .await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("a.ts"));
        assert!(!text.contains("b.rs"));
        let output = grep_tool(
            &directory,
            json!({"pattern": "value", "include": "*.ts", "caseSensitive": true}),
        )
        .await;
        assert_eq!(output.content, json!("No matches found"));
    }

    #[tokio::test]
    async fn literal_mode_escapes_regex() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "a.b\naxb\n").unwrap();
        // As a regex, `a.b` matches both lines.
        let output = grep_tool(&directory, json!({"pattern": "a.b"})).await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("  Line 1: a.b"));
        assert!(text.contains("  Line 2: axb"));
        // As a literal, only the exact `a.b` line matches.
        let output = grep_tool(&directory, json!({"pattern": "a.b", "literal": true})).await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("  Line 1: a.b"));
        assert!(!text.contains("Line 2"));
    }

    #[tokio::test]
    async fn limit_truncates_with_marker() {
        let directory = tempdir().unwrap();
        let body: String = (0..5).map(|index| format!("hit{index}\n")).collect();
        fs::write(directory.path().join("a.txt"), body).unwrap();
        let output = grep_tool(&directory, json!({"pattern": "hit", "limit": 2})).await;
        let text = output.content.as_str().unwrap();
        assert!(text.starts_with("Found 2 matches"));
        assert!(text.contains("(Results are truncated: showing first 2 results."));
    }

    #[tokio::test]
    async fn invalid_pattern_and_invalid_arguments() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "x\n").unwrap();
        let output = grep_tool(&directory, json!({"pattern": "(unclosed"})).await;
        assert!(
            output
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("Invalid regex pattern: ")),
            "unexpected: {output:?}"
        );
        let output = grep_tool(&directory, json!({"pattern": ""})).await;
        assert!(
            output
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("Invalid arguments for tool \"grep\":"))
        );
    }
}
