//! ox — pure-Rust, opencode v2 compatible chat-protocol server.
//!
//! This binary is the HTTP entrypoint. The library half (`lib.rs`) exposes
//! config/db/id/api so integration tests can drive the real router.
//! The `acp` subcommand runs the Agent Client Protocol translator instead
//! (`src/acp`); the default path boots the server exactly as before.
//!
//! ox owns the process-wide allocator: turso's default `mimalloc` feature
//! (which would install its own `#[global_allocator]`) is disabled in
//! Cargo.toml so the declaration can live here.
//!
//! macOS deliberately uses the system allocator. mimalloc's arena keeps pages
//! it has returned to the OS counted in the process footprint there, because
//! macOS `madvise` uses `MADV_FREE` (a mark-reusable hint) where Linux uses
//! `MADV_DONTNEED` (an immediate return). A/B on a real 9.4 MiB transcript,
//! identical source, one prompt per turn:
//!
//! - mimalloc 3.6: footprint starts at 101 MiB, peaks at 119 MiB after the
//!   first turn, then never drops below 100 MiB (100 -> 109 MiB over six
//!   turns); RSS climbs monotonically 118 -> 163 MiB.
//! - system allocator: footprint starts at 33 MiB, reaches 52-53 MiB by turn
//!   four and then stays there — twenty turns end at 40 MiB, i.e. it rises and
//!   gives the pages back; RSS 113 -> 140 MiB and flat from turn four on.
//!
//! CPU is unchanged (6.68 s vs 6.69 s over 60 turns). Linux keeps mimalloc —
//! that is where `MADV_DONTNEED` makes it behave.

#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL: std::alloc::System = std::alloc::System;

#[cfg(not(target_os = "macos"))]
#[global_allocator]
static GLOBAL: rustfs_mimalloc::MiMalloc = rustfs_mimalloc::MiMalloc;

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
