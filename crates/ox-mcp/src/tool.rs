//! `Tool` adapter: exposes one MCP tool to the runner.
//!
//! Result mapping mirrors upstream
//! `reference/opencode/packages/core/src/tool/mcp.ts` (text join, structured
//! output precedence, `isError` → tool failure, binary parts → attachments).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use ox_schema::wire::ToolDef;
use ox_tools::contract::{Attachment, Tool, ToolContext, ToolOutput};
use ox_tools::fail;

use super::client::ToolInfo;
use super::transport::{CallError, RequestMetadata, Rpc};

/// `<sanitized server>_<sanitized tool>`, the upstream `McpTool.name` rule.
pub(crate) fn qualified_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(tool))
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Media ox can hand to the model. Every attachment becomes an `image_url`
/// part, so anything else (audio, video, arbitrary binary) must stay text:
/// upstream gates on the same set for the same reason — an unsupported part
/// fails request lowering on every later turn.
const INLINE_MEDIA: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "application/pdf",
];

fn is_inline_media(mime: &str) -> bool {
    let base = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    INLINE_MEDIA.contains(&base.as_str())
}

pub(crate) struct McpTool {
    server: String,
    remote_name: String,
    qualified: String,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    rpc: Arc<dyn Rpc>,
    execution: Option<Duration>,
}

impl McpTool {
    pub(crate) fn new(
        server: String,
        info: ToolInfo,
        rpc: Arc<dyn Rpc>,
        execution: Option<Duration>,
    ) -> Self {
        Self {
            qualified: qualified_name(&server, &info.name),
            server,
            remote_name: info.name,
            description: info.description,
            input_schema: info.input_schema,
            output_schema: info.output_schema,
            rpc,
            execution,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            self.qualified.clone(),
            self.description.clone(),
            self.input_schema.clone(),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolOutput> + Send + '_>> {
        let rpc = Arc::clone(&self.rpc);
        let remote_name = self.remote_name.clone();
        let server = self.server.clone();
        let has_output_schema = self.output_schema.is_some();
        let execution = self.execution;
        let cancel = ctx.cancellation.clone();
        Box::pin(async move {
            let arguments = if input.is_object() { input } else { json!({}) };
            let result = rpc
                .request(
                    RequestMetadata::new("tools/call").tool(&remote_name),
                    Some(json!({ "name": remote_name, "arguments": arguments })),
                    execution,
                    &cancel,
                )
                .await;
            match result {
                Ok(value) => render_result(value, has_output_schema),
                Err(error) => {
                    let message = match &error {
                        CallError::Closed => format!("MCP server \"{server}\" is not available"),
                        CallError::Rpc(error) => format!(
                            "MCP tool \"{remote_name}\" on server \"{server}\" failed: {error}"
                        ),
                        CallError::Timeout => {
                            format!("MCP tool \"{remote_name}\" on server \"{server}\" timed out")
                        }
                        CallError::Cancelled => "MCP tool call cancelled".to_owned(),
                        CallError::Transport(message) => format!(
                            "MCP tool \"{remote_name}\" on server \"{server}\" failed: {message}"
                        ),
                        CallError::Unsupported(message) => format!(
                            "MCP tool \"{remote_name}\" on server \"{server}\" failed: {message}"
                        ),
                    };
                    fail(message)
                }
            }
        })
    }
}

/// Map one `tools/call` result object to `ToolOutput`.
fn render_result(value: Value, has_output_schema: bool) -> ToolOutput {
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut texts = Vec::new();
    let mut attachments = Vec::new();
    for part in content {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    texts.push(text.to_owned());
                }
            }
            Some("image") | Some("audio") => {
                let Some(data) = part.get("data").and_then(Value::as_str) else {
                    continue;
                };
                let mime = part
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("application/octet-stream");
                if is_inline_media(mime) {
                    attachments.push(Attachment {
                        uri: format!("data:{mime};base64,{data}"),
                        mime: mime.to_owned(),
                        name: None,
                    });
                } else {
                    texts.push(format!(
                        "[Binary content ({mime}) omitted: unsupported inline media]"
                    ));
                }
            }
            Some("resource") => {
                let resource = part.get("resource").unwrap_or(&Value::Null);
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    texts.push(text.to_owned());
                } else if let (Some(blob), Some(mime)) = (
                    resource.get("blob").and_then(Value::as_str),
                    resource.get("mimeType").and_then(Value::as_str),
                ) {
                    if is_inline_media(mime) {
                        attachments.push(Attachment {
                            uri: format!("data:{mime};base64,{blob}"),
                            mime: mime.to_owned(),
                            name: resource
                                .get("uri")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        });
                    } else {
                        texts.push(format!(
                            "[Binary resource ({mime}) omitted: unsupported inline media]"
                        ));
                    }
                }
            }
            Some("resource_link") => {
                if let Some(uri) = part.get("uri").and_then(Value::as_str) {
                    texts.push(uri.to_owned());
                }
            }
            _ => {}
        }
    }
    let text = texts.join("\n");

    if value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let message = if text.trim().is_empty() {
            "MCP tool returned an error".to_owned()
        } else {
            text.trim().to_owned()
        };
        return fail(message);
    }

    let output = value
        .get("structuredContent")
        .filter(|value| value.is_object())
        .cloned();
    let content = match output {
        Some(structured) if !structured.is_null() => structured,
        _ if text.is_empty() => Value::Null,
        _ if !has_output_schema && (text.starts_with('{') || text.starts_with('[')) => {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        }
        _ => Value::String(text),
    };
    let mut output = ToolOutput {
        content,
        error: None,
        metadata: None,
        attachments: None,
    };
    if !attachments.is_empty() {
        output.attachments = Some(attachments);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_content_takes_precedence_over_text() {
        let output = render_result(
            json!({
                "content": [{"type": "text", "text": "stale text"}],
                "structuredContent": {"answer": 42}
            }),
            true,
        );

        assert_eq!(output.content, json!({"answer": 42}));
    }

    #[test]
    fn non_object_structured_content_does_not_replace_text() {
        let output = render_result(
            json!({
                "content": [{"type": "text", "text": "fallback"}],
                "structuredContent": ["not", "an", "object"]
            }),
            true,
        );

        assert_eq!(output.content, json!("fallback"));
    }

    #[test]
    fn resource_links_text_and_supported_media_are_mapped() {
        let output = render_result(
            json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "resource", "resource": {"text": "resource text"}},
                    {"type": "resource_link", "uri": "https://example.test/file"},
                    {"type": "image", "data": "aW1hZ2U=", "mimeType": "image/png"}
                ]
            }),
            false,
        );

        assert_eq!(
            output.content,
            json!("hello\nresource text\nhttps://example.test/file")
        );
        assert_eq!(
            output.attachments,
            Some(vec![Attachment {
                uri: "data:image/png;base64,aW1hZ2U=".to_owned(),
                mime: "image/png".to_owned(),
                name: None,
            }])
        );
    }

    #[test]
    fn unsupported_media_stays_text_instead_of_attachment() {
        let output = render_result(
            json!({
                "content": [{"type": "audio", "data": "AAAA", "mimeType": "audio/wav"}]
            }),
            false,
        );

        assert!(output.attachments.is_none());
        assert_eq!(
            output.content,
            json!("[Binary content (audio/wav) omitted: unsupported inline media]")
        );
    }

    #[test]
    fn is_error_returns_failed_output() {
        let output = render_result(
            json!({
                "isError": true,
                "content": [{"type": "text", "text": "nope"}]
            }),
            false,
        );

        assert_eq!(output.error.as_deref(), Some("nope"));
    }
}
