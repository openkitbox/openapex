//! Engine-side Responses replay: durable provider state must reach the next
//! request only from the endpoint that produced it, and compaction must leave
//! no raw items behind.

use std::sync::Arc;

use ox::llm::chat_completions::wire::{ChatMessage, ChatRequest};
use ox::llm::responses::options::ResponsesOptions;
use ox::llm::responses::replay::{
    REPLAY_KEY, ResponsesOrigin, ResponsesReplayInput, ResponsesReplayState,
};
use ox::llm::responses::wire::serde_json::json;
use ox::llm::responses::wire::{build_request, serde_json};
use ox::runner::compaction::responses_compaction_marker;
use ox::schema::support::Record;

fn replay_state(provider: &str, endpoint: &str) -> ResponsesReplayState {
    ResponsesReplayState::new(
        ResponsesOrigin::new(provider, endpoint, "responses/v1"),
        ResponsesReplayInput {
            model: "gpt-5".to_owned(),
            response_id: Some("resp-1".to_owned()),
            items: vec![json!({"type": "message", "id": "raw-item", "role": "assistant"})],
            ..ResponsesReplayInput::default()
        },
    )
}

#[test]
fn responses_replay_items_reach_the_next_request_input() {
    let state = replay_state("responses", "https://api.example/v1/responses");
    let mut request = ChatRequest::new(
        "gpt-5",
        vec![
            ChatMessage::User {
                content: "hello".to_owned().into(),
            },
            ChatMessage::Assistant {
                content: Some("hi".to_owned()),
                tool_calls: None,
                reasoning_content: None,
                replay_state: None,
            },
        ],
    );
    request.responses_replay_state = Some(Arc::new(state));
    let replay = request
        .responses_replay_state
        .as_deref()
        .expect("state attached");

    let options = ResponsesOptions::default();
    let body = build_request(&request, &options, Some(replay)).expect("request builds");
    let value = serde_json::to_value(&body).expect("body serializes");
    let input = value
        .get("input")
        .and_then(|v| v.as_array())
        .expect("input array");
    assert!(
        input
            .iter()
            .any(|item| item.get("id") == Some(&json!("raw-item"))),
        "raw items must be replayed into the next request: {input:?}"
    );
}

#[test]
fn mismatched_origin_never_attaches_replay_state() {
    let state = replay_state("other-provider", "https://api.example/v1/responses");
    let origin = ResponsesOrigin::new(
        "responses",
        "https://api.example/v1/responses",
        "responses/v1",
    );
    assert!(!state.matches(&origin, "gpt-5"));

    let request = ChatRequest::new(
        "gpt-5",
        vec![ChatMessage::User {
            content: "hello".to_owned().into(),
        }],
    );
    let options = ResponsesOptions::default();
    let body = build_request(&request, &options, None).expect("request builds");
    assert!(
        !serde_json::to_value(&body)
            .expect("body serializes")
            .to_string()
            .contains("raw-item"),
        "a rejected origin must not contribute raw items"
    );
}

#[test]
fn compaction_marks_provider_state_as_not_replayable() {
    let marker: Record = responses_compaction_marker();
    assert_eq!(marker.get("ox.responses.compacted.v1"), Some(&json!(true)));
    assert_eq!(
        marker.get(REPLAY_KEY),
        None,
        "the marker is not replay state"
    );
}
