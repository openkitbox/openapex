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

use ox_schema::wire::ToolDef;
use ox_db::Db;
use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::message::{AssistantContent, Message, ToolState};
use crate::{fail, object_param, ok_text, param};

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
            reset_reminder_budget(&self.db, &ctx.session_id).await;
            ok_text(summarize(&todos)).with_metadata(json!({ "todos": todos }))
        })
    }
}

// ---------------------------------------------------------------------------
// Reminder budget (the kv row the engine's reminder loop also uses)
// ---------------------------------------------------------------------------

/// `kv` key for the per-session reminder counter. The list itself is not
/// stored server-side — `todowrite` echoes and the TUI reads the transcript —
/// so this counter is the only todo-related kv row left.
pub fn reminder_key(session_id: &str) -> String {
    format!("todo:reminders:{session_id}")
}

/// The session's current reminder count (0 when the row is absent or garbage).
pub async fn reminder_count(db: &Db, session_id: &str) -> anyhow::Result<u32> {
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare("SELECT value FROM kv WHERE key = ?1").await?;
    let Some(row) = ox_db::optional(&mut stmt, [reminder_key(session_id)]).await? else {
        return Ok(0);
    };
    let raw: String = row.get(0)?;
    Ok(raw.parse().unwrap_or(0))
}

pub async fn set_reminder_count(db: &Db, session_id: &str, count: u32) -> anyhow::Result<()> {
    let value = count.to_string();
    let now = ox_schema::id::now_millis();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
        (reminder_key(session_id), value.as_str(), now),
    )
    .await?;
    Ok(())
}

/// Whether a session's reminder counter is reset. Called by the todo tool after
/// every successful mutation (the reference resets `reminderCount` whenever the
/// list is touched or empties): any real progress re-arms the budget.
pub async fn reset_reminder_budget(db: &Db, session_id: &str) {
    if let Err(error) = set_reminder_count(db, session_id, 0).await {
        tracing::debug!(session = %session_id, error = %error, "todo reminder budget reset failed");
    }
}

// ---------------------------------------------------------------------------
// Reminder data source
// ---------------------------------------------------------------------------

/// Open todo items for the completion reminder: the `pending`/`in_progress`
/// entries of the session's newest COMPLETED `todowrite` call, as
/// `(content, status)`. This is the same selection rule the TUI's
/// `sessionTodos` applies (newest completed call wins; a newer running or
/// errored call does not shadow it), minus the revert boundary, which is a
/// TUI display concept. An empty newest list — or no call at all — means no
/// open work.
///
/// Reads the transcript tail streaming, newest first: the common case hits
/// within the first few rows, and only a session that never called todowrite
/// scans to the end.
pub async fn open_tasks(db: &Db, session_id: &str) -> anyhow::Result<Vec<(String, String)>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT data FROM session_message
             WHERE session_id = ?1 AND type = 'assistant'
             ORDER BY seq DESC",
        )
        .await?;
    let mut rows = stmt.query([session_id]).await?;
    let mut found: Option<Vec<(String, String)>> = None;
    while let Some(row) = rows.next().await? {
        let raw: String = row.get(0)?;
        if let Some(open) = open_from_assistant_row(&raw) {
            found = Some(open);
            break;
        }
    }
    drop(rows);
    drop(stmt);
    drop(conn);
    Ok(found.unwrap_or_default())
}

/// `Some` when this assistant row holds at least one completed `todowrite`
/// part — the value is that call's open items (possibly empty). `None` keeps
/// the scan walking older rows. A malformed row is skipped, not fatal: the
/// reminder is advisory.
fn open_from_assistant_row(raw: &str) -> Option<Vec<(String, String)>> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let Ok(Message::Assistant(assistant)) = Message::from_row_data("", "assistant", value) else {
        return None;
    };
    // Parts are in generation order; the last todowrite in a message is the
    // latest call.
    for part in assistant.content.iter().rev() {
        let AssistantContent::Tool { name, state, .. } = part else {
            continue;
        };
        if name != "todowrite" {
            continue;
        }
        let ToolState::Completed { input, .. } = &**state else {
            continue;
        };
        let Some(todos) = input.get("todos").and_then(Value::as_array) else {
            return Some(Vec::new());
        };
        return Some(
            todos
                .iter()
                .filter_map(|item| {
                    let status = item.get("status")?.as_str()?;
                    if !matches!(status, "pending" | "in_progress") {
                        return None;
                    }
                    Some((item.get("content")?.as_str()?.to_owned(), status.to_owned()))
                })
                .collect(),
        );
    }
    None
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
        let row = ox_db::optional(&mut stmt, ())
            .await
            .unwrap()
            .expect("row");
        let value: String = row.get(0).unwrap();
        assert_eq!(value, "0", "a touched list re-arms the budget");
    }

    /// Seed one assistant message row holding a single completed todowrite
    /// part. `seq` orders the rows; the newest wins.
    async fn seed_todowrite_row(db: &Db, session_id: &str, seq: i64, todos: Value) {
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO session_v2
               (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES (?1, 'default', 't', '/tmp', '1', 0, 0)",
            [session_id],
        )
        .await
        .unwrap();
        let data = json!({
            "id": format!("msg_{session_id}_{seq}"),
            "type": "assistant",
            "time": { "created": 0 },
            "agent": "build",
            "model": { "id": "m", "providerID": "p" },
            "content": [{
                "type": "tool",
                "id": "call_1",
                "name": "todowrite",
                "state": { "status": "completed", "input": { "todos": todos }, "content": [] },
                "time": { "created": 0 },
            }],
        });
        conn.execute(
            "INSERT INTO session_message
               (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, ?2, 'assistant', ?3, 0, 0, ?4)",
            (
                format!("msg_{session_id}_{seq}"),
                session_id.to_owned(),
                seq,
                data.to_string(),
            ),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn open_tasks_reads_the_newest_completed_call() {
        let db = Db::open_in_memory().await.unwrap();
        seed_todowrite_row(
            &db,
            "ses_a",
            1,
            json!([{ "content": "old", "status": "pending" }]),
        )
        .await;
        seed_todowrite_row(
            &db,
            "ses_a",
            2,
            json!([
                { "content": "done", "status": "completed" },
                { "content": "gone", "status": "cancelled" },
                { "content": "wip", "status": "in_progress" },
                { "content": "later", "status": "pending" }
            ]),
        )
        .await;
        let open = open_tasks(&db, "ses_a").await.unwrap();
        assert_eq!(
            open,
            vec![
                ("wip".to_owned(), "in_progress".to_owned()),
                ("later".to_owned(), "pending".to_owned())
            ],
            "only pending/in_progress of the NEWEST call"
        );

        // A session that never called todowrite has no open work.
        assert!(open_tasks(&db, "ses_b").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn open_tasks_empty_latest_list_means_nothing_open() {
        let db = Db::open_in_memory().await.unwrap();
        seed_todowrite_row(
            &db,
            "ses_a",
            1,
            json!([{ "content": "old", "status": "pending" }]),
        )
        .await;
        seed_todowrite_row(&db, "ses_a", 2, json!([])).await;
        assert!(
            open_tasks(&db, "ses_a").await.unwrap().is_empty(),
            "the newest call cleared the list; older state must not resurface"
        );
    }

    #[tokio::test]
    async fn open_tasks_skips_non_completed_calls() {
        let db = Db::open_in_memory().await.unwrap();
        seed_todowrite_row(
            &db,
            "ses_a",
            1,
            json!([{ "content": "a", "status": "pending" }]),
        )
        .await;
        // A newer row whose todowrite part is still running does not count.
        let conn = db.acquire().await.unwrap();
        let data = json!({
            "id": "msg_ses_a_2",
            "type": "assistant",
            "time": { "created": 0 },
            "agent": "build",
            "model": { "id": "m", "providerID": "p" },
            "content": [{
                "type": "tool",
                "id": "call_2",
                "name": "todowrite",
                "state": { "status": "running", "input": { "todos": [] }, "metadata": {} },
                "time": { "created": 0 },
            }],
        });
        conn.execute(
            "INSERT INTO session_message
               (id, session_id, type, seq, time_created, time_updated, data)
             VALUES ('msg_ses_a_2', 'ses_a', 'assistant', 2, 0, 0, ?1)",
            [data.to_string()],
        )
        .await
        .unwrap();
        drop(conn);
        assert_eq!(
            open_tasks(&db, "ses_a").await.unwrap(),
            vec![("a".to_owned(), "pending".to_owned())]
        );
    }
}
