//! Responses SSE decoding: output events, terminal state and replay items.
//!
//! Only the public decoder API is exercised here — no HTTP server or engine.

use ox::llm::chat_completions::stream::StreamEvent;
use ox::llm::responses::{ResponsesDecoder, from_provider_state};
use ox::llm::sse::SseDecoder;

const PROVIDER: &str = "vendor";
const ENDPOINT: &str = "https://vendor.example/v1/responses";
const MODEL: &str = "m1";

fn replay_decoder() -> ResponsesDecoder {
    ResponsesDecoder::new().with_replay(PROVIDER, ENDPOINT, MODEL)
}

fn decode(decoder: &mut ResponsesDecoder, payloads: &[&str]) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for payload in payloads {
        decoder
            .decode(payload, &mut events)
            .expect("payload decodes");
    }
    events
}

#[test]
fn text_reasoning_and_refusal_deltas_share_the_stream_vocabulary() {
    let events = decode(
        &mut ResponsesDecoder::new(),
        &[
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            r#"{"type":"response.refusal.delta","delta":"can't"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"why"}"#,
            r#"{"type":"response.output_text.delta","delta":" world"}"#,
            // Lifecycle, citation and future events carry no delta to forward.
            r#"{"type":"response.output_text.annotation_added","annotation":{"type":"url_citation"}}"#,
            r#"{"type":"response.future.event","delta":"ignored"}"#,
        ],
    );

    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta("hello".to_owned()),
            StreamEvent::TextDelta("can't".to_owned()),
            StreamEvent::ReasoningDelta("why".to_owned()),
            StreamEvent::TextDelta(" world".to_owned()),
        ]
    );
}

#[test]
fn function_and_custom_tool_deltas_use_the_announced_identity() {
    let events = decode(
        &mut ResponsesDecoder::new(),
        &[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{\"p\":"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"item-2","call_id":"call-2","name":"edit"}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","item_id":"item-2","delta":"patch"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"1}"}"#,
        ],
    );

    assert_eq!(
        events,
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_owned()),
                name: Some("read".to_owned()),
                arguments: r#"{"p":"#.to_owned(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: Some("call-2".to_owned()),
                name: Some("edit".to_owned()),
                arguments: "patch".to_owned(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_owned()),
                name: Some("read".to_owned()),
                arguments: "1}".to_owned(),
            },
        ]
    );
}

#[test]
fn lifecycle_done_events_and_server_side_tools_do_not_reemit_text() {
    let events = decode(
        &mut ResponsesDecoder::new(),
        &[
            r#"{"type":"response.output_item.added","item":{"type":"message","id":"msg-1"}}"#,
            r#"{"type":"response.content_part.added","content_part":{"type":"output_text"}}"#,
            r#"{"type":"response.output_text.done","text":"complete text"}"#,
            r#"{"type":"response.refusal.done","refusal":"complete refusal"}"#,
            r#"{"type":"response.reasoning_summary_text.done","text":"complete reasoning"}"#,
            r#"{"type":"response.function_call_arguments.done","arguments":"{\"p\":1}"}"#,
            r#"{"type":"response.custom_tool_call_input.done","input":"patch"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"web_search_call","id":"ws-1"}}"#,
            r#"{"type":"response.web_search_call.searching","item_id":"ws-1"}"#,
            r#"{"type":"response.web_search_call.completed","item_id":"ws-1"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws-1"}}"#,
            r#"{"type":"response.created","response":{"id":"resp-1"}}"#,
        ],
    );

    assert!(
        events.is_empty(),
        "done events must not duplicate deltas: {events:?}"
    );
}

#[test]
fn terminal_state_is_emitted_once_before_usage_finish_and_done() {
    let mut decoder = replay_decoder();
    let events = decode(
        &mut decoder,
        &[
            r#"{"type":"response.output_text.delta","sequence_number":7,"delta":"hi"}"#,
            r#"{"type":"response.completed","sequence_number":9,"response":{"id":"resp-9","conversation":{"id":"conv-1"},"usage":{"input_tokens":30,"output_tokens":12,"input_tokens_details":{"cached_tokens":10,"cache_write_tokens":4},"output_tokens_details":{"reasoning_tokens":3}},"output":[{"type":"message","id":"msg-1"},{"type":"reasoning","id":"rs-1"},{"type":"custom_tool_call","id":"ct-1"}]}}"#,
            r#"{"type":"response.completed","sequence_number":10,"response":{"id":"resp-9","output":[]}}"#,
        ],
    );

    assert_eq!(
        events.first(),
        Some(&StreamEvent::TextDelta("hi".to_owned())),
        "terminal state must come after streamed content"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ProviderState(_)))
            .count(),
        1,
        "only one replay record per stream: {events:?}"
    );
    let state_position = events
        .iter()
        .position(|event| matches!(event, StreamEvent::ProviderState(_)))
        .expect("provider state was emitted");
    let StreamEvent::Usage(usage) = &events[state_position + 1] else {
        panic!("usage must follow provider state: {events:?}");
    };
    assert_eq!(usage.prompt_tokens, 30);
    assert_eq!(usage.completion_tokens, 12);
    assert_eq!(usage.cached_tokens, Some(10));
    assert_eq!(usage.cache_write_tokens, Some(4));
    assert_eq!(usage.reasoning_tokens, Some(3));
    assert_eq!(
        events[state_position + 2],
        StreamEvent::Finish {
            reason: "stop".to_owned()
        }
    );
    assert_eq!(events[state_position + 3], StreamEvent::Done);
    assert_eq!(decoder.last_sequence_number(), Some(10));
    assert_eq!(decoder.response_id(), Some("resp-9"));

    let record = match &events[state_position] {
        StreamEvent::ProviderState(record) => record,
        _ => unreachable!("position was found above"),
    };
    let state = from_provider_state(Some(record)).expect("state decodes");
    assert_eq!(state.provider, PROVIDER);
    assert_eq!(state.endpoint, ENDPOINT);
    assert_eq!(state.model, MODEL);
    assert_eq!(state.protocol_marker, "responses/v1");
    assert_eq!(state.response_id.as_deref(), Some("resp-9"));
    assert_eq!(state.conversation_id.as_deref(), Some("conv-1"));
    assert_eq!(state.sequence_number, Some(9));
    assert_eq!(state.items.len(), 3);
    assert_eq!(state.items[0]["id"].as_str(), Some("msg-1"));
    assert_eq!(state.items[1]["id"].as_str(), Some("rs-1"));
    assert_eq!(state.items[2]["id"].as_str(), Some("ct-1"));
}

#[test]
fn terminal_without_output_uses_items_seen_in_the_stream() {
    let mut decoder = replay_decoder();
    let events = decode(
        &mut decoder,
        &[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg-1","content":[]}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg-1","content":[{"type":"output_text","text":"hi"}]}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read"}}"#,
            r#"{"type":"response.incomplete","response":{"id":"resp-2","usage":{"input_tokens":5,"output_tokens":1}}}"#,
        ],
    );

    let state_position = events
        .iter()
        .position(|event| matches!(event, StreamEvent::ProviderState(_)))
        .expect("provider state was emitted");
    let StreamEvent::Usage(usage) = &events[state_position + 1] else {
        panic!("usage must follow provider state: {events:?}");
    };
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.completion_tokens, 1);
    assert_eq!(
        events[state_position + 2],
        StreamEvent::Finish {
            reason: "length".to_owned()
        }
    );
    assert_eq!(events[state_position + 3], StreamEvent::Done);

    let StreamEvent::ProviderState(record) = &events[state_position] else {
        unreachable!("position was found above");
    };
    let state = from_provider_state(Some(record)).expect("state decodes");
    assert_eq!(state.items.len(), 2);
    assert_eq!(state.items[0]["id"].as_str(), Some("msg-1"));
    assert_eq!(state.items[0]["content"][0]["text"].as_str(), Some("hi"));
    assert_eq!(state.items[1]["id"].as_str(), Some("item-1"));
}
