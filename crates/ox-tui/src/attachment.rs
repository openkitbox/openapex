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
