use ox_schema::event::EventEnvelope;
use ox_tui::pipeline::blocks_from_store;
use ox_tui::ui::timeline::Transcript;

fn load_demo() -> Vec<EventEnvelope<serde_json::Value>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/mvp-demo.jsonl");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn demo_events_produce_expected_transcript() {
    let mut store = ox_client::store::DataStore::default();
    store.session = Some("ses_demo".into());
    for envelope in load_demo() {
        ox_client::store::reduce(&mut store, &envelope.parse_typed());
    }

    let blocks = blocks_from_store(&store);
    assert_eq!(blocks.len(), 1);
    let transcript_text = format!("{blocks:?}");
    assert!(transcript_text.contains("先看现有布局，然后把 dock 固定住。"));

    let mut transcript = Transcript::new(80, 10);
    transcript.sync(&blocks, 0);
    // No header line: upstream renders assistant text bare at column 3, so the
    // fixture's single short paragraph is one row.
    assert!(transcript.total_lines() >= 1);
}

// ---------------------------------------------------------------------------
// Completion notices (`↳ Shell finished · cmd`) from real wire payloads.
//
// Envelopes and messages are built with the ox-schema types the server
// serialises and sent through `serde_json` bytes, so the client parses what is
// on the wire. The metadata maps are copied key for key from the producers
// (`ox-tools/src/shell/completion.rs` `payload`, `ox-tools/src/subagent.rs`
// completion enqueue); ox-tui does not depend on ox-tools, so they are `json!`.
// ---------------------------------------------------------------------------

mod completion_notices {
    use ox_client::store::DataStore;
    use ox_schema::event::{EventEnvelope, SessionEventData};
    use ox_schema::inbox::{Delivery, Item as InboxItem, SyntheticPayload};
    use ox_schema::message::{Message, TimeCreated};
    use ox_schema::support::Record;
    use ox_tui::pipeline::blocks_from_store;
    use ox_tui::ui::timeline::Transcript;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use serde_json::{Value, json};
    use unicode_width::UnicodeWidthStr;

    const SESSION: &str = "ses_parent";

    fn record(value: Value) -> Record {
        serde_json::from_value(value).unwrap()
    }

    /// `shell::completion::payload` metadata: source, shellID, state,
    /// truncated, plus `exit` when the process exited.
    fn shell_metadata(state: &str) -> Record {
        record(json!({
            "source": "shell", "shellID": "shl_1", "state": state,
            "truncated": false, "exit": 0,
        }))
    }

    /// `subagent` completion metadata: source, childID, state.
    fn subagent_metadata(state: &str) -> Record {
        record(json!({"source": "subagent", "childID": "ses_child", "state": state}))
    }

    /// Serialise as the server publishes it, then parse as the SSE client does.
    fn wire(id: &str, data: SessionEventData) -> EventEnvelope<SessionEventData> {
        let line = serde_json::to_vec(&json!({
            "id": id, "created": 2000, "type": data.kind(), "data": data.to_data(),
        }))
        .unwrap();
        let envelope: EventEnvelope<Value> = serde_json::from_slice(&line).unwrap();
        envelope.parse_typed()
    }

    fn synthetic_item(text: &str, description: &str, metadata: Record) -> InboxItem {
        InboxItem::Synthetic {
            payload: SyntheticPayload {
                text: text.to_owned(),
                description: Some(description.to_owned()),
                metadata: Some(metadata),
            },
            delivery: Delivery::Steer,
        }
    }

    fn enqueue(store: &mut DataStore, inbox_id: &str, item: InboxItem) {
        let event = wire(
            &format!("evt_enq_{inbox_id}"),
            SessionEventData::InboxEnqueued {
                session_id: SESSION.into(),
                inbox_id: inbox_id.into(),
                item,
            },
        );
        ox_client::store::reduce(store, &event);
    }

    fn deliver(store: &mut DataStore, inbox_id: &str) {
        let event = wire(
            &format!("evt_del_{inbox_id}"),
            SessionEventData::InboxDelivered {
                session_id: SESSION.into(),
                inbox_id: inbox_id.into(),
            },
        );
        ox_client::store::reduce(store, &event);
    }

    fn cancel(store: &mut DataStore, inbox_id: &str) {
        let event = wire(
            &format!("evt_can_{inbox_id}"),
            SessionEventData::InboxCancelled {
                session_id: SESSION.into(),
                inbox_id: inbox_id.into(),
            },
        );
        ox_client::store::reduce(store, &event);
    }

    /// The assistant reply that precedes the notice, from the demo fixture's
    /// event shapes.
    fn store_with_assistant_text() -> DataStore {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        for (kind, data) in [
            (
                "session.text.started",
                json!({"sessionID": SESSION, "assistantMessageID": "msg_1", "ordinal": 0}),
            ),
            (
                "session.text.ended",
                json!({"sessionID": SESSION, "assistantMessageID": "msg_1",
                       "ordinal": 0, "text": "started the job"}),
            ),
            ("session.execution.succeeded", json!({"sessionID": SESSION})),
        ] {
            let envelope: EventEnvelope<Value> = serde_json::from_value(json!({
                "id": format!("evt_{kind}"), "created": 1000, "type": kind, "data": data,
            }))
            .unwrap();
            ox_client::store::reduce(&mut store, &envelope.parse_typed());
        }
        store
    }

    /// Draw the transcript on a `TestBackend` and read the rows back, skipping
    /// the continuation cells of wide glyphs.
    fn rows_of(store: &DataStore, width: u16) -> Vec<String> {
        let height = 12;
        let mut transcript = Transcript::new(width, height);
        transcript.sync(&blocks_from_store(store), 0);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| transcript.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let mut row = String::new();
                let mut tail = 0u16;
                for x in 0..width {
                    if tail > 0 {
                        tail -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    tail = symbol.cell_width().saturating_sub(1);
                }
                row.trim_end().to_owned()
            })
            .collect()
    }

    fn position(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    fn visible(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn live_shell_notice_appears_only_on_delivery_after_the_assistant_text() {
        let mut store = store_with_assistant_text();
        let before = rows_of(&store, 80);
        assert!(position(&before, "started the job").is_some());

        enqueue(
            &mut store,
            "inb_shell",
            synthetic_item(
                "<shell id=\"shl_1\" state=\"completed\">…</shell>",
                "cargo   test\n  -p ox-tui",
                shell_metadata("completed"),
            ),
        );
        assert_eq!(
            rows_of(&store, 80),
            before,
            "an enqueued notice must not render"
        );

        deliver(&mut store, "inb_shell");
        let rows = rows_of(&store, 80);
        let text_row = position(&rows, "started the job").expect("assistant text");
        let notice_row =
            position(&rows, "↳ Shell finished · cargo test -p ox-tui").unwrap_or_else(|| {
                panic!("notice row missing:\n{}", visible(&rows));
            });
        assert!(
            notice_row > text_row,
            "notice follows the text:\n{}",
            visible(&rows)
        );
    }

    #[test]
    fn live_subagent_error_is_a_failed_notice_and_a_cancelled_one_never_shows() {
        let mut store = store_with_assistant_text();
        enqueue(
            &mut store,
            "inb_sub",
            synthetic_item(
                "<subagent state=\"error\">Subagent failed: boom</subagent>",
                "explore the repo",
                subagent_metadata("error"),
            ),
        );
        enqueue(
            &mut store,
            "inb_gone",
            synthetic_item(
                "<subagent state=\"cancelled\">Subagent cancelled</subagent>",
                "abandoned job",
                subagent_metadata("cancelled"),
            ),
        );
        cancel(&mut store, "inb_gone");
        deliver(&mut store, "inb_sub");
        // A late delivery of the cancelled id must not resurrect it.
        deliver(&mut store, "inb_gone");

        let rows = rows_of(&store, 80);
        assert!(
            position(&rows, "! Subagent failed · explore the repo").is_some(),
            "failed notice missing:\n{}",
            visible(&rows)
        );
        assert!(
            position(&rows, "abandoned job").is_none(),
            "cancelled-before-delivered notice leaked:\n{}",
            visible(&rows)
        );
    }

    /// A `history` message list as the server returns it, built from the
    /// schema types and round-tripped through JSON bytes.
    fn history_store(synthetics: Vec<Message>) -> DataStore {
        let mut list = vec![
            json!({"type": "user", "id": "msg_u1", "time": {"created": 1}, "text": "run it"}),
            json!({"type": "assistant", "id": "msg_a1", "time": {"created": 2},
                   "agent": "build", "model": {"providerID": "p", "id": "m"},
                   "content": [{"type": "text", "text": "started the job"}]}),
        ];
        list.extend(
            synthetics
                .iter()
                .map(|message| serde_json::to_value(message).unwrap()),
        );
        let bytes = serde_json::to_vec(&list).unwrap();
        let messages: Vec<Message> = serde_json::from_slice(&bytes).unwrap();
        let mut store = DataStore::default();
        store.replace_items(ox_client::history::parse_messages(&messages));
        store
    }

    fn synthetic(id: &str, description: &str, metadata: Option<Record>) -> Message {
        Message::Synthetic {
            id: id.into(),
            metadata,
            time: TimeCreated { created: 3 },
            text: "body".into(),
            description: Some(description.into()),
        }
    }

    #[test]
    fn history_shell_synthetic_projects_to_the_same_row_and_other_synthetics_render_nothing() {
        let store = history_store(vec![
            synthetic(
                "inb_1",
                "cargo   test\n  -p ox-tui",
                Some(shell_metadata("completed")),
            ),
            // The todo reminder carries no `metadata.source`.
            synthetic("syn_2", "todo reminder", None),
            synthetic(
                "syn_3",
                "goal nudge",
                Some(record(json!({"source": "goal"}))),
            ),
        ]);
        let rows = rows_of(&store, 80);
        let text_row = position(&rows, "started the job").expect("assistant text");
        let notice_row = position(&rows, "↳ Shell finished · cargo test -p ox-tui")
            .unwrap_or_else(|| panic!("notice row missing:\n{}", visible(&rows)));
        assert!(notice_row > text_row);
        for hidden in ["todo reminder", "goal nudge"] {
            assert!(
                position(&rows, hidden).is_none(),
                "{hidden:?} must not render:\n{}",
                visible(&rows)
            );
        }
        assert_eq!(
            rows.iter().filter(|row| row.contains('↳')).count(),
            1,
            "exactly one notice row:\n{}",
            visible(&rows)
        );
    }

    #[test]
    fn history_subagent_error_projects_to_a_failed_row() {
        let store = history_store(vec![synthetic(
            "inb_2",
            "explore the repo",
            Some(subagent_metadata("error")),
        )]);
        let rows = rows_of(&store, 80);
        assert!(
            position(&rows, "! Subagent failed · explore the repo").is_some(),
            "{}",
            visible(&rows)
        );
    }

    #[test]
    fn narrow_width_truncates_the_description_with_an_ellipsis() {
        let mut store = store_with_assistant_text();
        enqueue(
            &mut store,
            "inb_long",
            synthetic_item(
                "body",
                "cargo test --workspace --all-targets --no-fail-fast --features everything",
                shell_metadata("completed"),
            ),
        );
        deliver(&mut store, "inb_long");

        let rows = rows_of(&store, 40);
        let row = rows
            .iter()
            .find(|row| row.contains("Shell finished"))
            .unwrap_or_else(|| panic!("notice row missing:\n{}", visible(&rows)));
        assert!(row.width() <= 40, "row is {} cells: {row:?}", row.width());
        assert!(row.ends_with('…'), "truncation marker missing: {row:?}");
        assert!(row.contains("↳ Shell finished · cargo test"), "{row:?}");
    }
}
