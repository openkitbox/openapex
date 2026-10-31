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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(value: Value) -> Record {
        match value {
            Value::Object(map) => map.into_iter().collect(),
            other => panic!("not an object: {other}"),
        }
    }

    fn paths() -> Paths {
        Paths {
            directory: Some("/work/repo".into()),
            home: Some("/home/me".into()),
        }
    }

    /// `(tool, input, metadata, state)` → the row's head as `tool_row` draws
    /// it, minus the spinner glyph (`*`).
    fn head(tool: &str, input: Value, metadata: Option<Value>, state: ToolState) -> String {
        let input = record(input);
        let metadata = metadata.map(record);
        let t = text(tool, &input, metadata.as_ref(), state, &paths()).expect("a text row");
        if t.spinner {
            format!("* {}", t.label)
        } else if t.complete || matches!(state, ToolState::Failed) {
            format!("{} {}", t.icon, t.label)
        } else {
            format!("* {}", t.pending)
        }
    }

    #[test]
    fn per_tool_rows_match_upstream() {
        use ToolState::*;
        use serde_json::json;
        let cases: Vec<(&str, Value, Option<Value>, ToolState, &str)> = vec![
            // Read
            (
                "read",
                json!({"path": "/work/repo/src/a.rs"}),
                None,
                Success,
                "\u{2192} Read src/a.rs",
            ),
            (
                "read",
                json!({"path": "src/a.rs", "offset": 10, "limit": 5}),
                None,
                Success,
                "\u{2192} Read src/a.rs:10-14",
            ),
            (
                "read",
                json!({"path": "a.rs", "offset": 0}),
                None,
                Success,
                "\u{2192} Read a.rs:1-",
            ),
            (
                "read",
                json!({"path": "a.rs", "limit": 3}),
                None,
                Success,
                "\u{2192} Read a.rs:1-3",
            ),
            (
                "read",
                json!({"path": "/home/me/x.txt"}),
                None,
                Success,
                "\u{2192} Read ~/x.txt",
            ),
            (
                "read",
                json!({"path": "/etc/hosts"}),
                None,
                Success,
                "\u{2192} Read /etc/hosts",
            ),
            (
                "read",
                json!({"path": "a.rs"}),
                None,
                Running,
                "* Read a.rs",
            ),
            ("read", json!({}), None, Input, "* Reading file\u{2026}"),
            ("read", json!({}), None, Success, "* Reading file\u{2026}"),
            ("read", json!({}), None, Failed, "\u{2192} Read "),
            // Glob / Grep
            (
                "glob",
                json!({"pattern": "**/*.rs", "path": "/work/repo/src"}),
                Some(json!({"count": 3})),
                Success,
                "\u{2731} Glob \"**/*.rs\" in src (3 matches)",
            ),
            (
                "glob",
                json!({"pattern": "*.rs"}),
                Some(json!({"count": 1})),
                Success,
                "\u{2731} Glob \"*.rs\" (1 match)",
            ),
            (
                "glob",
                json!({"pattern": "*.rs"}),
                Some(json!({"count": 0})),
                Success,
                "\u{2731} Glob \"*.rs\"",
            ),
            (
                "glob",
                json!({"pattern": "*.rs"}),
                None,
                Running,
                "\u{2731} Glob \"*.rs\"",
            ),
            ("glob", json!({}), None, Input, "* Finding files\u{2026}"),
            (
                "grep",
                json!({"pattern": "fn main", "path": "."}),
                Some(json!({"matches": 2})),
                Success,
                "\u{2731} Grep \"fn main\" in . (2 matches)",
            ),
            (
                "grep",
                json!({}),
                None,
                Input,
                "* Searching content\u{2026}",
            ),
            // WebFetch
            (
                "webfetch",
                json!({"url": "https://a.example"}),
                None,
                Success,
                "% WebFetch https://a.example",
            ),
            (
                "webfetch",
                json!({}),
                None,
                Running,
                "* Fetching from the web\u{2026}",
            ),
            // Skill
            (
                "skill",
                json!({"id": "rust-expert"}),
                Some(json!({"name": "Rust Expert"})),
                Success,
                "\u{2192} Skill \"Rust Expert\"",
            ),
            (
                "skill",
                json!({"id": "rust-expert"}),
                None,
                Running,
                "\u{2192} Skill \"rust-expert\"",
            ),
            ("skill", json!({}), None, Input, "* Loading skill\u{2026}"),
            // Question (inline branch)
            (
                "question",
                json!({"questions": [{"question": "a?"}, {"question": "b?"}]}),
                None,
                Running,
                "\u{2192} Asked 2 questions",
            ),
            (
                "question",
                json!({"questions": [{"question": "a?"}]}),
                None,
                Success,
                "\u{2192} Asked 1 question",
            ),
            (
                "question",
                json!({}),
                None,
                Input,
                "* Asking questions\u{2026}",
            ),
            // Subagent
            (
                "subagent",
                json!({"description": "scan", "prompt": "p"}),
                None,
                Success,
                "\u{2713} General Subagent \u{2014} scan",
            ),
            (
                "subagent",
                json!({"description": "scan", "agent": "explore"}),
                None,
                Running,
                "* Explore Subagent \u{2014} scan",
            ),
            (
                "subagent",
                json!({"description": "more", "sessionID": "ses_1"}),
                None,
                Success,
                "\u{21b3} Continue subagent \u{2014} more",
            ),
            (
                "subagent",
                json!({"description": "m", "model": "p/k3#high"}),
                None,
                Success,
                "\u{2713} General Subagent \u{2014} m \u{b7} p/k3 (high)",
            ),
            ("subagent", json!({}), None, Input, "* Delegating\u{2026}"),
            (
                "subagent",
                json!({"description": "x"}),
                None,
                Failed,
                "\u{2502} General Subagent \u{2014} x",
            ),
            // Generic
            (
                "capability_search",
                json!({"query": "a  b\nc", "max_results": 3, "nested": {"x": 1}}),
                None,
                Success,
                "\u{2713} capability_search [max_results=3, query=a b c]",
            ),
            ("mcp_tool", json!({}), None, Success, "\u{2713} mcp_tool"),
            (
                "mcp_tool",
                json!({"flag": true}),
                None,
                Failed,
                "\u{2717} mcp_tool [flag=true]",
            ),
            (
                "mcp_tool",
                json!({"flag": true}),
                None,
                Running,
                "* mcp_tool [flag=true]",
            ),
            // Edit without a diff block (`index.tsx:3485-3496`, inline here)
            (
                "edit",
                json!({"path": "/work/repo/src/a.rs"}),
                None,
                Success,
                "\u{2190} Edit src/a.rs",
            ),
            (
                "edit",
                json!({"path": "src/a.rs"}),
                None,
                Running,
                "* Edit src/a.rs",
            ),
            ("edit", json!({}), None, Input, "* Preparing edit\u{2026}"),
            (
                "edit",
                json!({"path": "a.rs"}),
                None,
                Failed,
                "\u{2190} Edit a.rs",
            ),
            // Write before it finishes (`index.tsx:3170-3174`)
            ("write", json!({}), None, Input, "* Preparing write\u{2026}"),
            (
                "write",
                json!({"path": "/work/repo/src/a.rs"}),
                None,
                Running,
                "* Write src/a.rs",
            ),
            (
                "write",
                json!({"path": "a.rs"}),
                None,
                Failed,
                "\u{2190} Write a.rs",
            ),
            // Patch without blocks (`index.tsx:3600-3617`, inline here)
            ("patch", json!({}), None, Running, "* Patching"),
            (
                "patch",
                json!({"patchText": "*** Update File: /work/repo/src/a.rs\n"}),
                None,
                Running,
                "* Patching src/a.rs",
            ),
            (
                "apply_patch",
                json!({"patchText": "*** Add File: a\n*** Add File: b\n"}),
                Some(json!({"files": []})),
                Success,
                "\u{2190} Patching",
            ),
        ];
        for (tool, input, metadata, state, want) in cases {
            let got = head(tool, input.clone(), metadata.clone(), state);
            assert_eq!(got, want, "{tool} {input} {metadata:?} {state:?}");
        }
    }

    #[test]
    fn rows_with_their_own_renderer_have_no_text() {
        for tool in ["shell", "todowrite"] {
            assert!(
                text(tool, &Record::new(), None, ToolState::Success, &paths()).is_none(),
                "{tool} keeps its own row"
            );
            assert!(!is_generic(tool), "{tool} keeps its own row");
            assert!(own_row(tool), "{tool} keeps its own row");
        }
    }

    #[test]
    fn only_generic_rows_are_generic() {
        let input = Record::new();
        let generic = |tool| {
            text(tool, &input, None, ToolState::Success, &paths())
                .expect("a text row")
                .generic
        };
        assert!(generic("capability_search"), "fallback");
        assert!(is_generic("capability_search"), "fallback");
        for tool in [
            "read",
            "glob",
            "grep",
            "webfetch",
            "skill",
            "question",
            "subagent",
            "edit",
            "write",
            "patch",
            "apply_patch",
        ] {
            assert!(!generic(tool), "{tool} has its own text");
            assert!(!is_generic(tool), "{tool} has its own text");
        }
    }

    #[test]
    fn read_lists_loaded_files_only_once_completed() {
        use serde_json::json;
        let input = record(json!({"path": "img.png"}));
        let metadata = record(json!({"loaded": ["/work/repo/img.png", 3, "/home/me/b.md"]}));
        let loaded = |state| {
            text("read", &input, Some(&metadata), state, &paths())
                .expect("a text row")
                .loaded
        };
        assert_eq!(
            loaded(ToolState::Success),
            ["img.png", "~/b.md"],
            "strings only"
        );
        assert!(
            loaded(ToolState::Running).is_empty(),
            "not before completion"
        );
        assert!(loaded(ToolState::Failed).is_empty(), "not on failure");
    }

    #[test]
    fn paths_fall_back_to_the_input_without_a_directory() {
        let none = Paths::default();
        assert_eq!(format_path("/a/b", &none), "/a/b");
        assert_eq!(format_path("rel/x", &none), "rel/x");
        assert_eq!(
            format_path("/work/repo", &paths()),
            ".",
            "the directory itself"
        );
        assert_eq!(
            format_path("/work/repo/../other/x", &paths()),
            "/work/other/x"
        );
        assert_eq!(format_path("/home/me", &paths()), "~");
        assert_eq!(format_path("", &paths()), "");
    }

    #[test]
    fn generic_details_list_inputs_then_trimmed_ansi_free_output() {
        use serde_json::json;
        let input = record(json!({"b": {"x": 1}, "a": "line1\nline2"}));
        let rows = generic_details(&input, "\n \u{1b}[31mred\u{1b}[0m done \n");
        assert_eq!(
            rows,
            [
                ("a", "line1\nline2".to_owned()),
                ("b", "{\n  \"x\": 1\n}".to_owned()),
                ("output", "red done".to_owned()),
            ],
            "strings raw, others pretty JSON, output last"
        );
        assert_eq!(
            generic_details(&Record::new(), "  \n"),
            [],
            "blank output is dropped"
        );
    }

    #[test]
    fn generic_rows_expand_when_they_have_input_or_output() {
        use serde_json::json;
        assert!(
            !generic_expandable(&Record::new(), " \n"),
            "nothing to show"
        );
        assert!(generic_expandable(&Record::new(), "x"), "output only");
        assert!(
            generic_expandable(&record(json!({"a": 1})), ""),
            "input only"
        );
    }
}
