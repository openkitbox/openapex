use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::de::DeserializeOwned;
use serde_json::json;

// ---------------------------------------------------------------------------
// Error responses (packages/protocol/errors.ts encoded shapes)
// ---------------------------------------------------------------------------

/// 404 `SessionNotFoundError`.
pub(crate) fn session_not_found(res: &mut Response, session_id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "SessionNotFoundError",
        "sessionID": session_id,
        "message": format!("Session not found: {session_id}"),
    })));
}

/// 404 `MessageNotFoundError`.
pub(crate) fn message_not_found(res: &mut Response, session_id: &str, message_id: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "MessageNotFoundError",
        "sessionID": session_id,
        "messageID": message_id,
        "message": format!("Message not found: {message_id}"),
    })));
}

/// 400 `InvalidRequestError`.
pub(crate) fn invalid_request(res: &mut Response, message: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
    })));
}

/// 404 `SkillNotFoundError` (protocol/src/errors.ts, `httpApiStatus: 404`).
pub(super) fn skill_not_found(res: &mut Response, skill: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "SkillNotFoundError",
        "skill": skill,
        "message": format!("Skill not found: {skill}"),
    })));
}

/// 400 `InvalidRequestError` carrying the `empty_session` kind (fork).
pub(super) fn invalid_request_kind(res: &mut Response, message: &str, kind: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
        "kind": kind,
    })));
}

/// 400 `InvalidRequestError` naming the offending request `field` (`files`).
pub(super) fn invalid_request_field(res: &mut Response, message: &str, field: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidRequestError",
        "message": message,
        "field": field,
    })));
}

/// 400 `InvalidCursorError` (protocol/src/errors.ts).
pub(super) fn invalid_cursor(res: &mut Response, message: &str) {
    res.status_code(StatusCode::BAD_REQUEST);
    res.render(Json(json!({
        "_tag": "InvalidCursorError",
        "message": message,
    })));
}

/// 500 `UnknownError` — never leaks internal detail on the wire.
pub(crate) fn unknown_error(res: &mut Response, error: &anyhow::Error) {
    tracing::error!(error = %error, "session handler failed");
    res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
    res.render(Json(json!({
        "_tag": "UnknownError",
        "message": "Unexpected server error. Check server logs for details.",
    })));
}

/// 204 No Content.
pub(super) fn no_content(res: &mut Response) {
    res.status_code(StatusCode::NO_CONTENT);
}

/// Largest JSON request body ox accepts.
///
/// salvo's default is 64 KB, which a single pasted message or an inline
/// attachment exceeds — upstream has no such limit, so a client would see
/// `InvalidRequestError` for a legal prompt. A 20 MiB attachment (the
/// materialization limit) is ~27 MB of base64 inside the JSON body, so this
/// leaves headroom above that while staying a bounded read (the body is
/// buffered once).
const MAX_JSON_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Parse an optional JSON request body; an empty body yields `T::default()`
/// (every body schema here has all-optional properties except where the
/// handler validates required fields itself). Malformed JSON is a 400.
pub(super) async fn parse_body<T>(req: &mut Request, res: &mut Response) -> Option<T>
where
    T: DeserializeOwned + Default,
{
    let bytes = match req.payload_with_max_size(MAX_JSON_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            invalid_request(res, "Invalid request body");
            return None;
        }
    };
    if bytes.is_empty() {
        return Some(T::default());
    }
    match serde_json::from_slice::<T>(bytes) {
        Ok(value) => Some(value),
        Err(_) => {
            invalid_request(res, "Invalid request body");
            None
        }
    }
}

/// 409 `ConflictError` (protocol/src/errors.ts).
pub(super) fn conflict_error(res: &mut Response, message: &str, resource: &str) {
    res.status_code(StatusCode::CONFLICT);
    res.render(Json(json!({
        "_tag": "ConflictError",
        "message": message,
        "resource": resource,
    })));
}
