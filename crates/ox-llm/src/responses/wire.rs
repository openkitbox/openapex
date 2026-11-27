//! OpenAI Responses request bodies and the one translation from ox's internal
//! [`ChatRequest`] into them.
//!
//! Responses is not Chat Completions with a different path: history is a flat
//! `input` array of typed items, tools are flattened to `{type, name,
//! description, parameters}`, and reasoning has its own `effort` object. The
//! translation lives here so the engine keeps building one request shape and
//! never learns which wire protocol an endpoint speaks.
//!
//! Phase A is stateless: `store: false`, no `previous_response_id`, and no
//! synthetic reasoning items. Tool definitions are borrowed from the
//! request's `Arc<Vec<ToolDef>>`; only the input items are built anew, once, at
//! the request boundary.

use serde::Serialize;
use serde_json::Value;

use crate::chat_completions::wire::{ChatMessage, ChatRequest, ToolChoice, UserContent, UserPart};

/// A `/v1/responses` request body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesRequest<'a> {
    /// Model identifier as the endpoint exposes it.
    pub model: &'a str,
    /// Flat list of input items: messages, function calls and their results.
    pub input: Vec<InputItem>,
    /// Function tools; omitted from the wire when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponsesTool<'a>>,
    /// `none` / `auto` / `required`; meaningless without tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Sampling temperature, when the turn set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Completion cap, translated from `ChatRequest::max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Reasoning effort, when the turn selected a supported level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// Always `true`: ox only issues streaming requests.
    pub stream: bool,
    /// Always `false`: phase A keeps no server-side response state.
    pub store: bool,
}

/// One entry of the Responses `input` array.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    /// A role-labelled message with typed content parts.
    #[serde(rename = "message")]
    Message {
        /// `developer`, `user` or `assistant`.
        role: &'static str,
        /// Content parts in this message.
        content: Vec<InputContent>,
    },
    /// An assistant tool call replayed from history.
    FunctionCall {
        /// Call id echoed by the matching result.
        call_id: String,
        /// Function name.
        name: String,
        /// JSON arguments string, as ox recorded them.
        arguments: String,
    },
    /// The result of a prior function call.
    FunctionCallOutput {
        /// The call this answers.
        call_id: String,
        /// Result text.
        output: String,
    },
}

/// One typed part of a Responses input message.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    /// User or developer text.
    InputText {
        /// The text itself.
        text: String,
    },
    /// A user image reference (URL or data URL).
    InputImage {
        /// Image URL.
        image_url: String,
    },
    /// Assistant output text, which Responses expects as an output item.
    OutputText {
        /// The text itself.
        text: String,
    },
}

/// A Responses function tool. All fields borrow the request's tool list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesTool<'a> {
    /// Always `function`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Function name.
    pub name: &'a str,
    /// Function description, omitted when empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
    /// JSON Schema for the arguments.
    pub parameters: &'a Value,
}

/// The `reasoning` request object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reasoning {
    /// One of `none`, `low`, `medium`, `high`, `xhigh`, `max`.
    pub effort: &'static str,
}

/// Translate a completions-shaped [`ChatRequest`] into a Responses body.
///
/// Infallible: every internal field has a legal Responses spelling. Unknown
/// effort names are dropped with a debug log rather than forwarded as an
/// invalid value that would 400 the turn.
#[must_use]
pub fn build_request(request: &ChatRequest) -> ResponsesRequest<'_> {
    let mut input = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        match message {
            ChatMessage::System { content } => input.push(InputItem::Message {
                role: "developer",
                content: vec![InputContent::InputText {
                    text: content.clone(),
                }],
            }),
            ChatMessage::User { content } => {
                let content = user_content(content);
                if !content.is_empty() {
                    input.push(InputItem::Message {
                        role: "user",
                        content,
                    });
                }
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                reasoning_content,
                replay_state: _,
            } => {
                if let Some(reasoning) = reasoning_content
                    && !reasoning.is_empty()
                {
                    tracing::debug!(
                        bytes = reasoning.len(),
                        "dropping assistant reasoning_content: phase A sends no Responses reasoning items"
                    );
                }
                if let Some(text) = content.as_deref().filter(|text| !text.is_empty()) {
                    input.push(InputItem::Message {
                        role: "assistant",
                        content: vec![InputContent::OutputText {
                            text: text.to_owned(),
                        }],
                    });
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        input.push(InputItem::FunctionCall {
                            call_id: call.id.clone(),
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                        });
                    }
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => input.push(InputItem::FunctionCallOutput {
                call_id: tool_call_id.clone(),
                output: content.clone(),
            }),
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|tool| ResponsesTool {
            kind: "function",
            name: &tool.function.name,
            description: (!tool.function.description.is_empty())
                .then_some(tool.function.description.as_str()),
            parameters: &tool.function.parameters,
        })
        .collect();

    ResponsesRequest {
        model: &request.model,
        input,
        tools,
        tool_choice: (!request.tools.is_empty()).then_some(request.tool_choice),
        temperature: request.temperature,
        max_output_tokens: request.max_tokens,
        reasoning: request.reasoning_effort.as_deref().and_then(effort),
        stream: true,
        store: false,
    }
}

/// The Responses effort for an internal variant name.
fn effort(value: &str) -> Option<Reasoning> {
    let normalized = value.trim().to_ascii_lowercase();
    let effort = match normalized.as_str() {
        "off" | "none" => "none",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => {
            tracing::debug!(
                variant = value,
                "unknown reasoning effort; not sending a Responses reasoning object"
            );
            return None;
        }
    };
    Some(Reasoning { effort })
}

/// The typed content parts of a user turn.
fn user_content(content: &UserContent) -> Vec<InputContent> {
    match content {
        UserContent::Text(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![InputContent::InputText { text: text.clone() }]
            }
        }
        UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                UserPart::Text { text } => {
                    (!text.is_empty()).then(|| InputContent::InputText { text: text.clone() })
                }
                UserPart::Image { image_url } => Some(InputContent::InputImage {
                    image_url: image_url.url.clone(),
                }),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::build_request;
    use crate::chat_completions::wire::{
        ChatMessage, ChatRequest, FunctionCall, FunctionType, ImageUrl, ToolCall, ToolChoice,
        ToolDef, UserContent, UserPart,
    };

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest::new("gpt-5", messages)
    }

    #[test]
    fn body_uses_responses_shape_only() {
        let mut request = request(vec![ChatMessage::user("hi")]);
        request.temperature = Some(0.3);
        request.max_tokens = Some(128);

        let body = serde_json::to_value(build_request(&request)).expect("body serializes");

        assert_eq!(body["model"], json!("gpt-5"));
        assert_eq!(
            body["input"],
            json!([{"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}])
        );
        assert_eq!(body["temperature"], json!(0.3));
        assert_eq!(body["max_output_tokens"], json!(128));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["store"], json!(false));
        for forbidden in [
            "messages",
            "stream_options",
            "previous_response_id",
            "tools",
            "tool_choice",
            "reasoning",
        ] {
            assert!(
                body.get(forbidden).is_none(),
                "`{forbidden}` must not be sent"
            );
        }
    }

    #[test]
    fn history_items_and_tools_map_to_responses() {
        let mut request = request(vec![
            ChatMessage::system("be brief"),
            ChatMessage::User {
                content: UserContent::Parts(vec![
                    UserPart::Text {
                        text: "look".to_owned(),
                    },
                    UserPart::Image {
                        image_url: ImageUrl {
                            url: "https://example.com/a.png".to_owned(),
                        },
                    },
                ]),
            },
            ChatMessage::Assistant {
                content: Some("ok".to_owned()),
                tool_calls: Some(vec![ToolCall {
                    id: "call-1".to_owned(),
                    kind: FunctionType::Function,
                    function: FunctionCall {
                        name: "read".to_owned(),
                        arguments: r#"{"path":"x"}"#.to_owned(),
                    },
                }]),
                reasoning_content: Some("must not be replayed".to_owned()),
                replay_state: None,
            },
            ChatMessage::tool("call-1", "contents"),
        ]);
        request.tools = Arc::new(vec![ToolDef::function(
            "read",
            "read a file",
            json!({"type": "object"}),
        )]);
        request.tool_choice = ToolChoice::Required;
        request.reasoning_effort = Some("off".to_owned());

        let body: Value = serde_json::to_value(build_request(&request)).expect("body serializes");

        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "developer",
                 "content": [{"type": "input_text", "text": "be brief"}]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "look"},
                    {"type": "input_image", "image_url": "https://example.com/a.png"}]},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "ok"}]},
                {"type": "function_call", "call_id": "call-1", "name": "read",
                 "arguments": r#"{"path":"x"}"#},
                {"type": "function_call_output", "call_id": "call-1", "output": "contents"}
            ])
        );
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "name": "read",
            "description": "read a file", "parameters": {"type": "object"}}])
        );
        assert_eq!(body["tool_choice"], json!("required"));
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
        assert!(!body.to_string().contains("must not be replayed"));
    }

    #[test]
    fn reasoning_effort_whitelist_is_respected() {
        for (input, expected) in [
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", Some("xhigh")),
            ("max", Some("max")),
            ("none", Some("none")),
            ("off", Some("none")),
            ("bogus", None),
        ] {
            let mut request = request(vec![ChatMessage::user("hi")]);
            request.reasoning_effort = Some(input.to_owned());

            let body = serde_json::to_value(build_request(&request)).expect("body serializes");

            match expected {
                Some(effort) => assert_eq!(body["reasoning"], json!({"effort": effort})),
                None => assert!(body.get("reasoning").is_none()),
            }
        }
    }
}
