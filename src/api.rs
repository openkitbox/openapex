//! HTTP API handlers.
//!
//! Field names follow the opencode v2 protocol
//! (`packages/protocol/openapi.json`): `server.info` -> `ServerInfo`,
//! `location.get` -> `Location.PublicInfo`, `project.list` -> `Project[]`,
//! `agent.list` / `model.list` -> `{ location, data }` envelopes.

use std::path::PathBuf;
use std::sync::Arc;

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Serialize;

use crate::config::Config;
use crate::db::Db;
use crate::event_bus::EventBus;
use crate::id::now_millis;
use crate::runner::Runner;

pub mod compat;
pub mod event;
pub mod session;
pub mod shell;

/// Shared application state, injected via `salvo::affix_state`.
#[derive(Clone)]
pub struct AppState {
    /// The loaded config, shared by reference; nothing mutates it after startup.
    pub config: Arc<Config>,
    pub db: Db,
    /// In-process event bus; every session mutation is published through it.
    pub bus: EventBus,
    /// Working directory, canonicalized once at startup.
    pub directory: PathBuf,
    /// Per-session execution actor registry (task 6).
    pub runner: Runner,
    /// The model catalogue the client lists (`GET /v1/models` on the gateway).
    pub model_catalog: Arc<crate::model_catalog::ModelCatalog>,
}

/// Fetch the injected app state; on failure logs the error and renders a
/// 500, returning `None` so the handler can bail out without panicking
/// (project rule: no expect/unwrap in request paths).
pub(crate) fn state<'a>(depot: &'a Depot, res: &mut Response) -> Option<&'a AppState> {
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
async fn location_get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    // Upstream `location.get` echoes the REQUESTED directory (deepObject
    // `location[directory]` query, or the `x-opencode-directory` header),
    // resolving its project; the client anchors the session's project on this
    // response.
    let directory = compat::directory_of(req);
    let (project_id, project_directory) =
        crate::api::session::resolve_project(&state.db, &directory).await;
    res.render(Json(LocationInfo {
        project: LocationProject {
            id: project_id,
            directory: project_directory,
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
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, worktree, time_created, time_updated, time_active, sandboxes FROM project",
        )
        .await?;
    let mut rows = stmt.query(()).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let sandboxes_raw: String = row.get(5)?;
        let sandboxes: Vec<String> = serde_json::from_str(&sandboxes_raw).unwrap_or_default();
        out.push(Project {
            id: row.get(0)?,
            canonical: row.get(1)?,
            time: ProjectTime {
                created: row.get(2)?,
                updated: row.get(3)?,
                active: row.get::<Option<i64>>(4)?.unwrap_or(0),
            },
            sandboxes,
        });
    }
    Ok(out)
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
async fn model_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let entries = state.model_catalog.entries().await;
    let data = entries.iter().map(model_info).collect();
    res.render(Json(ModelList {
        location: LocationRef {
            directory: compat::directory_of(req),
        },
        data,
    }));
}

/// The `Model.Info` projection of a configured model (required upstream
/// fields; limits default to zero — cardinal owns the real routing). `id`
/// and `modelID` are both the bare model id, matching upstream's
/// `Model.Info.default` — clients use `id` as the model identifier when
/// selecting, so a qualified value would leak into requests.
fn model_info(m: &crate::config::ModelEntry) -> ModelInfo {
    ModelInfo {
        id: m.id.clone(),
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
            context: m.context.unwrap_or(0),
            output: m.output.unwrap_or(0),
        },
    }
}

/// `model.default` — GET /api/model/default. The model backing sessions
/// without an explicit selection (config `default_model`, else the first
/// `[[models]]` entry); `data: null` when nothing is configured.
#[derive(Serialize)]
struct ModelDefault {
    location: LocationRef,
    data: Option<ModelInfo>,
}

#[handler] // operation_id: model.default
async fn model_default(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let data = state
        .model_catalog
        .default_entry(&state.config)
        .map(|entry| model_info(&entry));
    res.render(Json(ModelDefault {
        location: LocationRef {
            directory: compat::directory_of(req),
        },
        data,
    }));
}

// ---------------------------------------------------------------------------
// provider.list — GET /api/provider
// ---------------------------------------------------------------------------

/// Upstream `Provider.Info` (required: id, name, activation, package).
#[derive(Serialize)]
struct ProviderInfo {
    id: String,
    name: String,
    activation: &'static str,
    package: &'static str,
}

/// Upstream `Provider.Info[]` envelope: `{ location: { directory }, data }`.
/// Derived from `[[models]]`: one provider per distinct `provider` id, in
/// first-appearance order. ox never loads npm provider packages; the
/// package value mirrors what an OpenAI-compatible provider uses
/// (`provider.npm` in upstream config).
#[derive(Serialize)]
struct ProviderList {
    location: LocationRef,
    data: Vec<ProviderInfo>,
}

#[handler] // operation_id: provider.list
async fn provider_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let entries = state.model_catalog.entries().await;
    let mut data: Vec<ProviderInfo> = Vec::new();
    for m in entries.iter() {
        if data.iter().any(|p| p.id == m.provider) {
            continue;
        }
        // Display name: capitalize the provider id ("cardinal" → "Cardinal").
        let mut name = m.provider.clone();
        if let Some(first) = name.chars().next() {
            name = first.to_uppercase().collect::<String>() + &name[first.len_utf8()..];
        }
        data.push(ProviderInfo {
            id: m.provider.clone(),
            name,
            activation: "auto",
            package: "@ai-sdk/openai-compatible",
        });
    }
    res.render(Json(ProviderList {
        location: LocationRef {
            directory: compat::directory_of(req),
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
async fn agent_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(_state) = state(depot, res) else {
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
            directory: compat::directory_of(req),
        },
        data: vec![build],
    }));
}

/// Build the salvo router with all `/api` endpoints mounted.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .hoop(affix_state::inject(state))
        .push(
            Router::with_path("api")
                .push(Router::with_path("info").get(server_info))
                .push(Router::with_path("location").get(location_get))
                .push(Router::with_path("project").get(project_list))
                .push(Router::with_path("model").get(model_list))
                .push(Router::with_path("model/default").get(model_default))
                .push(Router::with_path("agent").get(agent_list))
                .push(Router::with_path("event").get(event::subscribe))
                .push(compat::mount(Router::new()))
                .push(
                    Router::with_path("shell")
                        .get(shell::list)
                        .push(
                            Router::with_path("{id}")
                                .get(shell::get)
                                .delete(shell::remove),
                        )
                        .push(Router::with_path("{id}/output").get(shell::output)),
                )
                .push(
                    Router::with_path("experimental/session/{sessionID}/wait")
                        .post(session::wait_idle),
                )
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
                                            Router::with_path("{messageID}")
                                                .get(session::message_get),
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
                                .push(Router::with_path("interrupt").post(session::interrupt))
                                .push(Router::with_path("permission").get(session::permission_list))
                                .push(Router::with_path("form").get(session::form_list)),
                        ),
                ),
        )
        .push(Router::with_path("{**rest}").goal(unmatched))
}

/// Fallback for unmatched routes: upstream's Effect HttpApi always answers
/// JSON, never salvo's default HTML 404 page. Log the miss (compat triage)
/// and answer a structured error.
#[handler]
async fn unmatched(req: &mut Request, res: &mut Response) {
    tracing::warn!(method = %req.method(), path = %req.uri().path(), "unmatched route");
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(serde_json::json!({
        "_tag": "NotFoundError",
        "message": format!("No route for {} {}", req.method(), req.uri().path()),
    })));
}
