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

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Debug, Eq, PartialEq)]
struct LogOutputs {
    stdout: bool,
    log_file: Option<PathBuf>,
}

impl Default for LogOutputs {
    fn default() -> Self {
        Self {
            stdout: true,
            log_file: None,
        }
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

fn open_log_file(path: &std::path::Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open log file `{}`", path.display()))
}
#[tokio::main]
async fn main() -> Result<()> {
    // Subcommand split happens before tracing init: in `acp` mode stdout is
    // the NDJSON wire, so logs go to stderr; the server mode keeps stdout.
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("acp") => {
            let (explicit_base, outputs) = parse_acp_args(args)?;
            if let Some(base) = &explicit_base {
                ox::acp::validate_base(base)?;
            }
            init_tracing(&outputs, true)?;
            let config = ox::config::Config::load()?;
            ox::acp::run(explicit_base, config).await
        }
        Some(flag) if flag.starts_with('-') => {
            let outputs = parse_server_args(std::iter::once(flag.to_owned()).chain(args))?;
            init_tracing(&outputs, false)?;
            let config = ox::config::Config::load()?;
            tracing::info!(
                listen = %config.listen,
                db = %config.db_path.display(),
                cardinal_url = %config.cardinal_url,
                "starting ox"
            );
            ox::run(config).await
        }
        Some(other) => anyhow::bail!("unknown subcommand `{other}`; usage: opencode-rs [acp]"),
        None => {
            let outputs = parse_server_args(args)?;
            init_tracing(&outputs, false)?;
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

/// In `acp` mode stdout is the NDJSON wire — logs always go to stderr.
fn init_tracing(outputs: &LogOutputs, acp: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if acp {
        let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
        let subscriber = tracing_subscriber::registry().with(filter).with(stderr);
        if let Some(path) = &outputs.log_file {
            let file = open_log_file(path)?;
            subscriber
                .with(tracing_subscriber::fmt::layer().with_writer(file))
                .try_init()
                .context("failed to initialize tracing")?;
        } else {
            subscriber
                .try_init()
                .context("failed to initialize tracing")?;
        }
        return Ok(());
    }

    let file = outputs.log_file.as_deref().map(open_log_file).transpose()?;
    let subscriber = tracing_subscriber::registry().with(filter);
    match (outputs.stdout, file) {
        (true, Some(file)) => subscriber
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
            .with(tracing_subscriber::fmt::layer().with_writer(file))
            .try_init(),
        (true, None) => subscriber
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
            .try_init(),
        (false, Some(file)) => subscriber
            .with(tracing_subscriber::fmt::layer().with_writer(file))
            .try_init(),
        (false, None) => subscriber.try_init(),
    }
    .context("failed to initialize tracing")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LogOutputs, parse_acp_args, parse_server_args};
    use std::io::Write;

    #[test]
    fn server_defaults_to_stdout_without_file() {
        assert_eq!(
            parse_server_args(std::iter::empty::<&str>()).unwrap(),
            LogOutputs::default()
        );
    }

    #[test]
    fn server_can_enable_file_alongside_stdout() {
        let outputs = parse_server_args(["--log-file", "/tmp/ox.log"]).unwrap();
        assert!(outputs.stdout);
        assert_eq!(
            outputs.log_file.as_deref().unwrap().to_str(),
            Some("/tmp/ox.log")
        );
    }

    #[test]
    fn server_can_use_file_exclusively() {
        let outputs = parse_server_args(["--log-file", "/tmp/ox.log", "--no-stdout"]).unwrap();
        assert!(!outputs.stdout);
        assert!(outputs.log_file.is_some());
    }

    #[test]
    fn server_can_disable_all_log_outputs() {
        assert_eq!(
            parse_server_args(["--no-stdout"]).unwrap(),
            LogOutputs {
                stdout: false,
                log_file: None,
            }
        );
    }

    #[test]
    fn conflicting_stdout_flags_are_rejected() {
        let error = parse_server_args(["--stdout", "--no-stdout"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot combine --stdout and --no-stdout")
        );
    }

    #[test]
    fn missing_file_path_and_unknown_flag_are_rejected() {
        let missing = parse_server_args(["--log-file"]).unwrap_err();
        assert!(missing.to_string().contains("--log-file needs a path"));
        let missing_before_flag = parse_server_args(["--log-file", "--no-stdout"]).unwrap_err();
        assert!(
            missing_before_flag
                .to_string()
                .contains("--log-file needs a path")
        );

        let unknown = parse_server_args(["--wat"]).unwrap_err();
        assert!(unknown.to_string().contains("unknown argument `--wat`"));
    }

    #[test]
    fn log_file_is_created_and_appended() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ox.log");
        {
            let mut file = super::open_log_file(&path).unwrap();
            writeln!(file, "first").unwrap();
        }
        {
            let mut file = super::open_log_file(&path).unwrap();
            writeln!(file, "second").unwrap();
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn acp_never_uses_stdout_for_tracing() {
        let error = parse_acp_args(["--stdout"]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ACP stdout is reserved for NDJSON")
        );

        let (server, outputs) = parse_acp_args(["--no-stdout"]).unwrap();
        assert!(server.is_none());
        assert!(!outputs.stdout);
        assert!(outputs.log_file.is_none());
    }
}
