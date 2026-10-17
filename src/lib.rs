//! ox library: config, storage, ID generation and HTTP API.
//!
//! Split out from `main.rs` so integration tests can build the router and
//! drive it over HTTP against an ephemeral port.

pub mod api;
pub mod config;
pub mod db;
pub mod event_bus;
pub mod id;
pub mod projector;
pub mod schema;

use anyhow::Result;
use salvo::prelude::*;

use crate::api::AppState;
use crate::config::Config;
use crate::db::Db;

/// Build application state: open the DB, run migrations, and canonicalize
/// the working directory once (falls back to the raw path on error).
pub async fn build_state(config: Config) -> Result<AppState> {
    let db = Db::open(&config.db_path).await?;
    let cwd = std::env::current_dir()?;
    let directory = cwd.canonicalize().unwrap_or(cwd);
    Ok(AppState {
        config,
        db,
        directory,
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
    let router = build_router(state);
    let acceptor = TcpListener::new(listen).bind().await;
    Server::new(acceptor).serve(router).await;
    Ok(())
}
