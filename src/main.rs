//! ox — pure-Rust, opencode v2 compatible chat-protocol server and ACP agent.
//!
//! One binary, two explicit modes picked by the first argument: `serve` runs
//! the HTTP server, and `acp` runs the Agent Client Protocol translator
//! (`src/acp`). With no subcommand, the binary starts the ordinary HTTP server.
//! The library half (`lib.rs`) exposes config/db/id/api so integration tests can
//! drive the real router.
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

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static LOG_GUARDS: OnceLock<Vec<WorkerGuard>> = OnceLock::new();

#[derive(Debug, Eq, PartialEq)]
struct LogOutputs {
    stdout: bool,
    log_file: Option<PathBuf>,
    exit_when_idle: bool,
    service: bool,
}

impl Default for LogOutputs {
    fn default() -> Self {
        Self {
            stdout: true,
            log_file: None,
            exit_when_idle: false,
            service: false,
        }
    }
}

/// What the command line asks `ox` to run.
#[derive(Debug, Eq, PartialEq)]
enum Command {
    Serve(LogOutputs),
    Acp(Option<String>, LogOutputs),
}

const USAGE: &str = "usage: ox [flags] | ox serve [flags] | ox acp [flags]";

fn parse_command<I, S>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Serve(LogOutputs::default()));
    };
    match first.as_ref() {
        "serve" => Ok(Command::Serve(parse_server_args(args)?)),
        "acp" => {
            let (explicit_base, outputs) = parse_acp_args(args)?;
            Ok(Command::Acp(explicit_base, outputs))
        }
        flag if flag.starts_with('-') => {
            let server_args = std::iter::once(first).chain(args);
            Ok(Command::Serve(parse_server_args(server_args)?))
        }
        other => anyhow::bail!("unknown subcommand `{other}`; {USAGE}"),
    }
}

fn parse_server_args<I, S>(args: I) -> Result<LogOutputs>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut outputs = LogOutputs::default();
    let mut stdout_flag = None;
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_ref() {
            "--log-file" => {
                let Some(path) = args.next() else {
                    anyhow::bail!("--log-file needs a path argument");
                };
                if path.as_ref().starts_with('-') {
                    anyhow::bail!("--log-file needs a path argument");
                }
                outputs.log_file = Some(PathBuf::from(path.as_ref()));
            }
            "--exit-when-idle" => outputs.exit_when_idle = true,
            "--service" => outputs.service = true,
            "--stdout" => set_stdout_flag(&mut outputs, &mut stdout_flag, true)?,
            "--no-stdout" => set_stdout_flag(&mut outputs, &mut stdout_flag, false)?,
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }
    Ok(outputs)
}

fn parse_acp_args<I, S>(args: I) -> Result<(Option<String>, LogOutputs)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut explicit_base = None;
    let mut outputs = LogOutputs::default();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        match argument.as_ref() {
            "--server" => {
                let Some(url) = args.next() else {
                    anyhow::bail!("ox acp: --server needs a URL argument");
                };
                explicit_base = Some(url.as_ref().to_owned());
            }
            "--log-file" => {
                let Some(path) = args.next() else {
                    anyhow::bail!("ox acp: --log-file needs a path argument");
                };
                if path.as_ref().starts_with('-') {
                    anyhow::bail!("ox acp: --log-file needs a path argument");
                }
                outputs.log_file = Some(PathBuf::from(path.as_ref()));
            }
            "--no-stdout" => outputs.stdout = false,
            "--stdout" => anyhow::bail!(
                "ACP stdout is reserved for NDJSON; use --log-file or the default stderr logs"
            ),
            other => anyhow::bail!("ox acp: unknown argument `{other}`"),
        }
    }
    Ok((explicit_base, outputs))
}

fn set_stdout_flag(
    outputs: &mut LogOutputs,
    previous: &mut Option<bool>,
    enabled: bool,
) -> Result<()> {
    if previous
        .replace(enabled)
        .is_some_and(|previous| previous != enabled)
    {
        anyhow::bail!("cannot combine --stdout and --no-stdout");
    }
    outputs.stdout = enabled;
    Ok(())
}

fn split_log_path(path: &Path) -> (PathBuf, String) {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ox.log")
        .to_owned();
    (directory.to_owned(), filename)
}

fn rolling_log_writer(path: &Path) -> Result<(NonBlocking, WorkerGuard)> {
    let (directory, filename) = split_log_path(path);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::HOURLY)
        .filename_prefix(filename)
        .build(&directory)
        .with_context(|| format!("failed to open log file `{}`", path.display()))?;
    Ok(tracing_appender::non_blocking(appender))
}

/// Load `~/.ox/ox.toml` and reject `[agents.<id>]` overrides naming no
/// built-in agent.
///
/// `ox-core` parses the table but cannot see the built-in id table, which
/// lives in `ox-tools`, so the check runs here on every path that loads the
/// config for a long-running process. A misspelled id fails the load instead
/// of being silently ignored.
fn load_config() -> Result<ox::config::Config> {
    let config = ox::config::Config::load()?;
    let path = ox::config::Config::default_path();
    ox_tools::agent::validate_ids(config.agents.keys().map(String::as_str))
        .with_context(|| format!("failed to load config file {}", path.display()))?;
    Ok(config)
}

fn load_service() -> Result<(
    ox_server::service::ServiceConfig,
    ox_server::service::ServicePaths,
)> {
    let home = ox_core::config::user_home();
    let config_dir = std::env::var_os("OPENCODE_CONFIG_DIR");
    let config_dir = config_dir.as_deref().map(Path::new);
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME");
    let xdg_config = xdg_config.as_deref().map(Path::new);
    let xdg_state = std::env::var_os("XDG_STATE_HOME");
    let xdg_state = xdg_state.as_deref().map(Path::new);
    let channel = std::env::var("OPENCODE_CHANNEL").unwrap_or_else(|_| "latest".to_owned());
    let paths = ox_server::service::service_paths_for_process(
        &home, config_dir, xdg_config, xdg_state, &channel,
    );
    let config = ox_server::service::load_or_create_config(
        &paths.config_path,
        ox_server::service::default_port(&channel),
    )
    .with_context(|| {
        format!(
            "failed to load service config {}",
            paths.config_path.display()
        )
    })?;
    Ok((config, paths))
}

fn serve_options(outputs: &LogOutputs) -> ox::ServeOptions {
    ox::ServeOptions {
        exit_when_idle: outputs.exit_when_idle,
        ..ox::ServeOptions::default()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Mode split happens before tracing init: `acp` uses stdout as the NDJSON
    // wire (logs go to stderr), and `serve` logs to stdout and/or a file.
    match parse_command(std::env::args().skip(1))? {
        Command::Acp(explicit_base, outputs) => {
            if let Some(base) = &explicit_base {
                ox::acp::validate_base(base)?;
            }
            init_tracing(&outputs, true)?;
            let config = load_config()?;
            ox::acp::run(explicit_base, config).await
        }
        Command::Serve(outputs) => {
            init_tracing(&outputs, false)?;
            let config = load_config()?;
            if outputs.service {
                let (service, paths) = load_service()?;
                let idle = outputs
                    .exit_when_idle
                    .then_some(ox::ServeOptions::default().grace);
                return ox::run_service(config, service, paths.registration_path, idle).await;
            }
            tracing::info!(
                listen = %config.listen,
                db = %config.db_path.display(),
                cardinal_url = %config.llm.cardinal_url,
                "starting ox"
            );
            ox::run_with(config, serve_options(&outputs)).await
        }
    }
}

fn init_tracing(outputs: &LogOutputs, acp: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let mut guards = Vec::new();
    let mut writer = if acp {
        Some(BoxMakeWriter::new(std::io::stderr))
    } else if outputs.stdout {
        Some(BoxMakeWriter::new(std::io::stdout))
    } else {
        None
    };

    if let Some(path) = &outputs.log_file {
        let (file_writer, guard) = rolling_log_writer(path)?;
        guards.push(guard);
        let file_writer = BoxMakeWriter::new(file_writer);
        writer = Some(match writer {
            Some(writer) => BoxMakeWriter::new(writer.and(file_writer)),
            None => file_writer,
        });
    }

    let subscriber = tracing_subscriber::registry().with(filter);
    if let Some(writer) = writer {
        subscriber
            .with(tracing_subscriber::fmt::layer().with_writer(writer))
            .try_init()
            .context("failed to initialize tracing")?;
    } else {
        subscriber
            .try_init()
            .context("failed to initialize tracing")?;
    }
    LOG_GUARDS
        .set(guards)
        .map_err(|_| anyhow::anyhow!("tracing guards already initialized"))?;
    Ok(())
}

/// In `acp` mode stdout is the NDJSON wire — logs always go to stderr.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_flag_selects_managed_service_mode() {
        let outputs =
            parse_server_args(["--service"]).expect("--service should select managed service mode");
        assert!(outputs.service);
        assert!(!outputs.exit_when_idle);
    }

    #[test]
    fn service_mode_accepts_exit_when_idle() {
        let outputs = parse_server_args(["--service", "--exit-when-idle"])
            .expect("managed service mode should accept idle shutdown");
        assert!(outputs.service);
        assert!(outputs.exit_when_idle);
    }

    #[test]
    fn ordinary_serve_remains_non_service_mode() {
        let outputs =
            parse_server_args(["--no-stdout"]).expect("ordinary serve flags should remain valid");
        assert!(!outputs.service);
        assert!(!outputs.stdout);
    }
}
