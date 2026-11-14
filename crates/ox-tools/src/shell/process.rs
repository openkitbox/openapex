//! Operating-system layer of the shell tool: pick a system shell, spawn a
//! command as its own process group with stdout and stderr merged onto one
//! pipe, read that pipe, and signal the group.
//!
//! This module owns nothing but OS resources. It does not know about the
//! session table, the completion gate or events.
//!
//! Invariants:
//! - A signal is only ever sent to a process group id greater than 1, so
//!   `kill(-0)` / `kill(-1)` (own group / everything) can never be issued.
//! - After `spawn` returns, the parent holds no write end of the pipe, so the
//!   reader reaches EOF exactly when every process in the command's tree
//!   (including background grandchildren) has closed its output.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::{Child, Command};

/// Size of the reusable read buffer of [`pump`].
const READ_BUF: usize = 64 * 1024;

/// Shells ox refuses to run `-c <bash-ish command>` through (upstream
/// `ShellSelect.META` deny list).
const DENIED_SHELLS: [&str; 2] = ["fish", "nu"];

/// A running command: the shell child, its process group, and the merged
/// output pipe.
pub(super) struct Spawned {
    pub child: Child,
    /// Process group id; equals the child's pid.
    pub pgid: u32,
    /// Read end of the pipe both stdout and stderr write to.
    pub reader: Receiver,
}

/// Signals ox sends to a command's process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Signal {
    Term,
    Kill,
}

impl Signal {
    fn raw(self) -> libc::c_int {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }
}

/// Whether `shell`'s basename is on the deny list (or missing).
fn denied(shell: &Path) -> bool {
    shell
        .file_name()
        .map(|name| DENIED_SHELLS.contains(&name.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(true)
}

/// Ordered shell candidates: `$SHELL`, then `/bin/zsh` on macOS, `/bin/bash`,
/// `/bin/sh`; denied shells are dropped. Pure: the environment is an input.
fn shell_candidates(env_shell: Option<OsString>, macos: bool) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    candidates.extend(env_shell.map(PathBuf::from));
    if macos {
        candidates.push(PathBuf::from("/bin/zsh"));
    }
    candidates.push(PathBuf::from("/bin/bash"));
    candidates.push(PathBuf::from("/bin/sh"));
    candidates.retain(|candidate| !denied(candidate));
    candidates
}

/// First candidate that `is_file` accepts, else `/bin/sh`.
fn pick_shell(candidates: Vec<PathBuf>, is_file: impl Fn(&Path) -> bool) -> PathBuf {
    candidates
        .into_iter()
        .find(|candidate| is_file(candidate))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Resolve the shell commands run under.
pub(super) fn select_shell() -> PathBuf {
    pick_shell(
        shell_candidates(std::env::var_os("SHELL"), cfg!(target_os = "macos")),
        Path::is_file,
    )
}

/// Names of the terminal-multiplexer variables that identify the pane the
/// server was started in (herdr's `HERDR_*`). Commands run in no pane of
/// their own, so a TUI started by one must not report itself as that pane's
/// agent. Pure: the environment is an input.
fn pane_identity_vars(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    vars.map(|(key, _)| key)
        .filter(|key| key.as_encoded_bytes().starts_with(b"HERDR_"))
        .collect()
}

/// Spawn `shell -c command` in `cwd` as the leader of a new process group,
/// with stdin at `/dev/null` and stdout+stderr on one pipe. Must be called
/// inside a tokio runtime (the read end is registered with its reactor).
pub(super) fn spawn(
    shell: &Path,
    command: &str,
    cwd: &Path,
    environment: Option<&HashMap<String, String>>,
    owned_session: bool,
) -> io::Result<Spawned> {
    let (read, write) = io::pipe()?;
    let write_err = write.try_clone()?;
    let mut cmd = Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(write)
        .stderr(write_err)
        .process_group(0)
        .kill_on_drop(true);
    if let Some(environment) = environment {
        // A configured session environment replaces the inherited process
        // environment. With no configured environment this branch is skipped,
        // preserving the shell's normal inherited environment.
        cmd.env_clear();
        cmd.envs(environment);
    }
    if owned_session {
        // Match upstream Session.shell: every session-owned local shell gets
        // a stable terminal identity, even when no custom session environment
        // has been configured.
        cmd.env("TERM", "xterm-256color");
        cmd.env("OPENCODE_TERMINAL", "1");
    }
    for key in pane_identity_vars(std::env::vars_os()) {
        cmd.env_remove(key);
    }
    if let Some(environment) = environment {
        for key in environment.keys().filter(|key| key.starts_with("HERDR_")) {
            cmd.env_remove(key);
        }
    }
    let child = cmd.spawn()?;
    // `cmd` owns the parent's copies of the write ends; release them so EOF
    // is reachable.
    drop(cmd);
    let pgid = child
        .id()
        .ok_or_else(|| io::Error::other("child exited before its pid was read"))?;
    // `from_file` switches the descriptor to non-blocking itself.
    let reader = Receiver::from_file(File::from(OwnedFd::from(read)))?;
    Ok(Spawned {
        child,
        pgid,
        reader,
    })
}

/// Read `reader` to EOF, handing every chunk to `sink` straight out of one
/// reused buffer.
pub(super) async fn pump(reader: &mut Receiver, mut sink: impl FnMut(&[u8])) -> io::Result<()> {
    let mut buf = vec![0u8; READ_BUF].into_boxed_slice();
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        sink(&buf[..n]);
    }
}

/// Validate a process group id for signalling: > 1 and representable as pid_t.
fn group_target(pgid: u32) -> io::Result<libc::pid_t> {
    match libc::pid_t::try_from(pgid) {
        Ok(pid) if pid > 1 => Ok(pid),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal process group {pgid}"),
        )),
    }
}

/// Send `sig` to every process in group `pgid`. A group that no longer
/// exists counts as success.
pub(super) fn kill_group(pgid: u32, sig: Signal) -> io::Result<()> {
    signal_group(pgid, sig.raw())
}

fn signal_group(pgid: u32, raw: libc::c_int) -> io::Result<()> {
    let pid = group_target(pgid)?;
    // SAFETY: plain syscall with validated, non-special arguments.
    if unsafe { libc::kill(-pid, raw) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(err),
    }
}

/// Whether any process (zombies included) is still in group `pgid`.
/// Special group ids (0, 1) are never probed and report `false`.
pub(super) fn group_alive(pgid: u32) -> bool {
    let Ok(pid) = group_target(pgid) else {
        return false;
    };
    // SAFETY: signal 0 only checks existence; arguments are validated.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return true;
    }
    // EPERM: the group exists but belongs to someone else.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
