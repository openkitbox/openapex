//! `shell.*` endpoints: list sessions, read one, page its output, remove it.
//!
//! Mirrors upstream `packages/server` `shell.create` / `shell.list` /
//! `shell.get` / `shell.output` / `shell.remove`, over the same in-memory table the `shell`
//! tool writes to ([`ox_tools::shell::sessions`]). Capture lives in memory,
//! so these handlers only read the retained tail: no database round trip and
//! no file I/O on the request path.

use std::path::{Path, PathBuf};

use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::api::compat::envelope;
use crate::api::state;
use ox_schema::support::Record;
use ox_tools::shell::sessions::SpawnRequest;
use ox_tools::shell::sessions::{self, DEFAULT_PAGE_BYTES, ShellSessions};

/// The composition's table, shared with its shell tool.
fn sessions(app: &crate::api::AppState) -> std::sync::Arc<ShellSessions> {
    std::sync::Arc::clone(&app.services().tools.shells)
}

#[derive(Debug, Default, Deserialize)]
struct CreateInput {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    metadata: Record,
}

fn resolve_workdir_impl(base: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    let path = requested.map_or_else(|| base.to_path_buf(), |cwd| base.join(cwd));
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("working directory cannot be resolved: {error}"))?;
    if !canonical.is_dir() {
        return Err("working directory is not a directory".to_owned());
    }
    Ok(canonical)
}

fn resolve_workdir(base: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    let requested = requested.filter(|cwd| !cwd.is_empty());
    resolve_workdir_impl(base, requested)
}

fn request_error(res: &mut Response, message: impl Into<String>) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message.into(),
    })));
}

/// `POST /api/shell` — upstream `shell.create`. The shell table owns process
/// capture, timeout, lifecycle events, and retention; this handler only maps
/// the request into its spawn contract and returns the created info.
#[handler] // operation_id: shell.create
pub(crate) async fn create(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let body = match req
        .parse_json_with_max_size::<CreateInput>(32 * 1024 * 1024)
        .await
    {
        Ok(body) => body,
        Err(_) => {
            request_error(res, "Invalid request body");
            return;
        }
    };
    if body.command.is_empty() {
        request_error(res, "command is required");
        return;
    }
    let requested_directory = crate::api::compat::directory_of(req);
    let base = PathBuf::from(&requested_directory);
    let cwd = match resolve_workdir(&base, body.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => {
            request_error(res, error);
            return;
        }
    };
    let started = sessions(app).spawn(SpawnRequest {
        command: body.command,
        cwd,
        timeout_ms: body.timeout,
        completion: None,
        foreground: false,
        owner: None,
        metadata: body.metadata,
    });
    let session = match started {
        Ok(session) => session,
        Err(error) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({
                "_tag": "UnknownError",
                "message": format!("shell command failed to start: {error}"),
            })));
            return;
        }
    };
    res.render(Json(crate::api::compat::envelope(
        &requested_directory,
        json!(session.info()),
    )));
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

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn shell_create_resolves_relative_workdir_against_requested_directory() {
        let root = unique_test_directory("relative");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("test directory should be created");

        let resolved = super::resolve_workdir(&root, Some("nested"))
            .expect("relative shell cwd should resolve");

        assert_eq!(
            resolved,
            nested.canonicalize().expect("nested should canonicalize")
        );
        fs::remove_dir_all(root).expect("test directory should be removed");
    }

    #[test]
    fn shell_create_rejects_missing_workdir() {
        let root = unique_test_directory("missing");
        fs::create_dir_all(&root).expect("test directory should be created");

        let error = super::resolve_workdir(&root, Some("does-not-exist"))
            .expect_err("missing shell cwd must fail closed");

        assert!(error.contains("working directory"));
        fs::remove_dir_all(root).expect("test directory should be removed");
    }

    fn unique_test_directory(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ox-shell-create-{label}-{}", std::process::id()))
    }
}
