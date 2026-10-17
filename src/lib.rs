//! ox library: config, storage, ID generation and HTTP API.
//!
//! Split out from `main.rs` so integration tests can build the router and
//! drive it over HTTP against an ephemeral port.

pub mod api;
pub mod config;
pub mod db;
pub mod id;

use anyhow::Result;
use salvo::prelude::*;
use std::time::Instant;

use crate::api::AppState;
use crate::config::Config;
use crate::db::Db;

/// Build application state: open the DB and run migrations.
pub async fn build_state(config: Config) -> Result<AppState> {
    let db = Db::open(&config.db_path).await?;
    Ok(AppState {
        config,
        db,
        started: Instant::now(),
        directory: std::env::current_dir()?,
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
