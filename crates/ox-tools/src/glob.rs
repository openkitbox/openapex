//! Port of upstream `packages/core/src/tool/plugin/glob.ts` (+ its
//! ripgrep `--files` adapter): pattern-based file search bounded by a
//! result limit and a search timeout.
//!
//! Uses the `ignore` crate (ripgrep's own library) with gitignore-style
//! matching from `globset`, standing in for the bundled rg binary.

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::time::{Duration, Instant};

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;

use super::{
    fail, int_param, object_param, ok_text, optional_bool, optional_positive_int, optional_str,
    param, required_str, resolve_path,
};

pub(crate) const DEFAULT_SEARCH_LIMIT: u64 = 100;
/// Upstream `FileSystem.DEFAULT_SEARCH_TIMEOUT_MS`.
pub(crate) const DEFAULT_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "glob",
            "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\").\n\n\
             When to use: locate files by name, extension, or directory pattern, including \
             paths outside the workspace via an absolute or `~/` path.\n\
             When NOT to use: search file contents (use grep), read a file you already have the \
             path for, or run find or ls through shell.",
            object_param(
                vec![
                    (
                        "pattern",
                        param("string", "Glob pattern to match files against"),
                    ),
                    (
                        "path",
                        param(
                            "string",
                            "Directory to search. Defaults to the current working directory.",
                        ),
                    ),
                    (
                        "hidden",
                        param(
                            "boolean",
                            "Include hidden files and directories (default: false).",
                        ),
                    ),
                    (
                        "limit",
                        int_param(
                            1,
                            "Maximum number of matching files to return (default: 100)",
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
        crate::contract::blocking(ctx, input, run)
    }
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let pattern = match required_str("glob", input, "pattern") {
        Ok(pattern) => pattern,
        Err(output) => return output,
    };
    let path = match optional_str("glob", input, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let hidden = match optional_bool("glob", input, "hidden") {
        Ok(value) => value.unwrap_or(false),
        Err(output) => return output,
    };
    let limit = match optional_positive_int("glob", input, "limit") {
        Ok(value) => value.unwrap_or(DEFAULT_SEARCH_LIMIT) as usize,
        Err(output) => return output,
    };
    // Upstream tolerates the literal strings "undefined" and "null".
    let search_path = path.filter(|path| *path != "undefined" && *path != "null");
    let root = resolve_path(&ctx.directory, search_path.unwrap_or("."));
    let Ok(metadata) = std::fs::metadata(&root) else {
        return fail(format!(
            "Search path does not exist: {}",
            search_path.unwrap_or(".")
        ));
    };
    if !metadata.is_dir() {
        return fail(format!(
            "Search path is not a directory: {}",
            search_path.unwrap_or(".")
        ));
    }
    let Ok(glob) = Glob::new(pattern) else {
        return fail(format!("Unable to find files matching {pattern}"));
    };
    let Ok(glob_set) = GlobSetBuilder::new().add(glob).build() else {
        return fail(format!("Unable to find files matching {pattern}"));
    };
    // rg: `--hidden` is opt-in, and `.git` is always excluded.
    let mut entries: Vec<String> = Vec::new();
    let mut timed_out = false;
    let started = Instant::now();
    for entry in WalkBuilder::new(&root)
        .hidden(!hidden)
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
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            continue;
        };
        if glob_set.is_match(relative) {
            entries.push(entry.path().to_string_lossy().to_string());
            if entries.len() > limit {
                break;
            }
        }
    }
    if timed_out && entries.len() <= limit {
        return fail(
            "Search timed out after 30 seconds. Consider using a more specific path or pattern.",
        );
    }
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    let count = entries.len();
    let mut lines: Vec<String> = if entries.is_empty() {
        vec!["No files found".to_owned()]
    } else {
        entries
    };
    if truncated {
        lines.push(String::new());
        lines.push(format!(
            "(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)",
            limit
        ));
    }
    ok_text(lines.join("\n")).with_metadata(json!({ "count": count, "truncated": truncated }))
}
