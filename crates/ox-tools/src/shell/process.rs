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

/// Spawn `shell -c command` in `cwd` as the leader of a new process group,
/// with stdin at `/dev/null` and stdout+stderr on one pipe. Must be called
/// inside a tokio runtime (the read end is registered with its reactor).
pub(super) fn spawn(shell: &Path, command: &str, cwd: &Path) -> io::Result<Spawned> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant};

    const SH: &str = "/bin/sh";
    const LIMIT: Duration = Duration::from_secs(10);

    /// Kills the whole group on drop so a failing test leaves nothing behind.
    struct GroupGuard(u32);
    impl Drop for GroupGuard {
        fn drop(&mut self) {
            let _ = kill_group(self.0, Signal::Kill);
        }
    }

    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < LIMIT, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn start(command: &str) -> (Spawned, GroupGuard) {
        let spawned = spawn(Path::new(SH), command, Path::new("/")).expect("spawn");
        let guard = GroupGuard(spawned.pgid);
        (spawned, guard)
    }

    /// Run a foreground-only command to completion and collect its output.
    async fn run(command: &str) -> (ExitStatus, Vec<u8>) {
        let (mut spawned, _guard) = start(command);
        let mut out = Vec::new();
        let (status, read) = tokio::time::timeout(LIMIT, async {
            tokio::join!(
                spawned.child.wait(),
                pump(&mut spawned.reader, |chunk| out.extend_from_slice(chunk))
            )
        })
        .await
        .expect("command finished in time");
        read.expect("pump");
        (status.expect("wait"), out)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exit_code_and_output() {
        let (status, out) = run("echo hi; exit 3").await;
        assert_eq!(status.code(), Some(3));
        assert!(String::from_utf8_lossy(&out).contains("hi"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stdout_and_stderr_share_one_ordered_stream() {
        let (status, out) = run("echo a; echo b >&2; echo c").await;
        assert!(status.success());
        assert_eq!(out, b"a\nb\nc\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn signal_exit_is_reported() {
        let (status, _) = run("kill -9 $$").await;
        assert_eq!(status.signal(), Some(9));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_group_takes_down_the_whole_pipeline() {
        let (mut spawned, _guard) = start("sleep 30 | cat");
        let pgid = spawned.pgid;
        assert_eq!(pgid, spawned.child.id().unwrap());
        wait_until("group to be alive", || group_alive(pgid)).await;
        kill_group(pgid, Signal::Kill).expect("kill_group");
        tokio::time::timeout(LIMIT, spawned.child.wait())
            .await
            .expect("shell reaped")
            .expect("wait");
        wait_until("group to vanish", || !group_alive(pgid)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn background_job_outlives_the_command() {
        let (mut spawned, _guard) = start("sleep 30 &");
        let pgid = spawned.pgid;
        let status = tokio::time::timeout(LIMIT, spawned.child.wait())
            .await
            .expect("shell exited")
            .expect("wait");
        assert!(status.success());
        assert!(group_alive(pgid), "background sleep must survive its shell");
        kill_group(pgid, Signal::Kill).expect("cleanup");
        wait_until("group to vanish", || !group_alive(pgid)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn busy_loop_background_is_a_killable_process() {
        let (mut spawned, _guard) = start("(while :; do :; done) &");
        let pgid = spawned.pgid;
        tokio::time::timeout(LIMIT, spawned.child.wait())
            .await
            .expect("shell exited")
            .expect("wait");
        assert!(group_alive(pgid), "the loop is a real process with a pgid");
        kill_group(pgid, Signal::Kill).expect("kill_group");
        wait_until("busy loop to vanish", || !group_alive(pgid)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reader_reaches_eof_once_background_output_closes() {
        let (mut spawned, _guard) = start("(sleep 0.2; echo late) &");
        let mut out = Vec::new();
        tokio::time::timeout(
            LIMIT,
            pump(&mut spawned.reader, |c| out.extend_from_slice(c)),
        )
        .await
        .expect("eof reached")
        .expect("pump");
        assert_eq!(out, b"late\n");
    }

    #[test]
    fn kill_group_refuses_special_groups() {
        for pgid in [0, 1, u32::MAX] {
            let err = kill_group(pgid, Signal::Kill).expect_err("must refuse");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "pgid {pgid}");
            assert!(!group_alive(pgid), "pgid {pgid}");
        }
    }

    #[test]
    fn candidates_drop_denied_shells_and_keep_order() {
        let got = shell_candidates(Some("/usr/bin/fish".into()), true);
        assert_eq!(
            got,
            [
                PathBuf::from("/bin/zsh"),
                PathBuf::from("/bin/bash"),
                PathBuf::from("/bin/sh")
            ]
        );
        let got = shell_candidates(Some("/opt/homebrew/bin/NU".into()), false);
        assert_eq!(got, [PathBuf::from("/bin/bash"), PathBuf::from("/bin/sh")]);
        let got = shell_candidates(Some("/usr/local/bin/zsh".into()), false);
        assert_eq!(got[0], PathBuf::from("/usr/local/bin/zsh"));
        assert_eq!(got.len(), 3);
        assert_eq!(shell_candidates(None, false).len(), 2);
    }

    #[test]
    fn pick_takes_first_existing_candidate_else_bin_sh() {
        let candidates = vec![
            PathBuf::from("/nope/zsh"),
            PathBuf::from("/yes/bash"),
            PathBuf::from("/yes/sh"),
        ];
        let picked = pick_shell(candidates.clone(), |p| p.starts_with("/yes"));
        assert_eq!(picked, PathBuf::from("/yes/bash"));
        assert_eq!(pick_shell(candidates, |_| false), PathBuf::from("/bin/sh"));
    }
}
