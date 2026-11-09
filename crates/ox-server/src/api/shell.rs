//! `shell.*` endpoints: list sessions, read one, page its output, remove it.
//!
//! Mirrors upstream `packages/server` `shell.list` / `shell.get` /
//! `shell.output` / `shell.remove`, over the same in-memory table the `shell`
//! tool writes to ([`ox_tools::shell::sessions`]). Capture lives in memory,
//! so these handlers only read the retained tail: no database round trip and
//! no file I/O on the request path.

use salvo::prelude::*;
use serde_json::json;

use crate::api::compat::envelope;
use crate::api::state;
use ox_tools::shell::sessions::{self, DEFAULT_PAGE_BYTES, ShellSessions};

/// The composition's table, shared with its shell tool.
fn sessions(app: &crate::api::AppState) -> std::sync::Arc<ShellSessions> {
    std::sync::Arc::clone(&app.services().tools.shells)
}

/// `GET /api/shell` — upstream `shell.list`: every running session in this
/// process, oldest first. Settled sessions stay readable through `get` and
/// `output`.
#[handler] // operation_id: shell.list
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let directory = crate::api::compat::directory_of(req);
    res.render(Json(envelope(&directory, json!(sessions(app).list()))));
}

/// `GET /api/shell/{id}` — upstream `shell.get`.
#[handler] // operation_id: shell.get
pub(crate) async fn get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
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
    match sessions(app).get(&id) {
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
    let Some(app) = state(depot, res) else {
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
    let Some(session) = sessions(app).get(&id) else {
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

/// `DELETE /api/shell/{id}` — upstream `shell.remove`: forget a session. A
/// running one is stopped first (it settles as killed and announces
/// `shell.exited`), then the table announces `shell.deleted` with the owning
/// session's location. An unknown id is a no-op: upstream maps
/// `Shell.NotFoundError` to success, so the answer is 204 either way.
#[handler] // operation_id: shell.remove
pub(crate) async fn remove(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
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
    let table = sessions(app);
    // Always `stop`, even for a settled session: `settled()` flips before the
    // exited notice goes out, and `stop` on a settled session only kills any
    // leftover background process group and waits for the announcement, so
    // `deleted` below can never overtake `exited`. No table lock is held
    // across the await.
    if let Some(session) = table.get(&id) {
        session.stop(true).await;
    }
    table.remove(&id);
    res.status_code(StatusCode::NO_CONTENT);
}
