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
