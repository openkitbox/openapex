//! The historian's chunk fence: it must measure in the counter it is handed.
//!
//! Both of the fence's ceilings ([`ChunkLimits`]) come from a model window, so
//! they are provider tokens. The rendered transcript and the collapsed
//! tool-result bodies are therefore counted with the session's calibrated
//! counter (`mctx::tokens::counter_fn`), not with the raw `bytes / 4`
//! estimate — otherwise the two sides of the comparison speak different units.
//!
//! These tests pin the injection point itself: the same rows cut to a
//! different range when the counter scales, and the absolute context ceiling
//! keeps rejecting oversize blocks whatever the scale.

use ox_engine::mctx::historian::{ChunkLimits, chunk};
use ox_engine::mctx::tokens::heuristic;
use ox_schema::message::{AssistantContent, AssistantMessage, AssistantTime, Message, TimeCreated};
use ox_schema::support::{ModelRef, Prompt};

/// One tail row in the shape [`chunk`] reads: the message encoded the way
/// `session_message.data` carries it.
fn row(ordinal: i64, role: &str, text: &str) -> (i64, serde_json::Value) {
    let id = format!("msg_{ordinal}");
    let message = if role == "user" {
        Message::User {
            id,
            metadata: None,
            time: TimeCreated { created: ordinal },
            prompt: Prompt {
                text: text.to_owned(),
                files: None,
                agents: None,
                skills: None,
            },
        }
    } else {
        Message::Assistant(Box::new(AssistantMessage {
            id,
            metadata: None,
            time: AssistantTime {
                created: ordinal,
                streamed: None,
                completed: Some(ordinal),
            },
            agent: "build".to_owned(),
            model: ModelRef {
                id: "mock".to_owned(),
                provider_id: "mock".to_owned(),
                variant: None,
            },
            content: vec![AssistantContent::Text {
                text: text.to_owned(),
                state: None,
            }],
            snapshot: None,
            finish: None,
            raw_finish: None,
            provider_state: None,
            cost: None,
            tokens: None,
            error: None,
            retry: None,
        }))
    };
    (ordinal, serde_json::to_value(&message).expect("row json"))
}

/// Alternating roles, so every message is its own block.
fn tail() -> Vec<(i64, serde_json::Value)> {
    vec![
        row(1, "user", "aaaa"),
        row(2, "assistant", "bbbb"),
        row(3, "user", "cccc"),
        row(4, "assistant", "dddd"),
    ]
}

#[test]
fn chunk_measures_in_the_injected_counter() {
    let rows = tail();
    let limits = ChunkLimits {
        request: 30,
        context: usize::MAX,
    };
    let raw = chunk(&rows, 1, i64::MAX, limits, &heuristic).expect("raw chunk");
    assert_eq!(
        raw.end_ordinal, 4,
        "every block fits the uncalibrated count"
    );
    let scaled_counter = |text: &str| heuristic(text) * 4;
    let scaled = chunk(&rows, 1, i64::MAX, limits, &scaled_counter).expect("scaled chunk");
    assert_eq!(
        scaled.end_ordinal, 2,
        "the calibrated count is four times larger, so it stops after one block"
    );
    // The framing is a unit change only: the rendered bytes are identical.
    assert!(scaled.text.starts_with("[1] U: aaaa"));
}

#[test]
fn chunk_fence_is_absolute_under_any_counter() {
    let rows = tail();
    let limits = ChunkLimits {
        request: usize::MAX,
        context: 5,
    };
    let raw = chunk(&rows, 1, i64::MAX, limits, &heuristic).expect("raw chunk");
    assert_eq!(raw.end_ordinal, 1, "one block fits the five-token fence");
    let scaled_counter = |text: &str| heuristic(text) * 4;
    assert!(
        chunk(&rows, 1, i64::MAX, limits, &scaled_counter).is_none(),
        "the absolute fence rejects the scaled block outright — it is never oversize-admitted"
    );
}
