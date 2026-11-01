use anyhow::{Result, anyhow};
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::support::{ForkBoundary, LocationRef, ModelRef, Rule, SessionMetadata};

use super::response::{
    invalid_request, invalid_request_kind, message_not_found, no_content, parse_body,
    session_not_found, unknown_error,
};
use super::session_row::{load_children, load_session, resolve_project, subpath_of};
use crate::api::state;

// ---------------------------------------------------------------------------
// session.create — POST /api/session
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct CreateBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<ModelRef>,
    #[serde(default)]
    location: Option<LocationRef>,
    #[serde(default)]
    metadata: Option<SessionMetadata>,
    #[serde(default)]
    permissions: Option<Vec<Rule>>,
}

#[handler] // operation_id: session.create
pub(crate) async fn create(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(body) = parse_body::<CreateBody>(req, res).await else {
        return;
    };

    let session_id = body.id.clone().unwrap_or_else(|| id::id("ses"));

    // Idempotent create: an existing row wins (upstream `store.get` shortcut).
    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => {
            res.render(Json(json!({ "data": row.info() })));
            return;
        }
        Ok(None) => {}
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    // Upstream resolves the create directory from the request itself when the
    // body carries none (`location.ts:73-75`): `location[directory]` query,
    // then the percent-encoded `x-opencode-directory` header, then the
    // server's directory.
    let request_directory = crate::api::compat::request_directory(req);

    // Location, metadata and permissions are inherited from the parent the way
    // upstream inherits them in `Session.create`.
    let (location, subpath, parent_metadata, parent_permissions, parent_agent, parent_model) =
        match &body.parent_id {
            Some(parent_id) => match load_session(&app.db, parent_id).await {
                Ok(Some(parent)) => {
                    let model = parent
                        .model
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<ModelRef>(raw).ok());
                    let metadata = parent
                        .metadata
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<SessionMetadata>(raw).ok());
                    let permissions = parent
                        .permission
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Vec<Rule>>(raw).ok());
                    (
                        LocationRef {
                            directory: parent.directory.clone(),
                            workspace_id: parent.workspace_id.clone(),
                        },
                        parent.path.clone(),
                        metadata,
                        permissions,
                        parent.agent.clone(),
                        model,
                    )
                }
                Ok(None) => {
                    session_not_found(res, parent_id);
                    return;
                }
                Err(error) => {
                    unknown_error(res, &error);
                    return;
                }
            },
            None => {
                let directory = body
                    .location
                    .as_ref()
                    .map(|l| l.directory.clone())
                    .or(request_directory)
                    .unwrap_or_else(|| app.directory.to_string_lossy().into_owned());
                let (_, project_directory) = resolve_project(&app.db, &directory).await;
                (
                    LocationRef {
                        directory: directory.clone(),
                        workspace_id: body.location.as_ref().and_then(|l| l.workspace_id.clone()),
                    },
                    subpath_of(&directory, &project_directory),
                    None,
                    None,
                    None,
                    None,
                )
            }
        };

    let (project_id, _) = resolve_project(&app.db, &location.directory).await;

    let event = SessionEventData::SessionCreated {
        session_id: session_id.clone(),
        project_id,
        location,
        subpath,
        parent_id: body.parent_id.clone(),
        slug: ox_schema::slug::create(),
        title: body.title.clone(),
        agent: body.agent.clone().or(parent_agent),
        model: body.model.clone().or(parent_model),
        metadata: body.metadata.clone().or(parent_metadata),
        permissions: body.permissions.clone().or(parent_permissions),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };

    if let Err(error) = app.bus.publish(event).await {
        // A concurrent create that lost the projection race is not an error:
        // the existing row wins.
        match load_session(&app.db, &session_id).await {
            Ok(Some(row)) => {
                res.render(Json(json!({ "data": row.info() })));
            }
            _ => unknown_error(res, &error),
        }
        return;
    }

    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => unknown_error(res, &anyhow!("session `{session_id}` missing after create")),
        Err(error) => unknown_error(res, &error),
    }
}

// ---------------------------------------------------------------------------
// session.get — GET /api/session/{sessionID}
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.get
pub(crate) async fn get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    match load_session(&app.db, &session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => session_not_found(res, &session_id),
        Err(error) => unknown_error(res, &error),
    }
}

// ---------------------------------------------------------------------------
// session.update — PATCH /api/session/{sessionID}
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct UpdateBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    metadata: Option<SessionMetadata>,
    #[serde(default)]
    permissions: Option<Vec<Rule>>,
    /// `true` archives the session, `false` restores it (upstream
    /// `session.update`); repeating the current state is a no-op.
    #[serde(default)]
    archived: Option<bool>,
}

#[handler] // operation_id: session.update
pub(crate) async fn update(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<UpdateBody>(req, res).await else {
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

    if let Some(title) = &body.title {
        if !title.is_empty() {
            if let Err(error) = app
                .bus
                .publish(SessionEventData::SessionRenamed {
                    session_id: session_id.clone(),
                    title: title.clone(),
                })
                .await
            {
                unknown_error(res, &error);
                return;
            }
        } else {
            // An empty title asks for a generated one (upstream handlers/
            // session.ts:234 → `title.generate`). Generation is background
            // work: the request answers 204 and the title arrives as a
            // `session.renamed` event, exactly as upstream does.
            spawn_title_generation(app, session_id.clone());
        }
    }

    if let Some(metadata) = body.metadata
        && let Err(error) = app
            .bus
            .publish(SessionEventData::SessionMetadataUpdated {
                session_id: session_id.clone(),
                metadata,
            })
            .await
    {
        unknown_error(res, &error);
        return;
    }

    if let Some(permissions) = body.permissions
        && let Err(error) = app
            .bus
            .publish(SessionEventData::SessionPermissions {
                session_id: session_id.clone(),
                permissions,
            })
            .await
    {
        unknown_error(res, &error);
        return;
    }

    if let Some(archived) = body.archived {
        // Upstream `Session.archive`/`Session.unarchive`: repeating the current
        // state is a no-op, so no second event and no `time_updated` bump.
        let already = session.time_archived.is_some();
        if archived != already {
            let event = if archived {
                SessionEventData::SessionArchived {
                    session_id: session_id.clone(),
                }
            } else {
                SessionEventData::SessionUnarchived {
                    session_id: session_id.clone(),
                }
            };
            if let Err(error) = app.bus.publish(event).await {
                unknown_error(res, &error);
                return;
            }
        }
    }

    no_content(res);
}

/// Spawn one background title generation for `session_id` (upstream
/// `SessionTitle.generate`). Fire-and-forget: title generation is advisory, so
/// a failure is logged inside the task and never reaches the response.
fn spawn_title_generation(app: &crate::api::AppState, session_id: String) {
    let db = app.db.clone();
    let bus = app.bus.clone();
    // One snapshot, so chat, catalog and config share a generation.
    let svc = app.services();
    let chat = Arc::clone(&svc.chat);
    let config = Arc::clone(&svc.config);
    let catalog = Arc::clone(&svc.model_catalog);
    tokio::spawn(async move {
        ox_engine::runner::title::generate(db, bus, chat, config, catalog, session_id).await;
    });
}

// ---------------------------------------------------------------------------
// session.remove — DELETE /api/session/{sessionID}
// ---------------------------------------------------------------------------

#[handler] // operation_id: session.remove
pub(crate) async fn remove(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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

    if let Err(error) = delete_session_tree(app, &session_id).await {
        unknown_error(res, &error);
        return;
    }

    no_content(res);
}

/// Delete `root` and every descendant: descendants first (deepest last-found
/// first), then the root. Each session goes through a `SessionDeleted` event
/// (so the projection, the event log and SSE subscribers all agree), then its
/// durable event log is purged and the runner forgets it. Stops at the first
/// error; sessions already deleted stay deleted and the root is still present,
/// so a retry finishes the job. Shared by the HTTP handler and any internal
/// caller that must delete exactly as the API does.
pub(crate) async fn delete_session_tree(app: &crate::api::AppState, root: &str) -> Result<()> {
    let mut order = collect_descendants(&app.db, root).await?;
    order.reverse();
    order.push(root.to_owned());

    for target in order {
        app.bus
            .publish(SessionEventData::SessionDeleted {
                session_id: target.clone(),
            })
            .await?;
        app.bus.remove(&target).await?;
        // Drop the runner's per-session tables now rather than waiting for the
        // idle sweep's 30-minute TTL (the sweep only runs inside a dispatch, so
        // a deleted session's entry could otherwise outlive it indefinitely).
        app.runner.forget_session(&target);
    }
    Ok(())
}

/// Every descendant of `root`, breadth-first (parents before children).
async fn collect_descendants(db: &Db, root: &str) -> Result<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut collected = Vec::new();
    let mut frontier = vec![root.to_owned()];
    while let Some(parent) = frontier.pop() {
        for child in load_children(db, &parent).await? {
            if seen.insert(child.clone()) {
                collected.push(child.clone());
                frontier.push(child);
            }
        }
    }
    Ok(collected)
}

// ---------------------------------------------------------------------------
// session.fork — POST /api/session/{sessionID}/fork
// ---------------------------------------------------------------------------

#[derive(Default, Deserialize)]
struct ForkBody {
    #[serde(default)]
    before: Option<String>,
}

#[handler] // operation_id: session.fork
pub(crate) async fn fork(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<ForkBody>(req, res).await else {
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

    let boundary = match find_fork_boundary(&app.db, &session_id, body.before.as_deref()).await {
        Ok(boundary) => boundary,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    let boundary = match boundary {
        Some(boundary) => boundary,
        None => {
            if let Some(message_id) = &body.before {
                message_not_found(res, &session_id, message_id);
            } else {
                invalid_request_kind(res, "Session has no messages to fork", "empty_session");
            }
            return;
        }
    };

    let new_session_id = id::id("ses");
    let event = SessionEventData::SessionForked {
        session_id: new_session_id.clone(),
        parent_id: session_id.clone(),
        boundary,
        instructions: None,
        instruction_entries: None,
    };

    if let Err(error) = app.bus.publish(event).await {
        unknown_error(res, &error);
        return;
    }

    match load_session(&app.db, &new_session_id).await {
        Ok(Some(row)) => res.render(Json(json!({ "data": row.info() }))),
        Ok(None) => unknown_error(res, &anyhow!("forked session `{new_session_id}` missing")),
        Err(error) => unknown_error(res, &error),
    }
}

/// The fork boundary: the named message (`before`, exclusive) or the newest
/// message (`through`, inclusive). Mirrors upstream `Session.fork`.
async fn find_fork_boundary(
    db: &Db,
    session_id: &str,
    before: Option<&str>,
) -> Result<Option<ForkBoundary>> {
    let conn = db.acquire().await?;
    let found: Option<String> = match before {
        Some(message_id) => {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM session_message WHERE session_id = ?1 AND id = ?2 \
                     ORDER BY seq DESC LIMIT 1",
                )
                .await?;
            ox_db::optional(&mut stmt, (session_id, message_id))
                .await?
                .map(|row| row.get::<String>(0))
                .transpose()?
        }
        None => {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM session_message WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                )
                .await?;
            ox_db::optional(&mut stmt, [session_id])
                .await?
                .map(|row| row.get::<String>(0))
                .transpose()?
        }
    };

    Ok(found.map(|message_id| match before {
        Some(_) => ForkBoundary::Before { message_id },
        None => ForkBoundary::Through { message_id },
    }))
}
