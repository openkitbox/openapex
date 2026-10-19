//! Session lifecycle endpoints (Task 4).
//!
//! Ports `packages/server/src/handlers/session.ts` (`session.*`) and
//! `packages/server/src/handlers/message.ts` (`session.messages`).
//!
//! Every mutation is published through [`crate::event_bus::EventBus`]; the
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
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use turso::Value as SqlValue;

use crate::db::Db;
use crate::id;
use crate::schema::event::SessionEventData;
use crate::schema::message::Message;
use crate::schema::support::{ForkBoundary, LocationRef, ModelRef, Rule, SessionMetadata};

use super::state;

/// Upstream `DefaultSessionsLimit` / `DefaultMessagesLimit`.
const DEFAULT_PAGE_LIMIT: i64 = 50;

// ---------------------------------------------------------------------------
// Error responses (packages/protocol/errors.ts encoded shapes)
// ---------------------------------------------------------------------------

/// 404 `SessionNotFoundError`.
fn session_not_found(res: &mut Response, session_id: &str) {
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
fn invalid_request(res: &mut Response, message: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
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

/// Parse an optional JSON request body; an empty body yields `T::default()`
/// (every body schema here has all-optional properties except where the
/// handler validates required fields itself). Malformed JSON is a 400.
async fn parse_body<T>(req: &mut Request, res: &mut Response) -> Option<T>
where
    T: DeserializeOwned + Default,
{
    let bytes = match req.payload().await {
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
     revert, idle_outcome, time_created, time_updated, time_idle, time_viewed, time_archived";

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
}

fn map_session_row(row: &turso::Row) -> turso::Result<SessionRow> {
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
        if let Some(subpath) = &self.path {
            m.insert("subpath".to_owned(), json!(subpath));
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
    let row = crate::db::optional(&mut stmt, [session_id]).await?;
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
            let row = crate::db::optional(&mut stmt, [directory]).await?;
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
/// or `None` when the location *is* the project directory.
fn subpath_of(directory: &str, project_directory: &str) -> Option<String> {
    let relative = directory.strip_prefix(project_directory)?;
    let trimmed = relative.trim_start_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
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

    // A cursor carries the whole query; otherwise read it from the request.
    let (query, direction) = match &decoded {
        Some(cursor) => (cursor.query.clone(), cursor.anchor.direction),
        None => {
            let query = match request_query(req) {
                Ok(query) => query,
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
        directory: req.query("directory"),
        project: req.query("project"),
        subpath: req.query("subpath"),
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
        slug: crate::slug::create(),
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

    if let Some(title) = &body.title
        && !title.is_empty()
        && let Err(error) = app
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
    // TODO(title-generation): an empty title asks upstream to generate one from
    // the session's history (a model-backed subsystem); skipped here.

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

    no_content(res);
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

    // Upstream removes descendants first (depth-first), then the session, then
    // purges each aggregate's durable log.
    let descendants = match collect_descendants(&app.db, &session_id).await {
        Ok(ids) => ids,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    let mut order = descendants;
    order.reverse();
    order.push(session_id.clone());

    for target in order {
        if let Err(error) = app
            .bus
            .publish(SessionEventData::SessionDeleted {
                session_id: target.clone(),
            })
            .await
        {
            unknown_error(res, &error);
            return;
        }
        if let Err(error) = app.bus.remove(&target).await {
            unknown_error(res, &error);
            return;
        }
    }

    no_content(res);
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
            crate::db::optional(&mut stmt, (session_id, message_id))
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
            crate::db::optional(&mut stmt, [session_id])
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
    let mut params: Vec<SqlValue> = vec![SqlValue::Text(session_id)];
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
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let message_id = row.get::<String>(0)?;
        let kind = row.get::<String>(1)?;
        let data = row.get::<String>(2)?;
        let value: Value = serde_json::from_str(&data).context("decode session_message.data")?;
        out.push(
            Message::from_row_data(&message_id, &kind, value)
                .context("decode session message row")?,
        );
    }
    if reverse {
        out.reverse();
    }
    Ok(out)
}

async fn message_seq(db: &Db, session_id: &str, message_id: &str) -> Result<Option<i64>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2")
        .await?;
    let row = crate::db::optional(&mut stmt, (session_id, message_id)).await?;
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
            crate::db::optional(&mut stmt, (session_id.as_str(), message_id.as_str())).await?
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
    let value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(error) => {
            unknown_error(res, &anyhow::Error::from(error));
            return;
        }
    };
    match Message::from_row_data(&message_id, &kind, value) {
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
    files: Option<Vec<crate::schema::support::FileAttachment>>,
    #[serde(default)]
    agents: Option<Vec<crate::schema::support::AgentAttachment>>,
    #[serde(default)]
    skills: Option<Vec<crate::schema::support::SkillAttachment>>,
    #[serde(default)]
    metadata: Option<crate::schema::support::Record>,
    #[serde(default)]
    delivery: Option<crate::schema::inbox::Delivery>,
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
    if !matches!(load_session(&app.db, &session_id).await, Ok(Some(_))) {
        session_not_found(res, &session_id);
        return;
    }

    let message_id = body.id.clone().unwrap_or_else(|| id::id("msg"));
    let delivery = body
        .delivery
        .unwrap_or(crate::schema::inbox::Delivery::Steer);
    let payload = crate::schema::inbox::UserPayload {
        prompt: crate::schema::support::Prompt {
            text,
            files: body.files,
            agents: body.agents,
            skills: body.skills,
        },
        metadata: body.metadata,
    };
    // Upstream `admission.reconcile`: re-prompting with the same id and
    // delivery is idempotent; a delivery mismatch is a prompt conflict.
    match load_inbox_row(&app.db, &session_id, &message_id).await {
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
        Ok(None) => {}
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    let item = crate::schema::inbox::Item::User {
        payload: payload.clone(),
        delivery,
    };
    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: message_id.clone(),
            item: item.clone(),
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
    res.render(Json(json!({
        "data": crate::schema::inbox::Info::User {
            id: message_id,
            session_id,
            time: crate::schema::inbox::EnqueuedTime {
                created: id::now_millis(),
            },
            payload,
            delivery,
        },
    })));
}

/// A pending `session_inbox` row decoded into its wire [`Info`] shape.
struct InboxRowInfo {
    delivery: crate::schema::inbox::Delivery,
    info: crate::schema::inbox::Info,
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
        let Some(row) = crate::db::optional(&mut stmt, (inbox_id, session_id)).await? else {
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
) -> Result<crate::schema::inbox::Info> {
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
fn info_delivery(info: &crate::schema::inbox::Info) -> crate::schema::inbox::Delivery {
    match info {
        crate::schema::inbox::Info::User { delivery, .. }
        | crate::schema::inbox::Info::Synthetic { delivery, .. }
        | crate::schema::inbox::Info::Compaction { delivery, .. }
        | crate::schema::inbox::Info::Move { delivery, .. } => *delivery,
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
    delivery: Option<crate::schema::inbox::Delivery>,
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
                    crate::schema::inbox::Delivery::Steer => "steer",
                    crate::schema::inbox::Delivery::Queue => "queue",
                }
            ),
            &inbox_id,
        );
        tracing::debug!(session = %session_id, inbox = %inbox_id, error = %error, "inbox update rejected");
        return;
    }
    // Upstream `steerInbox` wakes the session; `queueInbox` does not.
    if delivery == crate::schema::inbox::Delivery::Steer
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

/// `GET /api/session/{sessionID}/form` — upstream `session.form.list`.
/// ox has no form channel; the list is always empty.
#[handler]
pub(crate) async fn form_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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
