//! ox's `todowrite` tool: a port of upstream opencode's
//! `packages/core/src/tool/plugin/todo.ts` (reference @ 4d5965762).
//!
//! The tool is STATELESS: the model owns the list and sends the full
//! replacement on every call; the tool echoes it back. The TUI renders the
//! list from the transcript — the newest completed `todowrite` call's
//! `state.input.todos` (`packages/tui/src/util/todo.ts` `sessionTodos`) — so
//! there is no server-side list, no HTTP endpoint and no event. Reminder
//! eligibility is derived from completed calls and durable reminder admissions;
//! executing this tool never changes a separate budget.
//!
//! Deliberate deviation: upstream gates the tool behind `experimental.todo`;
//! ox has no such config surface and always advertises it.

use std::pin::Pin;

use serde_json::{Value, json};

use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::{fail, object_param, ok_text, param};
use ox_db::Db;
use ox_schema::message::{AssistantContent, Message, ToolState};
use ox_schema::wire::ToolDef;

/// Upstream `TodoTool.Info` status literals, in display order.
const STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

pub struct TodoWriteTool;

impl TodoWriteTool {
    #[must_use]
    pub fn new(_db: Db) -> Self {
        Self
    }
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todowrite"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Track multi-step work with todowrite: send the full list on every call; it replaces the previous one.",
        )
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "todowrite",
            // Upstream `packages/core/src/tool/todowrite.txt` verbatim.
            "Create and update a todo list for the current session. The user sees it as your \
             working plan.\n\n\
             Use it proactively when a task has 3 or more distinct steps, or when the user gives \
             several tasks or asks for a todo list. Skip it for a single simple task or a purely \
             conversational request. When in doubt, use it.\n\n\
             - Send the full list on every call; it replaces the previous one.\n\
             - Mark items `in_progress` when you start them and `completed` as soon as they're \
             actually done.\n\
             - Only call it when the list changes.",
            object_param(
                vec![(
                    "todos",
                    json!({
                        "type": "array",
                        "description": "The updated todo list",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": param("string", "Brief description of the task"),
                                "status": {
                                    "type": "string",
                                    "enum": STATUSES,
                                    "description": "Current status of the task: pending, \
                                                    in_progress, completed, cancelled",
                                },
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false,
                        },
                    }),
                )],
                &["todos"],
            ),
        )
    }

    fn execute(
        &self,
        _ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            let todos = match decode(&input) {
                Ok(todos) => todos,
                Err(message) => return fail(format!("Invalid todowrite arguments: {message}")),
            };
            ok_text(summarize(todos)).with_metadata(json!({ "todos": todos }))
        })
    }
}

/// Completed lists in generation order. Parsing failures deny a reminder rather
/// than guessing that a missing or malformed list represents progress.
pub fn completed_lists(raw: &str) -> anyhow::Result<Vec<Vec<(String, String)>>> {
    let value: Value = serde_json::from_str(raw)?;
    let Message::Assistant(assistant) = Message::from_row_data("", "assistant", value)? else {
        anyhow::bail!("expected assistant message");
    };
    let mut lists = Vec::new();
    for part in assistant.content {
        let AssistantContent::Tool { name, state, .. } = part else {
            continue;
        };
        if name != "todowrite" {
            continue;
        }
        let ToolState::Completed { input, .. } = *state else {
            continue;
        };
        let input = Value::Object(input.into_iter().collect());
        let todos = decode(&input).map_err(anyhow::Error::msg)?;
        let mut list = Vec::with_capacity(todos.len());
        for todo in todos {
            let content = todo["content"].as_str().expect("validated content");
            let status = todo["status"].as_str().expect("validated status");
            list.push((
                content.split_whitespace().collect::<Vec<_>>().join(" "),
                status.to_owned(),
            ));
        }
        lists.push(list);
    }
    Ok(lists)
}

// ---------------------------------------------------------------------------
// Decoding and rendering
// ---------------------------------------------------------------------------

/// Type-check the payload. ox has no schema-enforcement layer between the
/// model and `execute` (step.rs hands over the parsed arguments verbatim), so
/// the tool validates its own input; a failure is recoverable — the model
/// retries with a valid list.
fn decode(input: &Value) -> Result<&[Value], String> {
    let Some(object) = input.as_object() else {
        return Err("Expected an object".to_owned());
    };
    let Some(todos) = object.get("todos") else {
        return Err("Missing `todos`".to_owned());
    };
    let Some(todos) = todos.as_array() else {
        return Err("`todos` must be an array".to_owned());
    };
    for (index, item) in todos.iter().enumerate() {
        let Some(row) = item.as_object() else {
            return Err(format!("`todos[{index}]` must be an object"));
        };
        if !matches!(row.get("content"), Some(Value::String(_))) {
            return Err(format!("`todos[{index}].content` must be a string"));
        }
        match row.get("status").and_then(Value::as_str) {
            Some(status) if STATUSES.contains(&status) => {}
            _ => {
                return Err(format!(
                    "`todos[{index}].status` must be one of {}",
                    STATUSES.join(", ")
                ));
            }
        }
    }
    Ok(todos)
}

/// Upstream `summarize()` verbatim: `Todos updated: 2/5 completed, 1 in
/// progress, 2 pending. In progress: "a", "b"`; an empty list is
/// `Todos cleared`. In-progress contents are JSON-quoted (upstream
/// `JSON.stringify`).
fn summarize(todos: &[Value]) -> String {
    if todos.is_empty() {
        return "Todos cleared".to_owned();
    }
    let count = |status: &str| {
        todos
            .iter()
            .filter(|todo| todo.get("status").and_then(Value::as_str) == Some(status))
            .count()
    };
    let mut parts = vec![format!("{}/{} completed", count("completed"), todos.len())];
    for status in ["in_progress", "pending", "cancelled"] {
        let n = count(status);
        if n > 0 {
            parts.push(format!("{n} {}", status.replace('_', " ")));
        }
    }
    let active: Vec<String> = todos
        .iter()
        .filter(|todo| todo.get("status").and_then(Value::as_str) == Some("in_progress"))
        .map(|todo| {
            serde_json::to_string(todo.get("content").expect("validated string"))
                .expect("string serialization")
        })
        .collect();
    let mut out = format!("Todos updated: {}", parts.join(", "));
    if !active.is_empty() {
        out.push_str(&format!(". In progress: {}", active.join(", ")));
    }
    out
}
