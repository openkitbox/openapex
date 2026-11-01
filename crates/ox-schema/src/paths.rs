//! Filesystem locations shared by the server and its clients.
//!
//! Everything lives under `<home>/.ox/run`. The home lookup matches
//! `ox_core::config::ox_home()` (`dirs::home_dir()`, falling back to `.`);
//! a test in ox-core pins the two together.
//!
//! A unix socket path must fit `sun_path`: on macOS the whole path has to be
//! shorter than 104 bytes. That is not checked at runtime here.

use std::path::PathBuf;

fn ox_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ox")
}

/// Directory holding the socket and the start-up lock (`~/.ox/run`).
pub fn run_dir() -> PathBuf {
    ox_home().join("run")
}

/// The server's unix domain socket (`~/.ox/run/ox.sock`).
pub fn socket_path() -> PathBuf {
    run_dir().join("ox.sock")
}

/// The lock file serialising server start-up (`~/.ox/run/ox.lock`).
pub fn lock_path() -> PathBuf {
    run_dir().join("ox.lock")
}

/// Where a detached server writes its log (`~/.ox/ox.log`).
pub fn server_log_path() -> PathBuf {
    ox_home().join("ox.log")
}
