use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use ox_schema::event::SessionEventData;
use ox_schema::support::ModelRef;

use super::response::{invalid_request, no_content, parse_body, session_not_found, unknown_error};
use super::session_row::load_session;
use crate::api::state;

// ---------------------------------------------------------------------------
// session.view — POST /api/session/{sessionID}/view
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct ViewBody {
    #[serde(default)]
    idle: Option<i64>,
}

#[handler] // operation_id: session.view
pub(crate) async fn view(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ViewBody>(req, res).await else {
        return;
    };
    let Some(idle) = body.idle else {
        invalid_request(res, "Missing idle");
        return;
    };

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    // Upstream `Session.view`: only publish when the observed idle watermark is
    // new (not before the last idle transition, not already acknowledged).
    let should_publish = match session.time_idle {
        None => false,
        Some(time_idle) => {
            idle <= time_idle
                && session
                    .time_viewed
                    .map(|viewed| viewed < idle)
                    .unwrap_or(true)
        }
    };
    if should_publish
        && let Err(error) = app
            .bus
            .publish(SessionEventData::SessionViewed {
                session_id: session_id.clone(),
                idle,
            })
            .await
    {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

// ---------------------------------------------------------------------------
// session.switchAgent — POST /api/session/{sessionID}/agent
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct AgentBody {
    #[serde(default)]
    agent: Option<String>,
}

#[handler] // operation_id: session.switchAgent
pub(crate) async fn switch_agent(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<AgentBody>(req, res).await else {
        return;
    };
    let Some(agent) = body.agent else {
        invalid_request(res, "Missing agent");
        return;
    };
    if ox_tools::agent::get(&agent).is_none() {
        invalid_request(res, &format!("Unknown agent `{agent}`"));
        return;
    }

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    if let Err(error) = app
        .bus
        .publish(SessionEventData::SessionAgentSelected {
            session_id: session_id.clone(),
            agent,
            previous: session.agent,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

// ---------------------------------------------------------------------------
// session.switchModel — POST /api/session/{sessionID}/model
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct ModelBody {
    #[serde(default)]
    model: Option<ModelRef>,
}

#[handler] // operation_id: session.switchModel
pub(crate) async fn switch_model(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ModelBody>(req, res).await else {
        return;
    };
    let Some(model) = body.model else {
        invalid_request(res, "Missing model");
        return;
    };

    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    let previous = session
        .model
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ModelRef>(raw).ok());

    // Upstream `Session.switchModel`: no-op when the model is unchanged.
    let unchanged = previous.as_ref().is_some_and(|current| {
        current.provider_id == model.provider_id
            && current.id == model.id
            && variant_of(current) == variant_of(&model)
    });
    if unchanged {
        no_content(res);
        return;
    }

    // Normalize the variant the way `session.get` renders it
    // (`variant ?? "default"`, session/info.ts) so the event, the projected
    // row, and the `model-switched` message all carry a concrete variant.
    let mut model = model;
    if model.variant.is_none() {
        model.variant = Some("default".to_owned());
    }
    let previous = previous.map(|mut current| {
        if current.variant.is_none() {
            current.variant = Some("default".to_owned());
        }
        current
    });

    if let Err(error) = app
        .bus
        .publish(SessionEventData::SessionModelSelected {
            session_id: session_id.clone(),
            model,
            previous,
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

fn variant_of(model: &ModelRef) -> &str {
    model.variant.as_deref().unwrap_or("default")
}

// ---------------------------------------------------------------------------
// session.active — GET /api/session/active
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.active
pub(crate) async fn active(_req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    // Upstream reports the in-process `SessionExecution.active` set as
    // `{<sessionID>: {type: "running"}}` (`handlers/session.ts` `active`).
    let data: Map<String, Value> = app
        .runner
        .active_sessions()
        .into_iter()
        .map(|id| (id, json!({ "type": "running" })))
        .collect();
    res.render(Json(json!({ "data": data })));
}
