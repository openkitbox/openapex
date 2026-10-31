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

pub fn transport_of(server_arg: Option<String>) -> Transport {
    match server_arg {
        Some(url) => Transport::Explicit(url),
        None => Transport::Unix,
    }
}

/// Arguments for the self-exec'd server process.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    fn short_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ox")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp")
    }

    async fn read_head(stream: &mut UnixStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    const LEASE_HEAD: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                              transfer-encoding: chunked\r\nconnection: close\r\n\r\n";

    /// 200 + one `: lease` frame, then optionally end the stream.
    async fn write_lease(stream: &mut UnixStream, finish: bool) {
        let frame = ": lease\n\n";
        let mut out = format!("{LEASE_HEAD}{:x}\r\n{frame}\r\n", frame.len());
        if finish {
            out.push_str("0\r\n\r\n");
        }
        stream.write_all(out.as_bytes()).await.expect("write");
    }

    fn client_for(socket: &Path) -> Client {
        Client::over_unix(socket, "/tmp").expect("client")
    }

    #[test]
    fn server_args_are_the_detached_serve_invocation() {
        let args = server_args(Path::new("/tmp/ox.log"));
        let expected: Vec<OsString> = [
            "serve",
            "--exit-when-idle",
            "--no-stdout",
            "--log-file",
            "/tmp/ox.log",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn explicit_server_flag_selects_tcp_otherwise_unix() {
        assert_eq!(transport_of(None), Transport::Unix);
        assert_eq!(
            transport_of(Some("http://h:1".into())),
            Transport::Explicit("http://h:1".into())
        );
    }

    #[tokio::test]
    async fn a_listening_socket_is_not_respawned() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let _listener = UnixListener::bind(&socket).expect("bind");
        let spawned = AtomicBool::new(false);
        ensure_server(
            &socket,
            || {
                spawned.store(true, Ordering::SeqCst);
                Ok(())
            },
            Duration::from_secs(1),
        )
        .await
        .expect("already up");
        assert!(!spawned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_missing_server_is_spawned_once_and_awaited() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let held = Mutex::new(None);
        let calls = AtomicUsize::new(0);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            ensure_server(
                &socket,
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    *held.lock().expect("lock") = Some(UnixListener::bind(&socket)?);
                    Ok(())
                },
                Duration::from_secs(3),
            ),
        )
        .await
        .expect("bounded");
        result.expect("server came up");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_server_that_never_listens_times_out_naming_the_socket() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let err = ensure_server(&socket, || Ok(()), Duration::from_millis(300))
            .await
            .expect_err("timeout");
        assert!(
            err.to_string().contains(&socket.display().to_string()),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_failed_spawn_is_reported_without_polling() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ensure_server(
                &socket,
                || Err(io::Error::other("no exe")),
                Duration::from_secs(30),
            ),
        )
        .await
        .expect("returns at once");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn open_lease_is_none_when_refused_and_hold_returns_when_the_stream_ends() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.expect("accept");
            let head = read_head(&mut first).await;
            first
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .expect("write");
            let (mut second, _) = listener.accept().await.expect("accept");
            read_head(&mut second).await;
            write_lease(&mut second, true).await;
            head
        });
        let client = client_for(&socket);
        let refused = tokio::time::timeout(Duration::from_secs(5), open_lease(&client))
            .await
            .expect("503 returns");
        assert!(refused.is_none());
        let lease = tokio::time::timeout(Duration::from_secs(5), open_lease(&client))
            .await
            .expect("open returns")
            .expect("granted");
        tokio::time::timeout(Duration::from_secs(5), lease.hold())
            .await
            .expect("stream end returns");
        let head = server.await.expect("server");
        assert_eq!(head.lines().next(), Some("GET /ox/lease HTTP/1.1"));
    }

    #[tokio::test]
    async fn a_dead_socket_is_respawned_until_a_server_sticks() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let held = Mutex::new(None);
        let calls = AtomicUsize::new(0);
        // The first spawn "loses the lock" and leaves nothing listening; the
        // second one brings the socket up.
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            ensure_server(
                &socket,
                || {
                    if calls.fetch_add(1, Ordering::SeqCst) >= 1 {
                        *held.lock().expect("lock") = Some(UnixListener::bind(&socket)?);
                    }
                    Ok(())
                },
                Duration::from_secs(6),
            ),
        )
        .await
        .expect("bounded");
        result.expect("second spawn came up");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn acquire_lease_retries_past_a_refusing_server() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((mut stream, _)) = listener.accept().await {
                n += 1;
                tokio::spawn(async move {
                    read_head(&mut stream).await;
                    if n == 1 {
                        let _ = stream
                            .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                    } else {
                        write_lease(&mut stream, false).await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                });
            }
        });
        let reconnects = AtomicUsize::new(0);
        let client = client_for(&socket);
        let lease = tokio::time::timeout(
            Duration::from_secs(5),
            acquire_lease(&client, || {
                reconnects.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            }),
        )
        .await
        .expect("bounded")
        .expect("lease");
        drop(lease);
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn run_lease_reconnects_and_leases_again_after_the_server_drops_it() {
        let dir = short_dir();
        let socket = dir.path().join("s");
        let listener = UnixListener::bind(&socket).expect("bind");
        let connections = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&connections);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::spawn(async move {
                    read_head(&mut stream).await;
                    write_lease(&mut stream, n == 1).await;
                    if n > 1 {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                });
            }
        });

        let reconnects = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reconnects);
        let client = client_for(&socket);
        let first = open_lease(&client).await.expect("first lease");
        let task = tokio::spawn(run_lease(client, first, move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while (reconnects.load(Ordering::SeqCst) < 1 || connections.load(Ordering::SeqCst) < 2)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        task.abort();
        assert!(reconnects.load(Ordering::SeqCst) >= 1);
        assert!(connections.load(Ordering::SeqCst) >= 2);
    }
}
