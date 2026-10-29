//! Request-side prompt file input -> stored [`FileAttachment`].
//!
//! `session.prompt` receives upstream's `PromptInput.FileAttachment`
//! (`{uri, name?, description?, mention?}`); this module is the only owner of
//! turning that into the stored `{data, mime, source, …}` shape (upstream
//! `SessionPrompt.materializeAttachment`, `core/src/session/prompt.ts`).
//!
//! Deliberate deviations from upstream:
//!
//! 1. A `data:` URL must be base64. Non-base64 data URLs are rejected (the
//!    TUI only sends base64), which drops upstream's percent-decoding path.
//!    Validation is a charset + padding scan; upstream's canonical
//!    re-encode comparison (which also catches non-zero trailing bits) is not
//!    done, so the whole payload is never decoded.
//! 2. A data URL's mime is the magic-byte sniff of its first bytes, else the
//!    type declared in the URL header, else `application/octet-stream`.
//!    Upstream decodes the whole payload and text-sniffs it.
//! 3. Images are not resized or normalized (upstream `Image.normalize`);
//!    only the 20 MiB limit applies.
//! 4. `media:` references are unsupported (ox has no blob store).
//! 5. Same-mention image de-duplication (`userAttachmentContent`) is not done.
//! 6. Magic-byte sniffing covers PNG/JPEG/GIF/WebP/PDF only
//!    ([`ox_tools::read::sniff_media`]); BMP/AVIF/HEIC/MP4 files fall through
//!    to the text check.
//!
//! 7. Bounds upstream lacks: a `file:` read is capped at the per-file limit
//!    while reading (not just by its metadata), and one `session.prompt`
//!    call may materialize at most [`MAX_TOTAL_ATTACHMENT_BYTES`] decoded
//!    bytes across all its inputs.
//! 8. A declared data-URL `image/*`, `application/pdf` or
//!    `application/x-directory` type is ignored when the magic bytes do not
//!    confirm it (the type drives how the model content is rendered).
//!
//! Payload ownership: the data-URL path borrows the payload for validation and
//! copies it once into an `Arc<str>`; the `file:` path encodes the bytes once.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ox_schema::support::{FileAttachment, FileInput, FileSource};

/// Largest decoded attachment (upstream `MAX_ATTACHMENT_BYTES`).
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Largest decoded total across the inputs of one `materialize` call.
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;

/// Longest uri echoed into an error message or log line.
const MAX_ECHO_CHARS: usize = 200;

/// Base64 characters decoded to sniff a data URL's magic bytes (24 bytes).
const SNIFF_BASE64_CHARS: usize = 32;

/// A prompt file input that could not be materialized. `message` is the
/// client-facing text; `label` names the attachment (bounded, never
/// data-URL bytes).
#[derive(Debug)]
pub(crate) struct AttachmentError {
    pub label: String,
    pub message: String,
}

impl AttachmentError {
    fn new(label: &str, message: String) -> Self {
        Self {
            label: clip(label).into_owned(),
            message,
        }
    }
}

/// `text` bounded to [`MAX_ECHO_CHARS`] characters (cut on a char boundary,
/// ellipsis appended) so an oversized uri is never echoed back or logged.
fn clip(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(MAX_ECHO_CHARS) {
        Some((cut, _)) => Cow::Owned(format!("{}…", &text[..cut])),
        None => Cow::Borrowed(text),
    }
}

/// Add `bytes` to the running decoded total, failing past
/// [`MAX_TOTAL_ATTACHMENT_BYTES`].
fn charge(used: &mut usize, bytes: usize, label: &str) -> Result<(), AttachmentError> {
    match used
        .checked_add(bytes)
        .filter(|total| *total <= MAX_TOTAL_ATTACHMENT_BYTES)
    {
        Some(total) => {
            *used = total;
            Ok(())
        }
        None => Err(AttachmentError::new(
            label,
            format!(
                "Attachments exceed the {MAX_TOTAL_ATTACHMENT_BYTES} byte total limit: {label}"
            ),
        )),
    }
}

/// Materialize every input, stopping at the first failure. Blocking file I/O:
/// call from `spawn_blocking`.
pub(crate) fn materialize(inputs: Vec<FileInput>) -> Result<Vec<FileAttachment>, AttachmentError> {
    let mut used = 0;
    inputs
        .into_iter()
        .map(|input| materialize_one(input, &mut used))
        .collect()
}

fn materialize_one(input: FileInput, used: &mut usize) -> Result<FileAttachment, AttachmentError> {
    if input.uri.starts_with("data:") {
        from_data_url(input, used)
    } else {
        from_file_uri(input, used)
    }
}

// ---------------------------------------------------------------------------
// data: URLs
// ---------------------------------------------------------------------------

/// Name an attachment in errors without echoing a data URL's bytes
/// (upstream `attachmentLabel`).
fn label(input: &FileInput) -> &str {
    match input.name.as_deref() {
        Some(name) => name,
        None if input.uri.starts_with("data:") => "inline attachment",
        None => &input.uri,
    }
}

fn from_data_url(input: FileInput, used: &mut usize) -> Result<FileAttachment, AttachmentError> {
    let label = label(&input);
    let invalid = || AttachmentError::new(label, format!("Invalid attachment data URL: {label}"));
    let (declared, payload) = parse_data_url(&input.uri).ok_or_else(invalid)?;
    let size = decoded_len(payload).ok_or_else(invalid)?;
    if size > MAX_ATTACHMENT_BYTES {
        return Err(too_large(label));
    }
    charge(used, size, label)?;
    let prefix_len = payload.len().min(SNIFF_BASE64_CHARS);
    let prefix = STANDARD.decode(&payload[..prefix_len]).unwrap_or_default();
    let mime = detect_mime(&prefix, Some(declared));
    let data = Arc::<str>::from(payload);
    Ok(FileAttachment {
        data,
        mime,
        source: FileSource::Inline,
        name: input.name,
        description: input.description,
        mention: input.mention,
    })
}

fn too_large(label: &str) -> AttachmentError {
    AttachmentError::new(
        label,
        format!("Attachment exceeds the {MAX_ATTACHMENT_BYTES} byte limit: {label}"),
    )
}

/// Split `data:<metadata>,<payload>` into the declared mime (possibly empty)
/// and the payload. Only base64 data URLs are accepted.
fn parse_data_url(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("data:")?;
    let (metadata, payload) = rest.split_once(',')?;
    let mut parts = metadata.split(';');
    let declared = parts.next().unwrap_or_default();
    if !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some((declared, payload))
}

/// Decoded size of a standard-alphabet base64 payload, `None` when it is
/// malformed: length a multiple of 4, only `[A-Za-z0-9+/]`, and at most two
/// trailing `=`. A linear scan; nothing is decoded or allocated.
fn decoded_len(payload: &str) -> Option<usize> {
    let bytes = payload.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if padding > 2 {
        return None;
    }
    bytes[..bytes.len() - padding]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        .then(|| bytes.len() / 4 * 3 - padding)
}

/// Magic-byte sniff of `prefix`, else a usable declared type, else
/// `application/octet-stream`. Declared types that select special rendering
/// (`image/*`, PDF, directory) are only trusted through the sniff.
fn detect_mime(prefix: &[u8], declared: Option<&str>) -> String {
    if let Some(mime) = ox_tools::read::sniff_media(prefix) {
        return mime.to_owned();
    }
    match declared {
        Some(mime)
            if !mime.is_empty()
                && mime != "application/octet-stream"
                && mime != "application/pdf"
                && mime != "application/x-directory"
                && !mime.to_ascii_lowercase().starts_with("image/") =>
        {
            mime.to_owned()
        }
        _ => "application/octet-stream".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// file: URIs
// ---------------------------------------------------------------------------

fn from_file_uri(input: FileInput, used: &mut usize) -> Result<FileAttachment, AttachmentError> {
    let uri = input.uri.as_str();
    let shown = clip(uri);
    let named = clip(label(&input));
    let err = |message: &str| AttachmentError::new(uri, format!("{message}: {shown}"));
    let mut url = url::Url::parse(uri).map_err(|_| err("Invalid attachment URI"))?;
    if url.scheme() != "file" {
        return Err(err("Unsupported attachment URI"));
    }
    let start = query_positive_int(&url, "start");
    let end = query_positive_int(&url, "end");
    url.set_query(None);
    url.set_fragment(None);
    let target = url.to_file_path().map_err(|()| err("Invalid file URI"))?;
    let unreadable = || err("Unable to read attachment");
    let metadata = fs::metadata(&target).map_err(|_| unreadable())?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());

    let (bytes, mime) = if metadata.is_dir() {
        let listing = list_directory(&target).map_err(|_| unreadable())?;
        if listing.len() > MAX_ATTACHMENT_BYTES {
            return Err(too_large(&named));
        }
        charge(used, listing.len(), &named)?;
        (listing.into_bytes(), "application/x-directory".to_owned())
    } else if !metadata.is_file() {
        return Err(err("Attachment is not a file"));
    } else if metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err(too_large(&named));
    } else {
        // Bounded read: the file may have grown (or lied about its size)
        // since the metadata check.
        let mut bytes = Vec::new();
        File::open(&target)
            .and_then(|file| {
                file.take(MAX_ATTACHMENT_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| unreadable())?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(too_large(&named));
        }
        charge(used, bytes.len(), &named)?;
        let mime = match ox_tools::read::sniff_media(&bytes) {
            Some(mime) => mime,
            None if is_text(&bytes) => "text/plain",
            None => "application/octet-stream",
        };
        (bytes, mime.to_owned())
    };

    let content = match (mime.as_str(), start) {
        ("text/plain", Some(start)) => line_range(&bytes, start, end),
        _ => &bytes,
    };
    let encoded = STANDARD.encode(content);
    drop(bytes);
    Ok(FileAttachment {
        data: Arc::<str>::from(encoded),
        mime,
        source: FileSource::Uri {
            uri: input.uri.clone(),
        },
        name: input.name.or(name),
        description: input.description,
        mention: input.mention,
    })
}

/// First `key` query parameter as a positive integer (upstream `positiveInt`).
fn query_positive_int(url: &url::Url, key: &str) -> Option<usize> {
    let (_, value) = url.query_pairs().find(|(name, _)| name == key)?;
    value.parse::<usize>().ok().filter(|&n| n > 0)
}

/// Directory entries (files and directories only), directories first, each
/// group sorted by name, directories suffixed with `/`, joined by `\n`.
fn list_directory(dir: &Path) -> std::io::Result<String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_file() {
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                kind.is_dir(),
            ));
        }
    }
    Ok(render_listing(entries))
}

fn render_listing(mut entries: Vec<(String, bool)>) -> String {
    entries.sort_by(|(a_name, a_dir), (b_name, b_dir)| {
        b_dir.cmp(a_dir).then_with(|| a_name.cmp(b_name))
    });
    let mut out = String::new();
    for (index, (name, is_dir)) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(name);
        if *is_dir {
            out.push('/');
        }
    }
    out
}

/// Upstream `Mime.isText`: empty, or no NUL byte, valid UTF-8 and at most 30%
/// control bytes (`< 9` or `14..32`).
fn is_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return false;
    }
    let controls = bytes
        .iter()
        .filter(|&&b| b < 9 || (b > 13 && b < 32))
        .count();
    controls as f64 / bytes.len() as f64 <= 0.3
}

/// Lines `start..=end` (1-based; `end` defaults to the last line) of valid
/// UTF-8 text, as a subslice of `bytes` joined by their original `\n`
/// separators (upstream `split("\n").slice(start - 1, end).join("\n")`).
fn line_range(bytes: &[u8], start: usize, end: Option<usize>) -> &[u8] {
    let mut offset = 0;
    let mut begin = None;
    let mut stop = bytes.len();
    for (index, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let number = index + 1;
        if number == start {
            begin = Some(offset);
        }
        if end == Some(number) {
            stop = offset + line.len();
            break;
        }
        offset += line.len() + 1;
    }
    match begin {
        Some(begin) if begin <= stop => &bytes[begin..stop],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_url_requires_base64_metadata() {
        assert_eq!(
            parse_data_url("data:image/png;base64,QUJD"),
            Some(("image/png", "QUJD"))
        );
        assert_eq!(
            parse_data_url("data:;BASE64,QUJD"),
            Some(("", "QUJD")),
            "base64 flag is case-insensitive and the mime may be empty"
        );
        assert_eq!(
            parse_data_url("data:text/plain;charset=utf-8;base64,QUJD"),
            Some(("text/plain", "QUJD"))
        );
        assert!(parse_data_url("data:text/plain,hello").is_none());
        assert!(parse_data_url("data:image/png;base64").is_none());
        assert!(parse_data_url("https://example.com/a,b;base64").is_none());
    }

    #[test]
    fn decoded_len_validates_and_sizes() {
        assert_eq!(decoded_len(""), Some(0));
        assert_eq!(decoded_len("QUJD"), Some(3));
        assert_eq!(decoded_len("QUI="), Some(2));
        assert_eq!(decoded_len("QQ=="), Some(1));
        assert_eq!(decoded_len("a+/9"), Some(3));
        assert_eq!(decoded_len("QUJ"), None, "length must be a multiple of 4");
        assert_eq!(decoded_len("Q==="), None, "at most two padding characters");
        assert_eq!(decoded_len("===="), None);
        assert_eq!(decoded_len("="), None);
        assert_eq!(decoded_len("QU=J"), None, "padding only at the end");
        assert_eq!(
            decoded_len("QU-_"),
            None,
            "url-safe alphabet is not accepted"
        );
        assert_eq!(decoded_len("QU J"), None, "whitespace is not accepted");
        assert_eq!(decoded_len("QU\u{e9}"), None, "4 bytes, non-ASCII");
        let limit_chars = MAX_ATTACHMENT_BYTES / 3 * 4;
        assert_eq!(
            decoded_len(&"A".repeat(limit_chars)),
            Some(MAX_ATTACHMENT_BYTES - MAX_ATTACHMENT_BYTES % 3)
        );
        assert!(decoded_len(&"A".repeat(limit_chars + 4)) > Some(MAX_ATTACHMENT_BYTES));
    }

    #[test]
    fn detect_mime_prefers_magic_then_declared_then_octet_stream() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(
            detect_mime(png, Some("application/octet-stream")),
            "image/png"
        );
        assert_eq!(detect_mime(png, Some("image/jpeg")), "image/png");
        assert_eq!(detect_mime(b"hello", Some("text/plain")), "text/plain");
        assert_eq!(detect_mime(b"hello", Some("")), "application/octet-stream");
        assert_eq!(
            detect_mime(b"hello", Some("application/octet-stream")),
            "application/octet-stream"
        );
        assert_eq!(detect_mime(b"hello", None), "application/octet-stream");
    }

    #[test]
    fn detect_mime_ignores_unconfirmed_special_declared_types() {
        for declared in [
            "image/png",
            "IMAGE/JPEG",
            "image/svg+xml",
            "application/pdf",
            "application/x-directory",
        ] {
            assert_eq!(
                detect_mime(b"not magic", Some(declared)),
                "application/octet-stream",
                "{declared}"
            );
        }
    }

    #[test]
    fn charge_accumulates_and_rejects_past_the_total() {
        let mut used = 0;
        charge(&mut used, MAX_TOTAL_ATTACHMENT_BYTES - 1, "a").expect("fits");
        charge(&mut used, 1, "b").expect("exactly the limit");
        assert_eq!(used, MAX_TOTAL_ATTACHMENT_BYTES);
        let error = charge(&mut used, 1, "c.png").expect_err("over the total");
        assert_eq!(error.label, "c.png");
        assert!(
            error.message.contains("33554432 byte total limit"),
            "{}",
            error.message
        );
        assert!(error.message.contains("c.png"), "{}", error.message);
        assert_eq!(
            used, MAX_TOTAL_ATTACHMENT_BYTES,
            "a failed charge adds nothing"
        );
        assert!(charge(&mut 0, usize::MAX, "x").is_err(), "no overflow");
    }

    #[test]
    fn clip_bounds_echoed_text_on_a_char_boundary() {
        assert_eq!(clip("short"), "short");
        let long = "é".repeat(MAX_ECHO_CHARS + 50);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_ECHO_CHARS + 1);
        assert!(clipped.ends_with('…'));
        let exact = "a".repeat(MAX_ECHO_CHARS);
        assert_eq!(clip(&exact), exact.as_str());
    }

    #[test]
    fn label_never_echoes_data_url_bytes() {
        let data = FileInput {
            uri: "data:image/png;base64,QUJD".to_owned(),
            name: None,
            description: None,
            mention: None,
        };
        assert_eq!(label(&data), "inline attachment");
        let named = FileInput {
            name: Some("a.png".to_owned()),
            ..data
        };
        assert_eq!(label(&named), "a.png");
    }

    #[test]
    fn listing_puts_directories_first_and_suffixes_them() {
        let entries = vec![
            ("b.txt".to_owned(), false),
            ("zdir".to_owned(), true),
            ("a.txt".to_owned(), false),
            ("adir".to_owned(), true),
        ];
        assert_eq!(render_listing(entries), "adir/\nzdir/\na.txt\nb.txt");
        assert_eq!(render_listing(Vec::new()), "");
    }

    #[test]
    fn is_text_follows_upstream_rules() {
        assert!(is_text(b""));
        assert!(is_text("héllo\nworld\t".as_bytes()));
        assert!(!is_text(b"a\0b"));
        assert!(!is_text(&[0xff, 0xfe, 0x41]));
        assert!(!is_text(&[1, 2, 3, b'a']), "control ratio above 30%");
    }

    #[test]
    fn line_range_slices_the_original_bytes() {
        let text = b"one\ntwo\nthree\nfour\n";
        assert_eq!(line_range(text, 2, Some(3)), b"two\nthree");
        assert_eq!(line_range(text, 1, Some(1)), b"one");
        assert_eq!(line_range(text, 3, None), b"three\nfour\n");
        assert_eq!(line_range(text, 4, Some(99)), b"four\n");
        assert_eq!(line_range(text, 3, Some(2)), b"");
        assert_eq!(line_range(text, 99, None), b"");
        assert_eq!(line_range(b"solo", 1, None), b"solo");
    }
}
