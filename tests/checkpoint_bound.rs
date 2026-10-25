//! What a fold charges its own input for, and what it does not publish.
//!
//! A fold's checkpoint is the head of every later request. Two properties keep
//! it small:
//!
//! * the previous checkpoint is one of the fold's input rows, and its rendered
//!   `<conversation-checkpoint>` message is what the segment completion reads,
//!   so [`chunk_rows`] charges that row its rendered size ([`fold_row_text`]).
//!   Charging nothing let a checkpoint ride a segment for free and handed one
//!   completion far more than its budget;
//! * the per-segment summaries are the consolidation completion's input only.
//!   Publishing them made the checkpoint grow with the session, so
//!   `summarize_hierarchical` returns the consolidation alone — covered
//!   end-to-end by `tests/runner.rs::a_fold_resumes_from_the_previous_checkpoint`
//!   and by the shape assertions below.

use ox::runner::compaction::chunk_rows;
use serde_json::json;

/// Bytes as tokens: exact, tiny, and independent of the calibrated estimator.
fn bytes(text: &str) -> usize {
    text.len()
}

#[test]
fn a_fold_segment_is_charged_its_rendered_checkpoint_row() {
    let count: &dyn Fn(&str) -> usize = &bytes;
    // 100 KB of summary is 100k "tokens" under `bytes`, well past the
    // segment cap (24,000): the checkpoint owns a segment.
    let big = "s".repeat(100 * 1024);
    let rows = vec![
        (0i64, json!({ "type": "user", "text": "before" })),
        (
            1,
            json!({ "type": "compaction", "summary": big, "recent": "" }),
        ),
        (2, json!({ "type": "user", "text": "after" })),
    ];
    let chunks = chunk_rows(&rows, count);
    assert_eq!(chunks.len(), 3, "the checkpoint row gets its own segment");
    assert!(chunks.iter().all(|chunk| chunk.len() == 1), "{chunks:?}");

    // Control: the same three rows with an empty summary share one segment, so
    // the split above is the checkpoint's rendered cost and not its presence.
    let small = vec![
        (0i64, json!({ "type": "user", "text": "before" })),
        (
            1,
            json!({ "type": "compaction", "summary": "", "recent": "" }),
        ),
        (2, json!({ "type": "user", "text": "after" })),
    ];
    assert_eq!(chunk_rows(&small, count).len(), 1);
}
