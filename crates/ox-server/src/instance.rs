//! Single-instance lock and unix-socket binding.
//!
//! Owns two things: the exclusive lock that makes one server per home, and
//! the bind of the private unix socket. Callers pass every path in; nothing
//! here reads global state.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use salvo::conn::Listener;
use salvo::conn::unix::{UnixAcceptor, UnixListener};

/// Holds the exclusive lock for as long as it lives; the OS releases it when
/// the process dies, however it dies.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Take the lock at `path` without blocking. The lock file is never
    /// removed: deleting it would let a second server lock a fresh inode.
    pub fn acquire(path: &Path) -> Result<Self> {
        create_private_dir(path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot open lock file `{}`", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "another ox server is already running (lock `{}`)",
                    path.display()
                )
            }
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("cannot lock `{}`", path.display()))
            }
        }
    }
}

/// Bind the private unix socket at `path`.
///
/// Only valid while holding the [`InstanceLock`]: an existing file at `path`
/// is then necessarily stale and is unlinked first. The parent directory is
/// created at 0700 and the socket is chmodded 0600.
pub async fn bind_unix(path: &Path) -> Result<UnixAcceptor> {
    create_private_dir(path)?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot remove stale socket `{}`", path.display()));
        }
    }
    UnixListener::new(path.to_path_buf())
        .permissions(fs::Permissions::from_mode(0o600))
        .try_bind()
        .await
        .with_context(|| format!("cannot bind unix socket `{}`", path.display()))
}

/// Create the parent directory of `file` (and ancestors) at 0700 if missing.
fn create_private_dir(file: &Path) -> Result<()> {
    let Some(dir) = file.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create `{}`", dir.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use salvo::conn::Acceptor;

    use super::*;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn second_acquire_fails_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("run").join("ox.lock");
        let first = InstanceLock::acquire(&path).expect("first lock");
        let error = InstanceLock::acquire(&path).expect_err("second lock must fail");
        assert!(
            error.to_string().contains("already running"),
            "unexpected message: {error}"
        );
        drop(first);
        InstanceLock::acquire(&path).expect("lock after release");
    }

    #[tokio::test]
    async fn bind_unix_makes_a_private_socket_in_a_private_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = dir.path().join("run");
        let socket = run.join("ox.sock");
        let acceptor = bind_unix(&socket).await.expect("bind");
        assert_eq!(mode(&run), 0o700);
        assert_eq!(mode(&socket), 0o600);
        assert_eq!(acceptor.holdings().len(), 1);
    }

    #[tokio::test]
    async fn bind_unix_replaces_a_stale_plain_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("ox.sock");
        std::fs::write(&socket, b"stale").expect("write stale");
        let _acceptor = bind_unix(&socket).await.expect("bind over a plain file");
        tokio::net::UnixStream::connect(&socket)
            .await
            .expect("connect to the new socket");
    }

    #[tokio::test]
    async fn bind_unix_replaces_a_stale_socket_and_accepts_connections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("ox.sock");
        // A dead owner leaves its socket file behind.
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("stale socket"));
        let mut acceptor = bind_unix(&socket).await.expect("bind over a stale socket");
        let client = tokio::spawn({
            let socket = socket.clone();
            async move { tokio::net::UnixStream::connect(socket).await }
        });
        let accepted = acceptor.accept(None).await.expect("accept");
        drop(accepted);
        client.await.expect("join").expect("client connected");
    }
}
