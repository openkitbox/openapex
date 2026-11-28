//! OpenAI Responses request options and their strict merge rules.
//!
//! The same option object is declared at three levels: a `[providers.<id>]`
//! endpoint default, a `[models.<id>] candidates[]` override, and an internal
//! per-request override carried on `ChatRequest`. [`merge_options`] is the only
//! place that combines them, so an unknown key or an impossible combination is
//! rejected before a request reaches the wire. The wire translation consumes
//! the merged value; nothing here decides how a field is spelled on the wire.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// How the endpoint should retain state between Responses calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesStateMode {
    /// Stateless: no `previous_response_id`, no conversation.
    #[default]
    Manual,
    /// Continue from `previous_response_id`.
    PreviousResponse,
    /// Continue inside `conversation`.
    Conversation,
}

/// One optional Responses create-body parameter.
///
/// Every field is optional so it can be layered: `None` means "this layer has
/// no opinion", not "send null". Unknown keys fail at deserialization, which
/// is what makes a misspelled option a hard error instead of a silent drop.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct ResponsesOptions {
    /// State retention mode; defaults to [`ResponsesStateMode::Manual`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_mode: Option<ResponsesStateMode>,
    /// The response this call continues (`previous_response` mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Conversation id or creation parameters (`conversation` mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation: Option<Value>,
    /// System instructions for this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Prompt id or prompt object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Value>,
    /// Cache-scoped prompt options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<Value>,
    /// Output text settings (verbatim protocol object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<Value>,
    /// Reasoning settings (verbatim protocol object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    /// Extra output fields to include.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Maximum number of tool calls in one response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Whether the model may call several tools in parallel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Service tier (`auto`, `flex`, `priority`, `2h`, `8h`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Context overflow policy (`auto` or `disabled`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>,
    /// End-user identifier forwarded to the endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Opaque endpoint metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Whether this is a background (asynchronous) call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Background completion options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_options: Option<Value>,
    /// Context management settings (verbatim protocol object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Value>,
    /// Explicit `store` override; normally derived from [`Self::state_mode`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Additional protocol tools, appended after the translated tool list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
}

impl ResponsesOptions {
    /// Parse one option layer, rejecting unknown keys.
    pub fn from_json(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    /// Serialize the layer into an override object (only explicitly set keys).
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// The effective state mode, defaulting to manual.
    #[must_use]
    pub fn state_mode(&self) -> ResponsesStateMode {
        self.state_mode.unwrap_or_default()
    }

    /// Reject options that the protocol or ox cannot honour.
    fn validate(&self, stream: bool) -> Result<(), ResponsesOptionsError> {
        if self.conversation.is_some() && self.previous_response_id.is_some() {
            return Err(ResponsesOptionsError::invalid(
                "conversation",
                "cannot be combined with previous_response_id",
            ));
        }
        match self.state_mode() {
            ResponsesStateMode::Conversation if self.conversation.is_none() => {
                return Err(ResponsesOptionsError::invalid(
                    "state_mode",
                    "conversation mode requires conversation",
                ));
            }
            ResponsesStateMode::PreviousResponse if self.previous_response_id.is_none() => {
                return Err(ResponsesOptionsError::invalid(
                    "state_mode",
                    "previous_response mode requires previous_response_id",
                ));
            }
            ResponsesStateMode::Manual
                if self.conversation.is_some() || self.previous_response_id.is_some() =>
            {
                return Err(ResponsesOptionsError::invalid(
                    "state_mode",
                    "manual mode cannot carry conversation or previous_response_id",
                ));
            }
            _ => {}
        }
        if self.background_options.is_some() && self.background != Some(true) {
            return Err(ResponsesOptionsError::invalid(
                "background_options",
                "requires background = true",
            ));
        }
        if stream
            && self.background == Some(true)
            && matches!(self.service_tier.as_deref(), Some("2h" | "8h"))
        {
            return Err(ResponsesOptionsError::invalid(
                "background",
                "2h/8h service tiers cannot be combined with a streaming request",
            ));
        }
        if let Some(tier) = self.service_tier.as_deref()
            && !matches!(tier, "auto" | "flex" | "priority" | "2h" | "8h")
        {
            return Err(ResponsesOptionsError::invalid(
                "service_tier",
                "unknown service tier",
            ));
        }
        if let Some(truncation) = self.truncation.as_deref()
            && !matches!(truncation, "auto" | "disabled")
        {
            return Err(ResponsesOptionsError::invalid(
                "truncation",
                "expected auto or disabled",
            ));
        }
        if self.max_tool_calls == Some(0) {
            return Err(ResponsesOptionsError::invalid(
                "max_tool_calls",
                "must be positive",
            ));
        }
        Ok(())
    }
}

/// Why a set of Responses options was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResponsesOptionsError {
    /// A key's value or combination with another key is invalid.
    #[error("invalid Responses option `{key}`: {reason}")]
    Invalid {
        /// The offending option name.
        key: &'static str,
        /// Why it is invalid.
        reason: String,
    },
    /// An override object could not be decoded into options.
    #[error("invalid Responses option override: {0}")]
    Override(String),
}

impl ResponsesOptionsError {
    fn invalid(key: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            key,
            reason: reason.into(),
        }
    }
}

/// The request-level inputs to a merge: its option override and stream flag.
#[derive(Debug, Clone, Copy)]
pub struct RequestOptions<'a> {
    /// An internal override object, when the caller set one.
    pub overrides: Option<&'a Value>,
    /// Whether the request asks for a streaming response.
    pub stream: bool,
}

/// Merge three option layers, with later layers winning per key.
///
/// Provider defaults first, then the candidate's route override, then the
/// request override. `None` never overwrites a value from an earlier layer.
/// The result is validated whole: some combinations are only visible after
/// layering (for example a streaming request inheriting a 2h background tier).
pub fn merge_options(
    provider: Option<&ResponsesOptions>,
    candidate: Option<&ResponsesOptions>,
    request: Option<RequestOptions<'_>>,
) -> Result<ResponsesOptions, ResponsesOptionsError> {
    let parsed_request = match request.and_then(|request| request.overrides) {
        Some(value) => Some(
            ResponsesOptions::deserialize(value)
                .map_err(|error| ResponsesOptionsError::Override(error.to_string()))?,
        ),
        None => None,
    };

    /// Pick the most specific layer that stated this key.
    macro_rules! pick {
        ($field:ident) => {
            parsed_request
                .as_ref()
                .and_then(|options| options.$field.clone())
                .or_else(|| candidate.and_then(|options| options.$field.clone()))
                .or_else(|| provider.and_then(|options| options.$field.clone()))
        };
    }

    let merged = ResponsesOptions {
        state_mode: pick!(state_mode),
        previous_response_id: pick!(previous_response_id),
        conversation: pick!(conversation),
        instructions: pick!(instructions),
        prompt: pick!(prompt),
        prompt_cache_options: pick!(prompt_cache_options),
        text: pick!(text),
        reasoning: pick!(reasoning),
        include: pick!(include),
        max_tool_calls: pick!(max_tool_calls),
        parallel_tool_calls: pick!(parallel_tool_calls),
        service_tier: pick!(service_tier),
        truncation: pick!(truncation),
        user: pick!(user),
        metadata: pick!(metadata),
        background: pick!(background),
        background_options: pick!(background_options),
        context_management: pick!(context_management),
        store: pick!(store),
        tools: pick!(tools),
    };
    merged.validate(request.is_some_and(|request| request.stream))?;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_defaults_to_manual() {
        let merged = merge_options(None, None, None).expect("empty layers are valid");
        assert_eq!(merged.state_mode(), ResponsesStateMode::Manual);
        assert_eq!(
            merged.to_value().expect("compact object"),
            serde_json::json!({})
        );
    }

    #[test]
    fn conversation_mode_without_id_is_rejected() {
        let options = ResponsesOptions {
            state_mode: Some(ResponsesStateMode::Conversation),
            ..Default::default()
        };
        let error = merge_options(Some(&options), None, None)
            .expect_err("conversation mode needs a conversation");
        assert!(error.to_string().contains("conversation"), "{error}");
    }
}
