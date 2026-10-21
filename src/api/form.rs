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

/// The body of `POST /api/session/{sessionID}/form` — upstream
/// `FormCreatePayload` (`packages/schema/src/form.ts`): an optional id, a
/// title and the fields to render.
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub fields: Vec<crate::tool::form::Field>,
}

/// `POST /api/session/{sessionID}/form` — upstream `session.form.create`.
/// Creates a form in the registry, publishes `form.created`, and answers
/// `{data: Form.Info}`; a duplicate id conflicts.
#[handler]
pub async fn create(req: &mut Request, res: &mut Response) {
    let session_id = req.param::<String>("sessionID").unwrap_or_default();
    let Ok(body) = req.parse_json::<CreateBody>().await else {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Invalid form payload",
            "field": "fields",
        })));
        return;
    };
    match form::shared().create_with(
        &session_id,
        body.id,
        &directory_of(req),
        &body.title,
        body.fields,
        body.metadata,
    ) {
        Ok(pending) => {
            res.render(Json(json!({ "data": pending.info() })));
        }
        Err(form::CreateError::AlreadyExists) => {
            res.status_code(StatusCode::CONFLICT);
            res.render(Json(json!({
                "_tag": "ConflictError",
                "message": "Form already exists",
            })));
        }
    }
}

/// `GET /api/session/{sessionID}/form/{formID}`
#[handler]
pub async fn get(req: &mut Request, res: &mut Response) {    let session_id = req.param::<String>("sessionID").unwrap_or_default();
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
        None => not_found(res, &form_id),
    }
}

/// 404 `FormNotFoundError` — the wire shape the TUI client's error matcher
/// (`isFormNotFoundError`) swallows after a lost race.
fn not_found(res: &mut Response, form_id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "FormNotFoundError",
        "id": form_id,
        "message": format!("Form not found: {form_id}"),
    })));
}

/// 409 `FormAlreadySettledError` — answered twice or answered after a cancel.
/// The TUI's reply path treats exactly this tag as benign, so the tag must be
/// the settled one, not `FormNotFoundError`.
fn already_settled(res: &mut Response, form_id: &str) {
    res.status_code(StatusCode::CONFLICT);
    res.render(Json(json!({
        "_tag": "FormAlreadySettledError",
        "id": form_id,
        "message": format!("Form already settled: {form_id}"),
    })));
}

/// 204 for a settled transition — upstream `HttpApiSchema.NoContent`, and the
/// TUI client hard-codes `successStatus: 204` on both reply and cancel.
fn no_content(res: &mut Response) {
    res.status_code(StatusCode::NO_CONTENT);
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
    match form::shared().reply(&session_id, &form_id, body.answer) {
        Ok(()) => no_content(res),
        Err(form::SettleError::NotFound) => not_found(res, &form_id),
        Err(form::SettleError::AlreadySettled) => already_settled(res, &form_id),
    }
}

/// `DELETE /api/session/{sessionID}/form/{formID}?message=…`
#[handler]
pub async fn cancel(req: &mut Request, res: &mut Response) {
    let session_id = req.param::<String>("sessionID").unwrap_or_default();
    let form_id = req.param::<String>("formID").unwrap_or_default();
    let message = req
        .query::<String>("message")
        .filter(|message| !message.is_empty());
    match form::shared().cancel(&session_id, &form_id, message) {
        Ok(()) => no_content(res),
        Err(form::SettleError::NotFound) => not_found(res, &form_id),
        Err(form::SettleError::AlreadySettled) => already_settled(res, &form_id),
    }
}
