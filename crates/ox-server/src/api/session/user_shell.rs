//! `session.shell` — POST /api/session/{sessionID}/shell.
//!
//! Ports upstream `Session.shell` (`packages/core/src/session/session.ts`):
//! run a command the user typed, record it as `session.shell.started` /
//! `session.shell.ended` (projected into one `Message::Shell` row), and leave
//! the result in the transcript as a `source: "shell"` synthetic that does not
//! resume the agent. The run happens in a spawned task so the server records
//! the completion even when the submitting client disconnects; the handler
//! still awaits it, as upstream joins the fiber.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::inbox::{Delivery, Item, SyntheticPayload};
use ox_schema::support::{Record, ShellOutput};
use ox_tools::shell::sessions::{Owner, ShellSession, ShellSessions, SpawnRequest};
use ox_tools::shell::user;

use super::response::{invalid_request, no_content, parse_body, session_not_found, unknown_error};
use super::session_row::load_session;
use crate::api::state;

/// Upstream preview size: one `Shell.output` page of 1 MiB from cursor 0.
const PREVIEW_BYTES: usize = 1024 * 1024;
/// How long one settle wait lasts before it is re-armed. Bounded only so the
/// loop never parks on a single unbounded timer; the run itself has no
/// deadline (upstream `timeout: 0`).
const SETTLE_SLICE: Duration = Duration::from_secs(60);

#[derive(Default, Deserialize)]
struct ShellBody {
    #[serde(default)]
    id: Option<String>,
    command: String,
}

#[handler] // operation_id: session.shell
pub(crate) async fn shell(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ShellBody>(req, res).await else {
        return;
    };
    let directory = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row.directory,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    let shells = Arc::clone(&app.services().tools.shells);
    let started = shells.spawn(SpawnRequest {
        command: body.command.clone(),
        cwd: PathBuf::from(&directory),
        timeout_ms: None,
        completion: None,
        foreground: false,
        owner: Some(Owner {
            session_id: session_id.clone(),
            directory,
        }),
        metadata: Record::from([("background".to_owned(), json!(true))]),
    });
    let session = match started {
        Ok(session) => session,
        Err(error) => {
            let payload = SyntheticPayload {
                text: format!(
                    "User shell command failed to start:\n{}\n\n{error}",
                    body.command
                ),
                description: Some(body.command),
                metadata: Some(Record::from([
                    ("source".to_owned(), json!("shell")),
                    ("state".to_owned(), json!("error")),
                ])),
            };
            if let Err(publish) = enqueue(&app.bus, &session_id, payload).await {
                tracing::warn!(error = %publish, "shell start-failure synthetic not recorded");
            }
            unknown_error(res, &anyhow::Error::from(error));
            return;
        }
    };

    let event_id = body.id.map(|id| match id.strip_prefix("msg_") {
        Some(rest) => format!("evt_{rest}"),
        None => id,
    });
    let outcome = app
        .runtime
        .own(record(
            shells,
            app.db.clone(),
            app.bus.clone(),
            session_id,
            session,
            event_id,
        ))
        .await;
    match outcome {
        Ok(Ok(())) => no_content(res),
        Ok(Err(error)) => unknown_error(res, &error),
        Err(error) => unknown_error(res, &error),
    }
}

/// Publish started, wait for the terminal state, publish ended with the
/// output preview, then leave the user notification in the transcript.
async fn record(
    shells: Arc<ShellSessions>,
    db: Db,
    bus: EventBus,
    session_id: String,
    session: Arc<ShellSession>,
    event_id: Option<String>,
) -> anyhow::Result<()> {
    let started = SessionEventData::SessionShellStarted {
        session_id: session_id.clone(),
        shell: session.info(),
    };
    match event_id {
        Some(event_id) => bus.publish_as(started, event_id).await?,
        None => bus.publish(started).await?,
    }
    while !session.wait_settled(SETTLE_SLICE).await {}

    // Upstream reads the preview back by id; a session already dropped from
    // the table has no capture left.
    let output = shells
        .get(session.id.as_str())
        .map_or_else(user::unavailable_output, |live| {
            let (page, _) = live.poll(0, PREVIEW_BYTES);
            ShellOutput {
                output: page.output,
                cursor: i64::try_from(page.cursor).unwrap_or(i64::MAX),
                size: i64::try_from(page.size).unwrap_or(i64::MAX),
                truncated: page.truncated,
            }
        });
    bus.publish(SessionEventData::SessionShellEnded {
        session_id: session_id.clone(),
        shell: session.info(),
        output,
    })
    .await?;

    // Upstream ignores `Session.NotFoundError` here: the session may have
    // been deleted while the command ran.
    if load_session(&db, &session_id).await?.is_none() {
        return Ok(());
    }
    enqueue(&bus, &session_id, user::user_notification(&session)).await
}

/// Enqueue a steer synthetic without dispatching the runner (upstream
/// `synthetic(.., { resume: false })`).
async fn enqueue(
    bus: &EventBus,
    session_id: &str,
    payload: SyntheticPayload,
) -> anyhow::Result<()> {
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: session_id.to_owned(),
        inbox_id: id::id("msg"),
        item: Item::Synthetic {
            payload,
            delivery: Delivery::Steer,
        },
    })
    .await
}
