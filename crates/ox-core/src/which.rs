//! Minimal `PATH` lookup shared by the CBM gateway (`cbm`) and the shell
//! session helpers.
//!
//! Callers need the same rule: resolve a bare binary name to an
//! executable *file*, because a directory or a non-executable file would make
//! the later spawn fail. Hits are absolutized with [`std::path::absolute`]
//! (never `canonicalize` — symlinks are left alone) so that callers which
//! chdir before spawning still execute the binary `PATH` pointed at.
//!
//! On Windows the `PATHEXT` suffix probing lives here; on Unix the executable
//! bit (`0o111`) decides.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Resolve `name` on the process `PATH`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in(&std::env::var_os("PATH")?, name)
}

/// Resolve `name` on an explicit `PATH` string. Empty entries mean the
/// current directory (POSIX), which [`std::env::split_paths`] preserves.
pub(crate) fn find_in(paths: &OsStr, name: &str) -> Option<PathBuf> {
    let extensions = executable_extensions();
    for directory in std::env::split_paths(paths) {
        for extension in &extensions {
            let candidate = directory.join(format!("{name}{extension}"));
            if is_executable(&candidate) {
                return Some(absolutize(candidate));
            }
        }
    }
    None
}

/// The suffixes a bare name may carry: one empty suffix on Unix, `PATHEXT`
/// entries (falling back to the default Windows list when unset).
fn executable_extensions() -> Vec<String> {
    if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| extension.to_owned())
            .collect()
    } else {
        vec![String::new()]
    }
}

/// Whether `path` is a regular file this process may execute.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// On non-Unix hosts the extension probing is the gate (an existing file is
/// a hit).
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Anchor a relative hit to the process directory without resolving links.
fn absolutize(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}
