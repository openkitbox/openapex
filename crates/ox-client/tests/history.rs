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
        Item::User { text, .. } => assert_eq!(text.as_ref(), "hello"),
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
fn history_concatenates_text_and_keeps_tool_rows() {
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
          "state":{"status":"running","input":{"path":"a.txt"},"metadata":{}},
          "time":{"created":1}}
       ]}
    ]"#;
    let messages: Vec<ox_schema::message::Message> = serde_json::from_str(raw).unwrap();
    let items = parse_messages(&messages);

    assert_eq!(items.len(), 2, "tool-only messages render as tool rows");
    let Item::Assistant(assistant) = &items[0] else {
        panic!("expected assistant, got {:?}", items[0]);
    };
    assert_eq!(assistant.id, "msg_a1");
    assert_eq!(assistant.text.as_str(), "answer here");
    assert_eq!(assistant.reasoning.as_str(), "thinking hard");
    assert!(assistant.done);

    let Item::Assistant(tool_message) = &items[1] else {
        panic!("expected assistant, got {:?}", items[1]);
    };
    assert_eq!(tool_message.tools.len(), 1);
    assert_eq!(tool_message.tools[0].tool, "read");
    assert_eq!(tool_message.tools[0].summary, "a.txt");
    assert!(matches!(
        tool_message.tools[0].state,
        ox_client::store::ToolState::Running
    ));
}

#[test]
fn history_reads_per_turn_usage() {
    let raw = r#"[
      {"type":"assistant","id":"msg_a1","time":{"created":1000,"streamed":3000,"completed":4000},
       "agent":"build","model":{"providerID":"p","id":"m"},
       "content":[{"type":"text","text":"hi"}],
       "cost":0.01,
       "tokens":{"input":10,"output":60,"reasoning":10,"cache":{"read":0,"write":0}}}
    ]"#;
    let messages: Vec<ox_schema::message::Message> = serde_json::from_str(raw).unwrap();
    let items = parse_messages(&messages);

    let Item::Assistant(assistant) = &items[0] else {
        panic!("expected assistant, got {:?}", items[0]);
    };
    assert_eq!(assistant.output_tokens, 70, "output + reasoning");
    assert_eq!(assistant.active_ms, 2_000, "streamed − created");
    assert_eq!(assistant.turn_started_ms, Some(1_000));
    assert_eq!(assistant.turn_done_ms, Some(4_000));
    assert!((assistant.cost_usd - 0.01).abs() < 1e-9, "message cost");
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
    let Item::User { text, .. } = &items[0] else {
        panic!("expected user, got {:?}", items[0]);
    };
    assert_eq!(text.as_ref(), "kept");
}

#[test]
fn history_keeps_todowrite_input_and_compaction_boundaries() {
    let raw = r#"[
      {"type":"assistant","id":"msg_a1","time":{"created":1},"agent":"build",
       "model":{"providerID":"p","id":"m"},
       "content":[
         {"type":"tool","id":"call_1","name":"todowrite",
          "state":{"status":"completed",
                   "input":{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"pending"}]},
                   "content":[{"type":"text","text":"2 todos"}],"metadata":{}},
          "time":{"created":1}}
       ]},
      {"type":"compaction","id":"msg_c1","time":{"created":2},"status":"completed",
       "reason":"auto","summary":"Goal: ship","recent":"",
       "tokens":{"input":10,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}},
      {"type":"compaction","id":"msg_c2","time":{"created":3},"status":"failed",
       "reason":"manual","error":{"type":"aborted","message":"cancelled"}}
    ]"#;
    let messages: Vec<ox_schema::message::Message> = serde_json::from_str(raw).unwrap();
    let items = parse_messages(&messages);

    assert_eq!(items.len(), 3);
    let Item::Assistant(assistant) = &items[0] else {
        panic!("expected assistant, got {:?}", items[0]);
    };
    assert_eq!(assistant.tools[0].todos.len(), 2);
    assert_eq!(assistant.tools[0].todos[1].content, "b");

    let Item::Compaction(done) = &items[1] else {
        panic!("expected compaction, got {:?}", items[1]);
    };
    assert_eq!(done.id, "msg_c1");
    assert_eq!(done.status, ox_client::store::CompactionStatus::Completed);
    assert_eq!(done.text.as_str(), "Goal: ship");
    assert_eq!(done.tokens.unwrap().input, 10);

    let Item::Compaction(failed) = &items[2] else {
        panic!("expected compaction, got {:?}", items[2]);
    };
    assert_eq!(failed.status, ox_client::store::CompactionStatus::Failed);
    assert!(failed.cancelled);
}
