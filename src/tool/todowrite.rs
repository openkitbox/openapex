//! ox's `todowrite` tool: a port of upstream opencode's
//! `packages/core/src/tool/plugin/todo.ts` (reference @ 4d5965762).
//!
//! The tool is STATELESS: the model owns the list and sends the full
//! replacement on every call; the tool echoes it back. The TUI renders the
//! list from the transcript — the newest completed `todowrite` call's
//! `state.input.todos` (`packages/tui/src/util/todo.ts` `sessionTodos`) — so
//! there is no server-side list, no HTTP endpoint and no event. The only
//! state this module touches is the reminder budget row, reset on every
//! successful call (a touched list re-arms the completion reminder).
//!
//! Deliberate deviation: upstream gates the tool behind `experimental.todo`;
//! ox has no such config surface and always advertises it.

use std::pin::Pin;

use serde_json::{Value, json};

use crate::cardinal::wire::ToolDef;
use crate::db::Db;
use crate::runner::{Tool, ToolContext, ToolOutput};
use crate::tool::{fail, object_param, ok_text, param};

/// Upstream `TodoTool.Info` status literals, in display order.
const STATUSES: [&str; 4] = ["pending", "in_progress", "completed", "cancelled"];

pub struct TodoWriteTool {
    /// Only for the reminder budget reset: the list itself is not stored.
    db: Db,
}

impl TodoWriteTool {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todowrite"
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
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            let todos = match decode(&input) {
                Ok(todos) => todos,
                Err(message) => return fail(format!("Invalid todowrite arguments: {message}")),
            };
            // A touched list re-arms the completion reminder budget (was: the
            // stateful tool's successful-mutation hook).
            crate::runner::reset_todo_reminder_budget(&self.db, &ctx.session_id).await;
            ok_text(summarize(&todos)).with_metadata(json!({ "todos": todos }))
        })
    }
}

// ---------------------------------------------------------------------------
// Decoding and rendering
// ---------------------------------------------------------------------------

/// Type-check the payload. ox has no schema-enforcement layer between the
/// model and `execute` (step.rs hands over the parsed arguments verbatim), so
/// the tool validates its own input; a failure is recoverable — the model
/// retries with a valid list.
fn decode(input: &Value) -> Result<Vec<Value>, String> {
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
    Ok(todos.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn context(session_id: &str) -> ToolContext {
        ToolContext {
            session_id: session_id.to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_1".to_owned(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    async fn run(tool: &TodoWriteTool, session_id: &str, input: Value) -> ToolOutput {
        tool.execute(context(session_id), input).await
    }

    #[tokio::test]
    async fn echoes_the_input_and_summarizes_counts() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoWriteTool::new(db);
        let todos = json!([
            { "content": "read the code", "status": "completed" },
            { "content": "write the fix", "status": "in_progress" },
            { "content": "add tests", "status": "pending" },
            { "content": "dropped idea", "status": "cancelled" }
        ]);
        let output = run(&tool, "ses_a", json!({ "todos": todos.clone() })).await;
        assert!(output.error.is_none(), "{:?}", output.error);
        assert_eq!(
            output.content.as_str().expect("text content"),
            "Todos updated: 1/4 completed, 1 in progress, 1 pending, 1 cancelled. \
             In progress: \"write the fix\""
        );
        assert_eq!(output.metadata, Some(json!({ "todos": todos })));
    }

    #[tokio::test]
    async fn an_empty_list_reports_cleared() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoWriteTool::new(db);
        let output = run(&tool, "ses_a", json!({ "todos": [] })).await;
        assert!(output.error.is_none());
        assert_eq!(output.content.as_str().unwrap(), "Todos cleared");
        assert_eq!(output.metadata, Some(json!({ "todos": [] })));
    }

    #[tokio::test]
    async fn zero_counts_are_omitted_and_in_progress_is_json_quoted() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoWriteTool::new(db);
        let output = run(
            &tool,
            "ses_a",
            json!({ "todos": [
                { "content": "say \"hi\"", "status": "in_progress" },
                { "content": "done", "status": "completed" }
            ] }),
        )
        .await;
        assert_eq!(
            output.content.as_str().unwrap(),
            "Todos updated: 1/2 completed, 1 in progress. In progress: \"say \\\"hi\\\"\""
        );
    }

    #[tokio::test]
    async fn malformed_payloads_fail_recoverably() {
        let db = Db::open_in_memory().await.unwrap();
        let tool = TodoWriteTool::new(db);
        for input in [
            json!(null),
            json!({}),
            json!({ "todos": "x" }),
            json!({ "todos": [42] }),
            json!({ "todos": [{ "content": "a" }] }),
            json!({ "todos": [{ "content": "a", "status": "blocked" }] }),
            json!({ "todos": [{ "content": 1, "status": "pending" }] }),
        ] {
            let output = run(&tool, "ses_a", input.clone()).await;
            assert!(output.error.is_some(), "input must fail: {input}");
        }
    }

    #[tokio::test]
    async fn every_successful_call_resets_the_reminder_budget() {
        let db = Db::open_in_memory().await.unwrap();
        // Seed a spent budget row, the same key todo_reminder uses.
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "INSERT INTO kv (key, value, time_created, time_updated)
             VALUES ('todo:reminders:ses_a', '2', 0, 0)",
            (),
        )
        .await
        .unwrap();
        drop(conn);

        let tool = TodoWriteTool::new(db.clone());
        let output = run(
            &tool,
            "ses_a",
            json!({ "todos": [{ "content": "a", "status": "pending" }] }),
        )
        .await;
        assert!(output.error.is_none());

        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare("SELECT value FROM kv WHERE key = 'todo:reminders:ses_a'")
            .await
            .unwrap();
        let row = crate::db::optional(&mut stmt, ())
            .await
            .unwrap()
            .expect("row");
        let value: String = row.get(0).unwrap();
        assert_eq!(value, "0", "a touched list re-arms the budget");
    }
}
