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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    /// Probe artifacts live under the gitignored `target/` so a read-only
    /// checkout still runs the suite and a killed test leaves no repo debris.
    fn probe_dir() -> TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("which-tests");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("probe-")
            .tempdir_in(&root)
            .unwrap()
    }

    fn write_probe(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The probe's path relative to the test process working directory.
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        path.strip_prefix(&cwd)
            .unwrap_or_else(|_| panic!("{} is not under {}", path.display(), cwd.display()))
            .to_path_buf()
    }

    #[test]
    fn finds_executable_file() {
        let directory = probe_dir();
        let program = directory.path().join("ox-which-probe");
        write_probe(&program, 0o755);
        let found = find_in(
            &std::env::join_paths([directory.path()]).unwrap(),
            "ox-which-probe",
        )
        .unwrap();
        assert_eq!(found, program);
        assert!(found.is_absolute());
    }

    #[test]
    fn rejects_non_executable_file() {
        let directory = probe_dir();
        write_probe(&directory.path().join("ox-which-plain"), 0o644);
        assert!(
            find_in(
                &std::env::join_paths([directory.path()]).unwrap(),
                "ox-which-plain"
            )
            .is_none(),
            "a non-executable file must not count as a binary"
        );
    }

    #[test]
    fn finds_relative_entry_against_cwd() {
        let directory = probe_dir();
        let program = directory.path().join("rel-probe");
        write_probe(&program, 0o755);
        let relative = relative_to_cwd(directory.path());
        let found = find_in(relative.as_os_str(), "rel-probe").unwrap();
        assert_eq!(found, program);
    }

    #[test]
    fn finds_empty_entry_in_cwd() {
        let directory = probe_dir();
        let program = directory.path().join("empty-probe");
        write_probe(&program, 0o755);
        // A single empty entry means the current directory; the name carries
        // the relative subpath so the probe itself stays under `target/`.
        let name = relative_to_cwd(&program);
        let found = find_in(OsStr::new(""), &name.to_string_lossy()).unwrap();
        assert_eq!(found, program);
    }
}
