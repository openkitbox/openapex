//! Pure decisions about pasted text. No I/O, no widgets: the input widgets
//! call these and own the result.

use std::path::{Path, PathBuf};

/// Text as it should land in an input: `\r\n` and lone `\r` become `\n`, and
/// ANSI escape sequences (colours copied from a terminal, cursor moves) are
/// removed. Everything else, including Unicode text, is kept as is.
#[must_use]
pub fn normalize(text: &str) -> String {
    clean(text, '\n')
}

/// [`normalize`] for single-line inputs: every line break becomes a space.
#[must_use]
pub fn single_line(text: &str) -> String {
    clean(text, ' ')
}

/// One pass over `text`; each line break (`\n`, `\r\n`, `\r`) is written as
/// `newline`.
fn clean(text: &str, newline: char) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push(newline);
            }
            '\n' => out.push(newline),
            '\x1b' => skip_escape(&mut chars),
            _ => out.push(ch),
        }
    }
    out
}

/// Consume one escape sequence whose `ESC` was just read: CSI (`[` … final
/// byte), OSC (`]` … BEL or `ESC \`), or a short form (intermediates, one
/// final byte). A dangling `ESC` at the end of input is dropped.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for ch in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    break;
                }
            }
        }
        Some(']') => {
            while let Some(ch) = chars.next() {
                match ch {
                    '\x07' => break,
                    '\x1b' => {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                    _ => {}
                }
            }
        }
        Some(' '..='/') => {
            // Intermediate bytes, then the final byte.
            while chars.next_if(|ch| matches!(ch, ' '..='/')).is_some() {}
            chars.next_if(|ch| matches!(ch, '0'..='~'));
        }
        // A two-byte sequence (`ESC M`): its final byte was just consumed.
        Some(_) | None => {}
    }
}

/// A paste of at least this many lines is folded into a placeholder.
pub const FOLD_MIN_LINES: usize = 3;
/// A paste of more than this many characters is folded into a placeholder.
pub const FOLD_OVER_CHARS: usize = 150;

/// A folded paste: `label` stands in for `text` inside the input until the
/// draft is submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pasted {
    pub label: String,
    pub text: String,
}

/// The base placeholder (`[Pasted ~N lines]`, no uniqueness suffix) for a
/// paste that is too long to show inline, `None` for one that stays as is.
/// Upstream (`pasteInputText`) judges the trimmed text: at least
/// [`FOLD_MIN_LINES`] lines, or more than [`FOLD_OVER_CHARS`] characters.
#[must_use]
pub fn fold_label(text: &str) -> Option<String> {
    let text = text.trim();
    let lines = text.matches('\n').count() + 1;
    if lines >= FOLD_MIN_LINES || text.chars().count() > FOLD_OVER_CHARS {
        Some(format!("[Pasted ~{lines} lines]"))
    } else {
        None
    }
}

/// `base`, or `base #n` (n from 2) when `taken` says a registered label
/// already uses it.
pub fn unique_label(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base} #{n}"))
        .find(|label| !taken(label))
        .expect("an unbounded range always yields a free label")
}

/// Whether `uri` is an inline image (`data:image/…`).
#[must_use]
pub fn is_image_uri(uri: &str) -> bool {
    uri.starts_with("data:image/")
}

/// The base label of an attached file, upstream `promptAttachmentLabel`:
/// `[Image n]` where `n` is one past the images already attached, `[PDF: name]`
/// for a PDF, `[File: name]` for anything else.
#[must_use]
pub fn attachment_label(uri: &str, name: &str, images_attached: usize) -> String {
    if is_image_uri(uri) {
        format!("[Image {}]", images_attached + 1)
    } else if uri.starts_with("data:application/pdf;") {
        format!("[PDF: {name}]")
    } else {
        format!("[File: {name}]")
    }
}

/// The base label of an SVG pasted as text.
#[must_use]
pub fn svg_label(name: &str) -> String {
    format!("[SVG: {name}]")
}

/// `text` with each registered label that still stands in it replaced by its
/// original content, built in one pass. A label is expanded at its first
/// occurrence only (a second copy typed by hand stays literal), and where
/// labels share a prefix (`[Pasted ~3 lines]`, `[Pasted ~3 lines] #2`) the
/// longest match wins. Entries whose label is gone from `text` are ignored.
#[must_use]
pub fn expand(text: &str, pasted: &[Pasted]) -> String {
    let mut used = vec![false; pasted.len()];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let hit = pasted
            .iter()
            .enumerate()
            .filter(|(i, p)| !used[*i] && rest.starts_with(p.label.as_str()))
            .max_by_key(|(_, p)| p.label.len());
        match hit {
            Some((i, p)) => {
                used[i] = true;
                out.push_str(&p.text);
                rest = &rest[p.label.len()..];
            }
            None => {
                out.push('[');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Which of `labels` still stand in `text`, as indices in the order they
/// appear. Each label counts once, at its first free occurrence, and where
/// labels share a prefix (`[PDF: a.pdf]`, `[PDF: a.pdf] #2`) the longest match
/// wins, as in [`expand`].
#[must_use]
pub fn labels_in_order(text: &str, labels: &[&str]) -> Vec<usize> {
    let mut used = vec![false; labels.len()];
    let mut order = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        rest = &rest[start..];
        let hit = labels
            .iter()
            .enumerate()
            .filter(|(i, label)| !used[*i] && rest.starts_with(**label))
            .max_by_key(|(_, label)| label.len());
        match hit {
            Some((i, label)) => {
                used[i] = true;
                order.push(i);
                rest = &rest[label.len()..];
            }
            None => rest = &rest[1..],
        }
    }
    order
}

/// The entry to expand when `content` is pasted again right after its label:
/// `before_cursor` ends with the label of an entry whose text is `content`
/// (trimmed). Upstream's keyboard path for unfolding a placeholder.
#[must_use]
pub fn expandable_before(before_cursor: &str, content: &str, pasted: &[Pasted]) -> Option<usize> {
    let content = content.trim();
    pasted
        .iter()
        .enumerate()
        .filter(|(_, p)| p.text == content && before_cursor.ends_with(p.label.as_str()))
        .max_by_key(|(_, p)| p.label.len())
        .map(|(i, _)| i)
}

/// Trim leading and trailing whitespace without copying the text.
pub fn trim_in_place(text: &mut String) {
    let end = text.trim_end().len();
    text.truncate(end);
    let lead = text.len() - text.trim_start().len();
    text.drain(..lead);
}

/// At most this many paths in one paste are treated as attachments.
pub const MAX_PASTED_FILEPATHS: usize = 32;

/// A paste longer than this many bytes is not a list of paths, whatever it
/// holds; it is refused before any normalizing work.
const MAX_PASTED_PATH_TEXT_BYTES: usize = MAX_PASTED_FILEPATHS * 4096;

/// Extension (case-insensitive) → MIME type for what a pasted path may
/// attach: the images and PDFs upstream's `mimeTypes` table lists.
#[must_use]
pub fn mime_of_ext(ext: &str) -> Option<&'static str> {
    const MIMES: [(&str, &str); 8] = [
        ("png", "image/png"),
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("gif", "image/gif"),
        ("webp", "image/webp"),
        ("avif", "image/avif"),
        ("pdf", "application/pdf"),
        ("svg", "image/svg+xml"),
    ];
    MIMES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(ext))
        .map(|(_, mime)| *mime)
}

/// The files a paste names, when the whole paste is nothing but such paths:
/// one path (quoted, backslash-escaped, `file://`, or plain with spaces) or
/// 2..=[`MAX_PASTED_FILEPATHS`] separated by whitespace, each with an
/// extension [`mime_of_ext`] knows. Empty for anything else, including a
/// URL, so the caller pastes the text as it is. Whether the files exist is
/// not decided here.
#[must_use]
pub fn pasted_attachment_paths(text: &str) -> Vec<PathBuf> {
    if text.len() > MAX_PASTED_PATH_TEXT_BYTES {
        return Vec::new();
    }
    let text = normalize(text);
    let text = text.trim();
    if text.is_empty() || text.starts_with("http://") || text.starts_with("https://") {
        return Vec::new();
    }
    let listed = parse_paths(text);
    if listed.is_empty() {
        return Vec::new();
    }
    if listed.iter().all(|path| attachable(path)) {
        return listed;
    }
    // An unquoted path with spaces in it parses as several words; upstream
    // then tries the whole paste as one path. Here that needs an absolute
    // path, so a sentence that happens to end in `.png` is not sent to disk.
    if text.contains('\n') {
        return Vec::new();
    }
    match normalize_path(text) {
        Some(path) if path.starts_with('/') && attachable(Path::new(&path)) => {
            vec![PathBuf::from(path)]
        }
        _ => Vec::new(),
    }
}

fn attachable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| mime_of_ext(ext).is_some())
}

/// One pasted path as a file path: surrounding quotes removed, a `file://`
/// URL decoded (`None` when it is not a local file), otherwise backslash
/// escapes (`\ `) undone. Port of upstream `normalizePastedFilepath`, unix
/// branch.
#[must_use]
pub fn normalize_path(value: &str) -> Option<String> {
    let raw = value.trim_matches(['\'', '"']);
    if raw.starts_with("file://") {
        return decode_file_url(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push(chars.next().unwrap_or(ch)),
            _ => out.push(ch),
        }
    }
    Some(out)
}

/// The local path of a `file://` URL: empty or `localhost` host only, no
/// encoded `/` or `\` in the path, percent escapes decoded. Port of
/// upstream `decodeFileURL`, unix branch.
fn decode_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let (host, path) = rest.split_at(rest.find('/')?);
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") {
        return None;
    }
    percent_decode(path)
}

/// `decodeURIComponent`: `None` on a malformed escape or invalid UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // `from_str_radix` would take a leading `+`; demand two hex digits.
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Split a paste into paths: whitespace separates, quotes group, a backslash
/// escapes the next character. Empty when there are more than
/// [`MAX_PASTED_FILEPATHS`], a quote is left open, or a `file://` item is not
/// a local file (upstream keeps such an item raw; here it fails closed). With
/// a `file://` in the text, `#` comment lines (as in a `text/uri-list`) are
/// dropped first. Port of upstream `parsePastedFilepaths`, unix branch.
#[must_use]
pub fn parse_paths(value: &str) -> Vec<PathBuf> {
    let uri_list;
    let input = if value.contains("file://") {
        uri_list = value
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        uri_list.as_str()
    } else {
        value
    };
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else if ch == '\\' && open == '"' {
                current.push(chars.next().unwrap_or(ch));
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => current.push(chars.next().unwrap_or(ch)),
            ch if ch.is_whitespace() => {
                if !flush_path(&mut current, &mut paths) || paths.len() > MAX_PASTED_FILEPATHS {
                    return Vec::new();
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some()
        || !flush_path(&mut current, &mut paths)
        || paths.len() > MAX_PASTED_FILEPATHS
    {
        return Vec::new();
    }
    paths
}

/// Move the finished word in `current` (if any) onto `paths`; `false` when
/// it is a `file://` URL that is not a local file.
fn flush_path(current: &mut String, paths: &mut Vec<PathBuf>) -> bool {
    if current.is_empty() {
        return true;
    }
    let word = std::mem::take(current);
    if word.starts_with("file://") {
        let Some(path) = decode_file_url(&word) else {
            return false;
        };
        paths.push(PathBuf::from(path));
    } else {
        paths.push(PathBuf::from(word));
    }
    true
}
