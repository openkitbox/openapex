//! Regression coverage for native Messages thinking replay: a completed signed
//! and redacted block survives decoding, a `ProviderState` round-trip and the
//! real Messages writer, but never leaks to a different request identity.

use std::sync::Arc;

use ox_llm::chat_completions::stream::StreamEvent;
use ox_llm::chat_completions::wire::{ChatMessage, ChatRequest};
use ox_llm::messages::replay::{MessagesOrigin, MessagesReplayState, from_provider_state};
use ox_llm::messages::stream::MessagesDecoder;
use ox_llm::messages::wire::{MessagesPolicy, WireBlock, build_request};
use ox_llm::sse::SseDecoder;
use ox_schema::message::{AssistantContent, AssistantMessage, AssistantTime, Message};
use ox_schema::support::ModelRef;

const PROVIDER: &str = "anthropic-native";
const ENDPOINT: &str = "https://messages.example/v1/messages";
const MODEL: &str = "claude-test";

fn origin() -> MessagesOrigin<'static> {
    MessagesOrigin::new(PROVIDER, ENDPOINT)
}

fn sse(lines: &[&str]) -> Vec<StreamEvent> {
    let mut decoder = MessagesDecoder::new().with_replay(PROVIDER, ENDPOINT, MODEL);
    let mut out = Vec::new();
    for line in lines {
        decoder
            .decode(line, &mut out)
            .expect("well-formed Messages SSE payload");
    }
    out
}

fn signed_stream() -> Vec<StreamEvent> {
    sse(&[
        r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"first thought"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"alpha"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"redacted_thinking","data":"-tail"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"answer"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"call-1","name":"read","input":{}}}"#,
        r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#,
        r#"{"type":"content_block_stop","index":3}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":11}}"#,
        r#"{"type":"message_stop"}"#,
    ])
}

fn replay_from(events: &[StreamEvent]) -> Arc<MessagesReplayState> {
    let state = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::ProviderState(state) => Some(state.clone()),
            _ => None,
        })
        .expect("message_stop emitted a replayable provider state");
    from_provider_state(Some(&state)).expect("state survives the ProviderState round-trip")
}

fn durable_roundtrip(replay: Arc<MessagesReplayState>) -> Arc<MessagesReplayState> {
    let mut row = Message::Assistant(Box::new(AssistantMessage {
        id: "assistant-1".to_owned(),
        metadata: None,
        time: AssistantTime {
            created: 1,
            streamed: None,
            completed: Some(2),
        },
        agent: "apex".to_owned(),
        model: ModelRef {
            id: MODEL.to_owned(),
            provider_id: PROVIDER.to_owned(),
            variant: None,
        },
        content: vec![AssistantContent::Text {
            text: "answer".to_owned(),
            state: None,
        }],
        snapshot: None,
        finish: None,
        raw_finish: None,
        provider_state: Some(
            Arc::try_unwrap(replay)
                .expect("sole owner")
                .into_provider_state()
                .expect("state serializes"),
        ),
        cost: None,
        tokens: None,
        error: None,
        retry: None,
    }))
    .to_row_data();
    Message::merge_row_columns("assistant-1", "assistant", &mut row);
    let Message::Assistant(restored) = serde_json::from_value::<Message>(row).expect("durable row")
    else {
        panic!("assistant row must decode as assistant");
    };
    from_provider_state(restored.provider_state.as_ref()).expect("replay survives the row")
}

#[test]
fn complete_thinking_blocks_survive_decode_and_messages_write() {
    let replay = replay_from(&signed_stream());
    let replay = durable_roundtrip(replay);
    assert_eq!(replay.blocks.len(), 2, "signed and redacted blocks only");
    let assistant = ChatMessage::Assistant {
        content: Some("answer".to_owned()),
        tool_calls: None,
        reasoning_content: Some("first thought".to_owned()),
        replay_state: Some(Arc::clone(&replay)),
    };
    let request = ChatRequest::new(MODEL, vec![assistant, ChatMessage::user("continue")]);
    let body = build_request(&request, &MessagesPolicy::anthropic(), &origin())
        .expect("valid Messages body");
    let content = &body.messages[0].content;
    assert_eq!(
        content,
        &vec![
            WireBlock::Thinking {
                thinking: "first thought".to_owned(),
                signature: Some("sig-alpha".to_owned()),
            },
            WireBlock::RedactedThinking {
                data: "opaque-tail".to_owned(),
            },
            WireBlock::Text {
                text: "answer".to_owned(),
            },
        ],
        "original blocks replay in order, with no duplicate unsigned thinking"
    );
}

#[test]
fn incomplete_and_late_states_are_not_replayable() {
    let events = sse(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"unfinished"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::ProviderState(_))),
        "a thinking block without content_block_stop is not replayable"
    );

    let events = signed_stream();
    let replay = replay_from(&events);
    let assistant = ChatMessage::Assistant {
        content: None,
        tool_calls: None,
        reasoning_content: None,
        replay_state: Some(replay),
    };
    for (provider, endpoint, model) in [
        ("other-provider", ENDPOINT, MODEL),
        (PROVIDER, "https://other.example/v1/messages", MODEL),
        (PROVIDER, ENDPOINT, "other-model"),
    ] {
        let mut request = ChatRequest::new(model, vec![assistant.clone()]);
        request.model = model.to_owned();
        let body = build_request(
            &request,
            &MessagesPolicy::anthropic(),
            &MessagesOrigin::new(provider, endpoint),
        )
        .expect("valid Messages body");
        assert!(
            body.messages.is_empty()
                || !body.messages[0].content.iter().any(|block| matches!(
                    block,
                    WireBlock::Thinking {
                        signature: Some(_),
                        ..
                    } | WireBlock::RedactedThinking { .. }
                )),
            "state must not leak to {provider}/{endpoint}/{model}"
        );
    }
}

#[test]
fn replay_keeps_thinking_after_text() {
    let events = sse(&[
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"late thought"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig-late"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let replay = replay_from(&events);
    let request = ChatRequest::new(
        MODEL,
        vec![ChatMessage::Assistant {
            content: Some("answer".to_owned()),
            tool_calls: None,
            reasoning_content: Some("late thought".to_owned()),
            replay_state: Some(replay),
        }],
    );
    let body = build_request(&request, &MessagesPolicy::anthropic(), &origin())
        .expect("valid Messages body");
    assert_eq!(
        body.messages[0].content,
        vec![
            WireBlock::Text {
                text: "answer".to_owned(),
            },
            WireBlock::Thinking {
                thinking: "late thought".to_owned(),
                signature: Some("sig-late".to_owned()),
            },
        ],
        "content-block positions survive the round-trip"
    );
}

#[test]
fn legacy_request_bytes_are_unchanged() {
    let request = ChatRequest::new(
        MODEL,
        vec![ChatMessage::Assistant {
            content: Some("answer".to_owned()),
            tool_calls: None,
            reasoning_content: None,
            replay_state: None,
        }],
    );
    let body =
        build_request(&request, &MessagesPolicy::anthropic(), &origin()).expect("valid body");
    assert_eq!(
        body.messages[0].content,
        vec![WireBlock::Text {
            text: "answer".to_owned()
        }],
        "rows without replay state keep the previous wire shape"
    );
}

#[test]
fn internal_chat_wire_never_carries_native_state() {
    let replay = replay_from(&signed_stream());
    let with = ChatRequest::new(
        MODEL,
        vec![ChatMessage::Assistant {
            content: Some("answer".to_owned()),
            tool_calls: None,
            reasoning_content: Some("first thought".to_owned()),
            replay_state: Some(replay),
        }],
    );
    let without = ChatRequest::new(
        MODEL,
        vec![ChatMessage::Assistant {
            content: Some("answer".to_owned()),
            tool_calls: None,
            reasoning_content: Some("first thought".to_owned()),
            replay_state: None,
        }],
    );
    assert_eq!(
        serde_json::to_value(&with).expect("serializes"),
        serde_json::to_value(&without).expect("serializes"),
        "Chat/Completions and hidden requests must not see native state"
    );
}

#[test]
fn rejected_native_state_falls_back_to_unsigned_thinking_wire() {
    let replay = replay_from(&signed_stream());
    let request = ChatRequest::new(
        MODEL,
        vec![ChatMessage::Assistant {
            content: Some("answer".to_owned()),
            tool_calls: None,
            reasoning_content: Some("first thought".to_owned()),
            replay_state: Some(replay),
        }],
    );
    let body = build_request(
        &request,
        &MessagesPolicy::deepseek(),
        &MessagesOrigin::new("other-provider", ENDPOINT),
    )
    .expect("valid Messages body");
    assert_eq!(
        body.messages[0].content,
        vec![
            WireBlock::Thinking {
                thinking: "first thought".to_owned(),
                signature: None,
            },
            WireBlock::Text {
                text: "answer".to_owned(),
            },
        ],
        "a provider/model switch keeps the pre-existing reasoning fallback"
    );
}
