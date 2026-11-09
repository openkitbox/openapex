//! Anthropic Messages wire types and the one translation function that turns
//! ox's internal [`ChatRequest`] into them.
//!
//! ox's engine builds a chat-completions-shaped request (system as a message
//! role, `tool` results as their own role, `reasoning_effort` as a scalar).
//! Messages differs at every one of those points, so the translation lives
//! here, once, at the transport boundary — the engine stays protocol-blind and
//! the [`ChatRequest`] stays the single internal request form.
//!
//! Deliberate deviations from the completions shape, each forced by the
//! protocol:
//!
//! - **system** is a top-level string. Head `system` messages are joined into
//!   it in order; a system message that appears *after* the conversation began
//!   (ox inserts an instruction update before the current prompt) becomes a
//!   `user` turn, because Messages only accepts one system prompt and dropping
//!   the text would silently lose an instruction. This is what the reference
//!   harnesses do, and DeepSeek additionally documents in-history system
//!   messages as a fresh system prompt.
//! - **tool results** are `tool_result` blocks inside a `user` turn, and every
//!   result answering one assistant turn must ride that same turn: consecutive
//!   `tool` messages are merged into one user message.
//! - **reasoning** replays as a `thinking` block (optionally signed) rather
//!   than as `reasoning_content`.
//! - **effort** becomes `thinking` + `output_config`, per the endpoint's
//!   capability policy (see [`MessagesPolicy`]).

use serde::Serialize;
use serde_json::{Value, json};

use crate::cardinal::wire::{ChatMessage, ChatRequest, ToolChoice, UserContent, UserPart};
use crate::error::ChatError;

/// Which Messages dialect the endpoint speaks.
///
/// The protocol is one shape; providers differ in which optional knobs they
/// accept and how they want reasoning replayed. Everything a provider can
/// disagree about is here, so the translation has no provider `match` in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagesDialect {
    /// Anthropic's own endpoints: portable thinking budgets, unsigned
    /// reasoning blocks are invalid and therefore dropped on replay, mid-flow
    /// system prompts become user turns.
    Anthropic,
    /// DeepSeek's Anthropic-compatible endpoint
    /// (`https://api.deepseek.com/anthropic`): `thinking` is always sent
    /// (`enabled`/`disabled`), effort is one of `off|low|high|max` in
    /// `output_config`, unsigned reasoning is replayable, and a mid-flow
    /// `system` turn is read as the latest complete system prompt.
    DeepSeek,
}

/// How one Messages endpoint wants its requests spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessagesPolicy {
    /// The dialect (see [`MessagesDialect`]).
    pub dialect: MessagesDialect,
    /// `max_tokens` when the request sets none and the model is unknown.
    pub default_max_tokens: u32,
}

impl Default for MessagesPolicy {
    fn default() -> Self {
        Self::anthropic()
    }
}

impl MessagesPolicy {
    /// The conservative dialect: portable thinking budgets, unsigned reasoning
    /// dropped, mid-flow system prompts as user turns.
    #[must_use]
    pub fn anthropic() -> Self {
        Self {
            dialect: MessagesDialect::Anthropic,
            default_max_tokens: 8192,
        }
    }

    /// The DeepSeek dialect.
    #[must_use]
    pub fn deepseek() -> Self {
        Self {
            dialect: MessagesDialect::DeepSeek,
            default_max_tokens: 8192,
        }
    }

    /// The dialect a configured provider speaks. Unknown providers get the
    /// portable form; a provider that needs more opts in here.
    #[must_use]
    pub fn for_provider(provider: &str) -> Self {
        match provider {
            crate::config::DEEPSEEK => Self::deepseek(),
            _ => Self::anthropic(),
        }
    }

    /// Whether reasoning text with no signature can still be replayed.
    ///
    /// DeepSeek's compatibility layer replays thinking text and drops only the
    /// signature when it cannot be trusted; the native Anthropic API rejects
    /// an unsigned `thinking` block, so there the text is dropped instead.
    #[must_use]
    pub fn replays_unsigned_thinking(&self) -> bool {
        matches!(self.dialect, MessagesDialect::DeepSeek)
    }

    /// Whether a mid-conversation `system` message may stay a `system` turn
    /// (DeepSeek reads the latest one as the complete prompt) or must become a
    /// user turn (the native API takes one system prompt, at the top).
    #[must_use]
    pub fn keeps_system_in_history(&self) -> bool {
        matches!(self.dialect, MessagesDialect::DeepSeek)
    }

    /// The `max_tokens` for `model`: the request's own cap wins; otherwise a
    /// known DeepSeek model's output limit; otherwise the policy default.
    #[must_use]
    pub fn max_tokens_for(&self, model: &str, requested: Option<u64>) -> u32 {
        if let Some(tokens) = requested.and_then(|tokens| u32::try_from(tokens).ok()) {
            return tokens;
        }
        seed_of(model)
            .map(|seed| u32::try_from(seed.output).unwrap_or(self.default_max_tokens))
            .unwrap_or(self.default_max_tokens)
    }

    /// The `thinking` + `output_config` pair for a turn.
    ///
    /// DeepSeek always states the switch (the endpoint thinks by default, so
    /// silence is a decision too), accepts exactly `off|low|high|max`, and
    /// sends no `output_config` when thinking is off. Anthropic takes a fixed
    /// budget instead, and only when the turn asked for reasoning.
    fn thinking(
        &self,
        model: &str,
        effort: Option<&str>,
    ) -> (Option<Thinking>, Option<OutputConfig>) {
        let effort = effort
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .filter(|effort| !effort.is_empty() && effort != "default");
        let reasoning_model = seed_of(model).is_none_or(|seed| !seed.reasoning.is_empty());
        match self.dialect {
            MessagesDialect::DeepSeek => {
                if !reasoning_model {
                    return (None, None);
                }
                match effort.as_deref() {
                    Some("off" | "none" | "disabled" | "false") => (Some(Thinking::Disabled), None),
                    Some(level) => (
                        Some(Thinking::Enabled),
                        Some(OutputConfig {
                            effort: effort_name(level).to_owned(),
                        }),
                    ),
                    // No selection on a model that thinks by default: state
                    // the endpoint's own default explicitly, so a gateway
                    // default cannot silently disable reasoning.
                    None => (seed_of(model).map(|_| Thinking::Enabled), None),
                }
            }
            MessagesDialect::Anthropic => match effort.as_deref() {
                Some("off" | "none" | "disabled" | "false") => (Some(Thinking::Disabled), None),
                Some(level) => (
                    Some(Thinking::EnabledBudget {
                        budget_tokens: budget_for(level),
                    }),
                    None,
                ),
                None => (None, None),
            },
        }
    }
}

/// The DeepSeek seed for `model`, when the build knows it.
fn seed_of(model: &str) -> Option<&'static crate::config::SeedModel> {
    crate::config::builtin(crate::config::DEEPSEEK)?
        .models
        .iter()
        .find(|seed| seed.id == model)
}

/// A Messages request body.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    /// The single top-level system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<WireToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Always `true`: ox only streams.
    pub stream: bool,
}

/// One conversation turn.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WireMessage {
    pub role: &'static str,
    pub content: Vec<WireBlock>,
}

/// One content block. The tag values are the wire names.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireBlock {
    /// `{"type":"text","text":…}`
    Text {
        /// The text itself.
        text: String,
    },
    /// `{"type":"image","source":…}`
    Image {
        /// Where the bytes live.
        source: ImageSource,
    },
    /// `{"type":"tool_use","id":…,"name":…,"input":{…}}`
    ToolUse {
        /// Call id echoed by the matching result.
        id: String,
        /// Tool name.
        name: String,
        /// The parsed arguments (Messages carries an object, not a string).
        input: Value,
    },
    /// `{"type":"tool_result","tool_use_id":…,"content":…}`
    ToolResult {
        /// The call this answers.
        tool_use_id: String,
        /// The result text.
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        /// `true` for synthesized or failed results.
        is_error: Option<bool>,
    },
    /// `{"type":"thinking","thinking":…,"signature"?:…}`
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// The provider signature, when the row carries one.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// An image source: inline base64 or a URL.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// `{"type":"base64","media_type":…,"data":…}`
    Base64 {
        /// The MIME type, e.g. `image/png`.
        media_type: String,
        /// Base64 payload without the data-URL prefix.
        data: String,
    },
    /// `{"type":"url","url":…}`
    Url {
        /// The absolute URL.
        url: String,
    },
}

/// One tool definition: `{name, description?, input_schema}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WireTool {
    /// Tool name.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Tool description.
    pub description: Option<String>,
    /// JSON Schema for the arguments.
    pub input_schema: Value,
}

/// `tool_choice` in Messages form.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireToolChoice {
    /// The model decides.
    Auto,
    /// The model must not call tools.
    None,
    /// The model must call at least one tool.
    Any,
}

/// The `thinking` request field.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Thinking {
    /// `{"type":"enabled"}` — the endpoint sizes the budget itself (DeepSeek
    /// takes the level from `output_config.effort`).
    Enabled,
    /// `{"type":"enabled","budget_tokens":N}` — the portable form, used by
    /// Anthropic endpoints that do not take an effort level.
    EnabledBudget {
        /// Tokens the model may spend thinking.
        budget_tokens: u32,
    },
    /// `{"type":"disabled"}` — an explicit off.
    Disabled,
}

/// The `output_config` request field (effort level, DeepSeek).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OutputConfig {
    /// `low` | `medium` | `high` | `max`.
    pub effort: String,
}

/// The message-ox-received placeholder for an unanswered tool call: Messages
/// rejects an assistant `tool_use` with no result, and ox can emit one when a
/// crash lands mid-execution.
const NO_RESULT_CONTENT: &str = "[no result recorded: interrupted or folded]";

/// Translate a completions-shaped [`ChatRequest`] into a Messages body.
///
/// Infallible in practice (every field has a legal Messages spelling), but
/// returns [`ChatError`] so a future invalid combination has somewhere to go.
pub fn build_request(
    request: &ChatRequest,
    policy: &MessagesPolicy,
) -> Result<MessagesRequest, ChatError> {
    let mut system = Vec::new();
    let mut messages: Vec<WireMessage> = Vec::new();
    // Tool results collected for the turn currently being assembled; they must
    // all ride one `user` message directly after the assistant turn.
    let mut pending_results: Vec<WireBlock> = Vec::new();

    let flush_results = |messages: &mut Vec<WireMessage>, pending: &mut Vec<WireBlock>| {
        if pending.is_empty() {
            return;
        }
        let content = std::mem::take(pending);
        match messages.last_mut() {
            // A user turn that already exists (a media follow-up after a tool
            // result) absorbs the results: results must lead the turn.
            Some(WireMessage {
                role: "user",
                content: existing,
            }) => {
                let mut merged = content;
                merged.append(existing);
                *existing = merged;
            }
            _ => messages.push(WireMessage {
                role: "user",
                content,
            }),
        }
    };

    for message in &request.messages {
        match message {
            ChatMessage::System { content } => {
                // A system message before any conversation turn is the
                // top-level prompt; one after it is an in-flow instruction
                // update and becomes a user turn.
                if messages.is_empty() && pending_results.is_empty() {
                    system.push(content.clone());
                } else {
                    flush_results(&mut messages, &mut pending_results);
                    // DeepSeek reads the latest `system` turn as the complete
                    // system prompt wherever it sits, so it stays a system
                    // turn; the native API takes one system prompt at the top,
                    // so there it becomes a user turn rather than vanishing.
                    let role = if policy.keeps_system_in_history() {
                        "system"
                    } else {
                        "user"
                    };
                    messages.push(WireMessage {
                        role,
                        content: vec![WireBlock::Text {
                            text: content.clone(),
                        }],
                    });
                }
            }
            ChatMessage::User { content } => {
                // A user turn right after tool results (an image attachment
                // following its tool's result) joins them: the results must
                // lead the turn they answer, and a second consecutive user
                // turn would leave them stranded in the previous one.
                let mut blocks = std::mem::take(&mut pending_results);
                blocks.extend(user_blocks(content));
                if !blocks.is_empty() {
                    messages.push(WireMessage {
                        role: "user",
                        content: blocks,
                    });
                }
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                reasoning_content,
            } => {
                flush_results(&mut messages, &mut pending_results);
                let mut blocks = Vec::new();
                if let Some(reasoning) = reasoning_content
                    && !reasoning.is_empty()
                    && policy.replays_unsigned_thinking()
                {
                    // No signature is stored on the row; the policy decides
                    // whether an unsigned block is legal on this endpoint
                    // (DeepSeek replays reasoning, Anthropic does not).
                    blocks.push(WireBlock::Thinking {
                        thinking: reasoning.clone(),
                        signature: None,
                    });
                }
                if let Some(text) = content
                    && !text.is_empty()
                {
                    blocks.push(WireBlock::Text { text: text.clone() });
                }
                for call in tool_calls.iter().flatten() {
                    blocks.push(WireBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        input: parse_arguments(&call.function.arguments),
                    });
                }
                if blocks.is_empty() {
                    continue;
                }
                messages.push(WireMessage {
                    role: "assistant",
                    content: blocks,
                });
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                pending_results.push(WireBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: content.clone(),
                    // ox's synthesized placeholder is an error in spirit: the
                    // call never produced a result.
                    is_error: (content == NO_RESULT_CONTENT).then_some(true),
                });
            }
        }
    }
    flush_results(&mut messages, &mut pending_results);

    let tools: Vec<WireTool> = request
        .tools
        .iter()
        .map(|tool| WireTool {
            name: tool.function.name.clone(),
            description: if tool.function.description.is_empty() {
                None
            } else {
                Some(tool.function.description.clone())
            },
            input_schema: tool.function.parameters.clone(),
        })
        .collect();
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(match request.tool_choice {
            ToolChoice::Auto => WireToolChoice::Auto,
            ToolChoice::None => WireToolChoice::None,
            ToolChoice::Required => WireToolChoice::Any,
        })
    };
    let (thinking, output_config) =
        policy.thinking(&request.model, request.reasoning_effort.as_deref());
    let max_tokens = policy.max_tokens_for(&request.model, request.max_tokens);
    if request
        .max_tokens
        .is_some_and(|tokens| tokens == 0 || tokens > u64::from(u32::MAX))
    {
        return Err(ChatError::Http {
            status: 400,
            body: "context_length_exceeded: output cap is not representable by Messages".to_owned(),
            retry_after: None,
        });
    }
    if let Some(Thinking::EnabledBudget { budget_tokens }) = &thinking
        && *budget_tokens >= max_tokens
    {
        return Err(ChatError::Http {
            status: 400,
            body:
                "context_length_exceeded: output room cannot accommodate selected thinking budget"
                    .to_owned(),
            retry_after: None,
        });
    }

    // `system` is the *join* of every `System` message in the list, so a
    // request carrying one more or one fewer of them opens differently even
    // when the message bodies agree — and a prompt cache keys on exactly that,
    // so a divergence here costs the whole prefix. Logged as counts and hashes
    // rather than text, which keeps the line safe to leave on.
    let system_blocks = system.len();
    let system = (!system.is_empty()).then(|| system.join("\n\n"));
    if tracing::enabled!(tracing::Level::DEBUG) {
        let joined = system.as_deref().unwrap_or_default();
        tracing::debug!(
            system_blocks,
            system_bytes = joined.len(),
            system_hash = hash_bytes(joined.as_bytes()),
            tools = tools.len(),
            messages = messages.len(),
            "messages request shape"
        );
    }

    Ok(MessagesRequest {
        model: request.model.clone(),
        max_tokens,
        system,
        messages,
        tools,
        tool_choice,
        thinking,
        output_config,
        temperature: request.temperature,
        stream: true,
    })
}

/// The wire effort name for a variant name. `max`/`xhigh` collapse to `max`,
/// `medium`/`mid` to `medium`; anything unrecognized asks for `high`, the
/// densest level every adaptive endpoint accepts.
fn effort_name(level: &str) -> &'static str {
    // DeepSeek accepts exactly `off|low|high|max`; everything else snaps to a
    // neighbouring level rather than 400ing the turn.
    match level {
        "low" | "minimal" => "low",
        "max" | "xhigh" | "highest" => "max",
        _ => "high",
    }
}

/// The portable thinking budget for a variant name.
fn budget_for(level: &str) -> u32 {
    match level {
        "low" | "minimal" => 4096,
        "medium" | "mid" => 8192,
        "max" | "xhigh" | "highest" => 32768,
        _ => 16384,
    }
}

/// The content blocks of a user turn: text, or text plus images (a data URL
/// becomes inline base64, any other URL stays a URL).
fn user_blocks(content: &UserContent) -> Vec<WireBlock> {
    match content {
        UserContent::Text(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![WireBlock::Text { text: text.clone() }]
            }
        }
        UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                UserPart::Text { text } => Some(WireBlock::Text { text: text.clone() }),
                UserPart::Image { image_url } => image_block(&image_url.url),
            })
            .collect(),
    }
}

/// One image part as a Messages block; an unparseable reference degrades to a
/// visible text block instead of a failed turn.
fn image_block(url: &str) -> Option<WireBlock> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (header, data) = rest.split_once(',')?;
        if !header.ends_with(";base64") {
            return Some(WireBlock::Text {
                text: format!("[unsupported image reference: {url}]"),
            });
        }
        let media_type = header.trim_end_matches(";base64").to_owned();
        return Some(WireBlock::Image {
            source: ImageSource::Base64 {
                media_type,
                data: data.to_owned(),
            },
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(WireBlock::Image {
            source: ImageSource::Url {
                url: url.to_owned(),
            },
        });
    }
    Some(WireBlock::Text {
        text: format!("[unsupported image reference: {url}]"),
    })
}

/// Tool arguments as a JSON object for `tool_use.input`. An unparseable
/// argument string degrades to `{}`: the reference harnesses do the same rather
/// than failing the whole turn.
fn parse_arguments(arguments: &str) -> Value {
    if arguments.is_empty() {
        return json!({});
    }
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

/// FNV-1a over a byte slice: a stable, dependency-free fingerprint for the
/// debug line above. Collisions are irrelevant here; it only has to tell two
/// renderings apart within one log.
fn hash_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3)
    })
}
