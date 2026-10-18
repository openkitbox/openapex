//! HTTP API handlers.
//!
//! Field names follow the opencode v2 protocol
//! (`packages/protocol/openapi.json`): `server.info` -> `ServerInfo`,
//! `location.get` -> `Location.PublicInfo`, `project.list` -> `Project[]`,
//! `agent.list` / `model.list` -> `{ location, data }` envelopes.

use std::path::PathBuf;

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Serialize;

use crate::config::Config;
use crate::db::Db;
use crate::event_bus::EventBus;
use crate::id::now_millis;
use crate::runner::Runner;

pub mod session;

/// Shared application state, injected via `salvo::affix_state`.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Db,
    /// In-process event bus; every session mutation is published through it.
    pub bus: EventBus,
    /// Working directory, canonicalized once at startup.
    pub directory: PathBuf,
    /// Per-session execution actor registry (task 6).
    pub runner: Runner,
}

/// Fetch the injected app state; on failure logs the error and renders a
/// 500, returning `None` so the handler can bail out without panicking
/// (project rule: no expect/unwrap in request paths).
fn state<'a>(depot: &'a Depot, res: &mut Response) -> Option<&'a AppState> {
    match depot.get_typed::<AppState>() {
        Ok(state) => Some(state),
        Err(_) => {
            tracing::error!("app state missing from depot");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// server.info — GET /api/info
// ---------------------------------------------------------------------------

/// Upstream `ServerInfo` schema (required: version, pid, urls, paths).
#[derive(Serialize)]
struct ServerInfo {
    version: &'static str,
    pid: u32,
    urls: Vec<String>,
    paths: ServerPaths,
    capabilities: ServerCapabilities,
}

#[derive(Serialize)]
struct ServerPaths {
    tmp: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerCapabilities {
    persistent_pty: bool,
}

#[handler] // operation_id: server.info
async fn server_info(depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    res.render(Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        urls: vec![format!("http://{}", state.config.listen)],
        paths: ServerPaths {
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
        },
        capabilities: ServerCapabilities {
            persistent_pty: false,
        },
    }));
}

// ---------------------------------------------------------------------------
// location.get — GET /api/location
// ---------------------------------------------------------------------------

/// Upstream `Location.PublicInfo` schema.
#[derive(Serialize)]
struct LocationInfo {
    directory: String,
    project: LocationProject,
}

#[derive(Serialize)]
struct LocationProject {
    id: String,
    directory: String,
    canonical: String,
}

#[handler] // operation_id: location.get
async fn location_get(depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let directory = state.directory.to_string_lossy().into_owned();
    res.render(Json(LocationInfo {
        project: LocationProject {
            id: "default".to_owned(),
            directory: directory.clone(),
            canonical: directory.clone(),
        },
        directory,
    }));
}

// ---------------------------------------------------------------------------
// project.list — GET /api/project
// ---------------------------------------------------------------------------

/// Upstream `Project` schema (required: id, canonical, time, sandboxes).
/// Reads the `project` table when the DB file is shared with an upstream
/// opencode server; otherwise returns a single synthetic project.
#[derive(Serialize)]
struct Project {
    id: String,
    canonical: String,
    time: ProjectTime,
    sandboxes: Vec<String>,
}

#[derive(Serialize)]
struct ProjectTime {
    created: i64,
    updated: i64,
    active: i64,
}

fn synthetic_project(directory: &std::path::Path) -> Project {
    // `state.directory` is already canonicalized at startup.
    let now = now_millis();
    Project {
        id: "default".to_owned(),
        canonical: directory.to_string_lossy().into_owned(),
        time: ProjectTime {
            created: now,
            updated: now,
            active: now,
        },
        sandboxes: Vec::new(),
    }
}

#[handler] // operation_id: project.list
async fn project_list(depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };

    let projects = if state.db.table_exists("project").await.unwrap_or(false) {
        // Shared upstream DB: map its `project` table rows onto the protocol
        // shape (`worktree` column -> `canonical` field). Falls back to the
        // synthetic project if the query fails (e.g. schema drift).
        match query_projects(&state.db).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "project table exists but query failed; using synthetic project");
                vec![synthetic_project(&state.directory)]
            }
        }
    } else {
        vec![synthetic_project(&state.directory)]
    };

    res.render(Json(projects));
}

async fn query_projects(db: &Db) -> anyhow::Result<Vec<Project>> {
    // Upstream's drizzle schema stores the canonical directory in the
    // `worktree` column and exposes it as `canonical` in the protocol
    // (`core/src/project.ts`: `canonical: row.worktree`). `sandboxes` is a
    // JSON-encoded TEXT array of strings.
    let rows = db
        .conn()
        .call(|c| -> rusqlite::Result<Vec<Project>> {
            let mut stmt = c.prepare(
                "SELECT id, worktree, time_created, time_updated, time_active, sandboxes FROM project",
            )?;
            let rows = stmt.query_map([], |row| {
                let sandboxes_raw: String = row.get(5)?;
                let sandboxes: Vec<String> =
                    serde_json::from_str(&sandboxes_raw).unwrap_or_default();
                Ok(Project {
                    id: row.get(0)?,
                    canonical: row.get(1)?,
                    time: ProjectTime {
                        created: row.get(2)?,
                        updated: row.get(3)?,
                        active: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    },
                    sandboxes,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// model.list — GET /api/model
// ---------------------------------------------------------------------------

/// Upstream `Model.Info` envelope: `{ location: { directory }, data: [...] }`.
/// NOTE (Task 5): this will be replaced by a live model catalogue from the
/// cardinal gateway; for now it serves the statically configured `models`.
#[derive(Serialize)]
struct ModelList {
    location: LocationRef,
    data: Vec<ModelInfo>,
}

#[derive(Serialize)]
struct LocationRef {
    directory: String,
}

/// Minimal upstream `Model.Info` shape (required fields only).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    id: String,
    #[serde(rename = "modelID")]
    model_id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    name: String,
    capabilities: ModelCapabilities,
    variants: Vec<ModelVariant>,
    time: ModelTime,
    cost: Vec<serde_json::Value>,
    status: &'static str,
    enabled: bool,
    limit: ModelLimit,
}

#[derive(Serialize)]
struct ModelCapabilities {
    tools: bool,
    input: Vec<&'static str>,
    output: Vec<&'static str>,
}

#[derive(Serialize)]
struct ModelVariant {
    id: String,
}

#[derive(Serialize)]
struct ModelTime {
    released: i64,
}

#[derive(Serialize)]
struct ModelLimit {
    context: i64,
    output: i64,
}

#[handler] // operation_id: model.list
async fn model_list(depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let data = state
        .config
        .models
        .iter()
        .map(|m| {
            let qualified = format!("{}/{}", m.provider, m.id);
            ModelInfo {
                id: qualified.clone(),
                model_id: m.id.clone(),
                provider_id: m.provider.clone(),
                name: m.name.clone().unwrap_or_else(|| m.id.clone()),
                capabilities: ModelCapabilities {
                    tools: true,
                    input: vec!["text"],
                    output: vec!["text"],
                },
                variants: Vec::new(),
                time: ModelTime { released: 0 },
                cost: Vec::new(),
                status: "active",
                enabled: true,
                limit: ModelLimit {
                    context: 0,
                    output: 0,
                },
            }
        })
        .collect();
    res.render(Json(ModelList {
        location: LocationRef {
            directory: state.directory.to_string_lossy().into_owned(),
        },
        data,
    }));
}

// ---------------------------------------------------------------------------
// agent.list — GET /api/agent
// ---------------------------------------------------------------------------

/// Upstream `Agent.Info` envelope: `{ location: { directory }, data: [...] }`.
/// Static single `build` agent for the skeleton; agent definitions land in a
/// later task.
#[derive(Serialize)]
struct AgentList {
    location: LocationRef,
    data: Vec<AgentInfo>,
}

#[derive(Serialize)]
struct AgentInfo {
    id: String,
    name: String,
    request: AgentRequest,
    mode: &'static str,
    hidden: bool,
    permissions: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct AgentRequest {
    settings: serde_json::Value,
    headers: serde_json::Value,
    body: serde_json::Value,
}

#[handler] // operation_id: agent.list
async fn agent_list(depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let build = AgentInfo {
        id: "build".to_owned(),
        name: "build".to_owned(),
        request: AgentRequest {
            settings: serde_json::json!({}),
            headers: serde_json::json!({}),
            body: serde_json::json!({}),
        },
        mode: "primary",
        hidden: false,
        permissions: Vec::new(),
    };
    res.render(Json(AgentList {
        location: LocationRef {
            directory: state.directory.to_string_lossy().into_owned(),
        },
        data: vec![build],
    }));
}

/// Build the salvo router with all `/api` endpoints mounted.
pub fn build_router(state: AppState) -> Router {
    Router::new().hoop(affix_state::inject(state)).push(
        Router::with_path("api")
            .push(Router::with_path("info").get(server_info))
            .push(Router::with_path("location").get(location_get))
            .push(Router::with_path("project").get(project_list))
            .push(Router::with_path("model").get(model_list))
            .push(Router::with_path("agent").get(agent_list))
            .push(
                Router::with_path("session")
                    .get(session::list)
                    .post(session::create)
                    .push(Router::with_path("active").get(session::active))
                    .push(
                        Router::with_path("{sessionID}")
                            .get(session::get)
                            .patch(session::update)
                            .delete(session::remove)
                            .push(Router::with_path("fork").post(session::fork))
                            .push(Router::with_path("view").post(session::view))
                            .push(Router::with_path("agent").post(session::switch_agent))
                            .push(Router::with_path("model").post(session::switch_model))
                            .push(
                                Router::with_path("message")
                                    .get(session::message_list)
                                    .push(
                                        Router::with_path("{messageID}").get(session::message_get),
                                    ),
                            )
                            .push(Router::with_path("prompt").post(session::prompt))
                            .push(
                                Router::with_path("inbox").get(session::inbox_list).push(
                                    Router::with_path("{inboxID}")
                                        .patch(session::inbox_update)
                                        .delete(session::inbox_cancel),
                                ),
                            )
                            .push(Router::with_path("interrupt").post(session::interrupt)),
                    ),
            ),
    )
}
