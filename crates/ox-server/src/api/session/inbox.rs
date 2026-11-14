use anyhow::Result;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use ox_db::Db;
use ox_schema::event::SessionEventData;

use super::response::{
    conflict_error, invalid_request, no_content, parse_body, session_not_found, unknown_error,
};
use super::session_row::load_session;
use crate::api::state;

/// A pending `session_inbox` row decoded into its wire [`Info`] shape.
pub(super) struct InboxRowInfo {
    pub(super) delivery: ox_schema::inbox::Delivery,
    pub(super) info: ox_schema::inbox::Info,
}

/// `session_inbox` columns of a single-row lookup.
type InboxTuple = (String, String, String, i64, String);

/// Load one inbox row by id.
pub(super) async fn load_inbox_row(
    db: &Db,
    session_id: &str,
    inbox_id: &str,
) -> Result<Option<InboxRowInfo>> {
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
