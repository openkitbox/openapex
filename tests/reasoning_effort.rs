use ox::cardinal::wire::ChatRequest;
use ox_engine::mctx::dreamer::dreamer_request;
use ox_engine::mctx::historian::historian_request_with_prompt;
use serde_json::Value;

fn serialized_reasoning(request: &ChatRequest) -> Value {
    serde_json::to_value(request).expect("request serializes")
}

#[test]
fn chat_request_does_not_set_reasoning_by_default() {
    let body = serialized_reasoning(&ChatRequest::new("model", Vec::new()));
    assert!(
        !body
            .as_object()
            .expect("request object")
            .contains_key("reasoning_effort")
    );
}

#[test]
fn dreamer_request_uses_provider_default_reasoning() {
    let body = serialized_reasoning(&dreamer_request("model", "system", "user"));
    assert!(
        !body
            .as_object()
            .expect("request object")
            .contains_key("reasoning_effort")
    );
}

#[test]
fn explicit_reasoning_policy_controls_wire_field() {
    let mut request = ChatRequest::new("model", Vec::new());
    request.apply_reasoning_effort(Some("high"));

    let body = serialized_reasoning(&request);
    assert_eq!(body["reasoning_effort"], Value::String("high".to_owned()));
}

#[test]
fn reasoning_policy_preserves_compatibility_mapping() {
    for (variant, expected) in [
        (Some("off"), Some("off")),
        (Some("low"), Some("low")),
        (Some("medium"), Some("medium")),
        (Some("high"), Some("high")),
        (Some("xhigh"), Some("xhigh")),
        (Some("max"), Some("max")),
        (Some("none"), Some("off")),
    ] {
        let mut request = ChatRequest::new("model", Vec::new());
        request.apply_reasoning_effort(variant);
        assert_eq!(request.reasoning_effort.as_deref(), expected);
    }
}

#[test]
fn absent_default_empty_and_unknown_reasoning_are_omitted() {
    for variant in [None, Some("default"), Some(""), Some("unknown")] {
        let mut request = ChatRequest::new("model", Vec::new());
        request.apply_reasoning_effort(variant);
        let body = serialized_reasoning(&request);
        assert!(
            !body
                .as_object()
                .expect("request object")
                .contains_key("reasoning_effort"),
            "variant {variant:?} must omit reasoning_effort"
        );
    }
}

#[test]
fn historian_request_does_not_invent_a_session_reasoning_policy() {
    let request = historian_request_with_prompt("model", "prompt", None);
    assert_eq!(request.reasoning_effort, None);
}

#[test]
fn historian_request_applies_the_trigger_snapshot() {
    let request = historian_request_with_prompt("model", "prompt", Some("high"));
    assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
}
