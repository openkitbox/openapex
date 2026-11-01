//! Keeping a server alive for the TUI: connect to the unix socket, spawn
//! `serve --exit-when-idle` when nothing listens, and hold `GET /ox/lease`
//! for as long as the TUI runs so the server exits after the last client.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use ox_client::Client;

/// How long `ensure_server` waits for a freshly spawned server to listen.
pub const SERVER_START_WAIT: Duration = Duration::from_secs(10);

/// Pause between lease attempts and between reconnect attempts.
const RETRY: Duration = Duration::from_millis(500);

/// Poll interval while waiting for the socket to accept connections.
const POLL: Duration = Duration::from_millis(50);

/// How the TUI reaches the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// `--server <url>` was given: plain HTTP, no spawn, no lease.
    Explicit(String),
    /// Default: the local unix socket, spawned on demand and leased.
    Unix,
}

#[must_use]
pub fn transport_of(server_arg: Option<String>) -> Transport {
    match server_arg {
        Some(url) => Transport::Explicit(url),
        None => Transport::Unix,
    }
}

/// Arguments for the self-exec'd server process.
#[must_use]
pub fn server_args(log_file: &Path) -> Vec<OsString> {
    vec![
        "serve".into(),
        "--exit-when-idle".into(),
        "--no-stdout".into(),
        "--log-file".into(),
        log_file.into(),
    ]
}

/// Start `exe serve --exit-when-idle` detached from the terminal. It gets its
/// own process group, so Ctrl-C at the TUI does not reach it. The child is not
/// waited on: when the TUI exits it is adopted by init; if the server exits
/// first it stays a zombie until the TUI exits, which is harmless.
pub fn spawn_server(exe: &Path, log_file: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(exe)
        .args(server_args(log_file))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// How often `ensure_server` re-runs `spawn` while the socket stays dead. A
/// server that loses the instance lock exits at once, so a repeat is cheap, and
/// it lets a replacement start after an idle server that was shutting down
/// when the first spawn ran.
const RESPAWN: Duration = Duration::from_secs(1);

/// Return once something accepts connections on `socket`; spawn the server
/// when nothing does, then poll for up to `wait`, respawning every
/// [`RESPAWN`].
pub async fn ensure_server(
    socket: &Path,
    mut spawn: impl FnMut() -> io::Result<()>,
    wait: Duration,
) -> anyhow::Result<()> {
    if tokio::net::UnixStream::connect(socket).await.is_ok() {
        return Ok(());
    }
    spawn().context("failed to spawn the ox server")?;
    let start = tokio::time::Instant::now();
    let mut next_spawn = start + RESPAWN;
    loop {
        tokio::time::sleep(POLL).await;
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= start + wait {
            anyhow::bail!(
                "ox server did not start listening on {} within {:?}; check the server log",
                socket.display(),
                wait
            );
        }
        if now >= next_spawn {
            spawn().context("failed to spawn the ox server")?;
            next_spawn = now + RESPAWN;
        }
    }
}

/// An accepted `GET /ox/lease` stream. Holding it open is what tells the
/// server a client is online.
pub struct Lease(ox_client::reqwest::Response);

impl Lease {
    /// Read the stream until the server ends it or it fails.
    pub async fn hold(mut self) {
        while let Ok(Some(_)) = self.0.chunk().await {}
    }
}

/// Open the lease. `None` when the server cannot be reached or refuses it
/// (503 while it is shutting down).
pub async fn open_lease(client: &Client) -> Option<Lease> {
    let response = client
        .http
        .get(format!("{}/ox/lease", client.base))
        .send()
        .await
        .ok()?;
    response.status().is_success().then_some(Lease(response))
}

/// Bring a server up with `reconnect` and take a lease on it, retrying every
/// [`RETRY`] while the lease is refused. Only a server that granted a lease
/// is guaranteed not to be on its way out.
pub async fn acquire_lease<E, F>(client: &Client, mut reconnect: E) -> anyhow::Result<Lease>
where
    E: FnMut() -> F,
    F: Future<Output = anyhow::Result<()>>,
{
    loop {
        reconnect().await?;
        if let Some(lease) = open_lease(client).await {
            return Ok(lease);
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Keep a lease for the life of the process. When it ends (server gone), run
/// `reconnect` (which should bring a server back) and lease again. Never
/// returns; the caller spawns it and lets process exit end it.
pub async fn run_lease<E, F>(client: Client, first: Lease, mut reconnect: E)
where
    E: FnMut() -> F,
    F: Future<Output = anyhow::Result<()>>,
{
    let mut lease = first;
    loop {
        lease.hold().await;
        lease = loop {
            tokio::time::sleep(RETRY).await;
            match acquire_lease(&client, &mut reconnect).await {
                Ok(lease) => break lease,
                Err(err) => tracing::warn!(error = %err, "ox server unavailable; retrying"),
            }
        };
    }
}
