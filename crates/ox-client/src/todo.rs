//! The todo list a `todowrite` call carries. Pure parsing, no state: the
//! current list is always derived from the tool part's input, never stored
//! apart from it (upstream `util/todo.ts`).

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
}

/// Lenient, like upstream `parseTodos`: a non-array is an empty list, an
/// unknown status reads as pending, and an entry without content is dropped.
pub fn parse_todos(value: Option<&Value>) -> Vec<Todo> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let content = item.get("content")?.as_str().filter(|c| !c.is_empty())?;
            let status = match item.get("status").and_then(Value::as_str) {
                Some("in_progress") => TodoStatus::InProgress,
                Some("completed") => TodoStatus::Completed,
                Some("cancelled") => TodoStatus::Cancelled,
                _ => TodoStatus::Pending,
            };
            Some(Todo {
                content: content.to_owned(),
                status,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{TodoStatus, parse_todos};

    #[test]
    fn parses_every_status() {
        let value = json!([
            {"content": "a", "status": "pending"},
            {"content": "b", "status": "in_progress"},
            {"content": "c", "status": "completed"},
            {"content": "d", "status": "cancelled"},
        ]);
        let todos = parse_todos(Some(&value));
        let statuses: Vec<_> = todos.iter().map(|t| t.status).collect();
        assert_eq!(
            statuses,
            [
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Completed,
                TodoStatus::Cancelled
            ]
        );
    }

    #[test]
    fn unknown_status_is_pending_and_empty_content_is_dropped() {
        let value = json!([
            {"content": "kept", "status": "weird"},
            {"content": "", "status": "completed"},
            {"status": "completed"},
            "not a record",
            {"content": 3},
        ]);
        let todos = parse_todos(Some(&value));
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "kept");
        assert_eq!(todos[0].status, TodoStatus::Pending);
    }

    #[test]
    fn a_non_array_is_an_empty_list() {
        assert!(parse_todos(None).is_empty());
        assert!(parse_todos(Some(&json!({"todos": []}))).is_empty());
    }
}
