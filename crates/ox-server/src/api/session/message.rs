use anyhow::{Context as _, Result, anyhow};
use ox_db::turso::Value as SqlValue;
use salvo::prelude::*;
use serde_json::{Value, json};

use ox_db::Db;
use ox_schema::message::Message;

use super::cursor::{
    Direction, MessagesCursor, ascending_of, decode_cursor, encode_cursor, resolve_limit,
    resolve_order,
};
use super::response::{
    invalid_cursor, invalid_request, message_not_found, session_not_found, unknown_error,
};
use super::session_row::load_session;
use crate::api::state;

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
