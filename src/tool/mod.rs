//! The server-side built-in toolset (task 7): eight tools ported from
//! upstream `packages/core/src/tool/plugin/` — `read`, `write`, `edit`,
//! `patch`, `shell`, `glob`, `grep`, `webfetch`.
//!
//! Each module ports the upstream tool's exact name, model-facing
//! description, parameter JSON schema, execution semantics (error strings,
//! truncation limits, output format) so the model behaves the same as with
//! the Bun implementation.
//!
//! Scope notes (MVP, aligned with the plan):
//! - Upstream's permission system is **not** ported: every tool is
//!   allow-all, no approval prompts, no `tools.allow` config.
//! - File diffs are computed upstream for permission prompts and UI
//!   metadata (`FileDiff`); ox has no such channel yet, so they are not
//!   produced. The model-facing content strings are identical.
//! - Media files (images/PDF) that upstream ingests as model attachments
//!   are rejected with the binary-file error; multimodal reads land with
//!   the runner's attachment support.
//! - `webfetch`'s HTML→markdown converter is a compact port; upstream's
//!   is ~650 lines of htmlparser2 handling. Text extraction and all
//!   guards (content-type, size, timeout) are exact.

pub mod edit;
pub mod glob;
pub mod grep;
pub mod patch;
pub mod read;
pub(crate) mod rtk;
pub mod shell;
pub mod webfetch;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::Db;
use crate::runner::{Tool, ToolOutput, ToolRegistry};

/// The eight built-in tools, registered under their upstream names.
impl ToolRegistry {
    /// A registry containing every built-in tool.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register_all([
            Arc::new(edit::EditTool) as Arc<dyn Tool>,
            Arc::new(glob::GlobTool),
            Arc::new(grep::GrepTool),
            Arc::new(patch::PatchTool),
            Arc::new(read::ReadTool),
            Arc::new(shell::ShellTool::new()),
            Arc::new(webfetch::WebFetchTool::new()),
            Arc::new(write::WriteTool),
        ]);
        registry
    }
}

/// The magic-context tools (`ctx_reduce`, `ctx_expand`, `ctx_memory`,
/// `ctx_note`, `ctx_search`), registered ON TOP of `with_builtins` at
/// app-wiring time.
impl ToolRegistry {
    /// Register the magic-context tools. Deliberately separate from
    /// [`ToolRegistry::with_builtins`]:
    ///
    /// 1. They hold a [`Db`] handle (the `mc_tag`/`mc_drop` tables), which
    ///    only exists after the DB is opened — `with_builtins` runs before
    ///    that.
    /// 2. They must only be advertised when `magic_context.enabled` — a
    ///    default-off config keeps the advertised toolset byte-identical
    ///    (Task 1's zero-behavior-change guarantee), and the tool list is
    ///    part of the provider request.
    ///
    /// Called exactly once from `build_state_with`. Session scoping needs no
    /// dispatch change: `ToolContext` already carries `session_id`.
    pub fn register_mctx(&mut self, db: Db) {
        self.register_all(vec![
            Arc::new(crate::mctx::tools::CtxReduceTool::new(db.clone())) as Arc<dyn Tool>,
            Arc::new(crate::mctx::tools::CtxExpandTool::new(db.clone())),
            Arc::new(crate::mctx::memory::CtxMemoryTool::new(db.clone())),
            Arc::new(crate::mctx::memory::CtxNoteTool::new(db.clone())),
            Arc::new(crate::mctx::search::CtxSearchTool::new(db)),
        ]);
    }
}

// ---------------------------------------------------------------------------
// Shared model-facing error/output helpers
// ---------------------------------------------------------------------------

/// A successful output whose model content is a plain string.
pub(crate) fn ok_text(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: Value::String(text.into()),
        error: None,
    }
}

/// A failed output with the upstream `ToolFailure` message.
pub(crate) fn fail(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: Value::Null,
        error: Some(message.into()),
    }
}

/// Upstream `runtime.ts` `formatInputIssues`: the model-facing error for
/// arguments that fail input validation.
pub(crate) fn invalid_arguments(
    tool: &str,
    issues: &[(&str, String)],
    input: &Value,
) -> ToolOutput {
    let shown: Vec<String> = issues
        .iter()
        .take(5)
        .map(|(path, message)| format!("- {path}: {message}"))
        .collect();
    let mut details = shown.join("\n");
    if issues.len() > 5 {
        let more = issues.len() - 5;
        details.push_str(&format!(
            "\n- ...and {more} more {}",
            if more == 1 { "issue" } else { "issues" }
        ));
    }
    fail(format!(
        "Invalid arguments for tool \"{tool}\":\n{details}\n\nArguments provided:\n{}\n\nUpdate the arguments and call the tool again.",
        serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".to_owned())
    ))
}

// ---------------------------------------------------------------------------
// Input field extraction (missing/wrong-typed fields become the upstream
// invalid-arguments failure)
// ---------------------------------------------------------------------------

/// A required string field.
pub(crate) fn required_str<'a>(
    tool: &str,
    input: &'a Value,
    key: &str,
) -> Result<&'a str, ToolOutput> {
    let object = input
        .as_object()
        .ok_or_else(|| invalid_arguments(tool, &[(key, "Expected an object".to_owned())], input))?;
    match object.get(key) {
        Some(Value::String(text)) => Ok(text),
        Some(other) => Err(invalid_arguments(
            tool,
            &[(key, format!("Expected string, got {other}"))],
            input,
        )),
        None => Err(invalid_arguments(
            tool,
            &[(key, "Missing required field".to_owned())],
            input,
        )),
    }
}

/// An optional string field.
pub(crate) fn optional_str<'a>(
    tool: &str,
    input: &'a Value,
    key: &str,
) -> Result<Option<&'a str>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Ok(None);
    };
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(other) => Err(invalid_arguments(
            tool,
            &[(key, format!("Expected string, got {other}"))],
            input,
        )),
    }
}

/// An optional boolean field.
pub(crate) fn optional_bool(
    tool: &str,
    input: &Value,
    key: &str,
) -> Result<Option<bool>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Ok(None);
    };
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(invalid_arguments(
            tool,
            &[(key, format!("Expected boolean, got {other}"))],
            input,
        )),
    }
}

/// An optional non-negative integer field (`NonNegativeInt`).
pub(crate) fn optional_nonneg_int(
    tool: &str,
    input: &Value,
    key: &str,
) -> Result<Option<u64>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Ok(None);
    };
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => match value.as_u64() {
            Some(value) => Ok(Some(value)),
            None => Err(invalid_arguments(
                tool,
                &[(key, "Expected a non-negative integer".to_owned())],
                input,
            )),
        },
        Some(other) => Err(invalid_arguments(
            tool,
            &[(key, format!("Expected integer, got {other}"))],
            input,
        )),
    }
}

/// An optional positive integer field (`PositiveInt`); `0` fails.
pub(crate) fn optional_positive_int(
    tool: &str,
    input: &Value,
    key: &str,
) -> Result<Option<u64>, ToolOutput> {
    let value = optional_nonneg_int(tool, input, key)?;
    match value {
        Some(0) => Err(invalid_arguments(
            tool,
            &[(key, "Expected an integer greater than 0".to_owned())],
            input,
        )),
        other => Ok(other),
    }
}

/// An optional JSON number (absent or `null` → `None`); anything else is an
/// invalid-arguments error.
pub(crate) fn optional_number(
    tool: &str,
    input: &Value,
    key: &str,
) -> Result<Option<f64>, ToolOutput> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => Ok(value.as_f64()),
        Some(other) => Err(invalid_arguments(
            tool,
            &[(key, format!("Expected a number, got {other}"))],
            input,
        )),
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve a tool path argument: absolute paths pass through, relative ones
/// join the session directory (upstream `FileAccess.resolvePath`). `.` and
/// empty components are normalized away so result paths render cleanly.
pub(crate) fn resolve_path(directory: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        directory.join(candidate)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    normalized
}

/// Upstream `FileAccess` `resource`: the slash-form path relative to the
/// session directory when inside it, the absolute slash path otherwise.
pub(crate) fn resource_of(directory: &Path, absolute: &Path) -> String {
    match absolute.strip_prefix(directory) {
        Ok(relative) => {
            let slashed = to_slash(relative);
            if slashed.is_empty() {
                ".".to_owned()
            } else {
                slashed
            }
        }
        Err(_) => to_slash(absolute),
    }
}

/// Forward-slash form of a path (upstream `slash`).
pub(crate) fn to_slash(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::RootDir | std::path::Component::CurDir => None,
            std::path::Component::Prefix(prefix) => {
                Some(prefix.as_os_str().to_string_lossy().to_string())
            }
            other => Some(other.as_os_str().to_string_lossy().to_string()),
        })
        .collect();
    let prefix = if path.is_absolute() { "/" } else { "" };
    format!("{prefix}{}", parts.join("/"))
}

/// Upstream `normalizeForMatch` (edit.ts / patch.ts): 1:1 character
/// replacements so match positions are preserved. Compares typographic
/// quotes, dashes, and whitespace variants as their ASCII counterparts.
pub(crate) fn normalize_typographic(value: &[char]) -> Vec<char> {
    value
        .iter()
        .map(|&c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// A JSON-Schema fragment helper: `{"type": ..., "description": ...}`.
pub(crate) fn param(kind: &str, description: &str) -> Value {
    json!({ "type": kind, "description": description })
}

/// A JSON-Schema integer fragment with a minimum.
pub(crate) fn int_param(minimum: u64, description: &str) -> Value {
    json!({ "type": "integer", "minimum": minimum, "description": description })
}

/// An object parameter schema: `{"type":"object","properties":{…},
/// "required":[…],"additionalProperties":false}` — the shape upstream's
/// Effect Struct schemas produce (excess properties are an error there).
pub(crate) fn object_param(properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("type".to_owned(), json!("object"));
    object.insert(
        "properties".to_owned(),
        Value::Object(
            properties
                .into_iter()
                .map(|(name, schema)| (name.to_owned(), schema))
                .collect(),
        ),
    );
    if !required.is_empty() {
        object.insert("required".to_owned(), json!(required));
    }
    object.insert("additionalProperties".to_owned(), json!(false));
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_arguments_caps_issues_at_five() {
        let issues: Vec<(&str, String)> = (0..7)
            .map(|index| {
                let name: &'static str = Box::leak(format!("field{index}").into_boxed_str());
                (name, "bad".to_owned())
            })
            .collect();
        let output = invalid_arguments("demo", &issues, &json!({}));
        let text = output.error.expect("error");
        assert!(text.contains("- field4: bad"));
        assert!(text.contains("...and 2 more issues"));
        assert!(!text.contains("field5: bad"));
    }

    #[test]
    fn resource_of_prefers_relative_slash_form() {
        let directory = Path::new("/tmp/ws");
        assert_eq!(
            resource_of(directory, Path::new("/tmp/ws/src/a.rs")),
            "src/a.rs"
        );
        assert_eq!(resource_of(directory, directory), ".");
        assert_eq!(
            resource_of(directory, Path::new("/etc/passwd")),
            "/etc/passwd"
        );
    }

    #[test]
    fn optional_positive_int_rejects_zero() {
        let input = json!({"limit": 0});
        assert!(optional_positive_int("demo", &input, "limit").is_err());
        let input = json!({"limit": 3});
        assert_eq!(
            optional_positive_int("demo", &input, "limit").unwrap(),
            Some(3)
        );
    }

    #[test]
    fn with_builtins_registers_all_tools_with_schemas() {
        // ShellTool::new() reads $SHELL; serialize against env-mutating tests.
        let lock = crate::tool::shell::SHELL_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let registry = ToolRegistry::with_builtins();
        let mut names: Vec<String> = registry
            .definitions()
            .iter()
            .map(|def| def.function.name.clone())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "edit".to_owned(),
                "glob".to_owned(),
                "grep".to_owned(),
                "patch".to_owned(),
                "read".to_owned(),
                "shell".to_owned(),
                "webfetch".to_owned(),
                "write".to_owned(),
            ]
        );
        for definition in registry.definitions() {
            assert!(!definition.function.description.is_empty());
            let parameters = &definition.function.parameters;
            assert_eq!(parameters["type"], json!("object"));
            assert!(parameters["properties"].is_object());
            for field in parameters["required"].as_array().unwrap() {
                assert!(
                    parameters["properties"]
                        .get(field.as_str().unwrap())
                        .is_some(),
                    "required field {field} of {} missing from properties",
                    definition.function.name
                );
            }
        }
        // Representative schema shapes.
        let read = registry.get("read").unwrap().definition();
        assert_eq!(
            read.function.parameters["properties"]["path"]["type"],
            json!("string")
        );
        assert_eq!(read.function.parameters["required"], json!(["path"]));
        let edit = registry.get("edit").unwrap().definition();
        assert_eq!(
            edit.function.parameters["required"],
            json!(["path", "oldString", "newString"])
        );
        drop(lock);
    }
}
