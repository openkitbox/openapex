//! OpenAI Responses request bodies and the one translation from ox's internal
//! [`ChatRequest`] into them.
//!
//! Responses is not Chat Completions with a different path: system instructions
//! leave the `input` array for `instructions`, history is a flat array of typed
//! items, tools are flattened to `{type, name, description, parameters}`, and
//! reasoning has its own object. The translation lives here so the engine keeps
//! building one request shape and never learns which wire protocol an endpoint
//! speaks.
//!
//! The builder borrows history text, tool schemas, option objects and native
//! replay items from its arguments; only the unavoidable wrapper enums and the
//! fingerprint string are allocated. State-chain modes compare the fingerprint
//! of the logical prefix before dropping it: a changed prefix is a hard error,
//! never a silent truncation.

use std::borrow::Cow;
use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

// Integration tests inspect create bodies through this crate; re-exporting the
// JSON type here keeps them from needing a separate root dependency.
pub use serde_json;

use super::options::{ResponsesOptions, ResponsesStateMode};
use super::replay::ResponsesReplayState;
use crate::chat_completions::wire::{ChatMessage, ChatRequest, ToolChoice, UserContent, UserPart};
use crate::error::ChatError;

/// A `/v1/responses` create body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesRequest<'a> {
    /// Model identifier as the endpoint exposes it.
    pub model: &'a str,
    /// Input items: messages, function calls and their results, in order.
    pub input: Vec<InputItem<'a>>,
    /// System/developer instructions, joined with the option instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Cow<'a, str>>,
    /// Function tools followed by any verbatim protocol tools.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ResponsesTool<'a>>,
    /// `none` / `auto` / `required`; omitted without tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Sampling temperature, when the turn set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Completion cap, translated from `ChatRequest::max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Explicit reasoning object or the translated effort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningSpec<'a>>,
    /// Extra output fields; manual mode defaults to encrypted reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Include<'a>>,
    /// Prompt id or object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<&'a Value>,
    /// Cache-scoped prompt options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<&'a Value>,
    /// Verbatim output text settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a Value>,
    /// Maximum tool calls in one response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Whether parallel tool calls are allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Service tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<&'a str>,
    /// Context overflow policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<&'a str>,
    /// End-user identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<&'a str>,
    /// Verbatim endpoint metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a Value>,
    /// Background execution flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Verbatim background completion options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_options: Option<&'a Value>,
    /// Verbatim context management settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<&'a Value>,
    /// Continuation anchor for `previous_response` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<&'a str>,
    /// Continuation anchor for `conversation` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<&'a Value>,
    /// Streaming for `soonest` calls; long background tiers are polled.
    pub stream: bool,
    /// Server-side retention: off in manual mode, on in chain modes.
    pub store: bool,
    /// Fingerprint of the full logical input, before any suffix trimming.
    /// The caller stores it in the replay state so the next turn can confirm
    /// that the prefix it is about to drop is unchanged. Never on the wire.
    #[serde(skip)]
    fingerprint: Option<(String, usize)>,
}

impl ResponsesRequest<'_> {
    /// Fingerprint of the full logical input, for the next turn's replay state.
    #[must_use]
    pub fn logical_fingerprint(&self) -> Option<(&str, usize)> {
        self.fingerprint
            .as_ref()
            .map(|(hash, len)| (hash.as_str(), *len))
    }
}

/// One entry of the Responses `input` array. All borrowed fields point into
/// the request or the replay state, so rebuilding a turn never copies history.
#[derive(Debug, Clone, PartialEq)]
pub enum InputItem<'a> {
    /// A role-labelled message with typed content parts.
    Message {
        /// `user` or `assistant` (system text goes into `instructions`).
        role: &'static str,
        /// Content parts in this message.
        content: Vec<InputContent<'a>>,
    },
    /// An assistant tool call replayed from history.
    FunctionCall {
        /// Call id echoed by the matching result.
        call_id: &'a str,
        /// Function name.
        name: &'a str,
        /// JSON arguments string, as ox recorded them.
        arguments: &'a str,
    },
    /// The result of a prior function call.
    FunctionCallOutput {
        /// The call this answers.
        call_id: &'a str,
        /// Result text.
        output: &'a str,
    },
    /// A native Responses item replayed verbatim from the previous turn.
    Raw(&'a Value),
}

impl Serialize for InputItem<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Message<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            role: &'static str,
            content: &'a [InputContent<'a>],
        }
        #[derive(Serialize)]
        struct FunctionCall<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            call_id: &'a str,
            name: &'a str,
            arguments: &'a str,
        }
        #[derive(Serialize)]
        struct FunctionCallOutput<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            call_id: &'a str,
            output: &'a str,
        }

        match self {
            Self::Message { role, content } => Message {
                kind: "message",
                role,
                content,
            }
            .serialize(serializer),
            Self::FunctionCall {
                call_id,
                name,
                arguments,
            } => FunctionCall {
                kind: "function_call",
                call_id,
                name,
                arguments,
            }
            .serialize(serializer),
            Self::FunctionCallOutput { call_id, output } => FunctionCallOutput {
                kind: "function_call_output",
                call_id,
                output,
            }
            .serialize(serializer),
            Self::Raw(value) => value.serialize(serializer),
        }
    }
}

/// One typed part of a Responses input message.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent<'a> {
    /// User or developer text.
    InputText {
        /// The text itself.
        text: &'a str,
    },
    /// A user image reference (URL or data URL).
    InputImage {
        /// Image URL.
        image_url: &'a str,
    },
    /// Assistant output text.
    OutputText {
        /// The text itself.
        text: &'a str,
    },
}

/// A translated function tool or a verbatim protocol tool from the options.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponsesTool<'a> {
    /// A tool translated from ox's normalized tool registry.
    Function {
        /// Function name.
        name: &'a str,
        /// Function description, omitted when empty.
        description: Option<&'a str>,
        /// JSON Schema for the arguments.
        parameters: &'a Value,
    },
    /// An additional tool object forwarded unchanged.
    Raw(&'a Value),
}

impl Serialize for ResponsesTool<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Function<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            name: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            description: Option<&'a str>,
            parameters: &'a Value,
        }

        match self {
            Self::Function {
                name,
                description,
                parameters,
            } => Function {
                kind: "function",
                name,
                description: *description,
                parameters,
            }
            .serialize(serializer),
            Self::Raw(value) => value.serialize(serializer),
        }
    }
}

/// The `reasoning` request object.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reasoning {
    /// One of `none`, `low`, `medium`, `high`, `xhigh`, `max`.
    pub effort: &'static str,
}

/// An explicit reasoning option wins over the translated effort.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ReasoningSpec<'a> {
    /// Effort derived from the session's model variant.
    Effort(Reasoning),
    /// Verbatim reasoning object from the options.
    Raw(&'a Value),
}

/// The `include` array: configured values, or manual mode's safe default.
#[derive(Debug, Clone, PartialEq)]
pub enum Include<'a> {
    /// Values from the option layer.
    Configured(&'a [String]),
    /// `["reasoning.encrypted_content"]` for stateless replay.
    Manual,
}

impl Serialize for Include<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Configured(values) => values.serialize(serializer),
            Self::Manual => ["reasoning.encrypted_content"].serialize(serializer),
        }
    }
}

/// Translate a completions-shaped [`ChatRequest`] into a Responses create body.
///
/// Chain modes fail closed: without a matching replay state and an unchanged
/// logical prefix they return [`ChatError::Upstream`] instead of sending a
/// truncated `input`. Manual mode accepts stale or foreign state but drops its
/// native items, falling back to the readable history in the request.
///
/// The caller owns the origin gate (provider, endpoint, protocol marker) before
/// passing a state here; the wire only rechecks the model and marker so a
/// Messages state cannot be misrouted into a Responses request.
pub fn build_request<'a>(
    request: &'a ChatRequest,
    options: &'a ResponsesOptions,
    replay: Option<&'a ResponsesReplayState>,
) -> Result<ResponsesRequest<'a>, ChatError> {
    if options.conversation.is_some() && options.previous_response_id.is_some() {
        return Err(ChatError::Upstream(
            "Responses: conversation and previous_response_id are mutually exclusive; \
             keep only one state anchor"
                .to_owned(),
        ));
    }

    let mode = options.state_mode();
    match mode {
        ResponsesStateMode::Manual => {
            if options.conversation.is_some() || options.previous_response_id.is_some() {
                return Err(ChatError::Upstream(
                    "Responses: a state anchor requires previous_response or conversation mode"
                        .to_owned(),
                ));
            }
        }
        ResponsesStateMode::PreviousResponse => {
            if options.previous_response_id.is_none() {
                return Err(ChatError::Upstream(
                    "Responses: previous_response mode requires previous_response_id".to_owned(),
                ));
            }
        }
        ResponsesStateMode::Conversation => {
            if options.conversation.is_none() {
                return Err(ChatError::Upstream(
                    "Responses: conversation mode requires conversation".to_owned(),
                ));
            }
        }
    }

    let usable = replay.filter(|state| {
        state.model == request.model && state.protocol_marker.contains("responses")
    });
    if replay.is_some() && usable.is_none() {
        tracing::debug!(
            "Responses replay state belongs to another model or protocol; using readable history"
        );
    }
    let state = match (mode, usable) {
        (ResponsesStateMode::Manual, state) => state,
        (_, Some(state)) => Some(state),
        (_, None) => {
            return Err(ChatError::Upstream(
                "Responses: chained modes require matching native replay state; \
                 reset the state or use manual mode"
                    .to_owned(),
            ));
        }
    };

    let last_assistant = request
        .messages
        .iter()
        .rposition(|message| matches!(message, ChatMessage::Assistant { .. }));
    let replay_at = state.and(last_assistant);

    let mut instructions: Option<Cow<'a, str>> = None;
    let mut input = Vec::with_capacity(request.messages.len());
    let mut boundary = None;

    for (index, message) in request.messages.iter().enumerate() {
        match message {
            ChatMessage::System { content } => push_instructions(&mut instructions, content),
            ChatMessage::User { content } => {
                let parts = user_content(content);
                if !parts.is_empty() {
                    input.push(InputItem::Message {
                        role: "user",
                        content: parts,
                    });
                }
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                reasoning_content,
                replay_state: _,
            } => {
                if Some(index) == replay_at {
                    let items = &state.expect("replay_at is set only with state").items;
                    boundary = Some(input.len() + items.len());
                    input.extend(items.iter().map(InputItem::Raw));
                } else {
                    if let Some(reasoning) = reasoning_content
                        && !reasoning.is_empty()
                    {
                        tracing::debug!(
                            bytes = reasoning.len(),
                            "dropping assistant reasoning_content: no Responses reasoning item to replay"
                        );
                    }
                    if let Some(text) = content.as_deref().filter(|text| !text.is_empty()) {
                        input.push(InputItem::Message {
                            role: "assistant",
                            content: vec![InputContent::OutputText { text }],
                        });
                    }
                    if let Some(calls) = tool_calls {
                        for call in calls {
                            input.push(InputItem::FunctionCall {
                                call_id: &call.id,
                                name: &call.function.name,
                                arguments: &call.function.arguments,
                            });
                        }
                    }
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => input.push(InputItem::FunctionCallOutput {
                call_id: tool_call_id,
                output: content,
            }),
        }
    }
    if let Some(extra) = &options.instructions {
        push_instructions(&mut instructions, extra);
    }

    let fingerprint = input_fingerprint(&input);

    if mode != ResponsesStateMode::Manual {
        let end = match boundary {
            Some(end) => end,
            None => {
                return Err(ChatError::Upstream(
                    "Responses: chained request has no assistant turn to anchor its prefix"
                        .to_owned(),
                ));
            }
        };
        let (prefix, len) = input_fingerprint(&input[..end]);
        if !state
            .expect("a chained mode always has a state")
            .fingerprint_matches(&prefix, len)
        {
            return Err(ChatError::Upstream(format!(
                "Responses: input prefix changed since the stored response \
                 ({len} bytes, {prefix}); reset the chain or use manual mode"
            )));
        }
        input = input.split_off(end);
    }

    let mut tools = request
        .tools
        .iter()
        .map(|tool| ResponsesTool::Function {
            name: &tool.function.name,
            description: (!tool.function.description.is_empty())
                .then_some(tool.function.description.as_str()),
            parameters: &tool.function.parameters,
        })
        .collect::<Vec<_>>();
    if let Some(extra) = &options.tools {
        tools.extend(extra.iter().map(ResponsesTool::Raw));
    }

    let reasoning = match &options.reasoning {
        Some(raw) => Some(ReasoningSpec::Raw(raw)),
        None => request
            .reasoning_effort
            .as_deref()
            .and_then(effort)
            .map(ReasoningSpec::Effort),
    };
    let include = match (&options.include, mode) {
        (Some(values), _) => Some(Include::Configured(values)),
        (None, ResponsesStateMode::Manual) => Some(Include::Manual),
        (None, _) => None,
    };
    let long_background = options.background == Some(true)
        && matches!(options.service_tier.as_deref(), Some("2h" | "8h"));

    Ok(ResponsesRequest {
        model: &request.model,
        input,
        instructions,
        tools,
        tool_choice: (!request.tools.is_empty()
            || options
                .tools
                .as_ref()
                .is_some_and(|tools| !tools.is_empty()))
        .then_some(request.tool_choice),
        temperature: request.temperature,
        max_output_tokens: request.max_tokens,
        reasoning,
        include,
        prompt: options.prompt.as_ref(),
        prompt_cache_options: options.prompt_cache_options.as_ref(),
        text: options.text.as_ref(),
        max_tool_calls: options.max_tool_calls,
        parallel_tool_calls: options.parallel_tool_calls,
        service_tier: options.service_tier.as_deref(),
        truncation: options.truncation.as_deref(),
        user: options.user.as_deref(),
        metadata: options.metadata.as_ref(),
        background: options.background,
        background_options: options.background_options.as_ref(),
        context_management: options.context_management.as_ref(),
        previous_response_id: if mode == ResponsesStateMode::PreviousResponse {
            options.previous_response_id.as_deref()
        } else {
            None
        },
        conversation: if mode == ResponsesStateMode::Conversation {
            options.conversation.as_ref()
        } else {
            None
        },
        stream: !long_background,
        store: options.store.unwrap_or(mode != ResponsesStateMode::Manual),
        fingerprint: Some(fingerprint),
    })
}

/// FNV-1a over the canonical JSON of the input items, with the byte length.
/// A streaming hasher avoids materializing the serialized prefix.
///
/// The caller puts the pair into [`ResponsesReplayState`] when the response
/// completes; the next turn compares it before dropping the same prefix.
#[must_use]
pub fn input_fingerprint(items: &[InputItem<'_>]) -> (String, usize) {
    struct Sink {
        hash: u64,
        len: usize,
    }

    impl Default for Sink {
        fn default() -> Self {
            Self {
                hash: 0xcbf2_9ce4_8422_2325,
                len: 0,
            }
        }
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            for byte in buf {
                self.hash = (self.hash ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3);
            }
            self.len += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut sink = Sink::default();
    if serde_json::to_writer(&mut sink, items).is_err() {
        return (String::new(), 0);
    }
    (format!("fnv:{:016x}", sink.hash), sink.len)
}

/// Append one instruction source, allocating only when a second one appears.
fn push_instructions<'a>(target: &mut Option<Cow<'a, str>>, text: &'a str) {
    if text.is_empty() {
        return;
    }
    match target {
        None => *target = Some(Cow::Borrowed(text)),
        Some(Cow::Borrowed(existing)) => {
            let existing = *existing;
            let mut joined = String::with_capacity(existing.len() + 2 + text.len());
            joined.push_str(existing);
            joined.push_str("\n\n");
            joined.push_str(text);
            *target = Some(Cow::Owned(joined));
        }
        Some(Cow::Owned(joined)) => {
            joined.push_str("\n\n");
            joined.push_str(text);
        }
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

/// The typed content parts of a user turn, borrowed from the request.
fn user_content(content: &UserContent) -> Vec<InputContent<'_>> {
    match content {
        UserContent::Text(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![InputContent::InputText { text }]
            }
        }
        UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                UserPart::Text { text } => {
                    (!text.is_empty()).then(|| InputContent::InputText { text })
                }
                UserPart::Image { image_url } => Some(InputContent::InputImage {
                    image_url: &image_url.url,
                }),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::{build_request, input_fingerprint};
    use crate::chat_completions::wire::{
        ChatMessage, ChatRequest, FunctionCall, FunctionType, ToolCall, ToolDef, UserContent,
        UserPart,
    };
    use crate::error::ChatError;
    use crate::responses::options::{ResponsesOptions, ResponsesStateMode};
    use crate::responses::replay::{ResponsesOrigin, ResponsesReplayInput, ResponsesReplayState};

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest::new("gpt-5", messages)
    }

    fn replay(model: &str, fingerprint: &str, len: usize) -> ResponsesReplayState {
        ResponsesReplayState::new(
            ResponsesOrigin::new(
                "vendor",
                "https://vendor.example/v1/responses",
                "responses/v1",
            ),
            ResponsesReplayInput {
                model: model.to_owned(),
                items: vec![json!({"type": "message", "role": "assistant"})],
                input_fingerprint: fingerprint.to_owned(),
                input_len: len,
                ..Default::default()
            },
        )
    }

    fn assistant() -> ChatMessage {
        ChatMessage::Assistant {
            content: Some("old".to_owned()),
            tool_calls: None,
            reasoning_content: None,
            replay_state: None,
        }
    }

    #[test]
    fn body_uses_responses_shape_only() {
        let mut request = request(vec![ChatMessage::user("hi")]);
        request.temperature = Some(0.3);
        request.max_tokens = Some(128);

        let options = ResponsesOptions::default();
        let body = build_request(&request, &options, None).expect("valid body");
        let value = serde_json::to_value(body).expect("body serializes");

        assert_eq!(value["model"], json!("gpt-5"));
        assert_eq!(
            value["input"],
            json!([{"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]}])
        );
        assert_eq!(value["temperature"], json!(0.3));
        assert_eq!(value["max_output_tokens"], json!(128));
        assert_eq!(value["stream"], json!(true));
        assert_eq!(value["store"], json!(false));
        assert_eq!(value["include"], json!(["reasoning.encrypted_content"]));
        for forbidden in [
            "messages",
            "stream_options",
            "previous_response_id",
            "tools",
            "tool_choice",
            "reasoning",
        ] {
            assert!(
                value.get(forbidden).is_none(),
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
                        image_url: crate::chat_completions::wire::ImageUrl {
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

        let value: Value = serde_json::to_value(
            build_request(&request, &ResponsesOptions::default(), None).expect("valid body"),
        )
        .expect("body serializes");

        assert_eq!(
            value["input"],
            json!([
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
        assert_eq!(value["instructions"], json!("be brief"));
        assert_eq!(
            value["tools"],
            json!([{"type": "function", "name": "read",
            "description": "read a file", "parameters": {"type": "object"}}])
        );
        assert_eq!(value["tool_choice"], json!("auto"));
        assert!(!value.to_string().contains("must not be replayed"));
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

            let value = serde_json::to_value(
                build_request(&request, &ResponsesOptions::default(), None).expect("valid body"),
            )
            .expect("body serializes");

            match expected {
                Some(effort) => assert_eq!(value["reasoning"], json!({"effort": effort})),
                None => assert!(value.get("reasoning").is_none()),
            }
        }
    }

    #[test]
    fn prefix_mismatch_rejects_chain_modes_but_manual_keeps_full_history() {
        let request = request(vec![
            ChatMessage::user("hello"),
            assistant(),
            ChatMessage::user("next"),
        ]);
        let state = replay("gpt-5", "fnv:stale", 999);
        for options in [
            ResponsesOptions {
                state_mode: Some(ResponsesStateMode::PreviousResponse),
                previous_response_id: Some("resp_1".to_owned()),
                ..Default::default()
            },
            ResponsesOptions {
                state_mode: Some(ResponsesStateMode::Conversation),
                conversation: Some(json!("conv_1")),
                ..Default::default()
            },
        ] {
            let error = build_request(&request, &options, Some(&state))
                .expect_err("a changed prefix must reject the chain");
            assert!(matches!(error, ChatError::Upstream(_)), "{error:?}");
        }
        let value = serde_json::to_value(
            build_request(&request, &ResponsesOptions::default(), Some(&state))
                .expect("manual mode falls back to history"),
        )
        .expect("body serializes");
        assert_eq!(value["input"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn conversation_and_previous_response_id_are_mutually_exclusive() {
        let request = request(vec![ChatMessage::user("hi")]);
        let options = ResponsesOptions {
            state_mode: Some(ResponsesStateMode::PreviousResponse),
            previous_response_id: Some("resp_1".to_owned()),
            conversation: Some(json!("conv_1")),
            ..Default::default()
        };
        assert!(matches!(
            build_request(&request, &options, None),
            Err(ChatError::Upstream(_))
        ));
    }

    #[test]
    fn fingerprint_hashes_items_without_serializing_twice() {
        let request = request(vec![ChatMessage::user("hello")]);
        let options = ResponsesOptions::default();
        let body = build_request(&request, &options, None).expect("valid body");
        let (hash, len) = body
            .logical_fingerprint()
            .expect("every body carries its fingerprint");
        let expected = serde_json::to_string(&body.input).expect("items serialize");
        assert_eq!(len, expected.len());
        assert!(hash.starts_with("fnv:"));
        assert_eq!(
            input_fingerprint(&body.input),
            (hash.to_owned(), len),
            "the same items hash identically"
        );
    }
}
