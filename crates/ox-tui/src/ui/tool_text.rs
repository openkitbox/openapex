//! What one inline tool row says: the per-tool texts of upstream's tool
//! components (`routes/session/index.tsx`: `GenericTool` 2721-2776, `Glob`
//! 3179, `Read` 3192, `Grep` 3230, `WebFetch` 3243, `Subagent` 3276,
//! `Question` 3649 inline branch, `Skill` 3685). Pure: the call's input,
//! result metadata and state in, texts out. Drawing is `tool_row`'s.
//!
//! Deviations:
//! - Generic summaries and the expanded `key: value` list follow the input's
//!   key order, which is alphabetical here (`Record` is a `BTreeMap`) instead
//!   of upstream's insertion order.
//! - Paths are formatted against the TUI's working directory (`--directory`
//!   or the launch cwd, which is also the location the client sends), not a
//!   per-session stored location. With no directory known a path is shown
//!   as given.
//! - Skill names read `metadata.name`, then `input.id` (upstream verbatim);
//!   ox's `name` input alias is not consulted.
//! - Subagent model labels show the raw `provider/model` reference: the TUI
//!   has no model list to look the display name up in.
//! - `write` before it finishes is upstream's inline `← Write <path>` row
//!   (`index.tsx:3170-3174`); a failed write lists its error under it. The
//!   finished file is `write_block`'s.
//! - `patch` (also `apply_patch`) without blocks to draw (running, or
//!   finished without `metadata.files`) is an inline `← Patching[ <path>]`
//!   row, where upstream draws a title-only `BlockTool` with a spinner
//!   (`index.tsx:3600-3617`); the `←` icon is borrowed from Edit. Finished and failed blocks
//!   are `patch_block`'s.
//! - `edit` without a diff to draw (running, failed, no `metadata.files`) is
//!   this inline row, where upstream draws a title-only `BlockTool`
//!   (`index.tsx:3485-3496`); the finished diff block is `edit_block`'s.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use ox_client::store::ToolState;
use ox_schema::support::Record;
use serde_json::Value;

/// Where paths are shown relative to (upstream `usePathFormatter`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paths {
    /// The working directory; paths inside it are shown relative.
    pub directory: Option<String>,
    /// The home directory; other paths under it are shown as `~/…`.
    pub home: Option<String>,
}

static PATHS: OnceLock<Paths> = OnceLock::new();

/// Install the TUI's directories once, at startup. Later calls are ignored:
/// the working directory does not change under a running TUI.
pub fn init_paths(paths: Paths) {
    let _ = PATHS.set(paths);
}

/// The installed directories; none known (paths shown as given) until
/// [`init_paths`] ran, which keeps tests independent of the machine.
#[must_use]
pub fn paths() -> &'static Paths {
    PATHS.get_or_init(Paths::default)
}

/// One row's texts, in upstream `InlineTool` terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolText<'a> {
    /// Shown before `label` once the row is complete or failed.
    pub icon: &'static str,
    /// Shown next to the spinner while the row is not complete.
    pub pending: &'a str,
    /// The row's text (upstream `children`).
    pub label: String,
    /// Upstream `complete` truthiness: the label replaces `pending`.
    pub complete: bool,
    /// Upstream `spinner`: the spinner stays and the label follows it.
    pub spinner: bool,
    /// `GenericTool`: a click or `o` lists the input and output.
    pub generic: bool,
    /// Read's `↳ Loaded <file>` rows, already formatted.
    pub loaded: Vec<String>,
}

/// Whether `tool` keeps a row of its own instead of a text row here.
#[must_use]
pub fn own_row(tool: &str) -> bool {
    matches!(tool, "shell" | "todowrite")
}

/// Whether `tool` falls back to upstream `GenericTool` (no own texts, no
/// own renderer): MCP tools, `capability_search` and anything unknown.
#[must_use]
pub fn is_generic(tool: &str) -> bool {
    !own_row(tool)
        && !matches!(
            tool,
            "read"
                | "glob"
                | "grep"
                | "webfetch"
                | "skill"
                | "question"
                | "subagent"
                | "edit"
                | "write"
                | "patch"
                | "apply_patch"
        )
}

/// The texts for one call, or `None` for tools that keep their own row
/// (`shell`, `todowrite`).
#[must_use]
pub fn text<'a>(
    tool: &'a str,
    input: &Record,
    metadata: Option<&Record>,
    state: ToolState,
    paths: &Paths,
) -> Option<ToolText<'a>> {
    let meta = |key: &str| metadata.and_then(|m| m.get(key));
    let input_str = |key: &str| string_value(input.get(key));
    let completed = matches!(state, ToolState::Success);
    let base = |icon, pending, label: String, complete: bool| ToolText {
        icon,
        pending,
        label,
        complete,
        spinner: false,
        generic: false,
        loaded: Vec::new(),
    };
    if own_row(tool) {
        return None;
    }
    let text = match tool {
        "glob" | "grep" => {
            let (name, pending, count_key) = if tool == "glob" {
                ("Glob", "Finding files\u{2026}", "count")
            } else {
                ("Grep", "Searching content\u{2026}", "matches")
            };
            let pattern = input_str("pattern");
            let mut label = format!("{name} \"{}\"", pattern.unwrap_or_default());
            if let Some(path) = input_str("path").filter(|p| !p.is_empty()) {
                label.push_str(" in ");
                label.push_str(&format_path(path, paths));
            }
            if let Some(n) = finite_number(meta(count_key)).filter(|n| *n != 0.0) {
                let noun = if n == 1.0 { "match" } else { "matches" };
                label.push_str(&format!(" ({} {noun})", js_number(n)));
            }
            base("\u{2731}", pending, label, truthy_str(pattern))
        }
        "read" => {
            let path = input_str("path");
            let mut label = format!("Read {}", format_path(path.unwrap_or_default(), paths));
            if input.contains_key("offset") || input.contains_key("limit") {
                let from = finite_number(input.get("offset"))
                    .filter(|n| *n != 0.0)
                    .unwrap_or(1.0);
                label.push_str(&format!(":{}-", js_number(from)));
                if input.get("limit").is_some_and(js_truthy) {
                    let limit = finite_number(input.get("limit")).unwrap_or(0.0);
                    label.push_str(&js_number(from + limit - 1.0));
                }
            }
            let loaded = match meta("loaded") {
                Some(Value::Array(items)) if completed => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|file| format_path(file, paths))
                    .collect(),
                _ => Vec::new(),
            };
            ToolText {
                spinner: matches!(state, ToolState::Running | ToolState::Progress),
                loaded,
                ..base("\u{2192}", "Reading file\u{2026}", label, truthy_str(path))
            }
        }
        "webfetch" => {
            let url = input_str("url");
            let label = format!("WebFetch {}", url.unwrap_or_default());
            base("%", "Fetching from the web\u{2026}", label, truthy_str(url))
        }
        "skill" => {
            let name = string_value(meta("name")).or_else(|| input_str("id"));
            let label = format!("Skill \"{}\"", name.unwrap_or_default());
            base("\u{2192}", "Loading skill\u{2026}", label, truthy_str(name))
        }
        "question" => {
            let count = match input.get("questions") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter(|item| truthy_str(string_value(item.get("question"))))
                    .count(),
                _ => 0,
            };
            let plural = if count == 1 { "" } else { "s" };
            let label = format!("Asked {count} question{plural}");
            base("\u{2192}", "Asking questions\u{2026}", label, count > 0)
        }
        "subagent" => subagent(input, state),
        "edit" => {
            let path = input_str("path");
            let label = format!("Edit {}", format_path(path.unwrap_or_default(), paths));
            // The spinner carries the label only once the path is known.
            ToolText {
                spinner: running(state) && truthy_str(path),
                ..base(
                    "\u{2190}",
                    "Preparing edit\u{2026}",
                    label,
                    truthy_str(path),
                )
            }
        }
        "write" => {
            let path = input_str("path");
            let label = format!("Write {}", format_path(path.unwrap_or_default(), paths));
            ToolText {
                spinner: running(state) && truthy_str(path),
                ..base(
                    "\u{2190}",
                    "Preparing write\u{2026}",
                    label,
                    truthy_str(path),
                )
            }
        }
        "patch" | "apply_patch" => {
            let label =
                match super::patch_block::targets(input_str("patchText").unwrap_or_default())
                    .as_slice()
                {
                    [one] => format!("Patching {}", format_path(one, paths)),
                    _ => String::from("Patching"),
                };
            ToolText {
                spinner: running(state),
                ..base("\u{2190}", "Patching", label, true)
            }
        }
        _ => {
            let summary = generic_summary(tool, input);
            ToolText {
                spinner: running(state),
                generic: true,
                ..base(
                    if matches!(state, ToolState::Failed) {
                        "\u{2717}"
                    } else {
                        "\u{2713}"
                    },
                    tool,
                    summary,
                    completed,
                )
            }
        }
    };
    Some(text)
}

/// Upstream `Subagent` (`index.tsx:3276-3315`). The caller adds the
/// background state, which is not part of the call.
fn subagent(input: &Record, state: ToolState) -> ToolText<'static> {
    let description = string_value(input.get("description"));
    let continuation = truthy_str(string_value(input.get("sessionID")));
    let is_running = matches!(state, ToolState::Running | ToolState::Progress);
    let icon = if is_running {
        "\u{2502}"
    } else if continuation {
        "\u{21b3}"
    } else if matches!(state, ToolState::Success) {
        "\u{2713}"
    } else {
        "\u{2502}"
    };
    let lead = if continuation {
        Cow::Borrowed("Continue subagent")
    } else {
        let agent = string_value(input.get("agent"))
            .or_else(|| string_value(input.get("subagent_type")))
            .unwrap_or("General");
        Cow::Owned(format!("{} Subagent", titlecase(agent)))
    };
    let mut label = format!("{lead} \u{2014} {}", description.unwrap_or("Subagent"));
    if let Some(model) = string_value(input.get("model")).and_then(model_label) {
        label.push_str(" \u{b7} ");
        label.push_str(&model);
    }
    ToolText {
        icon,
        pending: "Delegating\u{2026}",
        label,
        complete: truthy_str(description),
        spinner: is_running,
        generic: false,
        loaded: Vec::new(),
    }
}

/// Upstream `subagentModelLabel` without a model list: `ref (variant)`.
fn model_label(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    Some(match value.split_once('#') {
        Some((reference, variant)) if !variant.is_empty() => format!("{reference} ({variant})"),
        Some((reference, _)) => reference.to_owned(),
        None => value.to_owned(),
    })
}

/// Upstream `genericToolSummary`: `tool [k=v, …]` over the primitive
/// inputs, whitespace runs collapsed to one space.
#[must_use]
pub fn generic_summary(tool: &str, input: &Record) -> String {
    let args: Vec<String> = input
        .iter()
        .filter_map(|(key, value)| {
            let shown = match value {
                Value::String(s) => Cow::Borrowed(s.as_str()),
                Value::Number(n) => Cow::Owned(n.as_f64().map_or_else(|| n.to_string(), js_number)),
                Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
                _ => return None,
            };
            Some(format!("{key}={shown}"))
        })
        .collect();
    if args.is_empty() {
        return tool.to_owned();
    }
    let joined = format!("[{}]", args.join(", "));
    let collapsed: Vec<&str> = joined.split_whitespace().collect();
    // `split_whitespace` drops edge whitespace that `replace(/\s+/g, " ")`
    // would keep as one space; a leading/trailing blank cannot occur inside
    // `[…]`, so the join is exact.
    format!("{tool} {}", collapsed.join(" "))
}

/// The expanded `GenericTool` body (`index.tsx:2742-2767`): one `key: value`
/// row per input (strings raw, other values pretty JSON), then `output:`
/// with the ANSI-free trimmed output when there is any.
#[must_use]
pub fn generic_details<'a>(input: &'a Record, output: &str) -> Vec<(&'a str, String)> {
    let mut rows: Vec<(&str, String)> = input
        .iter()
        .map(|(key, value)| {
            let shown = match value {
                Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            (key.as_str(), shown)
        })
        .collect();
    let output = super::paste::normalize(output.trim());
    let output = output.trim();
    if !output.is_empty() {
        rows.push(("output", output.to_owned()));
    }
    rows
}

/// Whether a generic row has anything to expand (upstream `expandable`).
#[must_use]
pub fn generic_expandable(input: &Record, output: &str) -> bool {
    !input.is_empty() || !output.trim().is_empty()
}

/// Upstream `formatPath` (`util/path-format.ts`): relative inside the
/// directory (`.` for the directory itself), else `~/…` under home, else
/// absolute. Without a directory the input is returned as given.
#[must_use]
pub fn format_path(input: &str, paths: &Paths) -> String {
    if input.is_empty() {
        return String::new();
    }
    let Some(directory) = paths.directory.as_deref() else {
        return input.to_owned();
    };
    let base = normalize(Path::new(directory));
    let absolute = normalize(&base.join(input));
    if let Ok(relative) = absolute.strip_prefix(&base) {
        let shown = relative.to_string_lossy();
        return if shown.is_empty() {
            ".".to_owned()
        } else {
            shown.into_owned()
        };
    }
    if let Some(home) = paths.home.as_deref().filter(|h| !h.is_empty())
        && let Ok(relative) = absolute.strip_prefix(normalize(Path::new(home)))
    {
        let shown = relative.to_string_lossy();
        return if shown.is_empty() {
            "~".to_owned()
        } else {
            format!("~/{shown}")
        };
    }
    absolute.to_string_lossy().into_owned()
}

/// Lexical `path.resolve` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn running(state: ToolState) -> bool {
    matches!(
        state,
        ToolState::Input | ToolState::Running | ToolState::Progress
    )
}

/// Upstream `stringValue`.
fn string_value(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

/// A JS-truthy string: present and non-empty.
fn truthy_str(value: Option<&str>) -> bool {
    value.is_some_and(|s| !s.is_empty())
}

/// Upstream `finiteNumber`.
fn finite_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// JS truthiness of a JSON value.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `String(n)` for the integers tool inputs carry; fractions as Rust prints them.
fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Upstream `Locale.titlecase`: upper-case the first letter of each word.
fn titlecase(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at_word_start = true;
    for ch in text.chars() {
        let word = ch.is_alphanumeric() || ch == '_';
        if word && at_word_start {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
        at_word_start = !word;
    }
    out
}
