//! The home-prefix contract for model-visible paths.
//!
//! Injected text renders the user's home directory as `~`; every path boundary
//! a model can write to expands it back. The two halves live together so a
//! redacted path can never become one the model cannot resolve.
//!
//! Port of CodeWhale's `sanitize_prompt_path_text` (workspace → `.`, home →
//! `~`), narrowed to the home half. There is deliberately **no** session
//! directory → `.` rule: a `./`-relative path resolves against the shell's
//! current directory, which is wrong for repo-root files seen from a
//! subdirectory session — accuracy beats prettiness.
//!
//! One implementation for every injected render site: `ox-engine`'s
//! instruction-file header and env block, and `ox-tools`' skill envelope.
//! Tool output, file contents and tool metadata stay absolute: only the
//! injected framing is redacted.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Render `path` with a leading `home` replaced by `~`, e.g. `/Users/me` →
/// `~`, `/Users/me/proj/AGENTS.md` → `~/proj/AGENTS.md`.
///
/// `home` must be the user's home directory (the one [`expand_tilde`] expands
/// to), not the ox home: the global instruction file lives in `<home>/.ox`,
/// and redacting against `<home>/.ox` would turn its path into `~/AGENTS.md`,
/// which expands to the wrong file.
///
/// A path outside `home` (or a relative one) is returned borrowed and
/// byte-identical — the common case allocates nothing.
#[must_use]
pub fn redact_home<'a>(path: &'a Path, home: &Path) -> Cow<'a, str> {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => Cow::Borrowed("~"),
        Ok(rest) => Cow::Owned(format!("~/{}", rest.to_string_lossy())),
        Err(_) => path.to_string_lossy(),
    }
}

/// Expand a leading `~` in a model-supplied path: `~` → `home`, `~/rest` →
/// `home/rest`.
///
/// `None` means "not a tilde form" — `~user` is deliberately not expanded
/// (user lookup is not supported, and guessing would read the wrong home),
/// so the caller keeps its normal resolution and the path simply fails to
/// resolve instead of silently naming another user's directory.
///
/// `~/..`-style escapes resolve like the shell would (`~/..` is the parent of
/// home); this is plain path arithmetic, not a sandbox.
#[must_use]
pub fn expand_tilde(path: &str, home: &Path) -> Option<PathBuf> {
    match path {
        "~" => Some(home.to_path_buf()),
        _ => path
            .strip_prefix("~/")
            .map(|rest| home.join(rest.trim_start_matches('/'))),
    }
}
