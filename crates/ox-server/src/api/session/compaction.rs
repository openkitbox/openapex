use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::inbox::{CompactionPayload, Delivery, Info, Item};

use super::inbox::load_inbox_row;
use super::response::{
    conflict_error, invalid_request, parse_body, session_not_found, unknown_error,
};
use super::session_row::load_session;
use crate::api::state;

// ---------------------------------------------------------------------------
// session.compact — POST /api/session/{sessionID}/compact
// ---------------------------------------------------------------------------

/// Request body of `session.compact`: an optional input id and delivery
/// (upstream defaults delivery to `steer`).
#[derive(Default, Deserialize)]
struct CompactBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    delivery: Option<Delivery>,
}

/// Serializes compaction admission process-wide, so two concurrent compacts
/// cannot both miss the pending-compaction check and enqueue twice (upstream
/// serializes per session; compaction requests are rare enough that one lock
/// costs nothing).
static ADMISSION: Mutex<()> = Mutex::const_new(());

/// Why an admission was refused.
#[derive(Debug)]
enum AdmitError {
    /// The input id already names a different durable record.
    Conflict,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for AdmitError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

/// Upstream `Session.compact`. Deviation: `SessionRevert.commit` is skipped,
/// ox has no revert.
#[handler] // operation_id: session.compact
pub(crate) async fn compact(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<CompactBody>(req, res).await else {
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
    let input_id = body.id.unwrap_or_else(|| id::id("msg"));
    let delivery = body.delivery.unwrap_or(Delivery::Steer);
    let admitted = match admit(&app.db, &app.bus, &session_id, &input_id, delivery).await {
        Ok(info) => info,
        Err(AdmitError::Conflict) => {
            conflict_error(
                res,
                &format!(
                    "Compaction input ID conflicts with an existing durable record: {input_id}"
                ),
                &input_id,
            );
            return;
        }
        Err(AdmitError::Other(error)) => {
            unknown_error(res, &error);
            return;
        }
    };
    // Upstream `execution.wake`: every successful admission wakes the
    // session, including an idempotent replay.
    let admitted_id = info_id(&admitted);
    if let Err(error) = app.runner.dispatch(&session_id, admitted_id).await {
        unknown_error(res, &error);
        return;
    }
    res.render(Json(json!({ "data": admitted })));
}

/// The inbox id of an admitted item.
fn info_id(info: &Info) -> &str {
    match info {
        Info::User { id, .. }
        | Info::Synthetic { id, .. }
        | Info::Compaction { id, .. }
        | Info::Move { id, .. } => id,
    }
}

/// Upstream `SessionInbox.admitCompaction`, in its order: an exact inbox
/// replay returns the stored item (conflict if it is not this session's
/// compaction); an id already used by a message conflicts; an already
/// pending compaction is returned instead of enqueueing a second; otherwise
/// a fresh compaction item is enqueued.
async fn admit(
    db: &Db,
    bus: &EventBus,
    session_id: &str,
    input_id: &str,
    delivery: Delivery,
) -> Result<Info, AdmitError> {
    let _serialized = ADMISSION.lock().await;
    if let Some((owner, kind)) = inbox_owner(db, input_id).await? {
        if owner != session_id || kind != "compaction" {
            return Err(AdmitError::Conflict);
        }
        return stored(db, session_id, input_id).await;
    }
    if message_exists(db, input_id).await? {
        return Err(AdmitError::Conflict);
    }
    if let Some(pending) = pending_compaction(db, session_id).await? {
        return stored(db, session_id, &pending).await;
    }
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: session_id.to_owned(),
        inbox_id: input_id.to_owned(),
        item: Item::Compaction {
            payload: CompactionPayload {},
            delivery,
        },
    })
    .await?;
    stored(db, session_id, input_id).await
}

/// The decoded inbox row the projector wrote (or kept) for `inbox_id`.
async fn stored(db: &Db, session_id: &str, inbox_id: &str) -> Result<Info, AdmitError> {
    match load_inbox_row(db, session_id, inbox_id).await? {
        Some(row) => Ok(row.info),
        None => Err(AdmitError::Other(anyhow::anyhow!(
            "compaction `{inbox_id}` is missing from the inbox of `{session_id}`"
        ))),
    }
}

/// `(session_id, type)` of an inbox row with this id, in any session.
async fn inbox_owner(db: &Db, inbox_id: &str) -> anyhow::Result<Option<(String, String)>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT session_id, type FROM session_inbox WHERE id = ?1")
        .await?;
    let Some(row) = ox_db::optional(&mut stmt, [inbox_id]).await? else {
        return Ok(None);
    };
    Ok(Some((row.get::<String>(0)?, row.get::<String>(1)?)))
}

/// Whether any session already has a message with this id.
async fn message_exists(db: &Db, message_id: &str) -> anyhow::Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT id FROM session_message WHERE id = ?1")
        .await?;
    Ok(ox_db::optional(&mut stmt, [message_id]).await?.is_some())
}

/// The id of the session's first pending compaction item, if any.
async fn pending_compaction(db: &Db, session_id: &str) -> anyhow::Result<Option<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id FROM session_inbox WHERE session_id = ?1 AND type = 'compaction'
             ORDER BY enqueued_seq ASC LIMIT 1",
        )
        .await?;
    let Some(row) = ox_db::optional(&mut stmt, [session_id]).await? else {
        return Ok(None);
    };
    Ok(Some(row.get::<String>(0)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_schema::support::LocationRef;

    async fn session(bus: &EventBus, session_id: &str) {
        bus.publish(SessionEventData::SessionCreated {
            session_id: session_id.to_owned(),
            project_id: "global".to_owned(),
            location: LocationRef {
                directory: std::env::temp_dir().display().to_string(),
                workspace_id: None,
            },
            subpath: None,
            parent_id: None,
            slug: ox_schema::slug::create(),
            title: None,
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            version: "test".to_owned(),
        })
        .await
        .expect("session row");
    }

    #[tokio::test]
    async fn admission_follows_upstream_order() {
        let db = Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        session(&bus, "ses_a").await;
        session(&bus, "ses_b").await;

        let first = admit(&db, &bus, "ses_a", "msg_c1", Delivery::Steer)
            .await
            .expect("fresh compaction is admitted");
        assert!(
            matches!(&first, Info::Compaction { id, .. } if id == "msg_c1"),
            "fresh admission enqueues the requested id, got {first:?}",
        );

        let replay = admit(&db, &bus, "ses_a", "msg_c1", Delivery::Queue)
            .await
            .expect("exact replay is idempotent");
        assert_eq!(replay, first, "a replay returns the stored item");

        let pending = admit(&db, &bus, "ses_a", "msg_c2", Delivery::Steer)
            .await
            .expect("a second compaction reuses the pending one");
        assert_eq!(pending, first, "the pending compaction is returned");

        let other_session = admit(&db, &bus, "ses_b", "msg_c1", Delivery::Steer).await;
        assert!(
            matches!(other_session, Err(AdmitError::Conflict)),
            "another session's inbox id conflicts, got {other_session:?}",
        );

        bus.publish(SessionEventData::InboxEnqueued {
            session_id: "ses_b".to_owned(),
            inbox_id: "msg_u1".to_owned(),
            item: Item::User {
                payload: ox_schema::inbox::UserPayload {
                    prompt: ox_schema::support::Prompt {
                        text: "hi".to_owned(),
                        files: None,
                        agents: None,
                        skills: None,
                    },
                    metadata: None,
                },
                delivery: Delivery::Steer,
            },
        })
        .await
        .expect("user prompt enqueued");
        let user_id = admit(&db, &bus, "ses_b", "msg_u1", Delivery::Steer).await;
        assert!(
            matches!(user_id, Err(AdmitError::Conflict)),
            "a pending user prompt id conflicts, got {user_id:?}",
        );

        bus.publish(SessionEventData::InboxDelivered {
            session_id: "ses_b".to_owned(),
            inbox_id: "msg_u1".to_owned(),
        })
        .await
        .expect("user prompt delivered");
        let message_id = admit(&db, &bus, "ses_b", "msg_u1", Delivery::Steer).await;
        assert!(
            matches!(message_id, Err(AdmitError::Conflict)),
            "a delivered message id conflicts, got {message_id:?}",
        );
    }
}
