//! Reading pasted files from disk. This is the only module that touches the
//! local filesystem for attachments; what counts as an attachable path is
//! decided by [`crate::ui::paste`].
//!
//! The calls block on file I/O, so the executor runs [`load`] under
//! `spawn_blocking`. Port of upstream `local-attachment.ts`
//! (`resolvePastedAttachments`, `readFileBounded`), with one deviation: the
//! `FileInput` carries no `source` URI. Upstream adds one when the server is
//! local; ox sends the captured bytes alone.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ox_schema::support::FileInput;

use crate::ui::paste::mime_of_ext;

/// Total size of the files one paste may attach (upstream value).
pub const MAX_LOCAL_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// One pasted file, read.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    /// An SVG: sent as text, so it joins the prompt like a folded paste.
    Svg { name: String, content: String },
    /// An image or PDF as a `data:` URI. `input.name` is the file name.
    File { name: String, input: FileInput },
}

/// Read every path, or `None` when any of them is missing, unreadable, not a
/// regular file, not an image/PDF, or the files together exceed
/// [`MAX_LOCAL_ATTACHMENT_BYTES`]. All or nothing: the caller then pastes the
/// text as typed.
pub fn load(paths: &[PathBuf]) -> Option<Vec<Loaded>> {
    load_with_budget(paths, MAX_LOCAL_ATTACHMENT_BYTES)
}

/// [`load`] with the byte budget as a parameter. Each file spends what it
/// weighs from the budget, so later files see what is left.
fn load_with_budget(paths: &[PathBuf], mut remaining: u64) -> Option<Vec<Loaded>> {
    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let mime = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(mime_of_ext)?;
        let bytes = read_bounded(path, remaining)?;
        remaining -= bytes.len() as u64;
        let name = path
            .file_name()
            .map_or_else(|| "image".to_owned(), |n| n.to_string_lossy().into_owned());
        loaded.push(if mime == "image/svg+xml" {
            let content = String::from_utf8(bytes).ok().filter(|c| !c.is_empty())?;
            Loaded::Svg { name, content }
        } else {
            let input = FileInput {
                uri: data_uri(mime, &bytes),
                source: None,
                name: Some(name.clone()),
                description: None,
                mention: None,
            };
            Loaded::File { name, input }
        });
    }
    Some(loaded)
}

/// The whole file, `None` if it is not a regular file or holds more than
/// `max` bytes. The type and size are checked on the path first (following
/// symlinks), because opening a FIFO or device would block this thread; the
/// size is checked again on what was read, since the file can grow in between.
fn read_bounded(path: &Path, max: u64) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).ok()?);
    file.take(max + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

/// `data:<mime>;base64,<payload>`, built in one allocation.
pub(crate) fn data_uri(mime: &str, bytes: &[u8]) -> String {
    let prefix = format!("data:{mime};base64,");
    let mut uri = String::with_capacity(prefix.len() + bytes.len().div_ceil(3) * 4);
    uri.push_str(&prefix);
    STANDARD.encode_string(bytes, &mut uri);
    uri
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::{Loaded, load, load_with_budget};

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_png_becomes_a_data_uri_that_decodes_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = [0x89, b'P', b'N', b'G', 0, 1, 2, 255];
        let path = write(&dir, "shot.png", &bytes);
        let loaded = load(&[path]).expect("a small png loads");
        let [Loaded::File { name, input }] = loaded.as_slice() else {
            panic!("one file expected: {loaded:?}");
        };
        assert_eq!(name, "shot.png");
        assert_eq!(input.name.as_deref(), Some("shot.png"));
        let payload = input
            .uri
            .strip_prefix("data:image/png;base64,")
            .expect("data URI with the png mime");
        assert_eq!(STANDARD.decode(payload).unwrap(), bytes);
        assert!(input.source.is_none() && input.mention.is_none());
    }

    #[test]
    fn a_pdf_gets_the_pdf_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "A.PDF", b"%PDF-1.4");
        let Some(loaded) = load(&[path]) else {
            panic!("a small pdf loads");
        };
        assert!(matches!(
            loaded.as_slice(),
            [Loaded::File { input, .. }] if input.uri.starts_with("data:application/pdf;base64,")
        ));
    }

    #[test]
    fn an_svg_is_read_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "logo.svg", "<svg>中</svg>".as_bytes());
        assert_eq!(
            load(&[path]),
            Some(vec![Loaded::Svg {
                name: "logo.svg".into(),
                content: "<svg>中</svg>".into()
            }])
        );
    }

    #[test]
    fn an_invalid_or_empty_svg_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&[write(&dir, "bad.svg", &[0xff, 0xfe])]), None);
        assert_eq!(load(&[write(&dir, "empty.svg", b"")]), None);
    }

    #[test]
    fn missing_unattachable_and_directory_paths_fail_the_load() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&[dir.path().join("nope.png")]), None);
        assert_eq!(load(&[write(&dir, "notes.txt", b"hi")]), None);
        std::fs::create_dir(dir.path().join("folder.png")).unwrap();
        assert_eq!(load(&[dir.path().join("folder.png")]), None);
    }

    /// A FIFO named `x.png` has no writer, so opening it blocks forever. The
    /// load must refuse it without opening, within a bounded wait.
    #[cfg(unix)]
    #[test]
    fn a_fifo_fails_the_load_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("x.png");
        match std::process::Command::new("mkfifo").arg(&fifo).status() {
            Ok(status) if status.success() => {}
            other => {
                eprintln!("skipped: mkfifo unavailable ({other:?})");
                return;
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load(&[fifo]));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("load returned instead of blocking on the FIFO");
        assert_eq!(result, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_loads_and_one_to_a_directory_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let target = write(&dir, "real.bin", b"png");
        let link = dir.path().join("link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let loaded = load(std::slice::from_ref(&link)).expect("a symlinked file loads");
        assert!(matches!(
            loaded.as_slice(),
            [Loaded::File { name, input }]
                if name == "link.png" && input.uri.starts_with("data:image/png;base64,")
        ));
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let dir_link = dir.path().join("dir.png");
        std::os::unix::fs::symlink(&folder, &dir_link).unwrap();
        assert_eq!(load(&[dir_link]), None);
    }

    #[test]
    fn one_bad_file_fails_the_whole_paste() {
        let dir = tempfile::tempdir().unwrap();
        let good = write(&dir, "a.png", b"png");
        assert!(load(std::slice::from_ref(&good)).is_some());
        assert_eq!(load(&[good, dir.path().join("gone.png")]), None);
    }

    #[test]
    fn a_file_over_the_budget_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "big.png", &[7; 21]);
        assert_eq!(load_with_budget(std::slice::from_ref(&path), 20), None);
        assert!(load_with_budget(std::slice::from_ref(&path), 21).is_some());
    }

    #[test]
    fn the_budget_is_spent_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            write(&dir, "a.png", &[1; 12]),
            write(&dir, "b.jpg", &[2; 12]),
        ];
        assert_eq!(load_with_budget(&paths, 20), None, "12 + 12 > 20");
        let loaded = load_with_budget(&paths, 30).expect("12 + 12 <= 30");
        assert_eq!(loaded.len(), 2);
    }
}
