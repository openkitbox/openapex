//! Real-data test: parse actual `session_message` rows exported from the
//! upstream opencode database.
//!
//! The fixture at `tests/fixtures/upstream_messages.json` was generated with:
//!
//! ```sh
//! sqlite3 -json ~/.local/share/opencode/opencode.db \
//!   "SELECT id, type, data FROM (SELECT id, type, data, \
//!     ROW_NUMBER() OVER (PARTITION BY type ORDER BY time_created DESC) rn \
//!    FROM session_message) WHERE rn <= 4 ORDER BY type, id" \
//!   > tests/fixtures/upstream_messages.json
//! ```
//!
//! Each row mirrors the `session_message` table layout: `id` and `type` are
//! columns, `data` is the message JSON minus those two keys. If the fixture
//! file is absent (e.g. a fresh checkout without an upstream DB), the test
//! is skipped.

use ox::schema::message::Message;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/upstream_messages.json"
);

#[test]
fn upstream_message_rows_round_trip() {
    let Ok(raw) = std::fs::read_to_string(FIXTURE) else {
        eprintln!("fixture {FIXTURE} not present; skipping");
        return;
    };
    let rows: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("fixture is valid JSON");
    assert!(!rows.is_empty(), "fixture should contain rows");

    let mut types_seen = std::collections::BTreeSet::new();
    for row in &rows {
        let id = row["id"].as_str().expect("row.id");
        let kind = row["type"].as_str().expect("row.type");
        let data: serde_json::Value = serde_json::from_str(row["data"].as_str().expect("row.data"))
            .expect("row.data is JSON");
        types_seen.insert(kind.to_string());

        // Merge columns back like upstream history.ts::decodeMessageRow and
        // assert the typed model accepts real upstream data.
        let message = Message::from_row_data(id, kind, data.clone())
            .unwrap_or_else(|e| panic!("upstream row {id} ({kind}) failed to parse: {e}"));
        assert_eq!(message.id(), id);
        assert_eq!(message.kind(), kind);

        // Splitting back must reproduce the exact stored `data` column.
        let round = message.to_row_data();
        assert_eq!(round, data, "row {id} ({kind}) data column mismatch");
    }

    eprintln!(
        "parsed {} upstream rows, message types: {types_seen:?}",
        rows.len()
    );
}
