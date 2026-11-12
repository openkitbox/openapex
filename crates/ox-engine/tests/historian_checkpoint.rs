use ox_engine::mctx::historian::{self, ChunkLimits};
use serde_json::{Value, json};

fn checkpoint(status: &str, summary: &str, recent: &str) -> Value {
    let mut row = json!({
        "type": "compaction", "id": status, "time": {"created": 1},
        "status": status, "reason": "manual"
    });
    match status {
        "failed" => row["error"] = json!({"type": "error", "message": "failed"}),
        _ => {
            row["summary"] = json!(summary);
            row["recent"] = json!(recent);
        }
    }
    row
}

fn skill() -> Value {
    json!({"type": "skill", "id": "after", "time": {"created": 2}, "skill": "s", "name": "n", "text": "after"})
}

#[test]
fn completed_checkpoint_preserves_multiline_content_and_ordinals() {
    let summary = "  标题\n    缩进  空格\n\n结尾  ";
    let recent = "\n  最近\t上下文\n末尾\n";
    let rows = vec![
        (1, checkpoint("running", "running sentinel", "ignored")),
        (2, checkpoint("failed", "failed sentinel", "ignored")),
        (3, checkpoint("completed", summary, recent)),
        (4, skill()),
    ];
    let count = |text: &str| text.len();
    let chunk =
        historian::chunk(&rows, 1, 5, ChunkLimits::unbounded(), &count).expect("renderable rows");
    assert!(chunk.text.contains(summary), "{}", chunk.text);
    assert!(chunk.text.contains(recent), "{}", chunk.text);
    assert!(!chunk.text.contains("running sentinel"));
    assert!(!chunk.text.contains("failed sentinel"));
    assert_eq!(chunk.covered, 1..=4);
    assert_eq!(chunk.blocks[0].start_ordinal, 1);
    assert_eq!(chunk.blocks[0].end_ordinal, 3);
    assert_eq!(chunk.token_estimate, chunk.text.len());
    assert_eq!(chunk.context_tokens, chunk.text.len());
}

#[test]
fn oversized_checkpoint_never_advances_coverage() {
    let rows = vec![(1, checkpoint("completed", &"汉".repeat(1000), " recent\n"))];
    let count = |text: &str| text.len();
    for limits in [
        ChunkLimits {
            request: 100,
            context: usize::MAX,
        },
        ChunkLimits {
            request: usize::MAX,
            context: 100,
        },
    ] {
        assert!(historian::chunk(&rows, 1, 2, limits, &count).is_none());
    }
    let rows = vec![
        (1, skill()),
        (2, checkpoint("completed", &"汉".repeat(1000), "recent")),
    ];
    for limits in [
        ChunkLimits {
            request: 100,
            context: usize::MAX,
        },
        ChunkLimits {
            request: usize::MAX,
            context: 100,
        },
    ] {
        let chunk = historian::chunk(&rows, 1, 3, limits, &count).expect("earlier block fits");
        assert_eq!(chunk.covered, 1..=1);
        assert!(!chunk.text.contains("Checkpoint"));
    }
}

#[test]
fn fitting_checkpoints_can_be_processed_in_successive_chunks() {
    let rows = vec![
        (1, skill()),
        (2, checkpoint("completed", "一\n  二", "三\n四")),
        (3, skill()),
    ];
    let count = |text: &str| text.len();
    let whole = historian::chunk(&rows, 1, 4, ChunkLimits::unbounded(), &count).expect("whole");
    assert_eq!(whole.covered, 1..=3);
    let first = historian::chunk(
        &rows,
        1,
        4,
        ChunkLimits {
            request: 70,
            context: usize::MAX,
        },
        &count,
    )
    .expect("first");
    assert_eq!(first.covered, 1..=1);
    let second = historian::chunk(
        &rows,
        2,
        4,
        ChunkLimits {
            request: 100,
            context: usize::MAX,
        },
        &count,
    )
    .expect("checkpoint fits");
    assert!(second.text.contains("一\n  二"));
    assert!(second.text.contains("三\n四"));
    assert!(*second.covered.end() >= 2);
}
