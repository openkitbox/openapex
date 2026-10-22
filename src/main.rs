//! ox — pure-Rust, opencode v2 compatible chat-protocol server.
//!
//! This binary is the HTTP entrypoint. The library half (`lib.rs`) exposes
//! config/db/id/api so integration tests can drive the real router.
//! The `acp` subcommand runs the Agent Client Protocol translator instead
//! (`src/acp`); the default path boots the server exactly as before.
//!
use anyhow::Result;
#[tokio::main]
async fn main() -> Result<()> {
    // Subcommand split happens before tracing init: in `acp` mode stdout is
    // the NDJSON wire, so logs go to stderr; the server mode keeps stdout.
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("acp") => {
            let mut explicit_base = None;
            let mut rest = args.peekable();
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--server" => {
                        let Some(url) = rest.next() else {
                            anyhow::bail!("ox acp: --server needs a URL argument");
                        };
                        explicit_base = Some(url);
                    }
                    other => anyhow::bail!("ox acp: unknown argument `{other}`"),
                }
            }
            if let Some(base) = &explicit_base {
                ox::acp::validate_base(base)?;
            }
            init_tracing(true);
            let config = ox::config::Config::load()?;
            ox::acp::run(explicit_base, config).await
        }
        Some(flag) if flag.starts_with('-') => {
            anyhow::bail!("unknown argument `{flag}`; usage: opencode-rs [acp [--server <url>]]")
        }
        Some(other) => anyhow::bail!("unknown subcommand `{other}`; usage: opencode-rs [acp]"),
        None => {
            init_tracing(false);
            let config = ox::config::Config::load()?;
            tracing::info!(
                listen = %config.listen,
                db = %config.db_path.display(),
                cardinal_url = %config.cardinal_url,
                "starting ox"
            );
            ox::run(config).await
        }
    }
}

/// In `acp` mode stdout is the NDJSON wire — logs go to stderr.
fn init_tracing(stderr: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if stderr {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stdout)
            .init();
    }
}
