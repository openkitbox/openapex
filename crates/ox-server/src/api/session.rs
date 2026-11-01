//! Session lifecycle endpoints (Task 4).
//!
//! Ports `packages/server/src/handlers/session.ts` (`session.*`) and
//! `packages/server/src/handlers/message.ts` (`session.messages`).
//!
//! Every mutation is published through [`ox_core::event_bus::EventBus`]; the
//! projector — never these handlers — writes `session_v2` / `session_message`.
//! Reads go straight to SQLite. Response shapes follow
//! `packages/protocol/openapi.json` field-for-field (the TUI deserializes
//! strictly).
//!
//! Deliberate scope limits (documented inline at each site):
//! - `session.update` with an empty title triggers upstream title generation
//!   (a whole model-backed subsystem) — not implemented here.
//! - `session.active` reports the in-process execution set, which ox has no
//!   engine for yet, so it is always empty.
//! - `session.fork` does not snapshot instruction state.

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ox_db::turso::Value as SqlValue;
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;

use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::message::Message;
use ox_schema::support::{
    ForkBoundary, LocationRef, ModelRef, Rule, SessionMetadata, SkillAttachment,
};

use super::{attachment, state};

/// Upstream `DefaultSessionsLimit` / `DefaultMessagesLimit`.
const DEFAULT_PAGE_LIMIT: i64 = 50;

// ---------------------------------------------------------------------------
// Error responses (packages/protocol/errors.ts encoded shapes)
// ---------------------------------------------------------------------------

/// 404 `SessionNotFoundError`.
pub(crate) fn session_not_found(res: &mut Response, session_id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "SessionNotFoundError",
        "sessionID": session_id,
        "message": format!("Session not found: {session_id}"),
    })));
}

/// 404 `MessageNotFoundError`.
fn message_not_found(res: &mut Response, session_id: &str, message_id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "MessageNotFoundError",
        "sessionID": session_id,
        "messageID": message_id,
        "message": format!("Message not found: {message_id}"),
    })));
}

/// 400 `InvalidRequestError`.
pub(crate) fn invalid_request(res: &mut Response, message: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
    })));
}

/// 404 `SkillNotFoundError` (protocol/src/errors.ts, `httpApiStatus: 404`).
fn skill_not_found(res: &mut Response, skill: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "SkillNotFoundError",
        "skill": skill,
        "message": format!("Skill not found: {skill}"),
    })));
}

/// 400 `InvalidRequestError` carrying the `empty_session` kind (fork).
fn invalid_request_kind(res: &mut Response, message: &str, kind: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
        "kind": kind,
    })));
}

/// 400 `InvalidRequestError` naming the offending request `field` (`files`).
fn invalid_request_field(res: &mut Response, message: &str, field: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
        "field": field,
    })));
}

/// 400 `InvalidCursorError` (protocol/src/errors.ts).
fn invalid_cursor(res: &mut Response, message: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidCursorError",
        "message": message,
    })));
}

/// 500 `UnknownError` — never leaks internal detail on the wire.
fn unknown_error(res: &mut Response, error: &anyhow::Error) {
    tracing::error!(error = %error, "session handler failed");
    res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
    res.render(Json(json!({
        "_tag": "UnknownError",
        "message": "Unexpected server error. Check server logs for details.",
    })));
}

/// 204 No Content.
fn no_content(res: &mut Response) {
    res.status_code(StatusCode::NO_CONTENT);
}

/// Largest JSON request body ox accepts.
///
/// salvo's default is 64 KB, which a single pasted message or an inline
/// attachment exceeds — upstream has no such limit, so a client would see
/// `InvalidRequestError` for a legal prompt. A 20 MiB attachment (the
/// materialization limit) is ~27 MB of base64 inside the JSON body, so this
/// leaves headroom above that while staying a bounded read (the body is
/// buffered once).
const MAX_JSON_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Parse an optional JSON request body; an empty body yields `T::default()`
/// (every body schema here has all-optional properties except where the
/// handler validates required fields itself). Malformed JSON is a 400.
async fn parse_body<T>(req: &mut Request, res: &mut Response) -> Option<T>
where
    T: DeserializeOwned + Default,
{
    let bytes = match req.payload_with_max_size(MAX_JSON_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            invalid_request(res, "Invalid request body");
            return None;
        }
    };
    if bytes.is_empty() {
        return Some(T::default());
    }
    match serde_json::from_slice::<T>(bytes) {
        Ok(value) => Some(value),
        Err(_) => {
            invalid_request(res, "Invalid request body");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// session_v2 row -> Session.Info
// ---------------------------------------------------------------------------

/// Column list read into [`SessionRow`]; order must match `map_session_row`.
const SESSION_COLUMNS: &str = "id, project_id, workspace_id, parent_id, fork_session_id, \
     fork_boundary, agent, model, cost, tokens_input, tokens_output, tokens_reasoning, \
     tokens_cache_read, tokens_cache_write, directory, path, title, metadata, permission, \
     revert, idle_outcome, time_created, time_updated, time_idle, time_viewed, time_archived, time_busy";

/// The `session_v2` columns that make up upstream `Session.Info`. JSON blobs
/// (`model`, `metadata`, `permission`, `revert`, `fork_boundary`) stay raw and
/// are parsed lazily in [`SessionRow::info`].
struct SessionRow {
    id: String,
    project_id: String,
    workspace_id: Option<String>,
    parent_id: Option<String>,
    fork_session_id: Option<String>,
    fork_boundary: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    cost: f64,
    tokens_input: i64,
    tokens_output: i64,
    tokens_reasoning: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
    directory: String,
    path: Option<String>,
    title: Option<String>,
    metadata: Option<String>,
    permission: Option<String>,
    revert: Option<String>,
    idle_outcome: Option<String>,
    time_created: i64,
    time_updated: i64,
    time_idle: Option<i64>,
    time_viewed: Option<i64>,
    time_archived: Option<i64>,
    time_busy: Option<i64>,
}

fn map_session_row(row: &ox_db::turso::Row) -> ox_db::turso::Result<SessionRow> {
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
    fn info(&self) -> Value {
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

async fn load_session(db: &Db, session_id: &str) -> Result<Option<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2 WHERE id = ?1");
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let row = ox_db::optional(&mut stmt, [session_id]).await?;
    Ok(row.map(|row| map_session_row(&row)).transpose()?)
}

/// Session ids whose `parent_id` is `parent_id` (upstream `list({ parentID })`).
async fn load_children(db: &Db, parent_id: &str) -> Result<Vec<String>> {
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

/// The project a session belongs to: the shared `project` table row whose
/// `worktree` is the directory, else the synthetic `default` project rooted at
/// the directory itself (ox has no worktree discovery).
pub(crate) async fn resolve_project(db: &Db, directory: &str) -> (String, String) {
    if db.table_exists("project").await.unwrap_or(false) {
        // A missing or unreadable `project` table falls through to the
        // synthetic project, exactly as the old `.call` error did.
        let found: Result<Option<String>> = async {
            let conn = db.acquire().await?;
            let mut stmt = conn
                .prepare("SELECT id FROM project WHERE worktree = ?1 LIMIT 1")
                .await?;
            let row = ox_db::optional(&mut stmt, [directory]).await?;
            Ok(row.map(|row| row.get::<String>(0)).transpose()?)
        }
        .await;
        if let Ok(Some(id)) = found {
            return (id, directory.to_owned());
        }
    }
    ("default".to_owned(), directory.to_owned())
}

/// `subpath` for a location inside a project directory: the relative remainder,
/// or the empty string when the location *is* the project directory.
///
/// Upstream stores the project-root subpath as `''` — never NULL
/// (`move.ts:89`: `RelativePath.make(path.relative(...))`, which yields `''`
/// at the root) — and the TUI's session list filters `subpath=""` to mean
/// "sessions at the project root". A `None` here would make that page
/// permanently empty.
fn subpath_of(directory: &str, project_directory: &str) -> Option<String> {
    let relative = directory.strip_prefix(project_directory)?;
    let trimmed = relative.trim_start_matches('/');
    Some(trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// Pagination cursors
// ---------------------------------------------------------------------------

/// Upstream `Session.ListAnchor` direction.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Direction {
    Previous,
    Next,
}

/// The request-shaped subset stored inside an opaque cursor. Field names match
/// the query parameters so a cursor round-trips verbatim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CursorQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(default, rename = "parentID", skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Anchor {
    id: String,
    time: i64,
    direction: Direction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionsCursor {
    query: CursorQuery,
    anchor: Anchor,
}

/// The message-list cursor (`{ id, order, direction }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessagesCursor {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order: Option<String>,
    direction: Direction,
}

fn encode_cursor<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_vec(value)
        .ok()
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor<T: DeserializeOwned>(raw: &str) -> Option<T> {
    URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Effective ascending order: "previous" walks the opposite way.
fn ascending_of(order: Option<&str>, direction: Direction) -> bool {
    let requested_asc = order == Some("asc");
    match direction {
        Direction::Previous => !requested_asc,
        Direction::Next => requested_asc,
    }
}

/// `PositiveInt` limit: a positive integer, defaulting to the page size.
fn resolve_limit(raw: Option<String>) -> Result<i64, String> {
    match raw {
        None => Ok(DEFAULT_PAGE_LIMIT),
        Some(value) => match value.parse::<i64>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("Invalid limit: {value}")),
        },
    }
}

/// `archived` list filter: `true` returns only archived sessions, `false` only
/// live ones, absent returns both (upstream `store.ts:116` adds the condition
/// only when the parameter is defined). Anything else is rejected rather than
/// silently treated as absent.
fn resolve_archived(raw: Option<String>) -> Result<Option<bool>, String> {
    match raw.as_deref() {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(value) => Err(format!("Invalid archived: {value}")),
    }
}

fn resolve_order(raw: Option<String>) -> Result<Option<String>, String> {
    match raw.as_deref() {
        None => Ok(None),
        Some("asc") => Ok(Some("asc".to_owned())),
        Some("desc") => Ok(Some("desc".to_owned())),
        Some(other) => Err(format!("Invalid order: {other}")),
    }
}

// ---------------------------------------------------------------------------
// session.list — GET /api/session
// ---------------------------------------------------------------------------

struct SessionsPage {
    directory: Option<String>,
    project: Option<String>,
    subpath: Option<String>,
    search: Option<String>,
    parent_id: Option<String>,
    archived: Option<bool>,
    ascending: bool,
    anchor: Option<(i64, String)>,
    limit: i64,
}

async fn query_sessions(db: &Db, page: SessionsPage) -> Result<Vec<SessionRow>> {
    let SessionsPage {
        directory,
        project,
        subpath,
        search,
        parent_id,
        archived,
        ascending,
        anchor,
        limit,
    } = page;

    let mut sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2");
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(directory) = directory {
        params.push(SqlValue::Text(directory));
        conds.push(format!("directory = ?{}", params.len()));
    }
    if let Some(project) = project {
        params.push(SqlValue::Text(project));
        conds.push(format!("project_id = ?{}", params.len()));
    }
    if let Some(subpath) = subpath {
        params.push(SqlValue::Text(subpath));
        conds.push(format!("path = ?{}", params.len()));
    }
    if let Some(search) = search {
        params.push(SqlValue::Text(format!("%{search}%")));
        conds.push(format!("title LIKE ?{}", params.len()));
    }
    if let Some(archived) = archived {
        conds.push(
            if archived {
                "time_archived IS NOT NULL"
            } else {
                "time_archived IS NULL"
            }
            .to_owned(),
        );
    }
    if let Some(parent_id) = parent_id {
        if parent_id == "null" {
            conds.push("parent_id IS NULL".to_owned());
        } else {
            params.push(SqlValue::Text(parent_id));
            conds.push(format!("parent_id = ?{}", params.len()));
        }
    }
    if let Some((time, anchor_id)) = anchor {
        params.push(SqlValue::Integer(time));
        let time_idx = params.len();
        params.push(SqlValue::Text(anchor_id));
        let id_idx = params.len();
        if ascending {
            conds.push(format!(
                "(time_updated > ?{time_idx} OR (time_updated = ?{time_idx} AND id > ?{id_idx}))"
            ));
        } else {
            conds.push(format!(
                "(time_updated < ?{time_idx} OR (time_updated = ?{time_idx} AND id < ?{id_idx}))"
            ));
        }
    }

    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(if ascending {
        " ORDER BY time_updated ASC, id ASC"
    } else {
        " ORDER BY time_updated DESC, id DESC"
    });
    sql.push_str(&format!(" LIMIT {limit}"));

    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(map_session_row(&row)?);
    }
    Ok(out)
}

#[handler] // operation_id: session.list
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };

    let raw_cursor: Option<String> = req.query("cursor");
    // A cursor carries the whole query including its order, so a request-level
    // `order` alongside a cursor is simply ignored (upstream `session.list`).

    let decoded: Option<SessionsCursor> = match raw_cursor.as_deref() {
        Some(raw) => match decode_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                invalid_cursor(res, "Invalid cursor");
                return;
            }
        },
        None => None,
    };

    // Validate the request-level filters even when a cursor carries the query:
    // an unparseable value fails closed instead of being silently ignored.
    let archived = match resolve_archived(req.query::<String>("archived")) {
        Ok(archived) => archived,
        Err(message) => {
            invalid_request(res, &message);
            return;
        }
    };

    // A cursor carries the whole query; otherwise read it from the request.
    let (query, direction) = match &decoded {
        Some(cursor) => (cursor.query.clone(), cursor.anchor.direction),
        None => {
            let query = match request_query(req) {
                Ok(mut query) => {
                    query.archived = archived;
                    query
                }
                Err(message) => {
                    invalid_request(res, &message);
                    return;
                }
            };
            (query, Direction::Next)
        }
    };

    let limit = match resolve_limit(req.query::<String>("limit")) {
        Ok(limit) => limit,
        Err(message) => {
            invalid_request(res, &message);
            return;
        }
    };
    let ascending = ascending_of(query.order.as_deref(), direction);
    let anchor = decoded
        .as_ref()
        .map(|cursor| (cursor.anchor.time, cursor.anchor.id.clone()));

    let page = SessionsPage {
        directory: query.directory.clone(),
        project: query.project.clone(),
        subpath: query.subpath.clone(),
        search: query.search.clone(),
        parent_id: query.parent_id.clone(),
        archived: query.archived,
        ascending,
        anchor,
        limit,
    };

    let rows = match query_sessions(&app.db, page).await {
        Ok(rows) => rows,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    // The fetch walks opposite to the requested order for `previous`; reverse
    // so the page comes out in the requested order (upstream store.ts).
    let mut rows = rows;
    if direction == Direction::Previous {
        rows.reverse();
    }

    let data: Vec<Value> = rows.iter().map(SessionRow::info).collect();
    let previous = rows.first().map(|row| {
        encode_cursor(&SessionsCursor {
            query: query.clone(),
            anchor: Anchor {
                id: row.id.clone(),
                time: row.time_updated,
                direction: Direction::Previous,
            },
        })
    });
    let next = rows.last().map(|row| {
        encode_cursor(&SessionsCursor {
            query: query.clone(),
            anchor: Anchor {
                id: row.id.clone(),
                time: row.time_updated,
                direction: Direction::Next,
            },
        })
    });

    res.render(Json(json!({
        "data": data,
        "cursor": {
            "previous": previous.flatten(),
            "next": next.flatten(),
        },
    })));
}

/// Read the session-list filter query from the request.
fn request_query(req: &Request) -> Result<CursorQuery, String> {
    Ok(CursorQuery {
        order: resolve_order(req.query::<String>("order"))?,
        search: req.query("search"),
        parent_id: req.query("parentID"),
        // ox maps every directory to one synthetic project, so a `project`
        // filter (the TUI's `/sessions`) alone spans all directories. Scope it
        // to the directory the client is working in unless `directory` is
        // given explicitly.
        directory: req.query("directory").or_else(|| {
            req.query::<String>("project")
                .and(super::compat::request_directory(req))
        }),
        project: req.query("project"),
        subpath: req.query("subpath"),
        // Filled in by `list` so it can be validated even with a cursor.
        archived: None,
    })
}

// ---------------------------------------------------------------------------
// session.create — POST /api/session
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct CreateBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<ModelRef>,
    #[serde(default)]
    location: Option<LocationRef>,
    #[serde(default)]
    metadata: Option<SessionMetadata>,
    #[serde(default)]
    permissions: Option<Vec<Rule>>,
}

#[handler] // operation_id: session.create
pub(crate) async fn create(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(body) = parse_body::<CreateBody>(req, res).await else {
        return;
    };

    let session_id = body.id.clone().unwrap_or_else(|| id::id("ses"));

    // Idempotent create: an existing row wins (upstream `store.get` shortcut).
    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => {
            res.render(Json(json!({ "data": row.info() })));
            return;
        }
        Ok(None) => {}
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    // Upstream resolves the create directory from the request itself when the
    // body carries none (`location.ts:73-75`): `location[directory]` query,
    // then the percent-encoded `x-opencode-directory` header, then the
    // server's directory.
    let request_directory = super::compat::request_directory(req);

    // Location, metadata and permissions are inherited from the parent the way
    // upstream inherits them in `Session.create`.
    let (location, subpath, parent_metadata, parent_permissions, parent_agent, parent_model) =
        match &body.parent_id {
            Some(parent_id) => match load_session(&app.db, parent_id).await {
                Ok(Some(parent)) => {
                    let model = parent
                        .model
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<ModelRef>(raw).ok());
                    let metadata = parent
                        .metadata
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<SessionMetadata>(raw).ok());
                    let permissions = parent
                        .permission
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Vec<Rule>>(raw).ok());
                    (
                        LocationRef {
                            directory: parent.directory.clone(),
                            workspace_id: parent.workspace_id.clone(),
                        },
                        parent.path.clone(),
                        metadata,
                        permissions,
                        parent.agent.clone(),
                        model,
                    )
                }
                Ok(None) => {
                    session_not_found(res, parent_id);
                    return;
                }
                Err(error) => {
                    unknown_error(res, &error);
                    return;
                }
            },
            None => {
                let directory = body
                    .location
                    .as_ref()
                    .map(|l| l.directory.clone())
                    .or(request_directory)
                    .unwrap_or_else(|| app.directory.to_string_lossy().into_owned());
                let (_, project_directory) = resolve_project(&app.db, &directory).await;
                (
                    LocationRef {
                        directory: directory.clone(),
                        workspace_id: body.location.as_ref().and_then(|l| l.workspace_id.clone()),
                    },
                    subpath_of(&directory, &project_directory),
                    None,
                    None,
                    None,
                    None,
                )
            }
        };

    let (project_id, _) = resolve_project(&app.db, &location.directory).await;

    let event = SessionEventData::SessionCreated {
        session_id: session_id.clone(),
        project_id,
        location,
        subpath,
        parent_id: body.parent_id.clone(),
        slug: ox_schema::slug::create(),
        title: body.title.clone(),
        agent: body.agent.clone().or(parent_agent),
        model: body.model.clone().or(parent_model),
        metadata: body.metadata.clone().or(parent_metadata),
        permissions: body.permissions.clone().or(parent_permissions),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };

    if let Err(error) = app.bus.publish(event).await {
        // A concurrent create that lost the projection race is not an error:
        // the existing row wins.
        match load_session(&app.db, &session_id).await {
            Ok(Some(row)) => {
                res.render(Json(json!({ "data": row.info() })));
            }
            _ => unknown_error(res, &error),
        }
        return;
    }

    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => unknown_error(res, &anyhow!("session `{session_id}` missing after create")),
        Err(error) => unknown_error(res, &error),
    }
}

// ---------------------------------------------------------------------------
// session.get — GET /api/session/{sessionID}
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.get
pub(crate) async fn get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => session_not_found(res, &session_id),
        Err(error) => unknown_error(res, &error),
    }
}

// ---------------------------------------------------------------------------
// session.update — PATCH /api/session/{sessionID}
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct UpdateBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    metadata: Option<SessionMetadata>,
    #[serde(default)]
    permissions: Option<Vec<Rule>>,
    /// `true` archives the session, `false` restores it (upstream
    /// `session.update`); repeating the current state is a no-op.
    #[serde(default)]
    archived: Option<bool>,
}

#[handler] // operation_id: session.update
pub(crate) async fn update(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<UpdateBody>(req, res).await else {
        return;
    };

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    if let Some(title) = &body.title {
        if !title.is_empty() {
            if let Err(error) = app
                .bus
                .publish(SessionEventData::SessionRenamed {
                    session_id: session_id.clone(),
                    title: title.clone(),
                })
                .await
            {
                unknown_error(res, &error);
                return;
            }
        } else {
            // An empty title asks for a generated one (upstream handlers/
            // session.ts:234 → `title.generate`). Generation is background
            // work: the request answers 204 and the title arrives as a
            // `session.renamed` event, exactly as upstream does.
            spawn_title_generation(app, session_id.clone());
        }
    }

    if let Some(metadata) = body.metadata
        && let Err(error) = app
            .bus
            .publish(SessionEventData::SessionMetadataUpdated {
                session_id: session_id.clone(),
                metadata,
            })
            .await
    {
        unknown_error(res, &error);
        return;
    }

    if let Some(permissions) = body.permissions
        && let Err(error) = app
            .bus
            .publish(SessionEventData::SessionPermissions {
                session_id: session_id.clone(),
                permissions,
            })
            .await
    {
        unknown_error(res, &error);
        return;
    }

    if let Some(archived) = body.archived {
        // Upstream `Session.archive`/`Session.unarchive`: repeating the current
        // state is a no-op, so no second event and no `time_updated` bump.
        let already = session.time_archived.is_some();
        if archived != already {
            let event = if archived {
                SessionEventData::SessionArchived {
                    session_id: session_id.clone(),
                }
            } else {
                SessionEventData::SessionUnarchived {
                    session_id: session_id.clone(),
                }
            };
            if let Err(error) = app.bus.publish(event).await {
                unknown_error(res, &error);
                return;
            }
        }
    }

    no_content(res);
}

/// Spawn one background title generation for `session_id` (upstream
/// `SessionTitle.generate`). Fire-and-forget: title generation is advisory, so
/// a failure is logged inside the task and never reaches the response.
fn spawn_title_generation(app: &crate::api::AppState, session_id: String) {
    let db = app.db.clone();
    let bus = app.bus.clone();
    // One snapshot, so chat, catalog and config share a generation.
    let svc = app.services();
    let chat = Arc::clone(&svc.chat);
    let config = Arc::clone(&svc.config);
    let catalog = Arc::clone(&svc.model_catalog);
    tokio::spawn(async move {
        ox_engine::runner::title::generate(db, bus, chat, config, catalog, session_id).await;
    });
}

// ---------------------------------------------------------------------------
// session.remove — DELETE /api/session/{sessionID}
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.remove
pub(crate) async fn remove(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };

    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    if let Err(error) = delete_session_tree(app, &session_id).await {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

/// Delete `root` and every descendant: descendants first (deepest last-found
/// first), then the root. Each session goes through a `SessionDeleted` event
/// (so the projection, the event log and SSE subscribers all agree), then its
/// durable event log is purged and the runner forgets it. Stops at the first
/// error; sessions already deleted stay deleted and the root is still present,
/// so a retry finishes the job. Shared by the HTTP handler and any internal
/// caller that must delete exactly as the API does.
pub(crate) async fn delete_session_tree(app: &crate::api::AppState, root: &str) -> Result<()> {
    let mut order = collect_descendants(&app.db, root).await?;
    order.reverse();
    order.push(root.to_owned());

    for target in order {
        app.bus
            .publish(SessionEventData::SessionDeleted {
                session_id: target.clone(),
            })
            .await?;
        app.bus.remove(&target).await?;
        // Drop the runner's per-session tables now rather than waiting for the
        // idle sweep's 30-minute TTL (the sweep only runs inside a dispatch, so
        // a deleted session's entry could otherwise outlive it indefinitely).
        app.runner.forget_session(&target);
    }
    Ok(())
}

/// Every descendant of `root`, breadth-first (parents before children).
async fn collect_descendants(db: &Db, root: &str) -> Result<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut collected = Vec::new();
    let mut frontier = vec![root.to_owned()];
    while let Some(parent) = frontier.pop() {
        for child in load_children(db, &parent).await? {
            if seen.insert(child.clone()) {
                collected.push(child.clone());
                frontier.push(child);
            }
        }
    }
    Ok(collected)
}

// ---------------------------------------------------------------------------
// session.fork — POST /api/session/{sessionID}/fork
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct ForkBody {
    #[serde(default)]
    before: Option<String>,
}

#[handler] // operation_id: session.fork
pub(crate) async fn fork(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ForkBody>(req, res).await else {
        return;
    };

    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    let boundary = match find_fork_boundary(&app.db, &session_id, body.before.as_deref()).await {
        Ok(boundary) => boundary,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    let boundary = match boundary {
        Some(boundary) => boundary,
        None => {
            if let Some(message_id) = &body.before {
                message_not_found(res, &session_id, message_id);
            } else {
                invalid_request_kind(res, "Session has no messages to fork", "empty_session");
            }
            return;
        }
    };

    let new_session_id = id::id("ses");
    let event = SessionEventData::SessionForked {
        session_id: new_session_id.clone(),
        parent_id: session_id.clone(),
        boundary,
        instructions: None,
        instruction_entries: None,
    };

    if let Err(error) = app.bus.publish(event).await {
        unknown_error(res, &error);
        return;
    }

    match load_session(&app.db, &new_session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => unknown_error(res, &anyhow!("forked session `{new_session_id}` missing")),
        Err(error) => unknown_error(res, &error),
    }
}

/// The fork boundary: the named message (`before`, exclusive) or the newest
/// message (`through`, inclusive). Mirrors upstream `Session.fork`.
async fn find_fork_boundary(
    db: &Db,
    session_id: &str,
    before: Option<&str>,
) -> Result<Option<ForkBoundary>> {
    let conn = db.acquire().await?;
    let found: Option<String> = match before {
        Some(message_id) => {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM session_message WHERE session_id = ?1 AND id = ?2 \
                     ORDER BY seq DESC LIMIT 1",
                )
                .await?;
            ox_db::optional(&mut stmt, (session_id, message_id))
                .await?
                .map(|row| row.get::<String>(0))
                .transpose()?
        }
        None => {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM session_message WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                )
                .await?;
            ox_db::optional(&mut stmt, [session_id])
                .await?
                .map(|row| row.get::<String>(0))
                .transpose()?
        }
    };

    Ok(found.map(|message_id| match before {
        Some(_) => ForkBoundary::Before { message_id },
        None => ForkBoundary::Through { message_id },
    }))
}

// ---------------------------------------------------------------------------
// session.view — POST /api/session/{sessionID}/view
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct ViewBody {
    #[serde(default)]
    idle: Option<i64>,
}

#[handler] // operation_id: session.view
pub(crate) async fn view(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ViewBody>(req, res).await else {
        return;
    };
    let Some(idle) = body.idle else {
        invalid_request(res, "Missing idle");
        return;
    };

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    // Upstream `Session.view`: only publish when the observed idle watermark is
    // new (not before the last idle transition, not already acknowledged).
    let should_publish = match session.time_idle {
        None => false,
        Some(time_idle) => {
            idle <= time_idle
                && session
                    .time_viewed
                    .map(|viewed| viewed < idle)
                    .unwrap_or(true)
        }
    };
    if should_publish
        && let Err(error) = app
            .bus
            .publish(SessionEventData::SessionViewed {
                session_id: session_id.clone(),
                idle,
            })
            .await
    {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

// ---------------------------------------------------------------------------
// session.switchAgent — POST /api/session/{sessionID}/agent
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct AgentBody {
    #[serde(default)]
    agent: Option<String>,
}

#[handler] // operation_id: session.switchAgent
pub(crate) async fn switch_agent(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<AgentBody>(req, res).await else {
        return;
    };
    let Some(agent) = body.agent else {
        invalid_request(res, "Missing agent");
        return;
    };

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    if let Err(error) = app
        .bus
        .publish(SessionEventData::SessionAgentSelected {
            session_id: session_id.clone(),
            agent,
            previous: session.agent,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

// ---------------------------------------------------------------------------
// session.switchModel — POST /api/session/{sessionID}/model
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct ModelBody {
    #[serde(default)]
    model: Option<ModelRef>,
}

#[handler] // operation_id: session.switchModel
pub(crate) async fn switch_model(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ModelBody>(req, res).await else {
        return;
    };
    let Some(model) = body.model else {
        invalid_request(res, "Missing model");
        return;
    };

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    let previous = session
        .model
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ModelRef>(raw).ok());

    // Upstream `Session.switchModel`: no-op when the model is unchanged.
    let unchanged = previous.as_ref().is_some_and(|current| {
        current.provider_id == model.provider_id
            && current.id == model.id
            && variant_of(current) == variant_of(&model)
    });
    if unchanged {
        no_content(res);
        return;
    }

    // Normalize the variant the way `session.get` renders it
    // (`variant ?? "default"`, session/info.ts) so the event, the projected
    // row, and the `model-switched` message all carry a concrete variant.
    let mut model = model;
    if model.variant.is_none() {
        model.variant = Some("default".to_owned());
    }
    let previous = previous.map(|mut current| {
        if current.variant.is_none() {
            current.variant = Some("default".to_owned());
        }
        current
    });

    if let Err(error) = app
        .bus
        .publish(SessionEventData::SessionModelSelected {
            session_id: session_id.clone(),
            model,
            previous,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

fn variant_of(model: &ModelRef) -> &str {
    model.variant.as_deref().unwrap_or("default")
}

// ---------------------------------------------------------------------------
// session.active — GET /api/session/active
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.active
pub(crate) async fn active(_req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(_app) = state(depot, res) else {
        return;
    };
    // Upstream reports the in-process `SessionExecution.active` set. ox has no
    // execution engine yet (a later task) so nothing can be running and the map
    // is always empty.
    let data: Map<String, Value> = Map::new();
    res.render(Json(json!({ "data": data })));
}

// ---------------------------------------------------------------------------
// session.message.list — GET /api/session/{sessionID}/message
// ---------------------------------------------------------------------------

struct MessagesPage {
    session_id: String,
    kind: Option<String>,
    ascending: bool,
    anchor_seq: Option<i64>,
    limit: i64,
    reverse: bool,
}

async fn query_messages(db: &Db, page: MessagesPage) -> Result<Vec<Message>> {
    let MessagesPage {
        session_id,
        kind,
        ascending,
        anchor_seq,
        limit,
        reverse,
    } = page;

    let mut sql = String::from("SELECT id, type, data FROM session_message WHERE session_id = ?1");
    let mut params: Vec<SqlValue> = vec![SqlValue::Text(session_id.clone())];
    if let Some(kind) = kind {
        params.push(SqlValue::Text(kind));
        sql.push_str(&format!(" AND type = ?{}", params.len()));
    }
    if let Some(seq) = anchor_seq {
        params.push(SqlValue::Integer(seq));
        sql.push_str(&format!(
            " AND seq {} ?{}",
            if ascending { ">" } else { "<" },
            params.len()
        ));
    }
    sql.push_str(if ascending {
        " ORDER BY seq ASC"
    } else {
        " ORDER BY seq DESC"
    });
    sql.push_str(&format!(" LIMIT {limit}"));

    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut values: Vec<Value> = Vec::new();
    while let Some(row) = rows.next().await? {
        let message_id = row.get::<String>(0)?;
        let kind = row.get::<String>(1)?;
        let data = row.get::<String>(2)?;
        let mut value: Value =
            serde_json::from_str(&data).context("decode session_message.data")?;
        // The overlay addresses a row by its id and type, which live in the
        // columns: merge them in the shape the request side decodes.
        Message::merge_row_columns(&message_id, &kind, &mut value);
        values.push(value);
    }
    drop(stmt);
    // A dropped item shows its placeholder here too: the transcript a human
    // reads must be the transcript the model was given, or `ctx_reduce` looks
    // like it did nothing. Cosmetic, so it never fails the read — the rows still
    // hold the original bytes and the model's own path treats this layer the
    // same way.
    if let Err(error) =
        ox_engine::mctx::tagger::apply_transcript_overlay(db, &session_id, &mut values).await
    {
        tracing::warn!(session = %session_id, error = %error, "drop overlay failed; serving the raw transcript");
    }
    if reverse {
        values.reverse();
    }
    values
        .into_iter()
        .map(|value| Message::from_merged_row(value).context("decode session message row"))
        .collect()
}

async fn message_seq(db: &Db, session_id: &str, message_id: &str) -> Result<Option<i64>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2")
        .await?;
    let row = ox_db::optional(&mut stmt, (session_id, message_id)).await?;
    Ok(row.map(|row| row.get::<i64>(0)).transpose()?)
}

#[handler] // operation_id: session.message.list
pub(crate) async fn message_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };

    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    let raw_cursor: Option<String> = req.query("cursor");
    let raw_order: Option<String> = req.query("order");
    if raw_cursor.is_some() && raw_order.is_some() {
        invalid_cursor(res, "Cursor cannot be combined with order");
        return;
    }

    let decoded: Option<MessagesCursor> = match raw_cursor.as_deref() {
        Some(raw) => match decode_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                invalid_cursor(res, "Invalid cursor");
                return;
            }
        },
        None => None,
    };

    // The effective order is always concrete: a cursor's embedded order wins,
    // then the request's, defaulting to newest-first.
    let order: Option<String> = match &decoded {
        Some(cursor) => Some(cursor.order.clone().unwrap_or_else(|| "desc".to_owned())),
        None => match resolve_order(raw_order) {
            Ok(order) => Some(order.unwrap_or_else(|| "desc".to_owned())),
            Err(message) => {
                invalid_request(res, &message);
                return;
            }
        },
    };
    let limit = match resolve_limit(req.query::<String>("limit")) {
        Ok(limit) => limit,
        Err(message) => {
            invalid_request(res, &message);
            return;
        }
    };
    let kind: Option<String> = req.query("type");

    let direction = decoded
        .as_ref()
        .map(|cursor| cursor.direction)
        .unwrap_or(Direction::Next);
    let ascending = ascending_of(order.as_deref(), direction);

    let anchor_seq = match &decoded {
        Some(cursor) => match message_seq(&app.db, &session_id, &cursor.id).await {
            Ok(Some(seq)) => Some(seq),
            Ok(None) => {
                // A cursor pointing at a vanished message yields an empty page.
                render_message_page(res, Vec::new(), &order);
                return;
            }
            Err(error) => {
                unknown_error(res, &error);
                return;
            }
        },
        None => None,
    };

    let page = MessagesPage {
        session_id,
        kind,
        ascending,
        anchor_seq,
        limit,
        reverse: direction == Direction::Previous,
    };
    let messages = match query_messages(&app.db, page).await {
        Ok(messages) => messages,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    render_message_page(res, messages, &order);
}

fn render_message_page(res: &mut Response, messages: Vec<Message>, order: &Option<String>) {
    let data: Vec<Value> = messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
        .collect();
    let previous = messages.first().map(|message| {
        encode_cursor(&MessagesCursor {
            id: message.id().to_owned(),
            order: order.clone(),
            direction: Direction::Previous,
        })
    });
    let next = messages.last().map(|message| {
        encode_cursor(&MessagesCursor {
            id: message.id().to_owned(),
            order: order.clone(),
            direction: Direction::Next,
        })
    });
    res.render(Json(json!({
        "data": data,
        "cursor": {
            "previous": previous.flatten(),
            "next": next.flatten(),
        },
    })));
}

// ---------------------------------------------------------------------------
// session.message.get — GET /api/session/{sessionID}/message/{messageID}
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.message.get
pub(crate) async fn message_get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(message_id) = req.param::<String>("messageID") else {
        invalid_request(res, "Missing messageID");
        return;
    };

    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    let row: Result<Option<(String, String)>> = async {
        let conn = app.db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT type, data FROM session_message WHERE session_id = ?1 AND id = ?2")
            .await?;
        let Some(row) =
            ox_db::optional(&mut stmt, (session_id.as_str(), message_id.as_str())).await?
        else {
            return Ok(None);
        };
        Ok(Some((row.get::<String>(0)?, row.get::<String>(1)?)))
    }
    .await;
    let row = match row {
        Ok(row) => row,
        Err(error) => {
            unknown_error(res, &anyhow!("message lookup failed: {error}"));
            return;
        }
    };

    let Some((kind, data)) = row else {
        message_not_found(res, &session_id, &message_id);
        return;
    };
    let mut value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(error) => {
            unknown_error(res, &anyhow::Error::from(error));
            return;
        }
    };
    // The overlay addresses a row by its id and type (see `query_messages`).
    Message::merge_row_columns(&message_id, &kind, &mut value);
    let mut values = [value];
    if let Err(error) =
        ox_engine::mctx::tagger::apply_transcript_overlay(&app.db, &session_id, &mut values).await
    {
        tracing::warn!(session = %session_id, error = %error, "drop overlay failed; serving the raw message");
    }
    let [value] = values;
    match Message::from_merged_row(value) {
        Ok(message) => res.render(Json(json!({ "data": message }))),
        Err(error) => unknown_error(res, &anyhow::Error::from(error)),
    }
}

// ---------------------------------------------------------------------------
// session.prompt — POST /api/session/{sessionID}/prompt
// ---------------------------------------------------------------------------

/// Request body of `session.prompt` (OpenAPI `PromptInput`): the prompt
/// fields plus admission controls. `delivery` defaults to `steer` upstream.
#[derive(Default, Deserialize)]
struct PromptBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Option<Vec<ox_schema::support::FileInput>>,
    #[serde(default)]
    agents: Option<Vec<ox_schema::support::AgentAttachment>>,
    #[serde(default)]
    skills: Option<Vec<ox_schema::support::SkillAttachment>>,
    #[serde(default)]
    metadata: Option<ox_schema::support::Record>,
    #[serde(default)]
    delivery: Option<ox_schema::inbox::Delivery>,
    #[serde(default)]
    resume: Option<bool>,
}

/// 409 `ConflictError` (protocol/src/errors.ts).
fn conflict_error(res: &mut Response, message: &str, resource: &str) {
    res.status_code(StatusCode::CONFLICT);
    res.render(Json(json!({
        "_tag": "ConflictError",
        "message": message,
        "resource": resource,
    })));
}

/// Resolve prompt skill attachments against an already-discovered skill set
/// (upstream `SessionPrompt.prepare`): ids become `{id, name, text}` with the
/// `<skill_content>` envelope, the first occurrence of an id carries the body
/// and later duplicates do not (upstream's de-dup), and an unknown id fails
/// the whole request with `SkillNotFoundError`.
///
/// Lookup is by id only — a display name in the `id` slot is an unknown id —
/// and touches no filesystem beyond a per-skill companion scan: discovery is
/// the caller's, one scan per admission answers every id here.
///
/// The returned `Err` id becomes a 404 `SkillNotFoundError`, which therefore
/// means either "no such skill" or "the skill exists but could not be read":
/// discovery skips unreadable roots and files, so both fail closed alike.
fn resolve_skills(
    skills: &[ox_tools::skills::Skill],
    requested: Option<Vec<SkillAttachment>>,
) -> Result<Option<Vec<SkillAttachment>>, String> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    if requested.is_empty() {
        return Ok(None);
    }
    // Resolved id -> display name, in first-occurrence order (upstream's
    // `prepared` map): a repeat emits its name but never the body again.
    let mut prepared: Vec<(String, String)> = Vec::with_capacity(requested.len());
    let mut resolved: Vec<SkillAttachment> = Vec::with_capacity(requested.len());
    for attachment in requested {
        let id = attachment.id;
        let mention = attachment.mention;
        if let Some((_, name)) = prepared.iter().find(|(known, _)| *known == id) {
            resolved.push(SkillAttachment {
                id,
                name: Some(name.clone()),
                text: None,
                mention,
            });
            continue;
        }
        let Some(skill) =
            ox_tools::skills::prepare_from(skills, &id, &ox_core::config::user_home())
        else {
            return Err(id);
        };
        prepared.push((skill.id.clone(), skill.name.clone()));
        resolved.push(SkillAttachment {
            id: skill.id,
            name: Some(skill.name),
            text: Some(skill.text),
            mention,
        });
    }
    Ok(Some(resolved))
}

#[handler] // operation_id: session.prompt
pub(crate) async fn prompt(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<PromptBody>(req, res).await else {
        return;
    };
    let Some(text) = body.text else {
        invalid_request(res, "Missing text");
        return;
    };
    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    let message_id = body.id.clone().unwrap_or_else(|| id::id("msg"));
    let delivery = body.delivery.unwrap_or(ox_schema::inbox::Delivery::Steer);
    // Upstream `admission.reconcile`: re-prompting with the same id and
    // delivery is idempotent; a delivery mismatch is a prompt conflict.
    let payload = match load_inbox_row(&app.db, &session_id, &message_id).await {
        Ok(Some(existing)) => {
            if existing.delivery != delivery {
                conflict_error(
                    res,
                    &format!(
                        "Prompt message ID conflicts with an existing durable record: {message_id}"
                    ),
                    &message_id,
                );
                return;
            }
            let info = existing.info;
            if body.resume != Some(false)
                && let Err(error) = app.runner.dispatch(&session_id, &message_id).await
            {
                unknown_error(res, &error);
                return;
            }
            res.render(Json(json!({ "data": info })));
            return;
        }
        // First admission only: skills resolve against the session's directory
        // (upstream `SessionPrompt.prepare`, which runs after the inbox
        // reconcile). A replay returns the payload stored back then — body
        // included, even if the skill was renamed or deleted since — so every
        // stored payload carries the resolved body while a replay scans
        // nothing. Discovery happens once here and answers every requested id,
        // and a prompt that requests no skill scans nothing at all.
        Ok(None) => {
            let discovered = if body
                .skills
                .as_ref()
                .is_some_and(|skills| !skills.is_empty())
            {
                ox_tools::skills::discover(Path::new(&session.directory))
            } else {
                Vec::new()
            };
            let skills = match resolve_skills(&discovered, body.skills) {
                Ok(skills) => skills,
                Err(skill) => {
                    skill_not_found(res, &skill);
                    return;
                }
            };
            // Upstream materializes `uri` inputs into stored attachments at
            // admission; file reads are blocking, so they run off the reactor.
            let files = match body.files {
                Some(inputs) if !inputs.is_empty() => {
                    match tokio::task::spawn_blocking(move || attachment::materialize(inputs)).await
                    {
                        Ok(Ok(files)) => Some(files),
                        Ok(Err(error)) => {
                            tracing::debug!(label = %error.label, "prompt attachment rejected");
                            invalid_request_field(res, &error.message, "files");
                            return;
                        }
                        Err(error) => {
                            unknown_error(res, &error.into());
                            return;
                        }
                    }
                }
                _ => None,
            };
            ox_schema::inbox::UserPayload {
                prompt: ox_schema::support::Prompt {
                    text,
                    files,
                    agents: body.agents,
                    skills,
                },
                metadata: body.metadata,
            }
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    // `payload` is cloned once, for the response; the original moves into the
    // event. Attachment data is `Arc<str>`, so the clone shares the big field.
    let info = ox_schema::inbox::Info::User {
        id: message_id.clone(),
        session_id: session_id.clone(),
        time: ox_schema::inbox::EnqueuedTime {
            created: id::now_millis(),
        },
        payload: payload.clone(),
        delivery,
    };
    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: message_id.clone(),
            item: ox_schema::inbox::Item::User { payload, delivery },
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }
    // Upstream wakes the session unless `resume` is explicitly false.
    if body.resume != Some(false)
        && let Err(error) = app.runner.dispatch(&session_id, &message_id).await
    {
        unknown_error(res, &error);
        return;
    }
    res.render(Json(DataEnvelope { data: &info }));
}

/// `{"data": …}` around a borrowed value, so a prompt response does not copy
/// the payload (attachments can be megabytes) into a `serde_json::Value`.
#[derive(serde::Serialize)]
struct DataEnvelope<'a, T> {
    data: &'a T,
}

/// A pending `session_inbox` row decoded into its wire [`Info`] shape.
struct InboxRowInfo {
    delivery: ox_schema::inbox::Delivery,
    info: ox_schema::inbox::Info,
}

/// `session_inbox` columns of a single-row lookup.
type InboxTuple = (String, String, String, i64, String);

/// Load one inbox row by id.
async fn load_inbox_row(db: &Db, session_id: &str, inbox_id: &str) -> Result<Option<InboxRowInfo>> {
    let row: Option<InboxTuple> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT type, payload, delivery, time_created, session_id
                 FROM session_inbox WHERE id = ?1 AND session_id = ?2",
            )
            .await?;
        let Some(row) = ox_db::optional(&mut stmt, (inbox_id, session_id)).await? else {
            return Ok(None);
        };
        Ok::<_, anyhow::Error>(Some((
            row.get::<String>(0)?,
            row.get::<String>(1)?,
            row.get::<String>(2)?,
            row.get::<i64>(3)?,
            row.get::<String>(4)?,
        )))
    }
    .await?;
    row.map(|(kind, payload, delivery, created, session_id)| {
        decode_inbox_info(&session_id, inbox_id, &kind, &payload, &delivery, created).map(|info| {
            InboxRowInfo {
                delivery: info_delivery(&info),
                info,
            }
        })
    })
    .transpose()
}

/// Decode `session_inbox` columns into the [`Info`] wire enum.
fn decode_inbox_info(
    session_id: &str,
    inbox_id: &str,
    kind: &str,
    payload: &str,
    delivery: &str,
    created: i64,
) -> Result<ox_schema::inbox::Info> {
    let payload: Value = serde_json::from_str(payload)?;
    serde_json::from_value(json!({
        "type": kind,
        "id": inbox_id,
        "sessionID": session_id,
        "time": { "created": created },
        "payload": payload,
        "delivery": delivery,
    }))
    .map_err(|error| anyhow::Error::from(error).context("decoding inbox row"))
}

/// The delivery policy of a decoded [`Info`].
fn info_delivery(info: &ox_schema::inbox::Info) -> ox_schema::inbox::Delivery {
    match info {
        ox_schema::inbox::Info::User { delivery, .. }
        | ox_schema::inbox::Info::Synthetic { delivery, .. }
        | ox_schema::inbox::Info::Compaction { delivery, .. }
        | ox_schema::inbox::Info::Move { delivery, .. } => *delivery,
    }
}

// ---------------------------------------------------------------------------
// session.inbox.list — GET /api/session/{sessionID}/inbox
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.inbox.list
pub(crate) async fn inbox_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    if !matches!(load_session(&app.db, &session_id).await, Ok(Some(_))) {
        session_not_found(res, &session_id);
        return;
    }
    // Rows decode as they stream: the wire shape is built once, with no
    // intermediate tuple vector to unpack.
    let data: anyhow::Result<Vec<_>> = async {
        let conn = app.db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, type, payload, delivery, time_created, session_id
                 FROM session_inbox WHERE session_id = ?1
                 ORDER BY enqueued_seq ASC",
            )
            .await?;
        let mut rows = stmt.query([session_id.as_str()]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(decode_inbox_info(
                &row.get::<String>(5)?,
                &row.get::<String>(0)?,
                &row.get::<String>(1)?,
                &row.get::<String>(2)?,
                &row.get::<String>(3)?,
                row.get::<i64>(4)?,
            )?);
        }
        Ok(out)
    }
    .await;
    let data = match data {
        Ok(data) => data,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    res.render(Json(json!({ "data": data })));
}

// ---------------------------------------------------------------------------
// session.inbox.update — PATCH /api/session/{sessionID}/inbox/{inboxID}
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct InboxUpdateBody {
    #[serde(default)]
    delivery: Option<ox_schema::inbox::Delivery>,
}

#[handler] // operation_id: session.inbox.update
pub(crate) async fn inbox_update(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(inbox_id) = req.param::<String>("inboxID") else {
        invalid_request(res, "Missing inboxID");
        return;
    };
    let Some(body) = parse_body::<InboxUpdateBody>(req, res).await else {
        return;
    };
    let Some(delivery) = body.delivery else {
        invalid_request(res, "Missing delivery");
        return;
    };
    if !matches!(load_session(&app.db, &session_id).await, Ok(Some(_))) {
        session_not_found(res, &session_id);
        return;
    }

    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxDeliveryChanged {
            session_id: session_id.clone(),
            inbox_id: inbox_id.clone(),
            delivery,
        })
        .await
    {
        // Upstream `pendingMutation`: any lifecycle conflict on a pending
        // row surfaces as 409 (including an unknown inbox id). The wire
        // message is `Pending input cannot change to ${delivery}: ${inboxID}`
        // with the literal delivery string (session.ts:47-53 + :535).
        conflict_error(
            res,
            &format!(
                "Pending input cannot change to {}: {inbox_id}",
                match delivery {
                    ox_schema::inbox::Delivery::Steer => "steer",
                    ox_schema::inbox::Delivery::Queue => "queue",
                }
            ),
            &inbox_id,
        );
        tracing::debug!(session = %session_id, inbox = %inbox_id, error = %error, "inbox update rejected");
        return;
    }
    // Upstream `steerInbox` wakes the session; `queueInbox` does not.
    if delivery == ox_schema::inbox::Delivery::Steer
        && let Err(error) = app.runner.dispatch(&session_id, &inbox_id).await
    {
        unknown_error(res, &error);
        return;
    }
    no_content(res);
}

// ---------------------------------------------------------------------------
// session.inbox.cancel — DELETE /api/session/{sessionID}/inbox/{inboxID}
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.inbox.cancel
pub(crate) async fn inbox_cancel(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(inbox_id) = req.param::<String>("inboxID") else {
        invalid_request(res, "Missing inboxID");
        return;
    };
    if !matches!(load_session(&app.db, &session_id).await, Ok(Some(_))) {
        session_not_found(res, &session_id);
        return;
    }

    // Upstream swallows inbox lifecycle conflicts here: cancelling an
    // already-delivered/absent item is still a 204.
    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxCancelled {
            session_id: session_id.clone(),
            inbox_id: inbox_id.clone(),
        })
        .await
    {
        tracing::debug!(session = %session_id, inbox = %inbox_id, error = %error, "inbox cancel was a no-op");
    }
    no_content(res);
}

// ---------------------------------------------------------------------------
// session.interrupt — POST /api/session/{sessionID}/interrupt
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.interrupt
pub(crate) async fn interrupt(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    // `resume` (query) drives upstream's auto-resume; ox has no resume flow
    // yet, so it is accepted and ignored.
    let _resume = req.query::<bool>("resume");
    let interrupted = app.runner.interrupt(&session_id);
    res.render(Json(json!({ "interrupted": interrupted })));
}

// ---------------------------------------------------------------------------
// session.background — POST /api/session/{sessionID}/background
// ---------------------------------------------------------------------------

/// Upstream `session.background` ("Move active foreground backgroundable
/// tools for this session into background observation. Idle requests are a
/// no-op."): every job blocking the session is handed to the background, and
/// when any moved, a steer synthetic tells the model not to wait on it
/// (`packages/core/src/session.ts` `Session.background`). Always 204.
#[handler] // operation_id: session.background
pub(crate) async fn background(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }
    let moved = app.services().tools.jobs.background_all(&session_id);
    if moved.is_empty() {
        no_content(res);
        return;
    }

    let message_id = id::id("msg");
    let item = ox_schema::inbox::Item::Synthetic {
        payload: ox_schema::inbox::SyntheticPayload {
            text: background_notice(&moved),
            description: None,
            metadata: None,
        },
        delivery: ox_schema::inbox::Delivery::Steer,
    };
    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: message_id.clone(),
            item,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }
    if let Err(error) = app.runner.dispatch(&session_id, &message_id).await {
        unknown_error(res, &error);
        return;
    }
    no_content(res);
}

/// The synthetic text upstream `Session.background` writes, verbatim: one
/// `- {type}: {title or id}` line per backgrounded job.
fn background_notice(moved: &[ox_tools::jobs::JobInfo]) -> String {
    use std::fmt::Write;

    let mut text = String::from(
        "User requested that active blocking work be moved to the background.\n\nBackgrounded work:\n",
    );
    for job in moved {
        let label = if job.title.is_empty() {
            &job.id
        } else {
            &job.title
        };
        writeln!(text, "- {}: {label}", job.kind).expect("writing to a String cannot fail");
    }
    text.push_str(
        "\nThe backgrounded work is still unfinished. Move on to other work if you can. \
         If there is nothing else useful to do, finish your response. Do not wait, sleep, \
         poll, or report the backgrounded work as complete until a later completion \
         notification is added to the conversation.",
    );
    text
}

// ---------------------------------------------------------------------------
// Permission / form listings + experimental wait (client compatibility)
// ---------------------------------------------------------------------------

/// `GET /api/session/{sessionID}/permission` — upstream `session.permission.list`.
/// ox executes tools without an approval flow, so no permission request is
/// ever pending and the list is always empty.
#[handler]
pub(crate) async fn permission_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => res.render(Json(json!({ "data": [] }))),
        Ok(None) => session_not_found(res, &session_id),
        Err(error) => unknown_error(res, &error),
    }
}

/// `POST /api/experimental/session/{sessionID}/wait` — upstream
/// `experimental.session.wait`: long-poll until the session's agent loop is
/// idle, then answer 204. Unknown sessions answer 404.
#[handler]
pub(crate) async fn wait_idle(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }
    while app.runner.is_active(&session_id) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    res.status_code(StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// session.generate — the TUI's `/btw` side question
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct GenerateBody {
    #[serde(default)]
    prompt: String,
}

/// `POST /api/session/{sessionID}/generate` — upstream `session.generate`
/// ("Generates text from current Session context without mutating the
/// Session"): one answer from the session's current context, nothing persisted
/// and no event published. The TUI's `/btw` side question is the caller
/// (`feature-plugins/prompt/btw.tsx` → `client.session.generate`); the reply is
/// `{"location":…,"data":{"text":…}}`, which the client unwraps to `{text}`.
#[handler] // operation_id: session.generate
pub(crate) async fn generate(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<GenerateBody>(req, res).await else {
        return;
    };
    let prompt_text = body.prompt.trim();
    if prompt_text.is_empty() {
        invalid_request(res, "Missing prompt");
        return;
    }
    if !matches!(load_session(&app.db, &session_id).await, Ok(Some(_))) {
        session_not_found(res, &session_id);
        return;
    }
    match app.runner.generate(&session_id, prompt_text).await {
        Ok(text) => {
            let directory = crate::api::compat::directory_of(req);
            res.render(Json(crate::api::compat::envelope(
                &directory,
                json!({ "text": text }),
            )));
        }
        Err(error) => unknown_error(res, &error),
    }
}

// ---------------------------------------------------------------------------
// session.move — the TUI's `/cd`
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct MoveBody {
    #[serde(default)]
    directory: String,
}

/// `POST /api/session/{sessionID}/move` — upstream `session.move`: re-root the
/// session at `directory` (the TUI's `/cd`). The destination resolves against
/// the session's current directory (`move.ts:81`), `~` expands to the user's
/// home, and it must exist as a directory.
///
/// Deliberate deviation from upstream: the move is published immediately
/// instead of being admitted as an inbox `move` item. ox rebuilds its drain
/// context per busy period from the session row, so a turn already in flight
/// keeps the old directory until the next one starts — the same boundary
/// upstream's steer delivery lands on, one turn coarser.
#[handler] // operation_id: session.move
pub(crate) async fn move_session(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<MoveBody>(req, res).await else {
        return;
    };
    let Some(session) = load_session(&app.db, &session_id).await.ok().flatten() else {
        session_not_found(res, &session_id);
        return;
    };
    let raw = body.directory.trim();
    if raw.is_empty() {
        invalid_request(res, "Missing directory");
        return;
    }
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let expanded = match raw {
        "~" => home,
        _ => match raw.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => std::path::PathBuf::from(raw),
        },
    };
    let destination = if expanded.is_absolute() {
        expanded
    } else {
        std::path::PathBuf::from(&session.directory).join(expanded)
    };
    let Ok(canonical) = destination.canonicalize() else {
        invalid_request(
            res,
            &format!("Destination does not exist: {}", destination.display()),
        );
        return;
    };
    if !canonical.is_dir() {
        invalid_request(
            res,
            &format!("Destination is not a directory: {}", canonical.display()),
        );
        return;
    }
    let directory = canonical.display().to_string();
    let (project_id, project_directory) = resolve_project(&app.db, &directory).await;
    let payload = ox_schema::inbox::MovePayload {
        location: LocationRef {
            directory: directory.clone(),
            workspace_id: session.workspace_id.clone(),
        },
        project_id,
        subpath: subpath_of(&directory, &project_directory),
    };
    if let Err(error) = app
        .bus
        .publish(SessionEventData::SessionMoved {
            session_id: session_id.clone(),
            payload,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }
    res.status_code(StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// session.command — POST /api/session/{sessionID}/command
// ---------------------------------------------------------------------------

/// Upstream payload: `name` plus the prompt fields and `delivery`. ox
/// registers no commands, so only `name` is read; unknown fields are ignored.
#[derive(Default, Deserialize)]
struct CommandBody {
    name: String,
}

/// Execute a slash command (upstream `session.command`). Handler order
/// follows upstream: session lookup, then command lookup. `GET /api/command`
/// lists no commands, so every name is unregistered and answers
/// `CommandNotFoundError` (404) — the same reply upstream gives for a name
/// that is not in its table.
#[handler] // operation_id: session.command
pub(crate) async fn command(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<CommandBody>(req, res).await else {
        return;
    };
    if body.name.is_empty() {
        invalid_request(res, "Invalid request body");
        return;
    }
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "CommandNotFoundError",
        "command": body.name,
        "message": format!("Command not found: {}", body.name),
    })));
}

// ---------------------------------------------------------------------------
// session.synthetic — POST /api/session/{sessionID}/synthetic
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct SyntheticBody {
    #[serde(default)]
    id: Option<String>,
    text: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: Option<ox_schema::support::Record>,
    #[serde(default)]
    delivery: Option<ox_schema::inbox::Delivery>,
    #[serde(default)]
    resume: Option<bool>,
}

/// Durable synthetic input: same admission path as `session.prompt`, with the
/// `synthetic` item instead of a user prompt. A re-posted id with a different
/// delivery is a conflict; a matching one is idempotent and only re-dispatches.
#[handler] // operation_id: session.synthetic
pub(crate) async fn synthetic(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<SyntheticBody>(req, res).await else {
        return;
    };
    if !matches!(load_session(&app.db, &session_id).await, Ok(Some(_))) {
        session_not_found(res, &session_id);
        return;
    }

    let message_id = body.id.clone().unwrap_or_else(|| id::id("msg"));
    let delivery = body.delivery.unwrap_or(ox_schema::inbox::Delivery::Steer);
    let payload = ox_schema::inbox::SyntheticPayload {
        text: body.text,
        description: body.description,
        metadata: body.metadata,
    };
    match load_inbox_row(&app.db, &session_id, &message_id).await {
        Ok(Some(existing)) => {
            if existing.delivery != delivery {
                conflict_error(
                    res,
                    &format!(
                        "Synthetic input ID conflicts with an existing durable record: {message_id}"
                    ),
                    &message_id,
                );
                return;
            }
            let info = existing.info;
            if body.resume != Some(false)
                && let Err(error) = app.runner.dispatch(&session_id, &message_id).await
            {
                unknown_error(res, &error);
                return;
            }
            res.render(Json(json!({ "data": info })));
            return;
        }
        Ok(None) => {}
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    let item = ox_schema::inbox::Item::Synthetic {
        payload: payload.clone(),
        delivery,
    };
    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: message_id.clone(),
            item,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }
    if body.resume != Some(false)
        && let Err(error) = app.runner.dispatch(&session_id, &message_id).await
    {
        unknown_error(res, &error);
        return;
    }
    res.render(Json(json!({
        "data": ox_schema::inbox::Info::Synthetic {
            id: message_id,
            session_id,
            time: ox_schema::inbox::EnqueuedTime {
                created: id::now_millis(),
            },
            payload,
            delivery,
        },
    })));
}

// ---------------------------------------------------------------------------
// session.export / session.import — /api/experimental/session
// ---------------------------------------------------------------------------

/// `GET /api/experimental/session/{sessionID}/export` — a projected transcript:
/// the session info plus every settled message in aggregate order. `sanitize`
/// replaces free text with redaction markers (upstream `session/transfer.ts`).
#[handler] // operation_id: experimental.session.export
pub(crate) async fn export(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let row = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    let mut messages = match export_messages(&app.db, &session_id).await {
        Ok(messages) => messages,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    messages.retain(settled);
    let mut info = row.info();
    let sanitize = matches!(
        req.query::<String>("sanitize").as_deref(),
        Some("true") | Some("1")
    );
    if sanitize {
        sanitize_transfer(&mut info, &mut messages);
    }
    res.render(Json(
        json!({ "data": { "info": info, "messages": messages } }),
    ));
}

/// Every message row of a session, ascending, in the wire shape.
async fn export_messages(db: &Db, session_id: &str) -> Result<Vec<Value>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, type, data FROM session_message WHERE session_id = ?1 ORDER BY seq ASC",
        )
        .await?;
    let mut rows = stmt.query((session_id,)).await?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().await? {
        let message_id = row.get::<String>(0)?;
        let kind = row.get::<String>(1)?;
        let data = row.get::<String>(2)?;
        let mut value: Value =
            serde_json::from_str(&data).context("decode session_message.data")?;
        Message::merge_row_columns(&message_id, &kind, &mut value);
        values.push(value);
    }
    Ok(values)
}

/// Upstream `isSettled`: a transcript carries only finished work.
fn settled(message: &Value) -> bool {
    match message.get("type").and_then(Value::as_str) {
        Some("assistant") => message
            .pointer("/time/completed")
            .is_some_and(|completed| !completed.is_null()),
        Some("shell") | Some("compaction") => {
            message.get("status").and_then(Value::as_str) != Some("running")
        }
        _ => true,
    }
}

#[derive(Default, Deserialize)]
struct ImportBody {
    info: Value,
    messages: Vec<Value>,
    #[serde(default)]
    location: Option<LocationRef>,
}

/// `POST /api/experimental/session/import` — recreate an exported transcript at
/// the requested location. A recorded id is a conflict; a parent must already
/// exist. Message rows are inserted directly (upstream does the same inside the
/// `SessionCreated` projection) and the imported totals replace the defaults.
#[handler] // operation_id: experimental.session.import
pub(crate) async fn import(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(body) = parse_body::<ImportBody>(req, res).await else {
        return;
    };
    let Some(session_id) = body
        .info
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        invalid_request(res, "Missing session id");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {
            conflict_error(
                res,
                &format!("Session already exists: {session_id}"),
                &session_id,
            );
            return;
        }
        Ok(None) => {}
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    let parent_id = body
        .info
        .get("parentID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(parent_id) = &parent_id
        && !matches!(load_session(&app.db, parent_id).await, Ok(Some(_)))
    {
        session_not_found(res, parent_id);
        return;
    }

    let directory = body
        .location
        .as_ref()
        .map(|location| location.directory.clone())
        .or_else(|| super::compat::request_directory(req))
        .unwrap_or_else(|| app.directory.to_string_lossy().into_owned());
    let (project_id, project_directory) = resolve_project(&app.db, &directory).await;
    let text = |key: &str| {
        body.info
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let json_field = |key: &str| body.info.get(key).filter(|value| !value.is_null()).cloned();

    let event = SessionEventData::SessionCreated {
        session_id: session_id.clone(),
        project_id,
        location: LocationRef {
            directory: directory.clone(),
            workspace_id: None,
        },
        subpath: subpath_of(&directory, &project_directory),
        parent_id,
        slug: ox_schema::slug::create(),
        title: text("title"),
        agent: text("agent"),
        model: json_field("model").and_then(|value| serde_json::from_value::<ModelRef>(value).ok()),
        metadata: json_field("metadata")
            .and_then(|value| serde_json::from_value::<SessionMetadata>(value).ok()),
        permissions: json_field("permissions")
            .and_then(|value| serde_json::from_value::<Vec<Rule>>(value).ok()),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if let Err(error) = app.bus.publish(event).await {
        unknown_error(res, &error);
        return;
    }

    let mut imported = 0i64;
    let mut rows: Vec<(String, String, i64, i64, String)> = Vec::new();
    for value in body.messages.iter().filter(|message| settled(message)) {
        let Ok(message) = Message::from_merged_row(value.clone()) else {
            invalid_request(res, "Invalid message in transcript");
            return;
        };
        imported += 1;
        rows.push((
            message.id().to_owned(),
            message.kind().to_owned(),
            imported,
            message.time_created(),
            message.to_row_data().to_string(),
        ));
    }
    let tx = match app.db.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    for (message_id, kind, seq, created, data) in &rows {
        if let Err(error) = tx
            .execute(
                "INSERT INTO session_message
                 (id, session_id, type, seq, time_created, time_updated, data)
                 VALUES (?1,?2,?3,?4,?5,?5,?6)",
                (
                    message_id.as_str(),
                    session_id.as_str(),
                    kind.as_str(),
                    *seq,
                    *created,
                    data.as_str(),
                ),
            )
            .await
        {
            unknown_error(res, &anyhow!("{error}"));
            return;
        }
    }
    let imported_at = id::now_millis();
    let number = |path: &str| body.info.pointer(path).and_then(Value::as_i64);
    let cost = body
        .info
        .get("cost")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let created = number("/time/created").unwrap_or(imported_at);
    let idle = number("/time/idle");
    let busy = number("/time/busy");
    let viewed = number("/time/viewed");
    let archived = number("/time/archived");
    let outcome = body
        .info
        .get("outcome")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Err(error) = tx
        .execute(
            "UPDATE session_v2 SET cost = ?2, tokens_input = ?3, tokens_output = ?4,
             tokens_reasoning = ?5, tokens_cache_read = ?6, tokens_cache_write = ?7,
             time_created = ?8, time_updated = ?9, time_idle = ?10, time_viewed = ?11,
             idle_outcome = ?12, time_archived = ?13, time_busy = ?14
             WHERE id = ?1",
            (
                session_id.as_str(),
                cost,
                number("/tokens/input").unwrap_or_default(),
                number("/tokens/output").unwrap_or_default(),
                number("/tokens/reasoning").unwrap_or_default(),
                number("/tokens/cache/read").unwrap_or_default(),
                number("/tokens/cache/write").unwrap_or_default(),
                created,
                imported_at,
                idle,
                idle.and(viewed)
                    .map(|idle| idle.min(viewed.unwrap_or(idle))),
                idle.and(outcome),
                archived,
                // Upstream transfer: only a completed execution keeps its start.
                idle.zip(busy).map(|(idle, busy)| idle.min(busy)),
            ),
        )
        .await
    {
        unknown_error(res, &anyhow!("{error}"));
        return;
    }
    if let Err(error) = tx.commit().await {
        unknown_error(res, &anyhow!("{error}"));
        return;
    }

    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => unknown_error(res, &anyhow!("session `{session_id}` missing after import")),
        Err(error) => unknown_error(res, &error),
    }
}

/// `[redacted:kind:id]` for non-empty text, unchanged when blank (upstream
/// `redact`).
fn redact(kind: &str, id: &str, value: &str) -> String {
    if value.trim().is_empty() {
        value.to_owned()
    } else {
        format!("[redacted:{kind}:{id}]")
    }
}

fn redact_key(map: &mut Map<String, Value>, key: &str, kind: &str, id: &str) {
    if let Some(Value::String(text)) = map.get(key) {
        let next = redact(kind, id, text);
        map.insert(key.to_owned(), Value::String(next));
    }
}

/// Non-empty metadata becomes a single `redacted` member; empty stays as is.
fn sanitize_metadata(value: &mut Value, kind: &str, id: &str) {
    let Some(map) = value.as_object() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    *value = json!({ "redacted": format!("{kind}:{id}") });
}

fn sanitize_transfer(info: &mut Value, messages: &mut [Value]) {
    let id = info
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Value::Object(map) = info {
        redact_key(map, "title", "session-title", &id);
        if let Some(metadata) = map.get_mut("metadata") {
            sanitize_metadata(metadata, "session-metadata", &id);
        }
        if let Some(Value::Object(location)) = map.get_mut("location")
            && let Some(Value::String(directory)) = location.get("directory")
        {
            let redacted = redact("session-directory", &id, directory);
            location.insert(
                "directory".to_owned(),
                Value::String(format!("/{redacted}")),
            );
        }
        if let Some(Value::Object(revert)) = map.get_mut("revert")
            && let Some(Value::Array(files)) = revert.get_mut("files")
        {
            for (index, file) in files.iter_mut().enumerate() {
                if let Value::Object(file) = file {
                    redact_key(file, "file", "revert-file", &index.to_string());
                    redact_key(file, "patch", "revert-patch", &index.to_string());
                }
            }
        }
    }
    for message in messages {
        sanitize_message(message);
    }
}

fn sanitize_message(message: &mut Value) {
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let kind = message
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let Some(map) = message.as_object_mut() else {
        return;
    };
    if let Some(metadata) = map.get_mut("metadata") {
        sanitize_metadata(metadata, "message-metadata", &id);
    }
    match kind.as_str() {
        "user" => {
            redact_key(map, "text", "text", &id);
            redact_attachments(map, "files", "file", &id);
            redact_attachments(map, "agents", "agent", &id);
            redact_attachments(map, "skills", "skill", &id);
        }
        "synthetic" => {
            redact_key(map, "text", "synthetic", &id);
            redact_key(map, "description", "synthetic-description", &id);
        }
        "system" => redact_key(map, "text", "system", &id),
        "skill" => redact_key(map, "text", "skill", &id),
        "shell" => {
            redact_key(map, "command", "shell-command", &id);
            if let Some(Value::Object(output)) = map.get_mut("output") {
                redact_key(output, "output", "shell-output", &id);
            }
        }
        "assistant" => {
            if let Some(Value::Array(content)) = map.get_mut("content") {
                for part in content {
                    let Some(part) = part.as_object_mut() else {
                        continue;
                    };
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            redact_key(part, "text", "text", &id);
                            if part.contains_key("native") {
                                part.insert(
                                    "native".to_owned(),
                                    json!({ "redacted": format!("text-native:{id}") }),
                                );
                            }
                        }
                        Some("reasoning") => {
                            redact_key(part, "text", "reasoning", &id);
                            if part.contains_key("native") {
                                part.insert(
                                    "native".to_owned(),
                                    json!({ "redacted": format!("reasoning-native:{id}") }),
                                );
                            }
                        }
                        _ => {
                            if part.contains_key("native") {
                                part.insert(
                                    "native".to_owned(),
                                    json!({ "redacted": format!("tool-native:{id}") }),
                                );
                            }
                            if let Some(state) = part.get_mut("state") {
                                sanitize_tool_state(state, &id);
                            }
                        }
                    }
                }
            }
        }
        "compaction" => {
            if map.get("status").and_then(Value::as_str) != Some("failed") {
                redact_key(map, "summary", "compaction-summary", &id);
            }
            if map.get("status").and_then(Value::as_str) == Some("completed")
                && let Some(native) = map.get_mut("native")
            {
                sanitize_metadata(native, "compaction-native", &id);
            }
        }
        _ => {}
    }
}

/// `files` / `agents` / `skills` arrays: every name, mention, description and
/// inline payload loses its content; file data is dropped outright.
fn redact_attachments(map: &mut Map<String, Value>, key: &str, kind: &str, id: &str) {
    let Some(Value::Array(items)) = map.get_mut(key) else {
        return;
    };
    for (index, item) in items.iter_mut().enumerate() {
        let Some(item) = item.as_object_mut() else {
            continue;
        };
        let slot = index.to_string();
        redact_key(item, "name", &format!("{kind}-name"), &slot);
        redact_key(item, "description", &format!("{kind}-description"), &slot);
        if let Some(Value::Object(mention)) = item.get_mut("mention") {
            redact_key(mention, "text", &format!("{kind}-mention"), &slot);
        }
        if kind == "file" {
            item.insert("data".to_owned(), Value::String(String::new()));
            item.insert("source".to_owned(), json!({ "type": "inline" }));
        }
        if kind == "skill" {
            redact_key(item, "text", "skill", &slot);
        }
        let _ = id;
    }
}

fn sanitize_tool_state(state: &mut Value, id: &str) {
    let Some(map) = state.as_object_mut() else {
        return;
    };
    match map.get("status").and_then(Value::as_str) {
        Some("streaming") => redact_key(map, "input", "tool-input", id),
        Some("running") => {
            map.insert(
                "input".to_owned(),
                json!({ "redacted": format!("tool-input:{id}") }),
            );
            map.insert(
                "metadata".to_owned(),
                json!({ "redacted": format!("tool-metadata:{id}") }),
            );
        }
        _ => {
            map.insert(
                "input".to_owned(),
                json!({ "redacted": format!("tool-input:{id}") }),
            );
            if map.contains_key("metadata") {
                map.insert(
                    "metadata".to_owned(),
                    json!({ "redacted": format!("tool-metadata:{id}") }),
                );
            }
            if let Some(Value::Array(content)) = map.get_mut("content") {
                for item in content {
                    sanitize_tool_content(item, id);
                }
            }
        }
    }
}

fn sanitize_tool_content(content: &mut Value, id: &str) {
    let Some(map) = content.as_object_mut() else {
        return;
    };
    if map.get("type").and_then(Value::as_str) == Some("text") {
        redact_key(map, "text", "tool-output", id);
        return;
    }
    redact_key(map, "uri", "tool-file-uri", id);
    redact_key(map, "name", "tool-file-name", id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_notice_matches_upstream_text() {
        let moved = [
            ox_tools::jobs::JobInfo {
                id: "shell_1".to_owned(),
                kind: "shell",
                title: "sleep 60".to_owned(),
            },
            ox_tools::jobs::JobInfo {
                id: "ses_child".to_owned(),
                kind: "subagent",
                title: String::new(),
            },
        ];
        assert_eq!(
            background_notice(&moved),
            "User requested that active blocking work be moved to the background.\n\
             \n\
             Backgrounded work:\n\
             - shell: sleep 60\n\
             - subagent: ses_child\n\
             \n\
             The backgrounded work is still unfinished. Move on to other work if you can. If there is nothing else useful to do, finish your response. Do not wait, sleep, poll, or report the backgrounded work as complete until a later completion notification is added to the conversation.",
        );
    }
}
