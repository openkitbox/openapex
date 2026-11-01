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
