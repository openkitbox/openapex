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
use super::transport::{CallError, Rpc};

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
        let output_schema = self.output_schema.clone();
        let execution = self.execution;
        let cancel = ctx.cancellation.clone();
        Box::pin(async move {
            let arguments = if input.is_object() { input } else { json!({}) };
            let result = rpc
                .request(
                    "tools/call",
                    Some(json!({ "name": remote_name, "arguments": arguments })),
                    execution,
                    &cancel,
                )
                .await;
            match result {
                Ok(value) => render_result(value, output_schema.as_ref()),
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
                    };
                    fail(message)
                }
            }
        })
    }
}

/// Map one `tools/call` result object to `ToolOutput`.
fn render_result(value: Value, output_schema: Option<&Value>) -> ToolOutput {
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut texts = Vec::new();
    let mut attachments = Vec::new();
    for part in &content {
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

    let output = value.get("structuredContent").cloned();
    let content = match output {
        Some(structured) if !structured.is_null() => structured,
        _ if text.is_empty() => Value::Null,
        _ if output_schema.is_none() && (text.starts_with('{') || text.starts_with('[')) => {
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
    use crate::client::ToolInfo;
    use crate::jsonrpc::RpcError;
    use crate::transport::{CallError, Rpc};
    use futures_util::future::BoxFuture;
    use ox_tools::contract::ToolContext;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct Canned {
        response: Mutex<Option<Result<Value, CallError>>>,
        calls: Mutex<Vec<Value>>,
    }

    impl Rpc for Canned {
        fn request<'a>(
            &'a self,
            _method: &'a str,
            params: Option<Value>,
            _timeout: Option<Duration>,
            _cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Value, CallError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(params.unwrap_or(Value::Null));
                self.response.lock().unwrap().take().unwrap()
            })
        }

        fn notify<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> BoxFuture<'a, Result<(), CallError>> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    fn info(name: &str, output_schema: Option<Value>) -> ToolInfo {
        ToolInfo {
            name: name.to_owned(),
            description: "Do the thing".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema,
        }
    }

    fn context() -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: PathBuf::from("/tmp"),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    fn tool(response: Result<Value, CallError>, output_schema: Option<Value>) -> McpTool {
        let canned = Arc::new(Canned {
            response: Mutex::new(Some(response)),
            calls: Mutex::new(Vec::new()),
        });
        McpTool::new(
            "my server".to_owned(),
            info("do it", output_schema),
            canned,
            None,
        )
    }

    #[test]
    fn name_is_sanitized_and_prefixed() {
        assert_eq!(qualified_name("my server", "do it"), "my_server_do_it");
        assert_eq!(qualified_name("srv", "tool/name"), "srv_tool_name");
        let definition = tool(Ok(json!({})), None).definition();
        assert_eq!(definition.function.name, "my_server_do_it");
        assert_eq!(definition.function.description, "Do the thing");
    }

    #[tokio::test]
    async fn text_result_becomes_string_content() {
        let output = tool(
            Ok(json!({"content": [{"type": "text", "text": "hello"}]})),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert!(output.error.is_none());
        assert_eq!(output.content, json!("hello"));
    }

    #[tokio::test]
    async fn structured_content_wins() {
        let output = tool(
            Ok(json!({
                "content": [{"type": "text", "text": "ignored"}],
                "structuredContent": {"answer": 42}
            })),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert_eq!(output.content, json!({"answer": 42}));
    }

    #[tokio::test]
    async fn json_text_without_output_schema_is_parsed() {
        let output = tool(
            Ok(json!({"content": [{"type": "text", "text": "{\"a\":1}"}]})),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert_eq!(output.content, json!({"a": 1}));
    }

    #[tokio::test]
    async fn is_error_becomes_tool_failure() {
        let output = tool(
            Ok(json!({
                "isError": true,
                "content": [{"type": "text", "text": "bad input"}]
            })),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert_eq!(output.error.as_deref(), Some("bad input"));
        assert_eq!(output.content, Value::Null);
    }

    #[tokio::test]
    async fn image_parts_become_attachments() {
        let output = tool(
            Ok(json!({"content": [
                {"type": "text", "text": "see"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"}
            ]})),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert_eq!(output.content, json!("see"));
        let attachments = output.attachments.unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].uri, "data:image/png;base64,aGk=");
        assert_eq!(attachments[0].mime, "image/png");
    }

    #[tokio::test]
    async fn unsupported_media_stays_text() {
        let output = tool(
            Ok(json!({"content": [
                {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"}
            ]})),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert!(output.attachments.is_none());
        assert_eq!(
            output.content,
            json!("[Binary content (audio/wav) omitted: unsupported inline media]")
        );
    }

    #[tokio::test]
    async fn rpc_error_uses_upstream_message_shape() {
        let output = tool(
            Err(CallError::Rpc(RpcError {
                code: -32000,
                message: "boom".to_owned(),
                data: None,
            })),
            None,
        )
        .execute(context(), json!({}))
        .await;
        assert_eq!(
            output.error.as_deref(),
            Some("MCP tool \"do it\" on server \"my server\" failed: boom (code -32000)")
        );
    }

    #[tokio::test]
    async fn closed_transport_reports_unavailable() {
        let output = tool(Err(CallError::Closed), None)
            .execute(context(), json!({}))
            .await;
        assert_eq!(
            output.error.as_deref(),
            Some("MCP server \"my server\" is not available")
        );
    }

    #[tokio::test]
    async fn arguments_are_forwarded_verbatim() {
        let canned = Arc::new(Canned {
            response: Mutex::new(Some(Ok(json!({"content": []})))),
            calls: Mutex::new(Vec::new()),
        });
        let tool = McpTool::new("srv".to_owned(), info("echo", None), canned.clone(), None);
        let _ = tool.execute(context(), json!({"text": "hi"})).await;
        assert_eq!(
            canned.calls.lock().unwrap().as_slice(),
            [json!({"name": "echo", "arguments": {"text": "hi"}})]
        );
    }
}
