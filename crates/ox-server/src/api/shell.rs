//! `shell.*` endpoints: list sessions, read one, page its output, delete it.
//!
//! Mirrors upstream `packages/server` `shell.list` / `shell.get` /
//! `shell.output` / `shell.delete`, over the same in-memory table the `shell`
//! tool writes to ([`ox_tools::shell::sessions`]). Capture lives in memory,
//! so these handlers only read the retained tail: no database round trip and
//! no file I/O on the request path.

use salvo::prelude::*;
use serde_json::json;

use crate::api::compat::envelope;
use crate::api::state;
use ox_tools::shell::sessions::{self, DEFAULT_PAGE_BYTES, ShellSessions};

/// The process-wide table, shared with the shell tool.
fn sessions() -> std::sync::Arc<ShellSessions> {
    sessions::shared()
}

/// `GET /api/shell` — upstream `shell.list`: every session in this process,
/// oldest first.
#[handler] // operation_id: shell.list
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(_app) = state(depot, res) else {
        return;
    };
    let directory = crate::api::compat::directory_of(req);
    res.render(Json(envelope(&directory, json!(sessions().list()))));
}

/// `GET /api/shell/{id}` — upstream `shell.get`.
#[handler] // operation_id: shell.get
pub(crate) async fn get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(_app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Missing id",
        })));
        return;
    };
    let directory = crate::api::compat::directory_of(req);
    match sessions().get(&id) {
        Some(session) => res.render(Json(envelope(&directory, json!(session.info())))),
        None => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(json!({
                "_tag": "ShellNotFoundError",
                "message": format!("Shell session not found: {id}"),
            })));
        }
    }
}

/// `GET /api/shell/{id}/output?cursor=N&limit=N` — upstream `shell.output`:
/// one page of the merged capture. `cursor` is a byte offset; the answer says
/// whether it predates the retained tail (`truncated`) and where the capture
/// currently ends (`size`).
#[handler] // operation_id: shell.output
pub(crate) async fn output(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(_app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Missing id",
        })));
        return;
    };
    let Some(session) = sessions().get(&id) else {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(json!({
            "_tag": "ShellNotFoundError",
            "message": format!("Shell session not found: {id}"),
        })));
        return;
    };
    // Upstream types both as numeric query strings; a malformed value falls
    // back to the default rather than failing the read.
    let cursor = req.query::<u64>("cursor").unwrap_or(0);
    let limit = req
        .query::<usize>("limit")
        .unwrap_or(DEFAULT_PAGE_BYTES)
        .clamp(1, sessions::OUTPUT_CAPACITY);
    let (page, _exit) = session.poll(cursor, limit);
    let directory = crate::api::compat::directory_of(req);
    res.render(Json(envelope(
        &directory,
        json!({
            "output": page.output,
            "cursor": page.cursor,
            "size": page.size,
            "truncated": page.truncated,
        }),
    )));
}

/// `DELETE /api/shell/{id}` — upstream `shell.delete`: forget a settled
/// session. A running one is refused so its process is never orphaned.
#[handler] // operation_id: shell.delete
pub(crate) async fn remove(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(_app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Missing id",
        })));
        return;
    };
    if sessions().remove(&id) {
        res.status_code(StatusCode::NO_CONTENT);
        return;
    }
    match sessions().get(&id) {
        Some(session) if !session.settled() => {
            res.status_code(StatusCode::CONFLICT);
            res.render(Json(json!({
                "_tag": "ShellRunningError",
                "message": format!("Shell session {id} is still running; stop it first"),
            })));
        }
        Some(_) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        None => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(json!({
                "_tag": "ShellNotFoundError",
                "message": format!("Shell session not found: {id}"),
            })));
        }
    }
}
