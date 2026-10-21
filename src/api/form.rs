//! `/api/form`, `/api/session/{id}/form` and the reply endpoints: the HTTP
//! face of the [`crate::tool::form`] registry.
//!
//! These mirror upstream `SessionForm` (packages/core/src/session/form.ts):
//! list pending forms for a session, read one, answer it, cancel it. Unknown
//! sessions answer `200 []` rather than 404 — the TUI's bootstrap asks for
//! `"global"` before any session exists.

use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::api::compat::directory_of;
use crate::tool::form::{self, Answer};

/// `GET /api/form` — every pending form in the process.
#[handler]
pub async fn list(req: &mut Request, res: &mut Response) {
    let forms = form::shared();
    res.render(Json(json!({
        "location": { "directory": directory_of(req) },
        "data": forms.list_all(),
    })));
}

/// `GET /api/session/{sessionID}/form`
#[handler]
pub async fn session_list(req: &mut Request, res: &mut Response) {
    let session_id = req.param::<String>("sessionID").unwrap_or_default();
    res.render(Json(json!({ "data": form::shared().list(&session_id) })));
}

/// `GET /api/session/{sessionID}/form/{formID}`
#[handler]
pub async fn get(req: &mut Request, res: &mut Response) {
    let session_id = req.param::<String>("sessionID").unwrap_or_default();
    let form_id = req.param::<String>("formID").unwrap_or_default();
    match form::shared().get(&session_id, &form_id) {
        Some(pending) => {
            let state = pending.state();
            res.render(Json(json!({
                "data": {
                    "id": pending.info().id,
                    "sessionID": pending.info().session_id,
                    "title": pending.info().title,
                    "fields": pending.info().fields,
                    "state": state,
                }
            })));
        }
        None => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(json!({
                "_tag": "FormNotFoundError",
                "message": format!("Form not found: {form_id}"),
            })));
        }
    }
}

/// The body of a reply: `{"answer": {"q0": "API"}}`.
#[derive(Debug, Deserialize)]
pub struct ReplyBody {
    pub answer: Answer,
}

/// `POST /api/session/{sessionID}/form/{formID}/reply`
#[handler]
pub async fn reply(req: &mut Request, res: &mut Response) {
    let session_id = req.param::<String>("sessionID").unwrap_or_default();
    let form_id = req.param::<String>("formID").unwrap_or_default();
    let body = match req.parse_json::<ReplyBody>().await {
        Ok(body) => body,
        Err(_) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(json!({
                "_tag": "InvalidRequestError",
                "message": "Expected JSON body {\"answer\": {<key>: <value>}}",
            })));
            return;
        }
    };
    if form::shared().reply(&session_id, &form_id, body.answer) {
        res.status_code(StatusCode::OK);
        return;
    }
    // Either the form is gone or it already settled — both mean this reply
    // cannot be applied.
    res.status_code(StatusCode::CONFLICT);
    res.render(Json(json!({
        "_tag": "FormNotFoundError",
        "message": format!("Form not found or already answered: {form_id}"),
    })));
}

/// `DELETE /api/session/{sessionID}/form/{formID}?message=…`
#[handler]
pub async fn cancel(req: &mut Request, res: &mut Response) {
    let session_id = req.param::<String>("sessionID").unwrap_or_default();
    let form_id = req.param::<String>("formID").unwrap_or_default();
    let message = req
        .query::<String>("message")
        .filter(|message| !message.is_empty());
    if form::shared().cancel(&session_id, &form_id, message) {
        form::shared().remove(&session_id, &form_id);
        res.status_code(StatusCode::OK);
        return;
    }
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "FormNotFoundError",
        "message": format!("Form not found: {form_id}"),
    })));
}
