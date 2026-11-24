//! Pure extraction of permission action/resources/save from a tool call.
//! Arguments are borrowed; only the extracted strings are allocated.

use std::path::Path;

use serde_json::Value;

use super::shell;

/// One tool call's permission surface. `save` empty means the client must not
/// offer "Always" (upstream `save: undefined`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    pub action: &'static str,
    pub resources: Vec<String>,
    pub save: Vec<String>,
}

/// Extract a built-in tool call. `None` marks tools that are never gated;
/// unrecognized tools fail closed as `action = "unknown"`, resource `*`, no
/// saved pattern. An MCP tool (`is_mcp`) is identified by its registered name
/// instead of by a naming convention.
#[must_use]
pub fn extract(tool: &str, input: &Value, directory: &Path, is_mcp: bool) -> Option<Extraction> {
    if is_mcp {
        return Some(extract_mcp(tool));
    }
    match tool {
        "shell" => Some(shell_call(input)),
        "write" | "edit" => Some(path_call("edit", input, directory)),
        "patch" => Some(patch_call(input, directory)),
        "read" => Some(path_call("read", input, directory)),
        "glob" | "grep" => Some(field_call("read", input, "pattern", &["*"])),
        "webfetch" => Some(field_call("webfetch", input, "url", &["*"])),
        "subagent" => Some(field_call("subagent", input, "agent", &["*"])),
        "skill" => Some(skill_call(input)),
        "question" | "todowrite" | "todo" | "capability_search" | "automation" => None,
        _ => Some(fail_closed()),
    }
}

/// MCP tools are registered under their own names; the action is always
/// `mcp` and the tool name is the resource, so rules can target a tool
/// without depending on the server's naming convention.
#[must_use]
pub fn extract_mcp(tool: &str) -> Extraction {
    if tool.trim().is_empty() {
        return fail_closed();
    }
    Extraction {
        action: "mcp",
        resources: vec![tool.to_owned()],
        save: vec!["*".to_owned()],
    }
}

fn fail_closed() -> Extraction {
    Extraction {
        action: "unknown",
        resources: vec!["*".to_owned()],
        save: Vec::new(),
    }
}

fn shell_call(input: &Value) -> Extraction {
    let command = input.get("command").and_then(Value::as_str).unwrap_or("");
    let call = shell::analyze(command);
    Extraction {
        action: "shell",
        resources: call.resources,
        save: call.save,
    }
}

fn field_call(action: &'static str, input: &Value, key: &str, save: &[&str]) -> Extraction {
    match input.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Extraction {
            action,
            resources: vec![value.to_owned()],
            save: save.iter().map(|pattern| (*pattern).to_owned()).collect(),
        },
        _ => fail_closed(),
    }
}

fn skill_call(input: &Value) -> Extraction {
    ["id", "name"]
        .into_iter()
        .filter_map(|key| input.get(key).and_then(Value::as_str))
        .find(|id| !id.trim().is_empty())
        .map_or_else(fail_closed, |id| Extraction {
            action: "skill",
            resources: vec![id.to_owned()],
            save: vec![id.to_owned()],
        })
}

fn path_call(action: &'static str, input: &Value, directory: &Path) -> Extraction {
    match input.get("path").and_then(Value::as_str) {
        Some(path) if !path.trim().is_empty() => {
            let resource = resource_of(directory, path);
            let save = match action {
                "edit" => vec![resource.clone()],
                _ => vec!["*".to_owned()],
            };
            Extraction {
                action,
                resources: vec![resource],
                save,
            }
        }
        _ => fail_closed(),
    }
}

fn patch_call(input: &Value, directory: &Path) -> Extraction {
    let Some(text) = input.get("patchText").and_then(Value::as_str) else {
        return fail_closed();
    };
    let mut resources = Vec::new();
    for line in text.lines() {
        for prefix in ["*** Add File: ", "*** Delete File: ", "*** Update File: "] {
            if let Some(path) = line.strip_prefix(prefix) {
                let resource = resource_of(directory, path.trim());
                if !resources.contains(&resource) {
                    resources.push(resource);
                }
            }
        }
    }
    if resources.is_empty() {
        return fail_closed();
    }
    Extraction {
        action: "edit",
        save: resources.clone(),
        resources,
    }
}

/// Lexical slash-form path relative to the session directory. No filesystem
/// access or symlink resolution is involved.
fn resource_of(directory: &Path, path: &str) -> String {
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        directory.join(candidate)
    };
    ox_tools::resource_of(directory, &joined)
}
