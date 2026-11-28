//! Responses option parsing, strict merging and the internal ChatRequest seam.
//!
//! These tests exercise the real config loader (TOML → `Config`) and the pure
//! merge decision, without a live endpoint.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use ox::config::Config;
use ox::llm::chat_completions::wire::{ChatMessage, ChatRequest, serialized_len};
use ox::llm::messages::wire::MessagesDialect;
use ox::llm::responses::options::{
    RequestOptions, ResponsesOptions, ResponsesStateMode, merge_options,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Load a TOML file through the production config path in an isolated directory.
fn load(raw: &str) -> anyhow::Result<Config> {
    let dir = std::env::temp_dir()
        .join("ox-responses-options-tests")
        .join(format!("{}", std::process::id()))
        .join(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
    fs::create_dir_all(&dir)?;
    let path = dir.join("ox.toml");
    fs::write(&path, raw)?;
    let config = Config::load_from(path);
    let _ = fs::remove_dir_all(&dir);
    config
}

#[test]
fn config_parses_provider_and_candidate_options_and_explicit_messages_dialect() {
    let config = load(
        r#"
[providers.resp]
base_url = "https://example.com"
protocol = "responses"
messages_dialect = "deepseek"
responses_options.instructions = "provider instruction"
responses_options.include = ["file_search_call.results"]
responses_options.state_mode = "manual"

[models.m]
candidates = [
  { provider = "resp", model = "m1", responses_options = { service_tier = "priority", state_mode = "previous_response", previous_response_id = "resp_1" } },
]
"#,
    )
    .expect("valid Responses options");

    let provider = config
        .llm
        .responses_options_providers
        .get("resp")
        .expect("provider-level options were parsed");
    assert_eq!(
        provider.instructions.as_deref(),
        Some("provider instruction")
    );
    assert_eq!(
        provider.include.as_deref(),
        Some(["file_search_call.results".to_owned()].as_slice())
    );
    assert_eq!(provider.state_mode, Some(ResponsesStateMode::Manual));

    let candidate = config
        .llm
        .responses_options_candidates
        .get(&("m".to_owned(), 0))
        .expect("candidate-level options were parsed");
    assert_eq!(candidate.service_tier.as_deref(), Some("priority"));
    assert_eq!(
        candidate.state_mode,
        Some(ResponsesStateMode::PreviousResponse)
    );
    assert_eq!(
        config.llm.messages_dialect_for("resp"),
        MessagesDialect::DeepSeek
    );
}

#[test]
fn unknown_response_option_key_is_rejected_at_load() {
    let error = load(
        r#"
[providers.resp]
base_url = "https://example.com"
protocol = "responses"
responses_options.nope = true
"#,
    )
    .expect_err("an unknown Responses option must not be silently ignored");
    assert!(
        error.to_string().contains("nope"),
        "error should name the unknown key: {error}"
    );
}

#[test]
fn merge_rejects_conversation_and_previous_response_id() {
    let config = load(
        r#"
[providers.resp]
base_url = "https://example.com"
protocol = "responses"
responses_options.conversation = "conv_1"
responses_options.previous_response_id = "resp_1"
responses_options.state_mode = "conversation"
"#,
    )
    .expect("the config itself parses; the combination is invalid at merge");
    let options = config
        .llm
        .responses_options_providers
        .get("resp")
        .expect("provider options");
    let error = merge_options(Some(options), None, None)
        .expect_err("conversation and previous_response_id are mutually exclusive");
    assert!(
        error.to_string().contains("previous_response_id"),
        "error should name the conflicting key: {error}"
    );
}

#[test]
fn merge_rejects_background_long_tier_for_streaming_requests() {
    let config = load(
        r#"
[providers.resp]
base_url = "https://example.com"
protocol = "responses"
responses_options.background = true
responses_options.service_tier = "2h"
"#,
    )
    .expect("the config itself parses; the stream conflict is invalid at merge");
    let options = config
        .llm
        .responses_options_providers
        .get("resp")
        .expect("provider options");
    let streaming = RequestOptions {
        overrides: None,
        stream: true,
    };
    let error = merge_options(Some(options), None, Some(streaming))
        .expect_err("2h background work cannot be streamed");
    assert!(
        error.to_string().contains("background"),
        "error should name the conflicting combination: {error}"
    );
    merge_options(
        Some(options),
        None,
        Some(RequestOptions {
            overrides: None,
            stream: false,
        }),
    )
    .expect("the same options are valid for a non-streaming request");
}

#[test]
fn merge_precedence_is_provider_then_candidate_then_request() {
    let config = load(
        r#"
[providers.resp]
base_url = "https://example.com"
protocol = "responses"
responses_options.instructions = "from provider"
responses_options.service_tier = "flex"

[models.m]
candidates = [
  { provider = "resp", model = "m1", responses_options = { service_tier = "priority", parallel_tool_calls = false } },
]
"#,
    )
    .expect("valid layered options");
    let provider = config
        .llm
        .responses_options_providers
        .get("resp")
        .expect("provider options");
    let candidate = config
        .llm
        .responses_options_candidates
        .get(&("m".to_owned(), 0))
        .expect("candidate options");
    let override_value = ResponsesOptions {
        user: Some("user-1".to_owned()),
        ..Default::default()
    }
    .to_value()
    .expect("options serialize to an override object");
    let request = RequestOptions {
        overrides: Some(&override_value),
        stream: false,
    };
    let merged = merge_options(Some(provider), Some(candidate), Some(request))
        .expect("layers merge without conflict");
    assert_eq!(merged.instructions.as_deref(), Some("from provider"));
    assert_eq!(merged.service_tier.as_deref(), Some("priority"));
    assert_eq!(merged.parallel_tool_calls, Some(false));
    assert_eq!(merged.user.as_deref(), Some("user-1"));
}

#[test]
fn request_override_rejects_unknown_keys() {
    let error = ResponsesOptions::from_json(r#"{"bogus": 1}"#)
        .expect_err("unknown request override keys must fail");
    assert!(
        error.to_string().contains("bogus"),
        "error should name the unknown key: {error}"
    );
}

#[test]
fn internal_responses_fields_do_not_change_chat_completion_bytes() {
    let mut request = ChatRequest::new("m", vec![ChatMessage::user("hello")]);
    let before = serialized_len(&request).expect("baseline body length");
    request
        .set_responses_options(&ResponsesOptions {
            instructions: Some("internal".to_owned()),
            state_mode: Some(ResponsesStateMode::Conversation),
            ..Default::default()
        })
        .expect("internal options attach without touching the wire");
    let after = serialized_len(&request).expect("body length with internal options");
    assert_eq!(
        before, after,
        "internal fields must not appear on the Chat wire"
    );

    let mut with_replay = request;
    with_replay.responses_replay_state = None;
    assert_eq!(
        serialized_len(&with_replay).expect("body length with replay state cleared"),
        after
    );
}
