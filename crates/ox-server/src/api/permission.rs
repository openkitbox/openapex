//! V2 permission endpoints. The HTTP layer reads and writes the exact gate
//! stored on [`AppState`]; it never creates a second pending registry.

use std::path::Path;
use std::sync::Arc;

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use ox_db::Db;
use ox_engine::permission::store::UpdateError;
use ox_schema::permission::Asked;

use super::compat::{directory_of, envelope};
use super::session::{invalid_request, unknown_error};
use super::state;
use crate::project::resolve_project;

/// Echo the requested location in a response header while leaving the
/// upstream body shape (`{data: …}` or a 204) untouched.
pub(crate) fn echo_location(req: &Request, res: &mut Response) -> String {
    let directory = directory_of(req);
    let _ = res.add_header("x-opencode-directory", directory.as_str(), true);
    directory
}

/// 404 `PermissionNotFoundError` for a missing or differently-owned request.
pub(crate) fn permission_not_found(res: &mut Response, request_id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "PermissionNotFoundError",
        "requestID": request_id,
        "message": format!("Permission request not found: {request_id}"),
    })));
}

fn saved_not_found(res: &mut Response, id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "PermissionSavedNotFoundError",
        "id": id,
        "message": format!("Saved permission not found: {id}"),
    })));
}

fn saved_conflict(res: &mut Response, id: &str, action: &str, resource: &str) {
    res.status_code(StatusCode::CONFLICT);
    res.render(Json(json!({
        "_tag": "ConflictError",
        "resource": id,
        "message": format!("A saved permission already allows {action} for {resource}"),
    })));
}

/// Upstream's global pending list is scoped by its location service. Ox keeps
/// one gate for the whole process, so derive the location filter from the
/// authoritative session rows rather than keeping a second pending map.
async fn pending_at(
    db: &Db,
    directory: &str,
    pending: &[Arc<Asked>],
) -> anyhow::Result<Vec<Arc<Asked>>> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT directory FROM session_v2 WHERE id = ?1")
        .await?;
    let mut visible = Vec::with_capacity(pending.len());
    for request in pending {
        let row = ox_db::optional(&mut stmt, (request.session_id.as_str(),)).await?;
        if let Some(row) = row {
            let stored: String = row.get(0)?;
            if stored == directory {
                visible.push(Arc::clone(request));
            }
        }
    }
    Ok(visible)
}

/// `GET /api/permission/request` — upstream `permission.request.list`.
#[handler] // operation_id: permission.request.list
pub(crate) async fn request_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let directory = echo_location(req, res);
    let pending = app.permissions.pending();
    let visible = match pending_at(&app.db, &directory, &pending).await {
        Ok(visible) => visible,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    let data = match visible
        .iter()
        .map(|request| serde_json::to_value(request.as_ref()))
        .collect::<Result<Vec<Value>, _>>()
    {
        Ok(data) => Value::Array(data),
        Err(error) => {
            unknown_error(res, &anyhow::anyhow!("{error}"));
            return;
        }
    };
    res.render(Json(envelope(&directory, data)));
}

async fn project_for(
    app: &super::AppState,
    directory: &str,
    project_id: Option<String>,
    res: &mut Response,
) -> Option<String> {
    if let Some(project_id) = project_id {
        return Some(project_id);
    }
    match resolve_project(&app.db, Path::new(directory)).await {
        Ok(project) => Some(project.id),
        Err(_) => {
            invalid_request(res, "Directory does not exist");
            None
        }
    }
}

/// `GET /api/permission/saved` — upstream `permission.saved.list`.
#[handler] // operation_id: permission.saved.list
pub(crate) async fn saved_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let directory = echo_location(req, res);
    let requested = req.query::<String>("projectID");
    let Some(project_id) = project_for(app, &directory, requested, res).await else {
        return;
    };
    match app.permissions.saved().list(&project_id).await {
        Ok(rows) => res.render(Json(json!({ "data": rows }))),
        Err(error) => unknown_error(res, &error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct SavedPatch {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

/// `PATCH /api/permission/saved/{id}` — upstream `permission.saved.update`.
#[handler] // operation_id: permission.saved.update
pub(crate) async fn saved_update(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        invalid_request(res, "Missing saved permission id");
        return;
    };
    let directory = echo_location(req, res);
    let Ok(body) = req.parse_json::<SavedPatch>().await else {
        invalid_request(res, "Invalid request body");
        return;
    };
    let Some(project_id) = project_for(app, &directory, None, res).await else {
        return;
    };
    match app
        .permissions
        .saved()
        .update(
            &project_id,
            &id,
            body.action.as_deref(),
            body.resource.as_deref(),
        )
        .await
    {
        Ok(info) => res.render(Json(json!({ "data": info }))),
        Err(UpdateError::NotFound { .. }) => saved_not_found(res, &id),
        Err(UpdateError::Conflict {
            action, resource, ..
        }) => saved_conflict(res, &id, &action, &resource),
        Err(error) => unknown_error(res, &anyhow::anyhow!("{error}")),
    }
}

/// `DELETE /api/permission/saved/{id}` — upstream `permission.saved.remove`.
/// Missing ids are a no-op so a repeated delete stays a 204.
#[handler] // operation_id: permission.saved.remove
pub(crate) async fn saved_delete(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        invalid_request(res, "Missing saved permission id");
        return;
    };
    let directory = echo_location(req, res);
    let Some(project_id) = project_for(app, &directory, None, res).await else {
        return;
    };
    match app.permissions.saved().delete(&project_id, &id).await {
        Ok(()) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(error) => unknown_error(res, &error),
    }
}
