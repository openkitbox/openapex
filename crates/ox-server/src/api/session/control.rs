use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::support::LocationRef;

use super::inbox::load_inbox_row;
use super::response::{
    conflict_error, invalid_request, no_content, parse_body, session_not_found, unknown_error,
};
use super::session_row::{load_session, subpath_of};
use crate::api::state;

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
    app.runner.wait_idle(&session_id).await;
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
    let project =
        match crate::project::resolve_project_canonical(&app.db, std::path::Path::new(&directory))
            .await
        {
            Ok(project) => project,
            Err(error) => {
                unknown_error(res, &error);
                return;
            }
        };
    let payload = ox_schema::inbox::MovePayload {
        location: LocationRef {
            directory: directory.clone(),
            workspace_id: session.workspace_id.clone(),
        },
        project_id: project.id,
        subpath: subpath_of(&directory, &project.directory.to_string_lossy()),
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
