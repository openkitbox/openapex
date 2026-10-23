//! `fs.read` — `GET /api/fs/read/*` (upstream `server/src/handlers/fs.ts`).
//!
//! Streams one file relative to the requested location with the same HTTP
//! semantics the TUI's file preview and image viewer rely on:
//!
//! - `ETag` (`W/"<size hex>-<mtime millis hex>"`), `Last-Modified`,
//!   `Cache-Control: no-cache, no-transform`, `Accept-Ranges: bytes`;
//! - `If-None-Match` / `If-Modified-Since` → `304` with the validators;
//! - a single `bytes=start-end`, `bytes=start-` or `bytes=-suffix` range →
//!   `206` with `Content-Range`; malformed or multi-range headers fall back to
//!   the full body, an unsatisfiable range is `416`;
//! - `If-Range` gates the range the same way upstream does.
//!
//! Deliberate deviation: the file is read into memory instead of streamed.
//! Previews are images and text; the extra copy is cheaper than a streaming
//! body on this server. Path resolution matches `fs.list`/`fs.find`: a
//! leading `~` expands to the user's home (batch G renders injected paths that
//! way, so they must still resolve), an absolute path is used as-is, anything
//! else is relative to the requested location, and `..` may leave it (upstream
//! documents the same escape).

use std::path::{Path, PathBuf};

use salvo::http::HeaderValue;
use salvo::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, RANGE,
};
use salvo::prelude::*;
use serde_json::json;

use super::compat::{directory_of, percent_decode};

const PREFIX: &str = "/api/fs/read/";

#[handler]
pub(crate) async fn read(req: &mut Request, res: &mut Response) {
    let raw = req.uri().path().strip_prefix(PREFIX).unwrap_or_default();
    let relative = percent_decode(raw);
    let location = directory_of(req);
    let path = resolve(&location, &relative);

    let Ok(metadata) = tokio::fs::metadata(&path).await else {
        return not_found(res, &relative);
    };
    if !metadata.is_file() {
        return not_found(res, &relative);
    }

    let size = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64);
    let tag = etag(size, modified);
    let last_modified = modified.and_then(http_date);

    if is_not_modified(req, &tag, last_modified.as_deref()) {
        res.status_code(StatusCode::NOT_MODIFIED);
        validators(res, &tag, last_modified.as_deref());
        return;
    }

    let range = match req
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(header) if if_range_matches(req, &tag, last_modified.as_deref()) => {
            parse_range(header, size)
        }
        _ => Range::Full,
    };
    let (status, start, length) = match range {
        Range::Unsatisfiable => {
            res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
            res.headers_mut()
                .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            set_header(res, CONTENT_RANGE, &format!("bytes */{size}"));
            return;
        }
        Range::Full => (StatusCode::OK, 0, size),
        Range::Single { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end.saturating_sub(start).saturating_add(1),
        ),
    };

    let path_clone = path.clone();
    let bytes =
        match tokio::task::spawn_blocking(move || read_slice(&path_clone, start, length)).await {
            Ok(Ok(bytes)) => bytes,
            _ => return not_found(res, &relative),
        };

    res.status_code(status);
    validators(res, &tag, last_modified.as_deref());
    res.headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    set_header(res, CONTENT_TYPE, mime_for(&path));
    set_header(res, CONTENT_LENGTH, &length.to_string());
    if let Range::Single { start, end } = range {
        set_header(res, CONTENT_RANGE, &format!("bytes {start}-{end}/{size}"));
    }
    res.body(bytes);
}

fn resolve(location: &str, relative: &str) -> PathBuf {
    resolve_with_home(location, relative, &ox_core::config::user_home())
}

/// [`resolve`] with an explicit home, so the tilde rule is testable without
/// touching the machine's home directory. `~user` is not a tilde form ox
/// expands: it falls through to the location-relative branch, so it fails to
/// resolve rather than naming another user's directory.
fn resolve_with_home(location: &str, relative: &str, home: &Path) -> PathBuf {
    if let Some(expanded) = ox_core::home_paths::expand_tilde(relative, home) {
        return expanded;
    }
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        Path::new(location).join(candidate)
    }
}

fn read_slice(path: &Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn not_found(res: &mut Response, path: &str) {
    res.status_code(StatusCode::NOT_FOUND);
    res.render(Json(json!({
        "_tag": "FileNotFoundError",
        "path": path,
        "message": format!("File not found: {path}"),
    })));
}

fn set_header(res: &mut Response, name: salvo::http::header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        res.headers_mut().insert(name, value);
    }
}

fn validators(res: &mut Response, tag: &str, last_modified: Option<&str>) {
    set_header(res, CACHE_CONTROL, "no-cache, no-transform");
    set_header(res, ETAG, tag);
    if let Some(last_modified) = last_modified {
        set_header(res, LAST_MODIFIED, last_modified);
    }
}

/// `W/"<size hex>-<mtime millis hex>"`, absent mtime as `0`.
fn etag(size: u64, modified: Option<i64>) -> String {
    let stamp = modified.unwrap_or(0).max(0) as u64;
    format!("W/\"{size:x}-{stamp:x}\"")
}

fn strip_weak(tag: &str) -> &str {
    let tag = tag.trim();
    match tag.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("w/") => tag[2..].trim(),
        _ => tag,
    }
}

fn is_not_modified(req: &Request, tag: &str, last_modified: Option<&str>) -> bool {
    if let Some(value) = req
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        return value
            .split(',')
            .any(|candidate| candidate.trim() == "*" || strip_weak(candidate) == strip_weak(tag));
    }
    match (
        req.headers()
            .get(IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok()),
        last_modified,
    ) {
        // Both sides are second-granular HTTP dates; the upstream comparison is
        // `Date.parse(lastModified) <= Date.parse(ifModifiedSince)`.
        (Some(since), Some(modified)) => {
            match (parse_http_date(since), parse_http_date(modified)) {
                (Some(since), Some(modified)) => modified <= since,
                _ => false,
            }
        }
        _ => false,
    }
}

fn if_range_matches(req: &Request, tag: &str, last_modified: Option<&str>) -> bool {
    let Some(condition) = req.headers().get(IF_RANGE).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let trimmed = condition.trim_start();
    if trimmed.starts_with('"')
        || trimmed
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("w/"))
    {
        return strip_weak(condition) == strip_weak(tag);
    }
    match (
        parse_http_date(condition),
        last_modified.and_then(parse_http_date),
    ) {
        (Some(condition), Some(modified)) => condition == modified,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Range {
    Full,
    Unsatisfiable,
    Single { start: u64, end: u64 },
}

/// Upstream `parseRange`: one range only; anything malformed means "full body".
fn parse_range(header: &str, size: u64) -> Range {
    let Some(spec) = header.trim().strip_prefix("bytes=").or_else(|| {
        header
            .trim()
            .get(..6)
            .filter(|prefix| prefix.eq_ignore_ascii_case("bytes="))
            .map(|_| &header.trim()[6..])
    }) else {
        return Range::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return Range::Full;
    };
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() && end.is_empty() {
        return Range::Full;
    }
    if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return Range::Full;
        };
        if suffix == 0 || size == 0 {
            return Range::Unsatisfiable;
        }
        return Range::Single {
            start: size.saturating_sub(suffix),
            end: size - 1,
        };
    }
    let Ok(start) = start.parse::<u64>() else {
        return Range::Full;
    };
    let end = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        match end.parse::<u64>() {
            Ok(end) => end,
            Err(_) => return Range::Full,
        }
    };
    if start > end || start >= size {
        return Range::Unsatisfiable;
    }
    Range::Single {
        start,
        end: end.min(size.saturating_sub(1)),
    }
}

/// RFC 1123 / HTTP-date (`Sat, 09 Nov 2024 12:34:56 GMT`) for a mtime.
fn http_date(millis: i64) -> Option<String> {
    let timestamp = jiff::Timestamp::from_millisecond(millis).ok()?;
    let mut bytes = Vec::with_capacity(32);
    jiff::fmt::rfc2822::DateTimePrinter::new()
        .print_timestamp_rfc9110(&timestamp, &mut bytes)
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn parse_http_date(value: &str) -> Option<i64> {
    jiff::fmt::rfc2822::parse(value)
        .ok()
        .map(|zoned| zoned.timestamp().as_millisecond())
}

/// The `mime-types` table upstream looks files up in, trimmed to what a
/// terminal client previews. Unknown extensions are `application/octet-stream`.
fn mime_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "json" => "application/json",
        "jsonl" => "application/jsonl",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "mts" | "cts" => "text/typescript",
        "tsx" | "jsx" => "text/jsx",
        "css" => "text/css",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "yaml" | "yml" => "text/yaml",
        "toml" => "text/toml",
        "md" | "markdown" | "mdx" => "text/markdown",
        "txt" | "log" | "lock" | "cfg" | "ini" | "env" => "text/plain",
        "csv" => "text/csv",
        "rs" | "py" | "go" | "rb" | "java" | "kt" | "swift" | "c" | "h" | "cc" | "cpp" | "hpp"
        | "cs" | "php" | "lua" | "zig" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "sql"
        | "diff" | "patch" => "text/plain",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_match_the_upstream_parser() {
        assert_eq!(
            parse_range("bytes=0-99", 1000),
            Range::Single { start: 0, end: 99 }
        );
        assert_eq!(parse_range("BYTES = 0-99", 1000), Range::Full);
        assert_eq!(
            parse_range("bytes=900-", 1000),
            Range::Single {
                start: 900,
                end: 999
            }
        );
        assert_eq!(
            parse_range("bytes=-100", 1000),
            Range::Single {
                start: 900,
                end: 999
            }
        );
        assert_eq!(
            parse_range("bytes=-2000", 1000),
            Range::Single { start: 0, end: 999 }
        );
        assert_eq!(
            parse_range("bytes=5-2000", 1000),
            Range::Single { start: 5, end: 999 }
        );
        assert_eq!(parse_range("bytes=1000-", 1000), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=20-10", 1000), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-10,20-30", 1000), Range::Full);
        assert_eq!(parse_range("bytes=-", 1000), Range::Full);
        assert_eq!(parse_range("items=0-10", 1000), Range::Full);
    }

    #[test]
    fn etags_are_weak_and_comparable() {
        assert_eq!(etag(255, Some(0x10)), "W/\"ff-10\"");
        assert_eq!(strip_weak("W/\"ff-10\""), "\"ff-10\"");
        assert_eq!(strip_weak("w/ \"ff-10\""), "\"ff-10\"");
        assert_eq!(strip_weak("\"ff-10\""), "\"ff-10\"");
    }

    #[test]
    fn http_dates_round_trip() {
        let stamp = 1_700_000_000_000;
        let formatted = http_date(stamp).unwrap();
        assert!(formatted.ends_with("GMT"), "{formatted}");
        let parsed = parse_http_date(&formatted).unwrap();
        // HTTP dates carry second granularity.
        assert_eq!(parsed / 1000, stamp / 1000);
        assert!(parse_http_date("not a date").is_none());
    }

    #[test]
    fn mime_lookup_falls_back_to_octet_stream() {
        assert_eq!(mime_for(Path::new("/tmp/a.PNG")), "image/png");
        assert_eq!(mime_for(Path::new("src/main.rs")), "text/plain");
        assert_eq!(mime_for(Path::new("Makefile")), "application/octet-stream");
    }

    #[test]
    fn paths_resolve_against_the_location() {
        assert_eq!(
            resolve("/repo", "src/main.rs"),
            PathBuf::from("/repo/src/main.rs")
        );
        assert_eq!(resolve("/repo", "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

    /// Batch G: injected paths render the home directory as `~`, so the read
    /// endpoint expands it back. `resolve_with_home` takes the home
    /// explicitly so the fixture never touches the machine's home directory:
    /// before batch G the same request resolved to `<location>/~/…` and 404ed.
    #[test]
    fn a_leading_tilde_resolves_under_home() {
        let home = Path::new("/fake/home");
        assert_eq!(
            resolve_with_home("/repo", "~/notes.txt", home),
            PathBuf::from("/fake/home/notes.txt")
        );
        assert_eq!(
            resolve_with_home("/repo", "~", home),
            PathBuf::from("/fake/home")
        );
        // `~user` is not expanded: it stays location-relative, so the request
        // fails instead of reading another user's home.
        assert_eq!(
            resolve_with_home("/repo", "~john/x", home),
            PathBuf::from("/repo/~john/x")
        );
    }

    /// The handler's resolution uses the one user-home source, so a redacted
    /// path resolves to the same file it was rendered from.
    #[test]
    fn resolve_expands_tilde_through_the_user_home() {
        assert_eq!(
            resolve("/repo", "~/notes.txt"),
            ox_core::config::user_home().join("notes.txt")
        );
    }
}
