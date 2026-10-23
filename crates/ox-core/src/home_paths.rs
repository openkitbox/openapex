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
pub fn expand_tilde(path: &str, home: &Path) -> Option<PathBuf> {
    match path {
        "~" => Some(home.to_path_buf()),
        _ => path
            .strip_prefix("~/")
            .map(|rest| home.join(rest.trim_start_matches('/'))),
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    fn home() -> &'static Path {
        Path::new("/Users/john")
    }

    #[test]
    fn a_path_under_home_is_tilde_prefixed() {
        assert_eq!(
            redact_home(Path::new("/Users/john/.ox/AGENTS.md"), home()),
            "~/.ox/AGENTS.md"
        );
        assert_eq!(
            redact_home(Path::new("/Users/john/Developer/ox/AGENTS.md"), home()),
            "~/Developer/ox/AGENTS.md"
        );
    }

    #[test]
    fn the_home_directory_itself_is_a_bare_tilde() {
        assert_eq!(redact_home(home(), home()), "~");
    }

    #[test]
    fn a_path_outside_home_is_borrowed_and_unchanged() {
        let outside = Path::new("/var/tmp/ox/AGENTS.md");
        let redacted = redact_home(outside, home());
        assert!(matches!(redacted, Cow::Borrowed(_)), "{redacted:?}");
        assert_eq!(redacted, "/var/tmp/ox/AGENTS.md");
    }

    #[test]
    fn a_relative_path_is_borrowed_and_unchanged() {
        let relative = Path::new(".ox/skills/review");
        let redacted = redact_home(relative, home());
        assert!(matches!(redacted, Cow::Borrowed(_)), "{redacted:?}");
        assert_eq!(redacted, ".ox/skills/review");
    }

    #[test]
    fn a_sibling_sharing_the_home_prefix_is_not_redacted() {
        // `strip_prefix` is component-wise: `/Users/johnny` is not under
        // `/Users/john`, and a string-prefix bug here would corrupt paths.
        let redacted = redact_home(Path::new("/Users/johnny/AGENTS.md"), home());
        assert!(matches!(redacted, Cow::Borrowed(_)), "{redacted:?}");
        assert_eq!(redacted, "/Users/johnny/AGENTS.md");
    }

    #[test]
    fn a_bare_tilde_expands_to_home() {
        assert_eq!(expand_tilde("~", home()), Some(home().to_path_buf()));
    }

    #[test]
    fn a_tilde_slash_path_expands_under_home() {
        assert_eq!(
            expand_tilde("~/.ox/AGENTS.md", home()),
            Some(PathBuf::from("/Users/john/.ox/AGENTS.md"))
        );
        // `~//x` is still home-relative, never an absolute escape.
        assert_eq!(
            expand_tilde("~//x", home()),
            Some(PathBuf::from("/Users/john/x"))
        );
    }

    #[test]
    fn other_tilde_forms_are_not_expanded() {
        // `~user` needs a user lookup ox does not do; expanding it to the
        // current user's home would name the wrong directory.
        assert_eq!(expand_tilde("~john/x", home()), None);
        assert_eq!(expand_tilde("~john", home()), None);
    }

    #[test]
    fn plain_paths_are_not_tilde_forms() {
        assert_eq!(expand_tilde("src/main.rs", home()), None);
        assert_eq!(expand_tilde("/etc/hosts", home()), None);
        assert_eq!(expand_tilde("", home()), None);
    }
}
