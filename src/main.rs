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
                cardinal_url = %config.llm.cardinal_url,
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
                cardinal_url = %config.llm.cardinal_url,
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

#[cfg(test)]
mod tests {
    use super::{LogOutputs, parse_acp_args, parse_server_args};
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

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
    fn log_path_is_split_into_parent_and_filename() {
        let path = std::path::Path::new("/tmp/logs/ox.log");
        assert_eq!(
            super::split_log_path(path),
            (std::path::PathBuf::from("/tmp/logs"), "ox.log".to_owned())
        );
    }

    #[test]
    fn rolling_log_writes_standard_hourly_filename() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ox.log");
        let (writer, guard) = super::rolling_log_writer(&path).unwrap();
        let mut file = writer.make_writer();
        writeln!(file, "rolling smoke").unwrap();
        drop(file);
        drop(writer);
        drop(guard);
        let files = std::fs::read_dir(directory.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let filename = files[0].file_name();
        assert!(filename.to_string_lossy().starts_with("ox.log."));
        assert_eq!(
            std::fs::read_to_string(directory.path().join(filename)).unwrap(),
            "rolling smoke\n"
        );
    }

    #[test]
    fn rolling_log_error_contains_original_path() {
        let path = std::path::Path::new("/definitely/missing/ox.log");
        let error = super::rolling_log_writer(path).unwrap_err();
        assert!(error.to_string().contains("/definitely/missing/ox.log"));
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
