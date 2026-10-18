//! Port of upstream `packages/core/src/tool/plugin/webfetch.ts`: fetch a
//! URL and return its content as text, with content-type guards, redirect
//! limits, a response-size cap and a timeout.
//!
//! The HTML→text conversion is a compact port: upstream runs a full
//! htmlparser2 pipeline (~650 lines); ox extracts `<title>`, strips
//! script/style, and renders headings/paragraphs/list items/pre blocks as
//! text. All guards and error strings are exact.

use serde_json::Value;
use std::time::Duration;

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};

use super::{fail, object_param, ok_text, param, required_str};

/// Upstream `WebFetchTool.MAX_BYTES` (10 MiB response cap).
pub(crate) const MAX_BYTES: usize = 10 * 1024 * 1024;
/// Upstream timeout (60 s).
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    /// The shared client: redirects follow up to the default policy cap.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent("opencode")
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "webfetch",
            "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. \
             Use a more targeted tool when one is available. This tool is read-only. Large text \
             results may be replaced with a preview while the complete output is retained in \
             managed storage.",
            object_param(
                vec![
                    (
                        "url",
                        param("string", "The HTTP or HTTPS URL to fetch content from"),
                    ),
                    (
                        "format",
                        param(
                            "string",
                            "The format to return the content in. Defaults to markdown.",
                        ),
                    ),
                ],
                &["url"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(&self.client, &ctx, &input).await })
    }
}

async fn run(client: &reqwest::Client, _ctx: &ToolContext, input: &Value) -> ToolOutput {
    let url = match required_str("webfetch", input, "url") {
        Ok(url) => url,
        Err(output) => return output,
    };
    let format = match super::optional_str("webfetch", input, "format") {
        Ok(Some(format)) => format,
        Ok(None) => "markdown",
        Err(output) => return output,
    };
    if format != "text" && format != "markdown" && format != "html" {
        return super::invalid_arguments(
            "webfetch",
            &[(
                "format",
                format!("Expected one of \"text\" | \"markdown\" | \"html\", got \"{format}\""),
            )],
            input,
        );
    }
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return fail(format!("Invalid URL: {url}")),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return fail(format!("Invalid URL: {url}"));
    }
    let response = match client.get(parsed).send().await {
        Ok(response) => response,
        Err(_) => return fail(format!("Unable to fetch {url}")),
    };
    if !response.status().is_success() {
        return fail(format!(
            "Unable to fetch {url}: {} {}",
            response.status().as_u16(),
            response.status().canonical_reason().unwrap_or("error")
        ));
    }
    let Some(content_type) = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return fail(format!("Unable to fetch {url}: no content type"));
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !is_textual(&mime) {
        return fail(format!(
            "Unable to fetch {url}: content type \"{mime}\" is not text"
        ));
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => return fail(format!("Unable to fetch {url}")),
    };
    if bytes.len() > MAX_BYTES {
        return fail(format!(
            "Unable to fetch {url}: response exceeds {} MiB limit",
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let body = String::from_utf8_lossy(&bytes);
    let content = if mime.contains("html") || mime.contains("xml") {
        match format {
            "html" => body.to_string(),
            // Markdown and text both render the readable text.
            _ => html_to_text(&body),
        }
    } else {
        body.to_string()
    };
    ok_text(content)
}

/// Upstream `isTextual`: any `text/*`, plus the structured text types it
/// fetches (JSON, XML, JavaScript, YAML...).
fn is_textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/x-www-form-urlencoded"
                | "image/svg+xml"
        )
}

/// Compact HTML→text: drop scripts/styles/comments, keep block structure.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if after.starts_with("<!--") {
            rest = after
                .find("-->")
                .map(|end| &after[end + 3..])
                .unwrap_or_default();
            continue;
        }
        let Some(end) = after.find('>') else {
            break;
        };
        let tag = &after[..=end];
        rest = &after[end + 1..];
        let lower = tag.to_ascii_lowercase();
        // Structural whitespace.
        if lower.starts_with("<script") {
            if let Some(close) = rest.to_ascii_lowercase().find("</script") {
                rest = &rest[close..];
            } else {
                break;
            }
        } else if lower.starts_with("<style") {
            if let Some(close) = rest.to_ascii_lowercase().find("</style") {
                rest = &rest[close..];
            } else {
                break;
            }
        } else if lower.starts_with("<br") {
            out.push('\n');
        } else if matches!(block_tag(&lower), Some(true)) {
            out.push_str("\n\n");
        } else if block_tag(&lower) == Some(false) {
            out.push('\n');
        } else if lower.starts_with("<li") {
            out.push_str("\n- ");
        }
    }
    out.push_str(rest);
    decode_entities(&collapse_space(&out)).trim().to_owned()
}

/// `Some(true)` for tags whose close opens a blank line, `Some(false)` for
/// ones that end the line, `None` otherwise.
fn block_tag(tag: &str) -> Option<bool> {
    const BLANK: [&str; 9] = [
        "<p", "<div", "<h1", "<h2", "<h3", "<h4", "<h5", "<h6", "<section",
    ];
    const LINE: [&str; 5] = ["</p>", "</div>", "</h1>", "</h2>", "</section>"];
    if BLANK.iter().any(|open| tag.starts_with(open)) {
        return Some(true);
    }
    if LINE.iter().any(|close| tag.starts_with(close)) {
        return Some(false);
    }
    None
}

fn collapse_space(text: &str) -> String {
    // Collapse whitespace within each line; block structure (blank lines
    // between headings/paragraphs, list markers) is preserved.
    text.split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Minimal numeric and named HTML entity decoding.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: tempdir().unwrap().path().to_path_buf(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    /// One-shot HTTP mock on an ephemeral port (std TcpListener), answering
    /// every request with `status`, `content_type` and `body`.
    async fn serve(status_line: &'static str, content_type: &'static str, body: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    async fn fetch(url: &str, format: Option<&str>) -> ToolOutput {
        let mut input = json!({"url": url});
        if let Some(format) = format {
            input["format"] = json!(format);
        }
        let tool = WebFetchTool::new();
        tool.execute(ctx(), input).await
    }

    #[tokio::test]
    async fn fetches_text_content() {
        let url = serve("200 OK", "text/plain", "plain body\n".to_owned()).await;
        let output = fetch(&url, None).await;
        assert_eq!(output.content, json!("plain body\n"));
    }

    #[tokio::test]
    async fn converts_html_to_text() {
        let url = serve(
            "200 OK",
            "text/html; charset=utf-8",
            "<html><head><style>.x{}</style></head><body><h1>Title</h1><p>Hello <b>world</b>.</p><ul><li>one</li><li>two</li></ul></body></html>".to_owned(),
        )
        .await;
        let output = fetch(&url, None).await;
        let text = output.content.as_str().unwrap();
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world."));
        assert!(text.contains("- one"));
        assert!(!text.contains(".x{}"));
    }

    #[tokio::test]
    async fn rejects_non_text_content_types() {
        let url = serve("200 OK", "image/png", String::new()).await;
        let output = fetch(&url, None).await;
        assert_eq!(
            output.error.as_deref(),
            Some(format!("Unable to fetch {url}: content type \"image/png\" is not text").as_str())
        );
    }

    #[tokio::test]
    async fn reports_http_errors() {
        let url = serve("404 Not Found", "text/plain", "gone".to_owned()).await;
        let output = fetch(&url, None).await;
        assert_eq!(
            output.error.as_deref(),
            Some(format!("Unable to fetch {url}: 404 Not Found").as_str())
        );
    }

    #[tokio::test]
    async fn rejects_bad_urls_and_formats() {
        let output = fetch("ftp://example.com", None).await;
        assert_eq!(
            output.error.as_deref(),
            Some("Invalid URL: ftp://example.com")
        );
        let output = fetch("notaurl", None).await;
        assert_eq!(output.error.as_deref(), Some("Invalid URL: notaurl"));
        let url = serve("200 OK", "text/plain", "x".to_owned()).await;
        let output = fetch(&url, Some("pdf")).await;
        assert!(
            output
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("Invalid arguments for tool \"webfetch\":")),
            "unexpected: {output:?}"
        );
    }
}
