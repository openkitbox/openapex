//! ox library: config, storage, ID generation and HTTP API.
//!
//! Split out from `main.rs` so integration tests can build the router and
//! drive it over HTTP against an ephemeral port.

pub mod api;
pub mod cardinal;
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

/// Build application state: open the DB, run migrations, and canonicalize
/// the working directory once (falls back to the raw path on error).
pub async fn build_state(config: Config) -> Result<AppState> {
    let cardinal = CardinalClient::new(config.cardinal_url.clone(), config.cardinal_key.clone())
        .map_err(|error| anyhow::anyhow!("cardinal client init failed: {error}"))?;
    build_state_with(
        config,
        Arc::new(cardinal),
        runner::ToolRegistry::with_builtins(),
    )
    .await
}

/// [`build_state`] with an injected chat transport and tool registry —
/// the seam tests (and later, alternate providers) use. The registry is
/// passed unwrapped so the wiring can attach the Db-holding magic-context
/// tools (`register_mctx`) after the DB opens; they are advertised only
/// when `magic_context.enabled`.
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
    let state = build_state(config).await?;
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
