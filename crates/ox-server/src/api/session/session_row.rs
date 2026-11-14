use anyhow::Result;
use serde_json::{Map, Value, json};

use ox_db::Db;

// ---------------------------------------------------------------------------
// session_v2 row -> Session.Info
// ---------------------------------------------------------------------------

/// Column list read into [`SessionRow`]; order must match `map_session_row`.
pub(super) const SESSION_COLUMNS: &str = "id, project_id, workspace_id, parent_id, fork_session_id, \
     fork_boundary, agent, model, cost, tokens_input, tokens_output, tokens_reasoning, \
     tokens_cache_read, tokens_cache_write, directory, path, title, metadata, permission, \
     revert, idle_outcome, time_created, time_updated, time_idle, time_viewed, time_archived, time_busy";

/// The `session_v2` columns that make up upstream `Session.Info`. JSON blobs
/// (`model`, `metadata`, `permission`, `revert`, `fork_boundary`) stay raw and
/// are parsed lazily in [`SessionRow::info`].
pub(super) struct SessionRow {
    pub(super) id: String,
    pub(super) project_id: String,
    pub(super) workspace_id: Option<String>,
    parent_id: Option<String>,
    fork_session_id: Option<String>,
    fork_boundary: Option<String>,
    pub(super) agent: Option<String>,
    pub(super) model: Option<String>,
    cost: f64,
    tokens_input: i64,
    tokens_output: i64,
    tokens_reasoning: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
    pub(super) directory: String,
    pub(super) path: Option<String>,
    title: Option<String>,
    pub(super) metadata: Option<String>,
    pub(super) permission: Option<String>,
    revert: Option<String>,
    idle_outcome: Option<String>,
    time_created: i64,
    pub(super) time_updated: i64,
    pub(super) time_idle: Option<i64>,
    pub(super) time_viewed: Option<i64>,
    pub(super) time_archived: Option<i64>,
    time_busy: Option<i64>,
}

pub(super) fn map_session_row(row: &ox_db::turso::Row) -> ox_db::turso::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        workspace_id: row.get(2)?,
        parent_id: row.get(3)?,
        fork_session_id: row.get(4)?,
        fork_boundary: row.get(5)?,
        agent: row.get(6)?,
        model: row.get(7)?,
        cost: row.get(8)?,
        tokens_input: row.get(9)?,
        tokens_output: row.get(10)?,
        tokens_reasoning: row.get(11)?,
        tokens_cache_read: row.get(12)?,
        tokens_cache_write: row.get(13)?,
        directory: row.get(14)?,
        path: row.get(15)?,
        title: row.get(16)?,
        metadata: row.get(17)?,
        permission: row.get(18)?,
        revert: row.get(19)?,
        idle_outcome: row.get(20)?,
        time_created: row.get(21)?,
        time_updated: row.get(22)?,
        time_idle: row.get(23)?,
        time_viewed: row.get(24)?,
        time_archived: row.get(25)?,
        time_busy: row.get(26)?,
    })
}

/// `Money.USD` is a JSON number; whole values are emitted without a trailing
/// `.0` so they compare equal to upstream's integer cents-of-a-dollar literals.
fn cost_value(cost: f64) -> Value {
    if cost.fract() == 0.0 && cost.abs() < 9.007_199_254_740_992e15 {
        json!(cost as i64)
    } else {
        json!(cost)
    }
}

impl SessionRow {
    /// The upstream `Session.Info` object (`additionalProperties: false`).
    pub(super) fn info(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".to_owned(), json!(self.id));
        m.insert("projectID".to_owned(), json!(self.project_id));
        m.insert("cost".to_owned(), cost_value(self.cost));
        m.insert(
            "tokens".to_owned(),
            json!({
                "input": self.tokens_input,
                "output": self.tokens_output,
                "reasoning": self.tokens_reasoning,
                "cache": {
                    "read": self.tokens_cache_read,
                    "write": self.tokens_cache_write,
                },
            }),
        );

        let mut time = Map::new();
        time.insert("created".to_owned(), json!(self.time_created));
        time.insert("updated".to_owned(), json!(self.time_updated));
        if let Some(busy) = self.time_busy {
            time.insert("busy".to_owned(), json!(busy));
        }
        if let Some(idle) = self.time_idle {
            time.insert("idle".to_owned(), json!(idle));
        }
        if let Some(viewed) = self.time_viewed {
            time.insert("viewed".to_owned(), json!(viewed));
        }
        if let Some(archived) = self.time_archived {
            time.insert("archived".to_owned(), json!(archived));
        }
        m.insert("time".to_owned(), Value::Object(time));

        m.insert(
            "location".to_owned(),
            json!({ "directory": self.directory }),
        );

        if let Some(parent_id) = &self.parent_id {
            m.insert("parentID".to_owned(), json!(parent_id));
        }
        if let (Some(fork_session_id), Some(fork_boundary)) =
            (&self.fork_session_id, &self.fork_boundary)
        {
            let boundary = serde_json::from_str::<Value>(fork_boundary).unwrap_or(Value::Null);
            m.insert(
                "fork".to_owned(),
                json!({ "sessionID": fork_session_id, "boundary": boundary }),
            );
        }
        if let Some(agent) = &self.agent {
            m.insert("agent".to_owned(), json!(agent));
        }
        if let Some(raw) = &self.model
            && let Ok(mut model) = serde_json::from_str::<Value>(raw)
            && let Value::Object(object) = &mut model
        {
            // Upstream `fromRow` defaults an absent variant.
            object
                .entry("variant".to_owned())
                .or_insert_with(|| json!("default"));
            m.insert("model".to_owned(), model);
        }
        if let Some(outcome) = &self.idle_outcome {
            m.insert("outcome".to_owned(), json!(outcome));
        }
        if let Some(title) = &self.title {
            m.insert("title".to_owned(), json!(title));
        }
        // Upstream `row.path ? RelativePath.make(row.path) : undefined`: the
        // empty-string root subpath stays absent from the wire shape.
        if self.path.as_deref().is_some_and(|p| !p.is_empty()) {
            m.insert("subpath".to_owned(), json!(self.path));
        }
        insert_json(&mut m, "metadata", &self.metadata);
        insert_json(&mut m, "permissions", &self.permission);
        insert_json(&mut m, "revert", &self.revert);

        Value::Object(m)
    }
}

/// Insert a raw JSON column under `key` when it parses.
fn insert_json(map: &mut Map<String, Value>, key: &str, raw: &Option<String>) {
    if let Some(raw) = raw
        && let Ok(value) = serde_json::from_str::<Value>(raw)
    {
        map.insert(key.to_owned(), value);
    }
}

// ---------------------------------------------------------------------------
// Read helpers
// ---------------------------------------------------------------------------

pub(super) async fn load_session(db: &Db, session_id: &str) -> Result<Option<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2 WHERE id = ?1");
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let row = ox_db::optional(&mut stmt, [session_id]).await?;
    Ok(row.map(|row| map_session_row(&row)).transpose()?)
}

/// Session ids whose `parent_id` is `parent_id` (upstream `list({ parentID })`).
pub(super) async fn load_children(db: &Db, parent_id: &str) -> Result<Vec<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT id FROM session_v2 WHERE parent_id = ?1")
        .await?;
    let mut rows = stmt.query([parent_id]).await?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next().await? {
        ids.push(row.get::<String>(0)?);
    }
    Ok(ids)
}

pub(super) async fn canonical_directory(directory: &str) -> Result<String> {
    let path = directory.to_owned();
    Ok(
        tokio::task::spawn_blocking(move || std::fs::canonicalize(path))
            .await??
            .to_string_lossy()
            .into_owned(),
    )
}

/// `subpath` for a location inside a project directory: the relative remainder,
/// or the empty string when the location *is* the project directory.
///
/// Upstream stores the project-root subpath as `''` — never NULL
/// (`move.ts:89`: `RelativePath.make(path.relative(...))`, which yields `''`
/// at the root) — and the TUI's session list filters `subpath=""` to mean
/// "sessions at the project root". A `None` here would make that page
/// permanently empty.
pub(super) fn subpath_of(directory: &str, project_directory: &str) -> Option<String> {
    let relative = directory.strip_prefix(project_directory)?;
    let trimmed = relative.trim_start_matches('/');
    Some(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::subpath_of;

    #[test]
    fn subpath_of_returns_nested_directory_relative_to_project_root() {
        assert_eq!(
            subpath_of("/workspace/project/packages/app", "/workspace/project"),
            Some("packages/app".to_owned())
        );
    }
}
