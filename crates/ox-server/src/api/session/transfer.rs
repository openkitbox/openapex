use anyhow::{Context as _, Result, anyhow};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::message::Message;
use ox_schema::support::{LocationRef, ModelRef, Rule, SessionMetadata};

use super::response::{
    conflict_error, invalid_request, parse_body, session_not_found, unknown_error,
};
use super::session_row::{load_session, resolve_project, subpath_of};
use crate::api::state;

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
        .or_else(|| crate::api::compat::request_directory(req))
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
