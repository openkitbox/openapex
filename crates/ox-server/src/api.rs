//! HTTP API handlers.
//!
//! Field names follow the opencode v2 protocol
//! (`packages/protocol/openapi.json`): `server.info` -> `ServerInfo`,
//! `location.get` -> `Location.PublicInfo`, `project.list` -> `Project[]`,
//! `agent.list` / `model.list` -> `{ location, data }` envelopes.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};

use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_engine::mctx::embed::Embedder;
use ox_engine::runner::{Live, Runner, Services};
use ox_llm::model_catalog::ModelCatalog;
use ox_schema::id::now_millis;

mod attachment;
pub mod compat;
pub mod event;
pub mod family;
pub mod files;
pub mod form;
mod integration;
pub mod lease;
mod mcp;
mod media;
pub mod session;
pub mod shell;
pub mod skill;
pub mod stats;
pub mod vcs;

/// Shared application state, injected via `salvo::affix_state`.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// In-process event bus; every session mutation is published through it.
    pub bus: EventBus,
    /// Working directory, canonicalized once at startup.
    pub directory: PathBuf,
    /// Per-session execution actor registry (task 6).
    pub runner: Runner,
    /// Config-derived services (config, model catalogue, MCP hub, toolset),
    /// shared with `Runner`; read through [`AppState::services`].
    pub live: Arc<Live>,
    /// The `ox.toml` a reload re-reads.
    pub config_path: PathBuf,
    /// Client leases; the server may exit only while none is held.
    pub lifecycle: Arc<crate::lifecycle::Lifecycle>,
    /// Embedding client shared by the memory tools and the coverage sweep; read
    /// only by the server's composer.
    pub(crate) embedder: Option<Arc<Embedder>>,
    /// Composition-scoped shutdown owner. HTTP handlers use it to reject new
    /// ingress, while the serve loop is its sole close/wait supervisor.
    pub runtime: Arc<crate::runtime::Runtime>,
    /// Optional password for managed service mode. `None` keeps the API anonymous.
    pub service_password: Option<String>,
}

impl AppState {
    /// The current config-derived snapshot. Take it once per request so a
    /// handler never mixes two generations.
    #[must_use]
    pub fn services(&self) -> Arc<Services> {
        self.live.current()
    }

    /// The shell domain shared by the registry and HTTP handlers.
    #[must_use]
    pub fn shells(&self) -> Arc<ox_tools::shell::sessions::ShellSessions> {
        Arc::clone(&self.services().tools.shells)
    }

    /// Whether this composition has closed HTTP ingress.
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.runtime.is_closing()
    }
}

/// A request is admitted only while the composition is open. This is a thin
/// ingress check: execution admission remains the runner's responsibility.
#[handler]
async fn reject_when_closing(depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
    let Some(state) = state(depot, res) else {
        ctrl.skip_rest();
        return;
    };
    if state.is_closing() {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        ctrl.skip_rest();
    }
}

/// Require the managed service credential before any API handler runs.
#[handler]
async fn authenticate_service(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let Some(state) = state(depot, res) else {
        ctrl.skip_rest();
        return;
    };
    let password = state.service_password.as_deref();
    if !service_authentication_required(req.local_addr(), password) {
        return;
    }
    let Some(password) = password else {
        return;
    };
    if service_authorization_is_valid(req.headers().get("authorization"), password) {
        return;
    }
    res.status_code(StatusCode::UNAUTHORIZED);
    let _ = res.add_header("WWW-Authenticate", "Basic realm=\"Secure Area\"", true);
    res.render(Json(serde_json::json!({
        "_tag": "UnauthorizedError",
        "message": "Authentication required",
    })));
    ctrl.skip_rest();
}

/// Return whether managed service authentication applies to this transport.
/// Unknown addresses fail closed; only an explicitly identified Unix socket
/// bypasses Basic authentication.
fn service_authentication_required(
    address: &salvo::conn::SocketAddr,
    password: Option<&str>,
) -> bool {
    password.is_some() && !address.is_unix()
}

/// Return whether an Authorization header exactly matches the service credential.
fn service_authorization_is_valid(
    authorization: Option<&salvo::http::HeaderValue>,
    password: &str,
) -> bool {
    authorization
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let mut fields = value.split_ascii_whitespace();
            let scheme = fields.next()?;
            let encoded = fields.next()?;
            (fields.next().is_none() && scheme.eq_ignore_ascii_case("basic")).then_some(encoded)
        })
        .and_then(|encoded| BASE64_STANDARD.decode(encoded).ok())
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .is_some_and(|credentials| credentials == format!("opencode:{password}"))
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

/// Keep the HTTP health version aligned with managed service registration.
fn server_info_version() -> &'static str {
    crate::service::UPSTREAM_COMPAT_VERSION
}

#[handler] // operation_id: server.info
async fn server_info(depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let svc = state.services();
    res.render(Json(ServerInfo {
        version: server_info_version(),
        pid: std::process::id(),
        urls: vec![format!("http://{}", svc.config.listen)],
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

// Keep the existing compat resolution contract local to these filesystem routes:
// request parsing is nonblocking, but canonicalization and cwd lookup are not.
async fn directory_base(req: &Request) -> Result<PathBuf, tokio::task::JoinError> {
    let requested = compat::request_directory(req);
    tokio::task::spawn_blocking(move || {
        let path = requested.map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            |raw| {
                let path = PathBuf::from(raw);
                path.canonicalize().unwrap_or(path)
            },
        );
        // compat::directory_of renders lossily before callers reparse the path.
        PathBuf::from(path.to_string_lossy().as_ref())
    })
    .await
}

/// Validate a Home directory without creating a session or changing location.get.
#[handler]
async fn location_validate(req: &mut Request, res: &mut Response) {
    let Some(raw) = req.query::<String>("directory") else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };
    let Ok(base) = directory_base(req).await else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };
    let expanded = ox_core::home_paths::expand_tilde(&raw, &ox_core::config::user_home())
        .unwrap_or_else(|| PathBuf::from(&raw));
    let destination = base.join(expanded);
    if let Ok(path) = tokio::fs::canonicalize(destination).await
        && !raw.trim().is_empty()
        && tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir())
    {
        res.render(Json(
            serde_json::json!({"directory": path.display().to_string()}),
        ));
    } else {
        res.status_code(StatusCode::BAD_REQUEST);
    }
}

/// Immediate child directories for /cd, including paths outside the project.
#[handler]
async fn directories(req: &mut Request, res: &mut Response) {
    let query = req.query::<String>("query").unwrap_or_default();
    let query = if query == "~" || !query.is_empty() && query.split('/').all(|part| part == "..") {
        format!("{query}/")
    } else {
        query
    };
    let (parent, prefix) = query
        .rsplit_once('/')
        .map_or(("", query.as_str()), |(p, s)| (&query[..p.len() + 1], s));
    let expanded = ox_core::home_paths::expand_tilde(parent, &ox_core::config::user_home())
        .unwrap_or_else(|| PathBuf::from(parent));
    let mut paths = Vec::new();
    let Ok(base) = directory_base(req).await else {
        res.render(Json(paths));
        return;
    };
    let root = base.join(expanded);
    let prefix = prefix.to_lowercase();
    if let Ok(mut entries) = tokio::fs::read_dir(root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_lowercase().starts_with(&prefix)
                && (!prefix.is_empty() || !name.starts_with('.'))
                && entry.metadata().await.is_ok_and(|m| m.is_dir())
            {
                paths.push(format!("{parent}{name}/"));
            }
        }
    }
    paths.sort();
    paths.truncate(200);
    res.render(Json(paths));
}

/// `POST /api/location/reload` — re-read `ox.toml` and hot-swap the services
/// derived from it. Any failure answers 503 `ServiceUnavailableError` and the
/// running services stay in place.
#[handler] // operation_id: location.reload
async fn location_reload(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let directory = compat::directory_of(req);
    match crate::reload::run(state, &directory).await {
        Ok(()) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "location reload failed");
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Json(serde_json::json!({
                "_tag": "ServiceUnavailableError",
                "message": format!("{error:#}"),
                "service": "location",
            })));
        }
    }
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
// worktree.list / worktree.refresh — the TUI's Worktrees panel
// ---------------------------------------------------------------------------

/// `GET /api/worktree?projectID=` — a bare `Worktree.Directory[]` (upstream
/// `Worktree.List`: no `location`/`data` envelope), read from git rather than
/// from an inventory table (see `crate::worktree`).
#[handler] // operation_id: worktree.list
async fn worktree_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let directory = worktree_directory(
        state,
        req,
        req.query::<String>("projectID")
            .as_deref()
            .unwrap_or_default(),
    )
    .await;
    res.render(Json(crate::worktree::list(&directory).await));
}

/// `POST /api/worktree/refresh` — the panel calls this on open to reconcile a
/// server-side inventory with git. ox keeps none, so it only acknowledges; the
/// following list already answers from fresh `git worktree list` state.
#[handler] // operation_id: worktree.refresh
async fn worktree_refresh(res: &mut Response) {
    res.status_code(StatusCode::NO_CONTENT);
}

/// Upstream `Worktree.CreateInput` as the TUI sends it: `projectID` plus an
/// optional name (`dialog-open.tsx:462`), with `from`/`branch`/`directory`
/// available to other clients.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeCreateBody {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Upstream `Worktree.RemoveInput`; `force` is required on the wire there, ox
/// treats a missing value as `false` so a plain delete still works.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorktreeRemoveBody {
    #[serde(default)]
    project_id: Option<String>,
    directory: String,
    #[serde(default)]
    force: bool,
}

/// `POST /api/worktree` — create a linked checkout and tell the panel to
/// refresh. Answers `Worktree.Info` (`{ directory }`), the exact field the TUI
/// navigates to afterwards.
#[handler] // operation_id: worktree.create
async fn worktree_create(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let Ok(body) = req.parse_json::<WorktreeCreateBody>().await else {
        return session::invalid_request(res, "Invalid worktree create payload");
    };
    let project_id = body.project_id.clone().unwrap_or_default();
    let directory = worktree_directory(state, req, &project_id).await;
    let input = crate::worktree::CreateInput {
        from: body.from,
        branch: body.branch,
        directory: body.directory,
        name: body.name,
    };
    match crate::worktree::create(&directory, input).await {
        Ok(created) => {
            state.bus.broadcast_ephemeral(
                "worktree.updated",
                serde_json::json!({ "projectID": project_id }),
                Some(&directory),
            );
            res.render(Json(serde_json::json!({ "directory": created })));
        }
        Err(failure) => worktree_error(res, failure),
    }
}

/// `DELETE /api/worktree` — remove a linked checkout. A dirty checkout without
/// `force` answers a `WorktreeError` carrying `forceRequired`, which is what the
/// panel turns into its force-confirmation prompt.
#[handler] // operation_id: worktree.remove
async fn worktree_remove(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let Ok(body) = req.parse_json::<WorktreeRemoveBody>().await else {
        return session::invalid_request(res, "Invalid worktree remove payload");
    };
    match crate::worktree::remove(&body.directory, body.force).await {
        Ok(()) => {
            state.bus.broadcast_ephemeral(
                "worktree.updated",
                serde_json::json!({ "projectID": body.project_id.unwrap_or_default() }),
                None,
            );
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(failure) => worktree_error(res, failure),
    }
}

fn worktree_error(res: &mut Response, failure: crate::worktree::Failure) {
    res.status_code(StatusCode::BAD_REQUEST);
    let mut data = serde_json::Map::new();
    data.insert("message".into(), failure.message.into());
    if failure.force_required {
        data.insert("forceRequired".into(), true.into());
    }
    res.render(Json(serde_json::json!({
        "name": "WorktreeError",
        "data": serde_json::Value::Object(data),
    })));
}

/// The directory whose checkouts the requested project covers: the `project`
/// row's canonical directory when the DB is shared with an upstream server,
/// otherwise the directory the client asked about (`x-opencode-directory`).
async fn worktree_directory(state: &AppState, req: &mut Request, requested: &str) -> String {
    if !requested.is_empty() && state.db.table_exists("project").await.unwrap_or(false) {
        match query_projects(&state.db).await {
            Ok(rows) => {
                if let Some(project) = rows.into_iter().find(|project| project.id == requested) {
                    return project.canonical;
                }
            }
            Err(error) => tracing::warn!(error = %error, "worktree list: project query failed"),
        }
    }
    compat::directory_of(req)
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
    #[serde(rename = "type")]
    model_type: String,
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
    temperature: bool,
    input: Vec<String>,
    output: Vec<String>,
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
    input: i64,
    output: i64,
}

#[handler] // operation_id: model.list
async fn model_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = state(depot, res) else {
        return;
    };
    let svc = state.services();
    let entries = svc.model_catalog.entries().await;
    let types = model_type_filter(req);
    let data = entries
        .iter()
        .filter(|entry| model_type_matches(entry, types.as_deref()))
        .map(|entry| model_info(entry, &svc.model_catalog))
        .collect();
    res.render(Json(ModelList {
        location: LocationRef {
            directory: compat::directory_of(req),
        },
        data,
    }));
}

fn model_type_filter(req: &Request) -> Option<Vec<String>> {
    let mut types = req.query::<Vec<String>>("types").unwrap_or_default();
    types.extend(req.query::<Vec<String>>("type").unwrap_or_default());
    (!types.is_empty()).then_some(types)
}

fn model_type_matches(entry: &ox_llm::model_catalog::ModelEntry, types: Option<&[String]>) -> bool {
    let model_type = entry.model_type.as_deref().unwrap_or("chat");
    let Some(types) = types else {
        return matches!(model_type, "chat" | "text");
    };
    types
        .iter()
        .flat_map(|value| value.split(','))
        .any(|requested| !requested.is_empty() && requested.trim() == model_type)
}

/// The `Model.Info` projection of a configured model (required upstream
/// fields; limits default to zero — cardinal owns the real routing). `id`
/// and `modelID` are both the bare model id, matching upstream's
/// `Model.Info.default` — clients use `id` as the model identifier when
/// selecting, so a qualified value would leak into requests. The reasoning
/// levels cardinal advertises become `{id}` variants in catalog order
/// (upstream `Model.Variant` is `{id}` on the wire); a model without
/// advertised levels gets none.
///
/// `limit.context` is the window the *runner* budgets against
/// (`ModelCatalog::window_of`: the report lowered by `[context_caps]`), not the
/// raw report: a client divides its context percentage by this value, so
/// advertising the uncapped window makes the footer disagree with ox's own
/// pressure. `window_of` answers `None` exactly when the entry carries no
/// usable context, which stays zero on the wire.
fn model_info(m: &ox_llm::model_catalog::ModelEntry, catalog: &ModelCatalog) -> ModelInfo {
    ModelInfo {
        id: m.id.clone(),
        model_id: m.id.clone(),
        provider_id: m.provider.clone(),
        name: m.name.clone().unwrap_or_else(|| m.id.clone()),
        model_type: m.model_type.clone().unwrap_or_else(|| "chat".to_owned()),
        capabilities: ModelCapabilities {
            tools: true,
            temperature: m.temperature.unwrap_or(false),
            input: modalities(&m.input_modalities),
            output: modalities(&m.output_modalities),
        },
        variants: if m.reasoning {
            m.supported_reasoning_levels
                .iter()
                .map(|level| ModelVariant {
                    id: level.effort.clone(),
                })
                .collect()
        } else {
            Vec::new()
        },
        time: ModelTime { released: 0 },
        cost: Vec::new(),
        status: "active",
        enabled: true,
        limit: ModelLimit {
            context: catalog
                .window_of_for(&m.provider, &m.id)
                .map_or_else(|| m.context.unwrap_or(0), i64::from),
            input: m.input.unwrap_or(0),
            output: m.output.unwrap_or(0),
        },
    }
}

/// The modalities to advertise for a model.
///
/// The endpoint's own list wins: DeepSeek reports `["text","image"]` for the
/// models that accept images. A listing that states nothing is a listing that
/// makes no claim, so it falls back to `text` — every chat model reads and
/// writes text, and advertising an image input the endpoint never offered
/// would make the client attach one and the request fail.
fn modalities(reported: &[String]) -> Vec<String> {
    if reported.is_empty() {
        vec!["text".to_owned()]
    } else {
        reported.to_vec()
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
    let svc = state.services();
    let data = svc
        .model_catalog
        .default_entry(&svc.config.llm)
        .map(|entry| model_info(&entry, &svc.model_catalog));
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
    let entries = state.services().model_catalog.entries().await;
    let mut data: Vec<ProviderInfo> = Vec::new();
    for m in entries.iter() {
        if data.iter().any(|p| p.id == m.provider) {
            continue;
        }
        // Display name: the builtin's own label when there is one
        // ("deepseek" → "DeepSeek"), else the capitalized id
        // ("cardinal" → "Cardinal").
        let name = match ox_llm::config::builtin(&m.provider) {
            Some(builtin) => builtin.label.to_owned(),
            None => {
                let mut name = m.provider.clone();
                if let Some(first) = name.chars().next() {
                    name = first.to_uppercase().collect::<String>() + &name[first.len_utf8()..];
                }
                name
            }
        };
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
/// One entry per built-in agent ([`ox_tools::agent::all`]), in display order;
/// clients filter subagents and hidden entries out of their pickers.
#[derive(Serialize)]
struct AgentList {
    location: LocationRef,
    data: Vec<AgentInfo>,
}

#[derive(Serialize)]
struct AgentInfo {
    id: &'static str,
    name: &'static str,
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
    let data: Vec<AgentInfo> = ox_tools::agent::all().iter().map(agent_info).collect();
    res.render(Json(AgentList {
        location: LocationRef {
            directory: compat::directory_of(req),
        },
        data,
    }));
}

/// The wire shape of one agent record. `name` is the id: clients capitalize
/// it for display. ox has no permission rule sets yet, so `permissions` is
/// always empty.
fn agent_info(agent: &'static ox_tools::agent::Agent) -> AgentInfo {
    AgentInfo {
        id: agent.id,
        name: agent.id,
        request: AgentRequest {
            settings: serde_json::json!({}),
            headers: serde_json::json!({}),
            body: serde_json::json!({}),
        },
        mode: match agent.mode {
            ox_tools::agent::Mode::Primary => "primary",
            ox_tools::agent::Mode::Subagent => "subagent",
        },
        hidden: false,
        permissions: Vec::new(),
    }
}

/// Build the salvo router with all `/api` endpoints mounted.
#[must_use]
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .hoop(affix_state::inject(state))
        .hoop(reject_when_closing)
        .hoop(authenticate_service)
        // ox-only endpoints live beside `/api`, never under it, so upstream
        // opencode paths stay distinguishable.
        .push(
            Router::with_path("ox")
                .push(Router::with_path("location").post(location_validate))
                .push(Router::with_path("directories").get(directories))
                .push(Router::with_path("lease").get(lease::lease))
                .push(Router::with_path("session/{id}/family").get(family::family))
                .push(Router::with_path("session/{sessionID}/media/{messageID}").get(media::media)),
        )
        .push(
            Router::with_path("api")
                .hoop(reject_when_closing)
                .push(Router::with_path("info").get(server_info))
                .push(
                    Router::with_path("location")
                        .get(location_get)
                        .push(Router::with_path("reload").post(location_reload)),
                )
                .push(Router::with_path("project").get(project_list))
                .push(
                    Router::with_path("worktree")
                        .get(worktree_list)
                        .post(worktree_create)
                        .delete(worktree_remove)
                        .push(Router::with_path("refresh").post(worktree_refresh)),
                )
                .push(Router::with_path("model").get(model_list))
                .push(Router::with_path("model/default").get(model_default))
                .push(Router::with_path("agent").get(agent_list))
                .push(Router::with_path("event").get(event::subscribe))
                .push(Router::with_path("fs/read/{**path}").get(files::read))
                .push(
                    Router::with_path("vcs")
                        .get(vcs::info)
                        .push(Router::with_path("base").get(vcs::base))
                        .push(Router::with_path("status").get(vcs::status))
                        .push(Router::with_path("branch").get(vcs::branches))
                        .push(Router::with_path("diff").get(vcs::diff))
                        .push(Router::with_path("init").post(vcs::init)),
                )
                .push(compat::mount(Router::new()))
                .push(Router::with_path("integration").get(integration::list))
                .push(
                    Router::with_path("integration/{integrationID}/connect/key")
                        .post(integration::connect_key),
                )
                .push(Router::with_path("form").get(form::list))
                .push(Router::with_path("skill").get(skill::list))
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
                .push(Router::with_path("experimental/session/stats").get(stats::get))
                .push(Router::with_path("experimental/session/import").post(session::import))
                .push(
                    Router::with_path("experimental/session/{sessionID}/export")
                        .get(session::export),
                )
                .push(Router::with_path("experimental/session/{sessionID}/log").get(event::log))
                .push(Router::with_path("session/{sessionID}/synthetic").post(session::synthetic))
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
                                    Router::with_path("environment").put(session::set_environment),
                                )
                                .push(
                                    Router::with_path("memory")
                                        .get(session::memory_get)
                                        .post(session::memory_set)
                                        .push(
                                            Router::with_path("clear").post(session::memory_clear),
                                        ),
                                )
                                .push(
                                    Router::with_path("message")
                                        .get(session::message_list)
                                        .push(
                                            Router::with_path("{messageID}")
                                                .get(session::message_get),
                                        ),
                                )
                                .push(Router::with_path("prompt").post(session::prompt))
                                .push(Router::with_path("compact").post(session::compact))
                                .push(Router::with_path("shell").post(session::shell))
                                .push(Router::with_path("command").post(session::command))
                                .push(Router::with_path("generate").post(session::generate))
                                .push(Router::with_path("move").post(session::move_session))
                                .push(
                                    Router::with_path("inbox").get(session::inbox_list).push(
                                        Router::with_path("{inboxID}")
                                            .patch(session::inbox_update)
                                            .delete(session::inbox_cancel),
                                    ),
                                )
                                .push(Router::with_path("interrupt").post(session::interrupt))
                                .push(Router::with_path("background").post(session::background))
                                .push(Router::with_path("permission").get(session::permission_list))
                                .push(
                                    Router::with_path("form")
                                        .get(form::session_list)
                                        .post(form::create)
                                        .push(
                                            Router::with_path("{formID}")
                                                .get(form::get)
                                                .delete(form::cancel)
                                                .push(Router::with_path("reply").post(form::reply)),
                                        ),
                                ),
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

#[cfg(test)]
mod tests {
    use salvo::http::HeaderValue;

    use super::*;

    #[test]
    fn server_info_version_matches_service_registration_version() {
        assert_eq!(
            server_info_version(),
            crate::service::UPSTREAM_COMPAT_VERSION
        );
    }

    #[test]
    fn service_authentication_requires_password_only_for_managed_tcp() {
        let tcp: salvo::conn::SocketAddr = "127.0.0.1:14090"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into();
        let unknown = salvo::conn::SocketAddr::Unknown;

        assert!(service_authentication_required(&tcp, Some("secret")));
        assert!(service_authentication_required(&unknown, Some("secret")));
        assert!(!service_authentication_required(&tcp, None));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn service_authentication_skips_explicit_unix_socket() {
        let path = std::env::temp_dir().join(format!(
            "ox-api-auth-{}-{}.sock",
            std::process::id(),
            now_millis()
        ));
        let listener = tokio::net::UnixListener::bind(&path).expect("bind test Unix socket");
        let unix: salvo::conn::SocketAddr = listener
            .local_addr()
            .expect("read test Unix socket address")
            .into();

        assert!(unix.is_unix());
        assert!(!service_authentication_required(&unix, Some("secret")));

        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn service_authorization_requires_exact_opencode_password() {
        let valid = HeaderValue::from_static("Basic b3BlbmNvZGU6c2VjcmV0");
        let lowercase_scheme = HeaderValue::from_static("basic b3BlbmNvZGU6c2VjcmV0");
        let multiple_spaces = HeaderValue::from_static("Basic   b3BlbmNvZGU6c2VjcmV0");
        let wrong_user = HeaderValue::from_static("Basic YWRtaW46c2VjcmV0");
        let wrong_password = HeaderValue::from_static("Basic b3BlbmNvZGU6d3Jvbmc=");

        assert!(service_authorization_is_valid(Some(&valid), "secret"));
        assert!(service_authorization_is_valid(
            Some(&lowercase_scheme),
            "secret"
        ));
        assert!(service_authorization_is_valid(
            Some(&multiple_spaces),
            "secret"
        ));
        assert!(!service_authorization_is_valid(Some(&wrong_user), "secret"));
        assert!(!service_authorization_is_valid(
            Some(&wrong_password),
            "secret"
        ));
        assert!(!service_authorization_is_valid(None, "secret"));
    }
}
