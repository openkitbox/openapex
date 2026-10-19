//! ox library: config, storage, ID generation and HTTP API.
//!
//! Split out from `main.rs` so integration tests can build the router and
//! drive it over HTTP against an ephemeral port.

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
/// passed unwrapped so the wiring can attach the Db-holding magic-context
/// tools (`register_mctx`) after the DB opens; they are advertised only
/// when `magic_context.enabled`.
///
/// Callers may pre-populate `tools` before handing it over (as [`run`] does
/// with the CBM tools); this function only adds the magic-context tools.
pub async fn build_state_with(
    config: Config,
    chat: std::sync::Arc<dyn runner::Chat>,
    mut tools: runner::ToolRegistry,
) -> Result<AppState> {
    let db = Db::open(&config.db_path).await?;
    if config.magic_context.enabled {
        let embedder = mctx::embed::Embedder::from_config(&config.magic_context.embedding);
        tools.register_mctx(db.clone(), embedder);
    }
    let tools = Arc::new(tools);
    let cwd = std::env::current_dir()?;
    let directory = cwd.canonicalize().unwrap_or(cwd);
    let bus = EventBus::new(db.clone());
    let model_catalog = model_catalog::ModelCatalog::new(&config);
    let runner = Runner::new(
        config.clone(),
        directory.clone(),
        db.clone(),
        bus.clone(),
        chat,
        tools,
        Arc::clone(&model_catalog),
    );
    Ok(AppState {
        config,
        db,
        bus,
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
    let listen: String = config.listen.clone();
    let mut tools = runner::ToolRegistry::with_builtins();
    match cbm::Cbm::discover() {
        Some(cbm) => tools.register_cbm(cbm),
        None => tracing::info!(
            "codebase-memory-mcp not found on PATH; code-graph tools disabled: \
             index_repository, search_graph, trace_path, get_code_snippet, \
             get_architecture. Install it on the server's PATH to enable them"
        ),
    }
    let chat = cardinal_chat(&config)?;
    let state = build_state_with(config, chat, tools).await?;
    // Fetch the model list before serving, so the first client that asks
    // (`/api/model` at TUI startup) gets models instead of an empty list. A
    // failure only warns: the request path retries on its own TTL.
    state.model_catalog.warm().await;
    // Background memory consolidation. Idle unless `[magic_context]` enables
    // it, and deliberately not part of `build_state_with`: tests must not
    // acquire a background loop that writes to their database.
    if state.config.magic_context.enabled && state.config.magic_context.dreamer_enabled {
        Arc::new(mctx::dreamer::Dreamer::new(
            state.db.clone(),
            state.config.clone(),
            state.runner.chat(),
            Arc::clone(&state.model_catalog),
        ))
        .spawn();
    }
    let router = build_router(state);
    let acceptor = TcpListener::new(listen).bind().await;
    Server::new(acceptor).serve(router).await;
    Ok(())
}
