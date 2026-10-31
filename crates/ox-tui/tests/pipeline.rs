use ox_schema::event::EventEnvelope;
use ox_tui::pipeline::blocks_from_store;
use ox_tui::shell_captures::ShellCaptures;
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

    let blocks = blocks_from_store(&store, &ShellCaptures::default());
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
    use ox_tui::shell_captures::ShellCaptures;
    use ox_tui::ui::timeline::Transcript;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use serde_json::{Value, json};
    use unicode_width::UnicodeWidthStr;

    pub(super) const SESSION: &str = "ses_parent";

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
    pub(super) fn wire(id: &str, data: SessionEventData) -> EventEnvelope<SessionEventData> {
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

    pub(super) fn enqueue(store: &mut DataStore, inbox_id: &str, item: InboxItem) {
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

    pub(super) fn deliver(store: &mut DataStore, inbox_id: &str) {
        let event = wire(
            &format!("evt_del_{inbox_id}"),
            SessionEventData::InboxDelivered {
                session_id: SESSION.into(),
                inbox_id: inbox_id.into(),
            },
        );
        ox_client::store::reduce(store, &event);
    }

    pub(super) fn cancel(store: &mut DataStore, inbox_id: &str) {
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
    pub(super) fn rows_of(store: &DataStore, width: u16) -> Vec<String> {
        rows_sized(store, width, 12)
    }

    pub(super) fn rows_sized(store: &DataStore, width: u16, height: u16) -> Vec<String> {
        let mut transcript = Transcript::new(width, height);
        transcript.sync(&blocks_from_store(store, &ShellCaptures::default()), 0);
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

    pub(super) fn position(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    pub(super) fn visible(rows: &[String]) -> String {
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

// ---------------------------------------------------------------------------
// Session-level rows (upstream `routes/session/index.tsx` ExecutionError,
// AssistantFooter error/retry, SessionSwitchMessageV2, SessionNoticeMessageV2,
// SessionSkillMessage, CompactionQueued) from wire JSON: live envelopes in the
// shapes `ox-schema/src/event.rs` serialises, history messages in the shapes
// `ox-core/src/projector/message_updater.rs` writes.
// ---------------------------------------------------------------------------

mod session_rows {
    use ox_client::store::DataStore;
    use ox_schema::event::EventEnvelope;
    use ox_schema::inbox::{CompactionPayload, Delivery, Item as InboxItem};
    use ox_schema::message::Message;
    use serde_json::{Value, json};

    use super::completion_notices::{
        SESSION, cancel, deliver, enqueue, position, rows_sized, visible,
    };

    fn apply(store: &mut DataStore, created: i64, kind: &str, data: Value) {
        let envelope: EventEnvelope<Value> = serde_json::from_value(json!({
            "id": format!("evt_{kind}_{created}"), "created": created, "type": kind, "data": data,
        }))
        .expect("event fixture is a valid envelope");
        ox_client::store::reduce(store, &envelope.parse_typed());
    }

    fn step_started(store: &mut DataStore, created: i64, message: &str) {
        apply(
            store,
            created,
            "session.step.started",
            json!({"sessionID": SESSION, "assistantMessageID": message, "agent": "build",
                   "model": {"id": "m", "providerID": "p"}, "started": created}),
        );
    }

    fn text(store: &mut DataStore, created: i64, message: &str, body: &str) {
        apply(
            store,
            created,
            "session.text.started",
            json!({"sessionID": SESSION, "assistantMessageID": message, "ordinal": 0}),
        );
        apply(
            store,
            created,
            "session.text.ended",
            json!({"sessionID": SESSION, "assistantMessageID": message, "ordinal": 0, "text": body}),
        );
    }

    fn error(message: &str) -> Value {
        json!({"type": "provider", "message": message})
    }

    /// A turn whose one step streamed `body` and then failed with `failure`.
    fn failed_turn(failure: &str) -> DataStore {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        apply(
            &mut store,
            1_000,
            "session.execution.started",
            json!({"sessionID": SESSION}),
        );
        step_started(&mut store, 1_100, "msg_1");
        text(&mut store, 1_200, "msg_1", "partial answer");
        apply(
            &mut store,
            1_300,
            "session.step.failed",
            json!({"sessionID": SESSION, "assistantMessageID": "msg_1", "error": error(failure)}),
        );
        store
    }

    fn draw(store: &DataStore) -> Vec<String> {
        rows_sized(store, 80, 30)
    }

    fn count(rows: &[String], needle: &str) -> usize {
        rows.iter().filter(|row| row.contains(needle)).count()
    }

    #[test]
    fn live_step_failure_shows_the_error_above_the_footer_once() {
        let mut store = failed_turn("upstream exploded");
        apply(
            &mut store,
            1_400,
            "session.execution.failed",
            json!({"sessionID": SESSION, "error": error("upstream exploded")}),
        );
        let rows = draw(&store);
        let text_row = position(&rows, "partial answer").expect("assistant text");
        let error_row = position(&rows, "   Error: upstream exploded")
            .unwrap_or_else(|| panic!("error row missing:\n{}", visible(&rows)));
        let footer_row = position(&rows, "Build").unwrap_or_else(|| {
            panic!("footer missing:\n{}", visible(&rows));
        });
        assert!(
            text_row < error_row && error_row < footer_row,
            "text, error, footer:\n{}",
            visible(&rows)
        );
        assert_eq!(
            count(&rows, "Error:"),
            1,
            "the execution failure repeats the step error:\n{}",
            visible(&rows)
        );
    }

    #[test]
    fn a_different_execution_failure_is_its_own_row_after_the_turn() {
        let mut store = failed_turn("step broke");
        apply(
            &mut store,
            1_400,
            "session.execution.failed",
            json!({"sessionID": SESSION, "error": error("run broke")}),
        );
        let rows = draw(&store);
        let step = position(&rows, "Error: step broke").expect("step error");
        let run = position(&rows, "   Error: run broke")
            .unwrap_or_else(|| panic!("execution error row missing:\n{}", visible(&rows)));
        assert!(step < run, "{}", visible(&rows));
    }

    #[test]
    fn an_execution_failure_without_any_step_is_an_error_row() {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        store.push_user("hi");
        apply(
            &mut store,
            1_400,
            "session.execution.failed",
            json!({"sessionID": SESSION, "error": error("no route")}),
        );
        let rows = draw(&store);
        assert!(
            position(&rows, "   Error: no route").is_some(),
            "{}",
            visible(&rows)
        );
    }

    fn scheduled_retry(at: i64) -> DataStore {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        apply(
            &mut store,
            1_000,
            "session.execution.started",
            json!({"sessionID": SESSION}),
        );
        step_started(&mut store, 1_100, "msg_1");
        apply(
            &mut store,
            1_200,
            "session.retry.scheduled",
            json!({"sessionID": SESSION, "assistantMessageID": "msg_1", "attempt": 2,
                   "at": at, "error": error("rate limited")}),
        );
        store
    }

    #[test]
    fn a_scheduled_retry_counts_down_with_the_attempt_and_shows_the_footer() {
        let far = 32_503_680_000_000; // year 3000: always in the future
        let store = scheduled_retry(far);
        let rows = draw(&store);
        let retry = rows
            .iter()
            .find(|row| row.contains("⚠ Retrying in "))
            .unwrap_or_else(|| panic!("retry row missing:\n{}", visible(&rows)));
        assert!(
            retry.starts_with("   ⚠ Retrying in ")
                && retry.ends_with("s · attempt 2 · rate limited"),
            "{retry:?}"
        );
        assert!(
            position(&rows, "Build").is_some(),
            "a retrying message shows its footer:\n{}",
            visible(&rows)
        );
    }

    #[test]
    fn a_past_retry_is_due_and_the_next_step_clears_it() {
        let mut store = scheduled_retry(0);
        let rows = draw(&store);
        assert!(
            position(&rows, "   ⚠ Retry due · attempt 2 · rate limited").is_some(),
            "{}",
            visible(&rows)
        );
        step_started(&mut store, 1_300, "msg_1");
        let rows = draw(&store);
        assert!(
            position(&rows, "⚠").is_none(),
            "retry cleared:\n{}",
            visible(&rows)
        );
    }

    #[test]
    fn live_agent_and_model_switches_are_muted_rows() {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        apply(
            &mut store,
            1_000,
            "session.agent.selected",
            json!({"sessionID": SESSION, "agent": "plan"}),
        );
        apply(
            &mut store,
            1_100,
            "session.agent.selected",
            json!({"sessionID": SESSION, "agent": "build", "previous": "plan"}),
        );
        apply(
            &mut store,
            1_200,
            "session.model.selected",
            json!({"sessionID": SESSION, "model": {"id": "m2", "providerID": "p"},
                   "previous": {"id": "m1", "providerID": "p", "variant": "default"}}),
        );
        apply(
            &mut store,
            1_300,
            "session.model.selected",
            json!({"sessionID": SESSION, "model": {"id": "m2", "providerID": "p", "variant": "high"}}),
        );
        let rows = draw(&store);
        let expected = [
            "   Switched agent to Plan",
            "   Switched agent from Plan to Build",
            "   Switched model to p/m2",
            "   Switched variant to high",
        ];
        let mut last = None;
        for line in expected {
            let at = position(&rows, line)
                .unwrap_or_else(|| panic!("{line:?} missing:\n{}", visible(&rows)));
            assert!(last < Some(at), "order of {line:?}:\n{}", visible(&rows));
            last = Some(at);
        }
    }

    #[test]
    fn live_move_and_instructions_update_are_rows_and_bookkeeping_is_not() {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        apply(
            &mut store,
            1_000,
            "session.moved",
            json!({"sessionID": SESSION, "location": {"directory": "/work/next"},
                   "projectID": "prj_1"}),
        );
        apply(
            &mut store,
            1_100,
            "session.instructions.updated",
            json!({"sessionID": SESSION, "delta": {"AGENTS.md": "h1", "docs/x.md": "h2"},
                   "text": "<instructions/>"}),
        );
        apply(
            &mut store,
            1_200,
            "session.instructions.updated",
            json!({"sessionID": SESSION, "delta": {"ignored.md": "h3"}}),
        );
        let rows = draw(&store);
        assert!(
            position(&rows, "   ↳ Moved to /work/next").is_some(),
            "{}",
            visible(&rows)
        );
        assert!(
            position(&rows, "   ◈ Instructions updated: AGENTS.md, docs/x.md").is_some(),
            "{}",
            visible(&rows)
        );
        assert!(
            position(&rows, "ignored.md").is_none(),
            "{}",
            visible(&rows)
        );
    }

    fn history(list: Value) -> DataStore {
        let bytes = serde_json::to_vec(&list).expect("json value serializes");
        let messages: Vec<Message> =
            serde_json::from_slice(&bytes).expect("history fixture parses as messages");
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        store.replace_items(ox_client::history::parse_messages(&messages));
        store
    }

    #[test]
    fn history_switches_notices_and_skills_render_in_order() {
        let store = history(json!([
            {"type": "agent-switched", "id": "m1", "time": {"created": 1},
             "agent": "build", "previous": "plan"},
            {"type": "model-switched", "id": "m2", "time": {"created": 2},
             "model": {"providerID": "p", "id": "m", "variant": "high"},
             "previous": {"providerID": "p", "id": "m"}},
            {"type": "location-switched", "id": "m3", "time": {"created": 3},
             "location": {"directory": "/repo/sub"}, "projectID": "prj"},
            {"type": "system", "id": "m4", "time": {"created": 4}, "text": "<x/>"},
            {"type": "system", "id": "m5", "time": {"created": 5}, "text": "<x/>",
             "description": "Instructions updated: AGENTS.md"},
            {"type": "skill", "id": "m6", "time": {"created": 6}, "skill": "sk_1",
             "name": "rust-expert", "text": "body"},
        ]));
        let rows = draw(&store);
        let expected = [
            "   Switched agent from Plan to Build",
            "   Switched variant to high",
            "   ↳ Moved to /repo/sub",
            "   ◈ Instructions updated",
            "   ◈ Instructions updated: AGENTS.md",
            "   → Skill rust-expert",
        ];
        let mut last = None;
        for line in expected {
            let at = rows
                .iter()
                .enumerate()
                .skip(last.map_or(0, |l: usize| l + 1))
                .find(|(_, row)| row.contains(line))
                .map(|(at, _)| at)
                .unwrap_or_else(|| panic!("{line:?} missing in order:\n{}", visible(&rows)));
            last = Some(at);
        }
    }

    fn assistant(id: &str, created: i64, extra: Value) -> Value {
        let mut message = json!({"type": "assistant", "id": id,
            "time": {"created": created, "completed": created + 1},
            "agent": "build", "model": {"providerID": "p", "id": "m"},
            "content": [{"type": "text", "text": "answer"}]});
        if let (Some(target), Some(fields)) = (message.as_object_mut(), extra.as_object()) {
            target.extend(fields.clone());
        }
        message
    }

    #[test]
    fn history_assistant_error_is_above_its_footer_and_a_repeated_idle_error_is_hidden() {
        let store = history(json!([
            {"type": "user", "id": "u1", "time": {"created": 1}, "text": "go"},
            assistant("a1", 2, json!({"finish": "error", "error": error("boom")})),
            {"type": "idle", "id": "i1", "time": {"created": 4}, "outcome": "failed",
             "error": error("boom")},
            {"type": "user", "id": "u2", "time": {"created": 5}, "text": "again"},
            assistant("a2", 6, json!({"finish": "stop"})),
            {"type": "idle", "id": "i2", "time": {"created": 8}, "outcome": "failed",
             "error": error("late failure")},
        ]));
        let rows = draw(&store);
        let error = position(&rows, "   Error: boom")
            .unwrap_or_else(|| panic!("assistant error missing:\n{}", visible(&rows)));
        let footer = position(&rows, "Build").expect("footer");
        assert!(error < footer, "{}", visible(&rows));
        assert_eq!(count(&rows, "Error: boom"), 1, "{}", visible(&rows));
        assert!(
            position(&rows, "   Error: late failure").is_some(),
            "{}",
            visible(&rows)
        );
    }

    #[test]
    fn history_retry_on_an_empty_assistant_still_renders() {
        let mut message = assistant(
            "a1",
            2,
            json!({"retry": {"attempt": 3, "at": 0,
            "error": error("overloaded")}}),
        );
        message["content"] = json!([]);
        let store = history(json!([
            {"type": "user", "id": "u1", "time": {"created": 1}, "text": "go"},
            message,
        ]));
        let rows = draw(&store);
        assert!(
            position(&rows, "   ⚠ Retry due · attempt 3 · overloaded").is_some(),
            "{}",
            visible(&rows)
        );
    }

    fn compaction_item() -> InboxItem {
        InboxItem::Compaction {
            payload: CompactionPayload {},
            delivery: Delivery::Queue,
        }
    }

    #[test]
    fn a_queued_compaction_shows_until_delivered_or_cancelled() {
        let mut store = DataStore::default();
        store.session = Some(SESSION.into());
        store.push_user("hi");
        enqueue(&mut store, "inb_c1", compaction_item());
        let rows = draw(&store);
        let row = rows
            .iter()
            .find(|row| row.contains("◇ Compaction queued"))
            .unwrap_or_else(|| panic!("queued row missing:\n{}", visible(&rows)));
        assert!(row.starts_with('─') && row.ends_with('─'), "{row:?}");
        assert_eq!(row.chars().count(), 80, "full-width rule: {row:?}");

        deliver(&mut store, "inb_c1");
        assert!(position(&rows_sized(&store, 80, 30), "Compaction queued").is_none());

        enqueue(&mut store, "inb_c2", compaction_item());
        cancel(&mut store, "inb_c2");
        assert!(position(&rows_sized(&store, 80, 30), "Compaction queued").is_none());
    }
}
