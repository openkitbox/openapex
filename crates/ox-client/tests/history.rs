use ox_client::history::parse_messages;
use ox_client::store::Item;

#[test]
fn history_maps_user_and_assistant_text_in_order() {
    let raw = r#"[
      {"type":"user","id":"msg_u1","time":{"created":1},"text":"hello"},
      {"type":"assistant","id":"msg_a1","time":{"created":2},"agent":"build",
       "model":{"providerID":"p","id":"m"},
       "content":[{"type":"text","text":"hi there"}]}
    ]"#;
    let messages: Vec<ox_schema::message::Message> = serde_json::from_str(raw).unwrap();
    let items = parse_messages(&messages);

    assert_eq!(items.len(), 2, "one ordered sequence, not parallel vectors");
    match &items[0] {
        Item::User { text } => assert_eq!(text.as_ref(), "hello"),
        other => panic!("expected user, got {other:?}"),
    }
    match &items[1] {
        Item::Assistant(assistant) => {
            assert_eq!(assistant.text.as_str(), "hi there");
            assert!(assistant.done);
        }
        other => panic!("expected assistant, got {other:?}"),
    }
}

#[test]
fn history_concatenates_text_and_reasoning_and_skips_tool_only() {
    let raw = r#"[
      {"type":"assistant","id":"msg_a1","time":{"created":1},"agent":"build",
       "model":{"providerID":"p","id":"m"},
       "content":[
         {"type":"reasoning","text":"thinking "},
         {"type":"reasoning","text":"hard"},
         {"type":"text","text":"answer "},
         {"type":"text","text":"here"}
       ]},
      {"type":"assistant","id":"msg_a2","time":{"created":2},"agent":"build",
       "model":{"providerID":"p","id":"m"},
       "content":[
         {"type":"tool","id":"call_1","name":"read",
          "state":{"status":"running","input":{},"metadata":{}},
          "time":{"created":1}}
       ]}
    ]"#;
    let messages: Vec<ox_schema::message::Message> = serde_json::from_str(raw).unwrap();
    let items = parse_messages(&messages);

    assert_eq!(items.len(), 1, "tool-only assistant messages are skipped");
    let Item::Assistant(assistant) = &items[0] else {
        panic!("expected assistant, got {:?}", items[0]);
    };
    assert_eq!(assistant.id, "msg_a1");
    assert_eq!(assistant.text.as_str(), "answer here");
    assert_eq!(assistant.reasoning.as_str(), "thinking hard");
    assert!(assistant.done);
}

#[test]
fn history_ignores_non_transcript_variants() {
    let raw = r#"[
      {"type":"synthetic","id":"msg_s1","time":{"created":1},"text":"ignore me"},
      {"type":"system","id":"msg_s2","time":{"created":2},"text":"ignore me too"},
      {"type":"user","id":"msg_u1","time":{"created":3},"text":"kept"}
    ]"#;
    let messages: Vec<ox_schema::message::Message> = serde_json::from_str(raw).unwrap();
    let items = parse_messages(&messages);

    assert_eq!(items.len(), 1);
    let Item::User { text } = &items[0] else {
        panic!("expected user, got {:?}", items[0]);
    };
    assert_eq!(text.as_ref(), "kept");
}
