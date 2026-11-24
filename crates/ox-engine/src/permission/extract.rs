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

#[cfg(test)]
mod permission_extract_tests {
    use std::path::Path;

    use serde_json::json;

    use super::{extract, extract_mcp};

    fn dir() -> &'static Path {
        Path::new("/work/project")
    }

    #[test]
    fn known_tools_map_to_upstream_actions() {
        let shell = extract("shell", &json!({"command": "git status"}), dir(), false)
            .expect("shell is gated");
        assert_eq!(shell.action, "shell");
        assert_eq!(shell.resources, ["git status"]);
        assert_eq!(shell.save, ["git status"]);

        let write =
            extract("write", &json!({"path": "src/a.rs"}), dir(), false).expect("write is gated");
        assert_eq!(write.action, "edit");
        assert_eq!(write.resources, ["src/a.rs"]);
        assert_eq!(write.save, ["src/a.rs"]);

        let edit = extract(
            "edit",
            &json!({"path": "/work/project/src/a.rs"}),
            dir(),
            false,
        )
        .expect("edit is gated");
        assert_eq!(edit.action, "edit");
        assert_eq!(edit.resources, ["src/a.rs"]);

        let patch = extract(
            "patch",
            &json!({"patchText": "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** Update File: src/b.rs\n*** End Patch"}),
            dir(),
            false,
        )
        .expect("patch is gated");
        assert_eq!(patch.action, "edit");
        assert_eq!(patch.resources, ["a.txt", "src/b.rs"]);
        assert_eq!(patch.save, ["a.txt", "src/b.rs"]);

        let read =
            extract("read", &json!({"path": "src/a.rs"}), dir(), false).expect("read is gated");
        assert_eq!(read.action, "read");
        assert_eq!(read.resources, ["src/a.rs"]);
        assert_eq!(read.save, ["*"]);

        for tool in ["glob", "grep"] {
            let call = extract(tool, &json!({"pattern": "**/*.ts"}), dir(), false)
                .expect("search is gated");
            assert_eq!(call.action, "read");
            assert_eq!(call.resources, ["**/*.ts"]);
            assert_eq!(call.save, ["*"]);
        }

        let web = extract(
            "webfetch",
            &json!({"url": "https://example.com"}),
            dir(),
            false,
        )
        .expect("webfetch is gated");
        assert_eq!(web.action, "webfetch");
        assert_eq!(web.resources, ["https://example.com"]);
        assert_eq!(web.save, ["*"]);

        let child = extract("subagent", &json!({"agent": "general"}), dir(), false)
            .expect("subagent is gated");
        assert_eq!(child.action, "subagent");
        assert_eq!(child.resources, ["general"]);
        assert_eq!(child.save, ["*"]);

        let skill = extract(
            "skill",
            &json!({"id": "test-driven-development"}),
            dir(),
            false,
        )
        .expect("skill is gated");
        assert_eq!(skill.action, "skill");
        assert_eq!(skill.resources, ["test-driven-development"]);
        assert_eq!(skill.save, ["test-driven-development"]);

        let mcp = extract("github_create_issue", &json!({}), dir(), true).expect("MCP is gated");
        assert_eq!(mcp.action, "mcp");
        assert_eq!(mcp.resources, ["github_create_issue"]);
        assert_eq!(mcp.save, ["*"]);
        let blank = extract_mcp("");
        assert_eq!(blank.action, "unknown", "blank MCP name fails closed");
        assert!(blank.save.is_empty());
    }

    #[test]
    fn unknown_tools_fail_closed_with_no_save() {
        let input = json!({"command": "echo hi", "metadata": {"nested": [1, 2, 3]}});
        let call =
            extract("mystery_tool", &input, dir(), false).expect("unknown tools must be gated");
        assert_eq!(call.action, "unknown");
        assert_eq!(call.resources, ["*"]);
        assert!(call.save.is_empty());
    }

    #[test]
    fn extraction_does_not_copy_unrelated_json_subtrees() {
        let needle = "x".repeat(4096);
        let input = json!({
            "url": "https://example.com",
            "metadata": {"payload": needle},
        });
        let call = extract("webfetch", &input, dir(), false).expect("webfetch is gated");
        assert_eq!(call.resources, ["https://example.com"]);
        assert_eq!(call.save, ["*"]);
        assert!(
            call.resources
                .iter()
                .chain(&call.save)
                .all(|text| !text.contains("xxxx")),
            "unrelated JSON subtrees must not leak into the extracted strings"
        );
    }

    #[test]
    fn ungated_tools_return_none_and_missing_resources_fail_closed() {
        for tool in ["question", "todowrite", "capability_search", "automation"] {
            assert!(
                extract(tool, &json!({}), dir(), false).is_none(),
                "{tool} is not gated"
            );
        }
        for (tool, input) in [
            ("shell", json!({})),
            ("write", json!({})),
            ("read", json!({})),
            ("webfetch", json!({})),
            ("subagent", json!({})),
        ] {
            let call = extract(tool, &input, dir(), false).expect("gated tool");
            assert_eq!(
                call.resources,
                ["*"],
                "{tool} without a resource fails closed"
            );
            assert!(
                call.save.is_empty(),
                "{tool} without a resource has no Always"
            );
        }
    }
}
