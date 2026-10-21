//! ox library: config, storage, ID generation and HTTP API.
//!
//! Split out from `main.rs` so integration tests can build the router and
//! drive it over HTTP against an ephemeral port.

pub mod acp;
pub mod api;
pub mod cardinal;
pub mod cbm;
pub mod config;
pub mod db;
pub mod event_bus;
pub mod id;
pub mod mctx;
pub mod model_catalog;
pub mod projector;
pub mod prompt;
pub mod runner;
pub mod schema;
pub mod slug;
pub mod tool;
pub(crate) mod which;

use std::sync::Arc;

use anyhow::Result;
use salvo::prelude::*;

use crate::api::AppState;
use crate::cardinal::client::CardinalClient;
use crate::config::Config;
use crate::db::Db;
use crate::event_bus::EventBus;
use crate::runner::Runner;

/// How ox identifies itself to everything it calls: the LLM gateway, the model
/// catalogue, the embeddings endpoint. One value, because a request's origin
/// must be unambiguous at the far end, and `ox/<version>` is what tells two ox
/// builds apart in a gateway's logs.
pub const USER_AGENT: &str = concat!("ox/", env!("CARGO_PKG_VERSION"));

/// The reqwest builder every outbound client starts from.
///
/// Identity and nothing else: each client still sets its own timeouts, because
/// a streaming completion and a 30-second embed call do not share one. Going
/// through here is what keeps a new client from sending no `User-Agent` at all,
/// which is reqwest's default and therefore an easy mistake to inherit.
pub(crate) fn http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder().user_agent(USER_AGENT)
}

/// One client constructor shared by [`build_state`] and [`run`], so both map
/// init failures identically.
fn cardinal_chat(config: &Config) -> Result<Arc<dyn runner::Chat>> {
    let cardinal = CardinalClient::from_config(config)
        .map_err(|error| anyhow::anyhow!("cardinal client init failed: {error}"))?;
    Ok(Arc::new(cardinal))
}

/// Build application state: open the DB, run migrations, canonicalize the
/// working directory once (falls back to the raw path on error), and register
/// the eight built-in tools plus the magic-context tools when enabled (the
/// default).
///
/// CBM discovery is deliberately not part of this builder: the optional
/// `codebase-memory-mcp` binary is probed at the composition root ([`run`]),
/// so tests get a deterministic toolset regardless of the host's `PATH`.
pub async fn build_state(config: Config) -> Result<AppState> {
    let chat = cardinal_chat(&config)?;
    build_state_with(config, chat, runner::ToolRegistry::with_builtins()).await
}

/// [`build_state`] with an injected chat transport and tool registry —
/// the seam tests (and later, alternate providers) use. The registry is
/// passed unwrapped so the wiring can attach the wiring-dependent tools
/// (`register_mctx`, `register_subagent`) after the DB opens. A caller that
/// must not carry a tool removes it by name; `ToolRegistry::remove` exists
/// for exactly that.
///
/// Callers may pre-populate `tools` before handing it over (as [`run`] does
/// with the CBM and todo tools).
pub async fn build_state_with(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    tools: runner::ToolRegistry,
) -> Result<AppState> {
    let db = Db::open(&config.db_path).await?;
    build_state_on(config, chat, tools, db).await
}

/// The shared tail of [`build_state_with`]: state over an already-open
/// database, so [`run`] can register DB-holding tools before handing the
/// registry over without opening the database twice.
async fn build_state_on(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    tools: runner::ToolRegistry,
    db: Db,
) -> Result<AppState> {
    let bus = EventBus::new(db.clone());
    build_state_on_with_bus(config, chat, tools, db, bus).await
}

/// [`build_state_on`] over a caller-supplied event bus, so the composition
/// root can publish to the same bus it registers tools against.
async fn build_state_on_with_bus(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    mut tools: runner::ToolRegistry,
    db: Db,
    bus: EventBus,
) -> Result<AppState> {
    if config.magic_context.enabled {
        let embedder = mctx::embed::Embedder::from_config(&config.magic_context.embedding);
        tools.register_mctx(db.clone(), embedder);
    }
    let cwd = std::env::current_dir()?;
    let directory = cwd.canonicalize().unwrap_or(cwd);
    let model_catalog = model_catalog::ModelCatalog::new(&config);
    // The TUI renders its question dialog purely from the ephemeral
    // `form.created` / `form.replied` / `form.cancelled` frames, so the
    // process-wide form registry must broadcast on this same bus (the one
    // `GET /api/event` serves).
    crate::tool::form::init(bus.clone());
    // One read-only config behind an `Arc`: every actor and spawned task clones
    // the handle, never the struct.
    let config = std::sync::Arc::new(config);
    let tools = Arc::new(tools);
    let db_handle = db.clone();
    let bus_handle = bus.clone();
    let runner = Runner::new(
        std::sync::Arc::clone(&config),
        directory.clone(),
        db,
        bus,
        chat,
        tools,
        Arc::clone(&model_catalog),
    );
    Ok(AppState {
        config,
        db: db_handle,
        bus: bus_handle,
        directory,
        runner,
        model_catalog,
    })
}

/// Build the salvo router for the given state.
pub fn build_router(state: AppState) -> Router {
    api::build_router(state)
}

/// Open the DB, build the router and serve on `config.listen`.
pub async fn run(config: Config) -> Result<()> {
    let (state, router) = compose(config).await?;
    let acceptor = TcpListener::new(state.config.listen.clone())
        .try_bind()
        .await?;
    serve_on(acceptor, state, router).await
}

/// Compose the full server state and router without binding anything —
/// the shared seam between [`run`] (binds `config.listen`) and `ox acp`
/// own-mode (binds an ephemeral port and reports the real address).
pub async fn compose(config: Config) -> Result<(AppState, Router)> {
    let mut tools = runner::ToolRegistry::with_builtins();
    match cbm::Cbm::discover() {
        Some(cbm) => tools.register_cbm(cbm),
        None => tracing::info!(
            "codebase-memory-mcp not found on PATH; code-graph tools disabled: \
             index_repository, search_graph, trace_path, get_code_snippet, \
             get_architecture. Install it on the server's PATH to enable them"
        ),
    }
    // The session todo list needs the DB (it stores state in the `kv` table),
    // so it joins here at the composition root rather than in
    // `with_builtins` — the same seam the CBM tools use. Tests that inject
    // their own registry keep a deterministic toolset. `subagent` joins the
    // same way below: it needs the runner, which only exists after
    // `build_state_on`, so `run` registers it through the runner's handle
    // slot and the registry here.
    let db = Db::open(&config.db_path).await?;
    let bus = EventBus::new(db.clone());
    tools.register_todo(db.clone());
    tools.register_subagent(db.clone(), bus.clone());
    let chat = cardinal_chat(&config)?;
    let state = build_state_on_with_bus(config, chat, tools, db, bus).await?;
    // Fetch the model list before serving, so the first client that asks
    // (`/api/model` at TUI startup) gets models instead of an empty list. A
    // failure only warns: the request path retries on its own TTL.
    state.model_catalog.warm().await;
    // Background memory consolidation. Idle unless `[magic_context]` enables
    // it, and deliberately not part of `build_state_with`: tests must not
    // acquire a background loop that writes to their database.
    if state.config.magic_context.enabled && state.config.magic_context.dreamer_enabled {
        mctx::dreamer::Dreamer::new(
            state.db.clone(),
            state.config.clone(),
            state.runner.chat(),
            Arc::clone(&state.model_catalog),
        )
        .spawn();
    }
    // A fold that was in flight when the previous process died left its
    // `mc_historian_run` row `running`, and "one pending run per session" reads
    // that row as a claim: without settling it here, every session that was
    // folding at exit would never fold again.
    match mctx::historian::settle_running_runs(&state.db).await {
        Ok(0) => {}
        Ok(settled) => tracing::info!(settled, "settled interrupted historian runs"),
        Err(error) => {
            tracing::warn!(error = %error, "settle interrupted historian runs failed");
        }
    }
    let router = build_router(state.clone());
    Ok((state, router))
}

/// Serve the composed router on an already-bound listener — the `ox acp`
/// own-mode seam (the ephemeral acceptor is bound by the caller so the real
/// port is known before the loop starts).
pub async fn serve_on(
    acceptor: impl salvo::conn::Acceptor,
    state: AppState,
    router: Router,
) -> Result<()> {
    let _ = state; // signature symmetry: state owns the runtime handles, the router holds them
    Server::new(acceptor).serve(router).await;
    Ok(())
}
