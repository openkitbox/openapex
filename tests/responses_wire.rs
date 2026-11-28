//! Create-body golden coverage and the Responses state-chain decisions.
//!
//! The builder is pure: no HTTP client is involved, so every rejection here
//! also means no request leaves the process.

use std::sync::Arc;

use ox::llm::chat_completions::wire::{
    ChatMessage, ChatRequest, FunctionCall, FunctionType, ToolCall, ToolChoice, ToolDef,
};
use ox::llm::error::ChatError;
use ox::llm::responses::options::{ResponsesOptions, ResponsesStateMode};
use ox::llm::responses::replay::{ResponsesOrigin, ResponsesReplayInput, ResponsesReplayState};
use ox::llm::responses::wire::serde_json;
use ox::llm::responses::wire::serde_json::json;
use ox::llm::responses::wire::{InputContent, InputItem};
use ox::llm::responses::wire::{build_request, input_fingerprint};

fn raw_items() -> Vec<serde_json::Value> {
    vec![
        json!({"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "previous"}
        ]}),
        json!({"type": "function_call", "call_id": "call-1", "name": "read",
               "arguments": "{}"}),
    ]
}

fn state_with_items(
    model: &str,
    items: Vec<serde_json::Value>,
    fingerprint: String,
    len: usize,
) -> ResponsesReplayState {
    ResponsesReplayState::new(
        ResponsesOrigin::new(
            "vendor",
            "https://vendor.example/v1/responses",
            "responses/v1",
        ),
        ResponsesReplayInput {
            model: model.to_owned(),
            response_id: Some("resp_1".to_owned()),
            items,
            input_fingerprint: fingerprint,
            input_len: len,
            ..Default::default()
        },
    )
}

fn state(model: &str, fingerprint: String, len: usize) -> ResponsesReplayState {
    state_with_items(model, raw_items(), fingerprint, len)
}

fn assistant_with_call() -> ChatMessage {
    ChatMessage::Assistant {
        content: Some("old".to_owned()),
        tool_calls: Some(vec![ToolCall {
            id: "call-1".to_owned(),
            kind: FunctionType::Function,
            function: FunctionCall {
                name: "read".to_owned(),
                arguments: r#"{"path":"x"}"#.to_owned(),
            },
        }]),
        reasoning_content: None,
        replay_state: None,
    }
}

#[test]
fn create_body_covers_every_declared_option_group() {
    let mut request = ChatRequest::new(
        "gpt-5",
        vec![
            ChatMessage::system("system one"),
            ChatMessage::user("hello"),
        ],
    );
    request.tools = Arc::new(vec![ToolDef::function(
        "read",
        "read a file",
        json!({"type": "object"}),
    )]);
    request.tool_choice = ToolChoice::Required;
    request.temperature = Some(0.25);
    request.max_tokens = Some(64);
    request.reasoning_effort = Some("high".to_owned());

    let options = ResponsesOptions {
        instructions: Some("call instruction".to_owned()),
        prompt: Some(json!({"id": "pmpt_1"})),
        prompt_cache_options: Some(json!({"scope": "cache-1"})),
        text: Some(json!({"format": {"type": "text"}})),
        reasoning: Some(json!({"effort": "medium", "summary": "detailed"})),
        include: Some(vec!["file_search_call.results".to_owned()]),
        max_tool_calls: Some(3),
        parallel_tool_calls: Some(false),
        service_tier: Some("priority".to_owned()),
        truncation: Some("auto".to_owned()),
        user: Some("user-1".to_owned()),
        metadata: Some(json!({"case": "wire"})),
        background: Some(true),
        background_options: Some(json!({"webhook": {"url": "https://example.com/hook"}})),
        context_management: Some(json!({"automatic": {"compaction": {}}})),
        store: Some(true),
        tools: Some(vec![json!({"type": "web_search"})]),
        ..Default::default()
    };

    let body = build_request(&request, &options, None).expect("valid option set");
    let value = serde_json::to_value(&body).expect("create body serializes");

    assert_eq!(
        value,
        json!({
            "model": "gpt-5",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "hello"}]}],
            "instructions": "system one\n\ncall instruction",
            "tools": [
                {"type": "function", "name": "read", "description": "read a file",
                 "parameters": {"type": "object"}},
                {"type": "web_search"}
            ],
            "tool_choice": "required",
            "temperature": 0.25,
            "max_output_tokens": 64,
            "reasoning": {"effort": "medium", "summary": "detailed"},
            "include": ["file_search_call.results"],
            "prompt": {"id": "pmpt_1"},
            "prompt_cache_options": {"scope": "cache-1"},
            "text": {"format": {"type": "text"}},
            "max_tool_calls": 3,
            "parallel_tool_calls": false,
            "service_tier": "priority",
            "truncation": "auto",
            "user": "user-1",
            "metadata": {"case": "wire"},
            "background": true,
            "background_options": {"webhook": {"url": "https://example.com/hook"}},
            "context_management": {"automatic": {"compaction": {}}},
            "stream": true,
            "store": true
        })
    );
    assert!(value.get("stream_options").is_none());
    assert!(value.get("previous_response_id").is_none());
    assert!(value.get("conversation").is_none());
}

#[test]
fn manual_mode_replays_raw_items_and_keeps_the_whole_history() {
    let request = ChatRequest::new(
        "gpt-5",
        vec![
            ChatMessage::system("system"),
            ChatMessage::user("hello"),
            assistant_with_call(),
            ChatMessage::tool("call-1", "contents"),
            ChatMessage::user("next"),
        ],
    );
    let replay = state("gpt-5", "fnv:stale".to_owned(), 999);

    let options = ResponsesOptions::default();
    let body = build_request(&request, &options, Some(&replay))
        .expect("manual mode never rejects on prefix mismatch");
    let value = serde_json::to_value(&body).expect("create body serializes");

    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "previous"}]},
            {"type": "function_call", "call_id": "call-1", "name": "read",
             "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call-1", "output": "contents"},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "next"}]}
        ])
    );
    assert_eq!(value["instructions"], json!("system"));
    assert_eq!(value["store"], json!(false));
    assert_eq!(
        value["include"],
        json!(["reasoning.encrypted_content"]),
        "manual mode asks for encrypted reasoning by default"
    );
    assert!(value.get("previous_response_id").is_none());
    assert!(value.get("conversation").is_none());
    assert!(body.logical_fingerprint().is_some());
}

#[test]
fn foreign_model_falls_back_to_readable_text_in_manual_mode() {
    let request = ChatRequest::new(
        "gpt-5",
        vec![
            ChatMessage::user("hello"),
            assistant_with_call(),
            ChatMessage::tool("call-1", "contents"),
        ],
    );
    let replay = state("other-model", "fnv:stale".to_owned(), 999);

    let value = serde_json::to_value(
        build_request(&request, &ResponsesOptions::default(), Some(&replay))
            .expect("manual mode falls back"),
    )
    .expect("create body serializes");

    assert_eq!(
        value["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "old"}]},
            {"type": "function_call", "call_id": "call-1", "name": "read",
             "arguments": r#"{"path":"x"}"#},
            {"type": "function_call_output", "call_id": "call-1", "output": "contents"}
        ]),
        "raw items from another model must never be replayed"
    );
}

/// Build a state whose fingerprint describes the logical history through the
/// assistant turn: exactly what a chained follow-up must confirm before it
/// drops that prefix. Written with the same item shapes the builder emits.
fn state_for_prefix(model: &str) -> ResponsesReplayState {
    let raw = raw_items();
    let mut items = vec![InputItem::Message {
        role: "user",
        content: vec![InputContent::InputText { text: "hello" }],
    }];
    items.extend(raw.iter().map(InputItem::Raw));
    let (fingerprint, len) = input_fingerprint(&items);
    state_with_items(model, raw, fingerprint, len)
}

fn chained_request() -> ChatRequest {
    ChatRequest::new(
        "gpt-5",
        vec![
            ChatMessage::system("system"),
            ChatMessage::user("hello"),
            assistant_with_call(),
            ChatMessage::tool("call-1", "contents"),
            ChatMessage::user("next"),
        ],
    )
}

#[test]
fn chained_modes_send_only_the_suffix_after_a_matching_prefix() {
    let replay = state_for_prefix("gpt-5");
    let request = chained_request();

    let previous = ResponsesOptions {
        state_mode: Some(ResponsesStateMode::PreviousResponse),
        previous_response_id: Some("resp_1".to_owned()),
        ..Default::default()
    };
    let body = build_request(&request, &previous, Some(&replay)).expect("prefix matches");
    let value = serde_json::to_value(&body).expect("create body serializes");
    assert_eq!(
        value["input"],
        json!([
            {"type": "function_call_output", "call_id": "call-1", "output": "contents"},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "next"}]}
        ])
    );
    assert_eq!(value["previous_response_id"], json!("resp_1"));
    assert_eq!(value["store"], json!(true));
    assert!(value.get("conversation").is_none());
    assert!(value.get("include").is_none());

    let conversation = ResponsesOptions {
        state_mode: Some(ResponsesStateMode::Conversation),
        conversation: Some(json!("conv_1")),
        ..Default::default()
    };
    let body = build_request(&request, &conversation, Some(&replay)).expect("prefix matches");
    let value = serde_json::to_value(&body).expect("create body serializes");
    assert_eq!(value["conversation"], json!("conv_1"));
    assert_eq!(
        value["input"],
        json!([
            {"type": "function_call_output", "call_id": "call-1", "output": "contents"},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "next"}]}
        ])
    );
    assert!(value.get("previous_response_id").is_none());
}

#[test]
fn prefix_mismatch_rejects_chain_modes_but_not_manual() {
    let request = chained_request();
    let replay = state("gpt-5", "fnv:stale".to_owned(), 999);
    let modes = [
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
    ];

    for options in &modes {
        let error = build_request(&request, options, Some(&replay))
            .expect_err("a changed prefix must not be silently truncated");
        assert!(matches!(error, ChatError::Upstream(_)), "{error:?}");
        assert!(
            error.to_string().contains("manual"),
            "the error must offer the safe way out: {error}"
        );
    }

    let options = ResponsesOptions::default();
    let full = build_request(&request, &options, Some(&replay))
        .expect("manual mode still sends every message");
    assert_eq!(
        serde_json::to_value(full).expect("body serializes")["input"]
            .as_array()
            .map(Vec::len),
        Some(5)
    );
}

#[test]
fn conversation_and_previous_response_id_are_mutually_exclusive() {
    let request = chained_request();
    let options = ResponsesOptions {
        state_mode: Some(ResponsesStateMode::PreviousResponse),
        previous_response_id: Some("resp_1".to_owned()),
        conversation: Some(json!("conv_1")),
        ..Default::default()
    };
    let error = build_request(&request, &options, None)
        .expect_err("the two state anchors cannot be combined");
    assert!(matches!(error, ChatError::Upstream(_)), "{error:?}");
    assert!(
        error.to_string().contains("previous_response_id"),
        "the error names the conflicting key: {error}"
    );
}

#[test]
fn chained_modes_without_a_state_fail_closed() {
    let request = chained_request();
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
        assert!(matches!(
            build_request(&request, &options, None),
            Err(ChatError::Upstream(_))
        ));
    }
}

#[test]
fn logical_fingerprint_is_stable_and_changes_with_input() {
    let request = ChatRequest::new("gpt-5", vec![ChatMessage::user("hello")]);
    let options = ResponsesOptions::default();
    let body = build_request(&request, &options, None).expect("valid body");
    let (hash, len) = body
        .logical_fingerprint()
        .expect("every body carries its fingerprint");
    assert_eq!(
        input_fingerprint(&body.input),
        (hash.to_owned(), len),
        "the exposed fingerprint describes the actual input items"
    );

    let again = build_request(&request, &options, None).expect("valid body");
    assert_eq!(hash, again.logical_fingerprint().expect("fingerprint").0);

    let changed = ChatRequest::new("gpt-5", vec![ChatMessage::user("something else")]);
    let changed = build_request(&changed, &options, None).expect("valid body");
    assert_ne!(
        hash,
        changed.logical_fingerprint().expect("fingerprint").0,
        "a different prefix must not compare equal"
    );
}
