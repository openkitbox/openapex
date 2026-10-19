//! ox — pure-Rust, opencode v2 compatible chat-protocol server.
//!
//! This binary is the HTTP entrypoint. The library half (`lib.rs`) exposes
//! config/db/id/api so integration tests can drive the real router.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ox::config::Config::load()?;
    tracing::info!(
        listen = %config.listen,
        db = %config.db_path.display(),
        cardinal_url = %config.cardinal_url,
        "starting ox"
    );
    ox::run(config).await
}
