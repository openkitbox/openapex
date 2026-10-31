//! Pure decisions about pasted text. No I/O, no widgets: the input widgets
//! call these and own the result.

use std::path::{Path, PathBuf};

/// Text as it should land in an input: `\r\n` and lone `\r` become `\n`, and
/// ANSI escape sequences (colours copied from a terminal, cursor moves) are
/// removed. Everything else, including Unicode text, is kept as is.
pub fn normalize(text: &str) -> String {
    clean(text, '\n')
}

/// [`normalize`] for single-line inputs: every line break becomes a space.
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
pub fn is_image_uri(uri: &str) -> bool {
    uri.starts_with("data:image/")
}

/// The base label of an attached file, upstream `promptAttachmentLabel`:
/// `[Image n]` where `n` is one past the images already attached, `[PDF: name]`
/// for a PDF, `[File: name]` for anything else.
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
pub fn svg_label(name: &str) -> String {
    format!("[SVG: {name}]")
}

/// `text` with each registered label that still stands in it replaced by its
/// original content, built in one pass. A label is expanded at its first
/// occurrence only (a second copy typed by hand stays literal), and where
/// labels share a prefix (`[Pasted ~3 lines]`, `[Pasted ~3 lines] #2`) the
/// longest match wins. Entries whose label is gone from `text` are ignored.
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        MAX_PASTED_FILEPATHS, Pasted, attachment_label, expand, expandable_before, fold_label,
        labels_in_order, mime_of_ext, normalize, normalize_path, parse_paths,
        pasted_attachment_paths, percent_decode, single_line, svg_label, trim_in_place,
        unique_label,
    };

    #[test]
    fn an_oversize_paste_is_never_taken_for_paths() {
        let big = format!("/tmp/a.png {}", "x".repeat(MAX_PASTED_FILEPATHS * 4096));
        assert!(pasted_attachment_paths(&big).is_empty());
        let many = "/tmp/a.png ".repeat(MAX_PASTED_FILEPATHS * 4096 / 11 + 1);
        assert!(pasted_attachment_paths(&many).is_empty());
    }

    #[test]
    fn percent_escapes_need_two_hex_digits() {
        assert_eq!(percent_decode("a%20b%2F").as_deref(), Some("a b/"));
        assert_eq!(percent_decode("%e4%B8%ad").as_deref(), Some("中"));
        for bad in ["%+a", "%-1", "% a", "%a", "%", "%zz", "%a+"] {
            assert_eq!(percent_decode(bad), None, "{bad:?}");
        }
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn pasted(label: &str, text: &str) -> Pasted {
        Pasted {
            label: label.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn fold_threshold_is_three_lines_or_over_150_chars() {
        assert_eq!(fold_label("a\nb"), None);
        assert_eq!(fold_label("a\nb\nc").as_deref(), Some("[Pasted ~3 lines]"));
        assert_eq!(fold_label(&"x".repeat(149)), None);
        assert_eq!(fold_label(&"x".repeat(150)), None);
        assert_eq!(
            fold_label(&"x".repeat(151)).as_deref(),
            Some("[Pasted ~1 lines]")
        );
    }

    #[test]
    fn fold_counts_characters_not_bytes_and_judges_the_trimmed_text() {
        assert_eq!(fold_label(&"中".repeat(150)), None, "450 bytes, 150 chars");
        assert!(fold_label(&"中".repeat(151)).is_some());
        assert_eq!(fold_label("a\nb\n\n"), None, "trailing breaks do not count");
        assert_eq!(fold_label("\n\na\nb"), None, "leading breaks do not count");
    }

    #[test]
    fn labels_are_made_unique_with_a_numeric_suffix() {
        let base = "[Pasted ~12 lines]";
        let taken = |entries: &'static [&'static str]| move |l: &str| entries.contains(&l);
        assert_eq!(unique_label(base, taken(&[])), base);
        assert_eq!(
            unique_label(base, taken(&["[Pasted ~12 lines]"])),
            "[Pasted ~12 lines] #2"
        );
        let two = taken(&["[Pasted ~12 lines]", "[Pasted ~12 lines] #2"]);
        assert_eq!(unique_label(base, two), "[Pasted ~12 lines] #3");
        assert_eq!(unique_label("[Pasted ~3 lines]", two), "[Pasted ~3 lines]");
    }

    #[test]
    fn attachment_labels_follow_upstream() {
        assert_eq!(
            attachment_label("data:image/png;base64,AA", "a.png", 0),
            "[Image 1]"
        );
        assert_eq!(
            attachment_label("data:image/jpeg;base64,AA", "b.jpg", 2),
            "[Image 3]"
        );
        assert_eq!(
            attachment_label("data:application/pdf;base64,AA", "r.pdf", 5),
            "[PDF: r.pdf]"
        );
        assert_eq!(
            attachment_label("data:text/plain;base64,AA", "t", 0),
            "[File: t]"
        );
        assert_eq!(svg_label("logo.svg"), "[SVG: logo.svg]");
    }

    #[test]
    fn expand_replaces_labels_in_place_and_in_order() {
        let entries = [
            pasted("[Pasted ~3 lines]", "a\nb\nc"),
            pasted("[Pasted ~3 lines] #2", "d\ne\nf"),
        ];
        assert_eq!(
            expand("x [Pasted ~3 lines] y [Pasted ~3 lines] #2 z", &entries),
            "x a\nb\nc y d\ne\nf z"
        );
        assert_eq!(
            expand("[Pasted ~3 lines] #2[Pasted ~3 lines]", &entries),
            "d\ne\nfa\nb\nc"
        );
    }

    #[test]
    fn expand_skips_missing_labels_and_expands_each_entry_once() {
        let entries = [pasted("[Pasted ~3 lines]", "a\nb\nc")];
        assert_eq!(
            expand("[Pasted ~3 line] [x]", &entries),
            "[Pasted ~3 line] [x]"
        );
        assert_eq!(
            expand("[Pasted ~3 lines] [Pasted ~3 lines]", &entries),
            "a\nb\nc [Pasted ~3 lines]"
        );
        assert_eq!(expand("plain 你好", &entries), "plain 你好");
    }

    #[test]
    fn expandable_needs_same_content_right_before_the_cursor() {
        let entries = [
            pasted("[Pasted ~3 lines]", "a\nb\nc"),
            pasted("[Pasted ~3 lines] #2", "d\ne\nf"),
        ];
        assert_eq!(
            expandable_before("hi [Pasted ~3 lines]", "a\nb\nc\n", &entries),
            Some(0)
        );
        assert_eq!(
            expandable_before("[Pasted ~3 lines] #2", "d\ne\nf", &entries),
            Some(1)
        );
        assert_eq!(
            expandable_before("[Pasted ~3 lines] ", "a\nb\nc", &entries),
            None
        );
        assert_eq!(
            expandable_before("[Pasted ~3 lines]", "other\ntext\nhere", &entries),
            None
        );
    }

    #[test]
    fn trim_in_place_strips_both_ends() {
        let mut text = String::from(" \n a\nb \n");
        trim_in_place(&mut text);
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn crlf_and_lone_cr_become_lf() {
        assert_eq!(normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize("a\r\n\r\nb"), "a\n\nb");
        assert_eq!(normalize("trailing\r"), "trailing\n");
    }

    #[test]
    fn csi_sequences_are_stripped() {
        assert_eq!(normalize("\x1b[31mred\x1b[0m text"), "red text");
        assert_eq!(normalize("a\x1b[1;38;5;208mb\x1b[K"), "ab");
        assert_eq!(normalize("\x1b[?25lhidden\x1b[?25h"), "hidden");
    }

    #[test]
    fn osc_and_two_byte_escapes_are_stripped() {
        assert_eq!(normalize("\x1b]0;title\x07body"), "body");
        assert_eq!(
            normalize("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(normalize("a\x1bMb"), "ab");
        assert_eq!(normalize("dangling\x1b"), "dangling");
    }

    #[test]
    fn chinese_and_plain_text_are_unchanged() {
        for text in [
            "你好，世界",
            "第一行\n第二行",
            "tab\there",
            "emoji 🎉 ok",
            "",
        ] {
            assert_eq!(normalize(text), text);
        }
    }

    #[test]
    fn single_line_folds_every_break_to_a_space() {
        assert_eq!(single_line("a\r\nb\nc\rd"), "a b c d");
        assert_eq!(single_line("\x1b[1mbold\x1b[0m\nnext"), "bold next");
        assert_eq!(single_line("你好\n世界"), "你好 世界");
    }

    #[test]
    fn one_plain_path_is_an_attachment() {
        assert_eq!(
            pasted_attachment_paths("/tmp/a.png"),
            paths(&["/tmp/a.png"])
        );
        assert_eq!(
            pasted_attachment_paths("  /tmp/a.pdf\n"),
            paths(&["/tmp/a.pdf"])
        );
        assert_eq!(
            pasted_attachment_paths("/tmp/A.JPG"),
            paths(&["/tmp/A.JPG"])
        );
    }

    #[test]
    fn quotes_and_backslash_escapes_are_undone() {
        assert_eq!(
            pasted_attachment_paths("'/tmp/my shot.png'"),
            paths(&["/tmp/my shot.png"])
        );
        assert_eq!(
            pasted_attachment_paths("\"/tmp/my shot.png\""),
            paths(&["/tmp/my shot.png"])
        );
        assert_eq!(
            pasted_attachment_paths("/tmp/my\\ shot.png"),
            paths(&["/tmp/my shot.png"])
        );
        assert_eq!(
            pasted_attachment_paths("/tmp/my shot.png"),
            paths(&["/tmp/my shot.png"]),
            "an unquoted path with a space is tried whole"
        );
    }

    #[test]
    fn file_urls_decode_to_local_paths() {
        assert_eq!(
            pasted_attachment_paths("file:///tmp/my%20shot.png"),
            paths(&["/tmp/my shot.png"])
        );
        assert_eq!(
            pasted_attachment_paths("file://localhost/tmp/a.png"),
            paths(&["/tmp/a.png"])
        );
        assert_eq!(
            pasted_attachment_paths("file:///tmp/%E4%B8%AD.png"),
            paths(&["/tmp/中.png"])
        );
        assert!(pasted_attachment_paths("file:///tmp%2Fa.png").is_empty());
        assert!(pasted_attachment_paths("file:///tmp%5Ca.png").is_empty());
        assert!(pasted_attachment_paths("file://server/share/a.png").is_empty());
        assert!(pasted_attachment_paths("file:///tmp/%zz.png").is_empty());
    }

    #[test]
    fn several_paths_split_on_whitespace_quotes_and_lines() {
        assert_eq!(
            pasted_attachment_paths("/a/1.png /a/2.pdf"),
            paths(&["/a/1.png", "/a/2.pdf"])
        );
        assert_eq!(
            pasted_attachment_paths("/a/1.png\r\n'/a/b c/2.jpg'\n/a/3.gif"),
            paths(&["/a/1.png", "/a/b c/2.jpg", "/a/3.gif"])
        );
    }

    #[test]
    fn a_uri_list_drops_comment_lines() {
        assert_eq!(
            pasted_attachment_paths("# copied from Finder\nfile:///a/1.png\nfile:///a/2.png"),
            paths(&["/a/1.png", "/a/2.png"])
        );
        assert_eq!(
            parse_paths("# only\n#comments"),
            paths(&["#", "only", "#comments"])
        );
    }

    #[test]
    fn more_than_32_paths_is_not_an_attachment() {
        let many = |n: usize| (0..n).map(|i| format!("/a/{i}.png")).collect::<Vec<_>>();
        assert_eq!(pasted_attachment_paths(&many(32).join(" ")).len(), 32);
        assert!(pasted_attachment_paths(&many(33).join(" ")).is_empty());
    }

    #[test]
    fn anything_else_stays_text() {
        for text in [
            "http://x/a.png",
            "https://x/a.png",
            "/tmp/a.txt",
            "/tmp/a.png /tmp/b.txt",
            "look at /tmp/a.png please",
            "see /tmp/notes.txt and a.png",
            "a plain sentence.",
            "'/tmp/a.png",
            "",
            "   \n ",
        ] {
            assert!(
                pasted_attachment_paths(text).is_empty(),
                "{text:?} must stay text"
            );
        }
    }

    #[test]
    fn normalize_path_strips_quotes_and_unescapes() {
        assert_eq!(
            normalize_path("'/a b/c.png'").as_deref(),
            Some("/a b/c.png")
        );
        assert_eq!(
            normalize_path("/a\\ b/c\\(1).png").as_deref(),
            Some("/a b/c(1).png")
        );
        assert_eq!(
            normalize_path("file:///a%20b.png").as_deref(),
            Some("/a b.png")
        );
        assert_eq!(normalize_path("file://host/a.png"), None);
    }

    #[test]
    fn extensions_map_to_mime_types_ignoring_case() {
        assert_eq!(mime_of_ext("png"), Some("image/png"));
        assert_eq!(mime_of_ext("JPG"), Some("image/jpeg"));
        assert_eq!(mime_of_ext("jpeg"), Some("image/jpeg"));
        assert_eq!(mime_of_ext("Pdf"), Some("application/pdf"));
        assert_eq!(mime_of_ext("svg"), Some("image/svg+xml"));
        for ext in ["gif", "webp", "avif"] {
            assert_eq!(mime_of_ext(ext), Some(format!("image/{ext}").as_str()));
        }
        assert_eq!(mime_of_ext("txt"), None);
        assert_eq!(mime_of_ext(""), None);
    }

    #[test]
    fn labels_in_order_follow_the_text_and_skip_the_missing() {
        let labels = ["[Image 1]", "[PDF: a.pdf]", "[PDF: a.pdf] #2", "[Image 2]"];
        assert_eq!(
            labels_in_order("x [PDF: a.pdf] #2 y [Image 1] [PDF: a.pdf]", &labels),
            vec![2, 0, 1]
        );
        assert_eq!(labels_in_order("[Image 1][Image 1]", &labels), vec![0]);
        assert_eq!(
            labels_in_order("[Image 3] plain 你好", &labels),
            Vec::<usize>::new()
        );
    }
}
