//! The user's control over long-term memory, addressed through a session.
//!
//! - `GET  /api/session/{sessionID}/memory` — the session's memory switch.
//! - `POST /api/session/{sessionID}/memory` — flip it (`{"mode":"on"|"off"}`).
//! - `POST /api/session/{sessionID}/memory/clear` — archive every active
//!   memory of the session's project (`{"scope":"project"}`, the default) or
//!   of the user tier (`{"scope":"user"}`); answers `{"data":{"archived": n}}`.
//!
//! The switch lives in `ox_engine::mctx::memory_mode`; this module only
//! translates HTTP into it. The clear is addressed through a session rather
//! than a raw directory so the owner key is exactly the one the engine uses
//! for that session (`session_v2.directory`), never a client's respelling.

use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;

use ox_engine::mctx::memory::{USER_SCOPE, clear_owner};
use ox_engine::mctx::memory_mode::{self, MemoryMode};

use super::response::{invalid_request, no_content, parse_body, session_not_found, unknown_error};
use super::session_row::load_session;
use crate::api::state;

#[handler] // operation_id: session.memory.get
pub(crate) async fn memory_get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return session_not_found(res, &session_id),
        Err(error) => return unknown_error(res, &error),
    }
    match memory_mode::mode_of(&app.db, &session_id).await {
        Ok(mode) => res.render(Json(json!({ "data": { "mode": mode.as_str() } }))),
        Err(error) => unknown_error(res, &error),
    }
}

#[derive(Default, Deserialize)]
struct ModeBody {
    #[serde(default)]
    mode: Option<String>,
}

#[handler] // operation_id: session.memory.set
pub(crate) async fn memory_set(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ModeBody>(req, res).await else {
        return;
    };
    let Some(mode) = body.mode.as_deref().and_then(MemoryMode::parse) else {
        invalid_request(res, "mode must be \"on\" or \"off\"");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return session_not_found(res, &session_id),
        Err(error) => return unknown_error(res, &error),
    }
    match memory_mode::set(&app.db, &session_id, mode).await {
        Ok(()) => no_content(res),
        Err(error) => unknown_error(res, &error),
    }
}

/// Which tier a clear empties.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ClearScope {
    /// The session's project (its directory).
    #[default]
    Project,
    /// The user's cross-project tier.
    User,
}

#[derive(Default, Deserialize)]
struct ClearBody {
    #[serde(default)]
    scope: ClearScope,
}

/// The archive reason written on every row a clear touches.
const CLEAR_REASON: &str = "cleared by user";

#[handler] // operation_id: session.memory.clear
pub(crate) async fn memory_clear(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ClearBody>(req, res).await else {
        return;
    };
    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return session_not_found(res, &session_id),
        Err(error) => return unknown_error(res, &error),
    };
    let owner = match body.scope {
        ClearScope::Project => session.directory.as_str(),
        ClearScope::User => USER_SCOPE,
    };
    match clear_owner(&app.db, owner, CLEAR_REASON).await {
        Ok(archived) => res.render(Json(json!({ "data": { "archived": archived } }))),
        Err(error) => unknown_error(res, &error),
    }
}
