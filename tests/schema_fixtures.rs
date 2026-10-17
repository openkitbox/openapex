//! Schema layer tests: hand-written JSON fixtures derived field-by-field
//! from the upstream Effect-Schema sources (`packages/schema/src/*.ts`),
//! asserting lossless serde round-trips on the exact wire JSON.

use ox::schema::event::{DurableEnvelope, EventEnvelope, SessionEventData};
use ox::schema::inbox::Info as InboxInfo;
use ox::schema::message::Message;
use serde_json::json;

/// Deserialize, re-serialize, and assert byte-level (Value) equality.
fn round_trip<T>(fixture: serde_json::Value)
where
    T: for<'de> serde::Deserialize<'de> + serde::Serialize + std::fmt::Debug,
{
    let parsed: T = serde_json::from_value(fixture.clone())
        .unwrap_or_else(|e| panic!("failed to parse fixture: {e}\nfixture: {fixture:#}"));
    let encoded = serde_json::to_value(&parsed).expect("failed to re-serialize");
    assert_eq!(encoded, fixture, "round-trip mismatch");
}

// ===========================================================================
// Messages — all 11 variants of SessionMessage.Info
// ===========================================================================

#[test]
fn message_agent_switched() {
    round_trip::<Message>(json!({
        "type": "agent-switched",
        "id": "msg_01",
        "time": { "created": 1700000000000_i64 },
        "agent": "orchestrator",
        "previous": "plan"
    }));
}

#[test]
fn message_model_switched() {
    round_trip::<Message>(json!({
        "type": "model-switched",
        "id": "msg_02",
        "time": { "created": 1700000000000_i64 },
        "model": { "id": "k3-256k", "providerID": "cardinal", "variant": "high" },
        "previous": { "id": "glm-4.6", "providerID": "cardinal" }
    }));
}

#[test]
fn message_location_switched() {
    round_trip::<Message>(json!({
        "type": "location-switched",
        "id": "msg_03",
        "metadata": { "host": "devbox" },
        "time": { "created": 1700000000000_i64 },
        "location": { "directory": "/repo", "workspaceID": "wrk_01" },
        "projectID": "prj_01",
        "subpath": "packages/app",
        "previous": {
            "location": { "directory": "/old" },
            "projectID": "prj_00"
        }
    }));
}

#[test]
fn message_user_minimal() {
    round_trip::<Message>(json!({
        "type": "user",
        "id": "msg_04",
        "time": { "created": 1700000000000_i64 },
        "text": "hello"
    }));
}

#[test]
fn message_user_with_attachments() {
    round_trip::<Message>(json!({
        "type": "user",
        "id": "msg_05",
        "time": { "created": 1700000000000_i64 },
        "text": "look at this file",
        "files": [
            {
                "data": "aGVsbG8=",
                "mime": "text/plain",
                "source": { "type": "inline" },
                "name": "note.txt",
                "description": "a note",
                "mention": { "start": 8, "end": 17, "text": "this file" }
            },
            {
                "data": "cGFzcw==",
                "mime": "image/png",
                "source": { "type": "uri", "uri": "file:///tmp/x.png" }
            }
        ],
        "agents": [
            { "name": "implementer", "mention": { "start": 0, "end": 4, "text": "@impl" } }
        ],
        "skills": [
            { "id": "skl_01", "name": "tdd", "text": "use tdd", "mention": { "start": 0, "end": 4, "text": "#tdd" } }
        ]
    }));
}

#[test]
fn message_synthetic() {
    round_trip::<Message>(json!({
        "type": "synthetic",
        "id": "msg_06",
        "time": { "created": 1700000000000_i64 },
        "text": "<system-reminder>do the thing</system-reminder>",
        "description": "injected reminder"
    }));
}

#[test]
fn message_system() {
    round_trip::<Message>(json!({
        "type": "system",
        "id": "msg_07",
        "time": { "created": 1700000000000_i64 },
        "text": "context compacted"
    }));
}

#[test]
fn message_skill() {
    round_trip::<Message>(json!({
        "type": "skill",
        "id": "msg_08",
        "time": { "created": 1700000000000_i64 },
        "skill": "skl_01",
        "name": "test-driven-development",
        "text": "# TDD skill content"
    }));
}

#[test]
fn message_shell() {
    round_trip::<Message>(json!({
        "type": "shell",
        "id": "msg_09",
        "shellID": "shl_01",
        "command": "cargo test",
        "status": "exited",
        "exit": 0,
        "output": { "output": "ok\n", "cursor": 3, "size": 3, "truncated": false },
        "time": { "created": 1700000000000_i64, "completed": 1700000001234_i64 }
    }));
}

#[test]
fn message_assistant_full() {
    round_trip::<Message>(json!({
        "type": "assistant",
        "id": "msg_10",
        "time": { "created": 1700000000000_i64, "streamed": 1700000001000_i64, "completed": 1700000002000_i64 },
        "agent": "orchestrator",
        "model": { "id": "k3-256k", "providerID": "cardinal" },
        "content": [
            { "type": "text", "text": "thinking...", "state": { "anthropic": { "signature": "sig" } } },
            { "type": "reasoning", "text": "hmm", "time": { "created": 1700000000100_i64, "completed": 1700000000500_i64 } },
            {
                "type": "tool",
                "id": "tool_01",
                "name": "shell",
                "executed": true,
                "providerState": { "openai": { "id": "call_1" } },
                "providerResultState": { "openai": { "ok": true } },
                "state": {
                    "status": "completed",
                    "input": { "command": "ls" },
                    "content": [
                        { "type": "text", "text": "file.rs" },
                        { "type": "file", "uri": "file:///tmp/out.txt", "mime": "text/plain", "name": "out.txt" }
                    ],
                    "metadata": { "duration": 42 }
                },
                "time": { "created": 1700000000600_i64, "ran": 1700000000700_i64, "completed": 1700000000800_i64 }
            },
            {
                "type": "tool",
                "id": "tool_02",
                "name": "edit",
                "state": { "status": "streaming", "input": "{\"filePath\":\"/x" },
                "time": { "created": 1700000000900_i64 }
            },
            {
                "type": "tool",
                "id": "tool_03",
                "name": "read",
                "state": { "status": "running", "input": { "path": "/x" }, "metadata": {} },
                "time": { "created": 1700000000950_i64, "ran": 1700000000960_i64 }
            },
            {
                "type": "tool",
                "id": "tool_04",
                "name": "write",
                "state": {
                    "status": "error",
                    "input": { "path": "/x" },
                    "error": { "type": "Tool.Failed", "message": "disk full", "status": 507, "response": { "body": "no space" } },
                    "content": [{ "type": "text", "text": "partial" }],
                    "metadata": {}
                },
                "time": { "created": 1700000000970_i64, "completed": 1700000000990_i64 }
            }
        ],
        "snapshot": { "start": "snp_01", "end": "snp_02", "files": ["/repo/src/a.rs"] },
        "finish": "tool-calls",
        "rawFinish": "tool_calls",
        "providerState": { "openai": { "responseId": "resp_1" } },
        "cost": 0.0123,
        "tokens": { "input": 100, "output": 50, "reasoning": 25, "cache": { "read": 10, "write": 5 } },
        "error": { "type": "LLM.Overloaded", "message": "slow down" },
        "retry": { "attempt": 2, "at": 1700000003000_i64, "error": { "type": "LLM.Overloaded", "message": "slow down" } }
    }));
}

#[test]
fn message_compaction_running() {
    round_trip::<Message>(json!({
        "type": "compaction",
        "id": "msg_11",
        "time": { "created": 1700000000000_i64 },
        "status": "running",
        "reason": "auto",
        "summary": "",
        "recent": "[]"
    }));
}

#[test]
fn message_compaction_completed() {
    round_trip::<Message>(json!({
        "type": "compaction",
        "id": "msg_12",
        "time": { "created": 1700000000000_i64 },
        "status": "completed",
        "reason": "manual",
        "model": { "id": "k3-256k", "providerID": "cardinal", "variant": "high" },
        "providerState": { "openai": { "id": "x" } },
        "summary": "conversation so far...",
        "recent": "[]",
        "providerContext": {
            "version": 1,
            "provenance": {
                "providerID": "cardinal",
                "provider": "cardinal",
                "modelID": "k3-256k",
                "route": "default",
                "protocol": "openai-chat",
                "endpoint": "http://localhost:8787/v1"
            },
            "messages": []
        },
        "cost": 0.001,
        "tokens": { "input": 1, "output": 2, "reasoning": 0, "cache": { "read": 0, "write": 0 } }
    }));
}

#[test]
fn message_compaction_failed() {
    round_trip::<Message>(json!({
        "type": "compaction",
        "id": "msg_13",
        "time": { "created": 1700000000000_i64 },
        "status": "failed",
        "reason": "auto",
        "error": { "type": "LLM.ContextOverflow", "message": "too long" }
    }));
}

#[test]
fn message_idle() {
    round_trip::<Message>(json!({
        "type": "idle",
        "id": "msg_14",
        "time": { "created": 1700000000000_i64 },
        "outcome": "interrupted",
        "error": { "type": "Session.Interrupted", "message": "user aborted" }
    }));
}

// ===========================================================================
// Message storage split (session_message table)
// ===========================================================================

#[test]
fn message_row_data_split_matches_upstream() {
    // Upstream stores `data` = message JSON minus `id` and `type`
    // (`DistributiveOmit<Info.Encoded, "type" | "id">` in
    // packages/core/src/session/sql.ts), and decodes with
    // `{ ...data, id, type }` (packages/core/src/session/history.ts).
    let full = json!({
        "type": "user",
        "id": "msg_row",
        "time": { "created": 1700000000000_i64 },
        "text": "hi",
        "files": []
    });
    let message: Message = serde_json::from_value(full).unwrap();
    assert_eq!(message.kind(), "user");
    assert_eq!(message.id(), "msg_row");
    assert_eq!(message.time_created(), 1700000000000);

    let row_data = message.to_row_data();
    assert_eq!(
        row_data,
        json!({ "time": { "created": 1700000000000_i64 }, "text": "hi", "files": [] })
    );

    let restored = Message::from_row_data("msg_row", "user", row_data).unwrap();
    assert_eq!(serde_json::to_value(&restored).unwrap(), {
        let mut v = serde_json::to_value(&message).unwrap();
        let _ = &mut v;
        v
    });
}

// ===========================================================================
// Inbox — Item + Info
// ===========================================================================

#[test]
fn inbox_info_user_queue() {
    round_trip::<InboxInfo>(json!({
        "type": "user",
        "id": "msg_inbox_01",
        "sessionID": "ses_01",
        "time": { "created": 1700000000000_i64 },
        "payload": { "text": "queued follow-up" },
        "delivery": "queue"
    }));
}

#[test]
fn inbox_info_user_steer() {
    round_trip::<InboxInfo>(json!({
        "type": "user",
        "id": "msg_inbox_02",
        "sessionID": "ses_01",
        "time": { "created": 1700000000000_i64 },
        "payload": {
            "text": "steer me",
            "files": [
                { "data": "AA==", "mime": "text/plain", "source": { "type": "inline" } }
            ],
            "metadata": { "origin": "tui" }
        },
        "delivery": "steer"
    }));
}

#[test]
fn inbox_info_compaction() {
    round_trip::<InboxInfo>(json!({
        "type": "compaction",
        "id": "msg_inbox_03",
        "sessionID": "ses_01",
        "time": { "created": 1700000000000_i64 },
        "payload": {},
        "delivery": "queue"
    }));
}

#[test]
fn inbox_info_move() {
    round_trip::<InboxInfo>(json!({
        "type": "move",
        "id": "msg_inbox_04",
        "sessionID": "ses_01",
        "time": { "created": 1700000000000_i64 },
        "payload": {
            "location": { "directory": "/elsewhere" },
            "projectID": "prj_02",
            "subpath": "pkg"
        },
        "delivery": "steer"
    }));
}

// ===========================================================================
// Events — envelope + every ported event type
// ===========================================================================

/// Build a full wire envelope around a data payload.
fn envelope(kind: &str, data: serde_json::Value) -> serde_json::Value {
    json!({
        "id": "evt_01",
        "created": 1700000000000_i64,
        "type": kind,
        "data": data,
        "durable": { "aggregateID": "ses_01", "seq": 7, "version": 1 }
    })
}

/// Round-trip a typed event through the generic wire envelope.
fn round_trip_event(kind: &str, data: serde_json::Value) {
    let wire = envelope(kind, data);
    let parsed: EventEnvelope = serde_json::from_value(wire.clone())
        .unwrap_or_else(|e| panic!("envelope parse failed for {kind}: {e}"));
    let typed = parsed.parse_typed();
    assert_eq!(
        typed.kind,
        typed.data.kind(),
        "kind invariant violated for {kind}"
    );
    let back = serde_json::to_value(typed.into_generic()).expect("encode failed");
    assert_eq!(back, wire, "event round-trip mismatch for {kind}");
}

#[test]
fn event_envelope_minimal_and_optional_parts() {
    // Ephemeral event: no durable/location/metadata at all.
    let minimal = json!({
        "id": "evt_min",
        "created": 1_i64,
        "type": "session.text.delta",
        "data": { "sessionID": "ses_01", "assistantMessageID": "msg_01", "ordinal": 0, "delta": "x" }
    });
    let parsed: EventEnvelope = serde_json::from_value(minimal.clone()).unwrap();
    assert!(parsed.durable.is_none());
    assert!(parsed.location.is_none());
    assert!(parsed.metadata.is_none());
    assert_eq!(serde_json::to_value(&parsed).unwrap(), minimal);

    // Full: all optional parts present.
    let full = json!({
        "id": "evt_full",
        "created": 1_i64,
        "type": "session.text.delta",
        "data": { "sessionID": "ses_01", "assistantMessageID": "msg_01", "ordinal": 0, "delta": "x" },
        "durable": { "aggregateID": "ses_01", "seq": 3, "version": 1 },
        "location": { "directory": "/repo", "workspaceID": "wrk_01" },
        "metadata": { "trace": "abc" }
    });
    let parsed: EventEnvelope = serde_json::from_value(full.clone()).unwrap();
    assert_eq!(
        parsed.durable,
        Some(DurableEnvelope {
            aggregate_id: "ses_01".into(),
            seq: 3,
            version: 1
        })
    );
    assert_eq!(serde_json::to_value(&parsed).unwrap(), full);
}

#[test]
fn event_session_created() {
    round_trip_event(
        "session.created",
        json!({
            "sessionID": "ses_01",
            "projectID": "prj_01",
            "project": { "id": "prj_01", "canonical": "/repo" },
            "location": { "directory": "/repo" },
            "slug": "brave-fox",
            "title": "hello world",
            "agent": "orchestrator",
            "model": { "id": "k3-256k", "providerID": "cardinal", "variant": "high" },
            "metadata": { "host": "devbox" },
            "permissions": [{ "action": "shell", "resource": "*", "effect": "allow" }],
            "version": "2.0.26"
        }),
    );
}

#[test]
fn event_session_renamed() {
    round_trip_event(
        "session.renamed",
        json!({ "sessionID": "ses_01", "title": "new title" }),
    );
}

#[test]
fn event_session_deleted() {
    round_trip_event("session.deleted", json!({ "sessionID": "ses_01" }));
}

#[test]
fn event_session_forked() {
    round_trip_event(
        "session.forked",
        json!({
            "sessionID": "ses_02",
            "parentID": "ses_01",
            "boundary": { "type": "through", "messageID": "msg_01" }
        }),
    );
    round_trip_event(
        "session.forked",
        json!({
            "sessionID": "ses_02",
            "parentID": "ses_01",
            "boundary": { "type": "before", "messageID": "msg_02" }
        }),
    );
}

#[test]
fn event_session_usage_recorded() {
    round_trip_event(
        "session.usage.recorded",
        json!({
            "sessionID": "ses_01",
            "source": "compaction",
            "cost": 0.01,
            "tokens": { "input": 1, "output": 2, "reasoning": 3, "cache": { "read": 4, "write": 5 } }
        }),
    );
}

#[test]
fn event_session_usage_updated() {
    round_trip_event(
        "session.usage.updated",
        json!({
            "sessionID": "ses_01",
            "cost": 0.5,
            "tokens": { "input": 100, "output": 200, "reasoning": 0, "cache": { "read": 0, "write": 0 } }
        }),
    );
}

#[test]
fn event_session_synthetic() {
    round_trip_event(
        "session.synthetic",
        json!({
            "sessionID": "ses_01",
            "text": "<reminder/>",
            "description": "a reminder",
            "metadata": { "k": "v" }
        }),
    );
}

#[test]
fn event_session_retry_scheduled() {
    round_trip_event(
        "session.retry.scheduled",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "attempt": 3,
            "at": 1700000010000_i64,
            "error": { "type": "LLM.Overloaded", "message": "slow down", "status": 429 }
        }),
    );
}

#[test]
fn event_session_message_content_updated() {
    round_trip_event(
        "session.message.content.updated",
        json!({
            "sessionID": "ses_01",
            "messageID": "msg_01",
            "content": [
                { "type": "text", "text": "hello" },
                {
                    "type": "tool",
                    "id": "tool_01",
                    "name": "read",
                    "state": { "status": "running", "input": {}, "metadata": {} },
                    "time": { "created": 1700000000000_i64 }
                }
            ]
        }),
    );
}

#[test]
fn event_session_moved() {
    round_trip_event(
        "session.moved",
        json!({
            "sessionID": "ses_01",
            "location": { "directory": "/new" },
            "projectID": "prj_02"
        }),
    );
}

#[test]
fn event_execution_group() {
    round_trip_event(
        "session.execution.started",
        json!({ "sessionID": "ses_01" }),
    );
    round_trip_event(
        "session.execution.succeeded",
        json!({ "sessionID": "ses_01" }),
    );
    round_trip_event(
        "session.execution.failed",
        json!({
            "sessionID": "ses_01",
            "error": { "type": "LLM.Transport", "message": "connection reset" }
        }),
    );
    for reason in ["user", "shutdown", "superseded", "inactivity"] {
        round_trip_event(
            "session.execution.interrupted",
            json!({ "sessionID": "ses_01", "reason": reason }),
        );
    }
}

#[test]
fn event_inbox_group() {
    round_trip_event(
        "session.inbox.enqueued",
        json!({
            "sessionID": "ses_01",
            "inboxID": "msg_inbox_01",
            "item": {
                "type": "user",
                "payload": { "text": "steer" },
                "delivery": "steer"
            }
        }),
    );
    round_trip_event(
        "session.inbox.enqueued",
        json!({
            "sessionID": "ses_01",
            "inboxID": "msg_inbox_02",
            "item": { "type": "compaction", "payload": {}, "delivery": "queue" }
        }),
    );
    round_trip_event(
        "session.inbox.delivered",
        json!({ "sessionID": "ses_01", "inboxID": "msg_inbox_01" }),
    );
    round_trip_event(
        "session.inbox.cancelled",
        json!({ "sessionID": "ses_01", "inboxID": "msg_inbox_01" }),
    );
    round_trip_event(
        "session.inbox.delivery.changed",
        json!({ "sessionID": "ses_01", "inboxID": "msg_inbox_01", "delivery": "queue" }),
    );
}

#[test]
fn event_step_group() {
    round_trip_event(
        "session.step.started",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "agent": "orchestrator",
            "model": { "id": "k3-256k", "providerID": "cardinal" },
            "snapshot": "snp_01",
            "started": 1700000000000_i64
        }),
    );
    round_trip_event(
        "session.step.streamed",
        json!({ "sessionID": "ses_01", "assistantMessageID": "msg_01" }),
    );
    round_trip_event(
        "session.step.ended",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "finish": "stop",
            "rawFinish": "end_turn",
            "providerState": { "k": 1 },
            "cost": 0.02,
            "tokens": { "input": 1, "output": 2, "reasoning": 3, "cache": { "read": 4, "write": 5 } },
            "snapshot": "snp_02",
            "files": ["/repo/a.rs"]
        }),
    );
    round_trip_event(
        "session.step.failed",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "error": { "type": "LLM.ContentFilter", "message": "filtered" },
            "finish": "content-filter"
        }),
    );
    // Upstream wire bug: a resumed tool-call can emit a *zero* `started`
    // timestamp (packages/core/src/session/runner/step.ts publishes with
    // `started ?? 0`-style fallback). Must still deserialize.
    round_trip_event(
        "session.step.started",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "agent": "orchestrator",
            "model": { "id": "k3-256k", "providerID": "cardinal" },
            "started": 0
        }),
    );
}

#[test]
fn event_text_group() {
    round_trip_event(
        "session.text.started",
        json!({ "sessionID": "ses_01", "assistantMessageID": "msg_01", "ordinal": 0 }),
    );
    round_trip_event(
        "session.text.delta",
        json!({ "sessionID": "ses_01", "assistantMessageID": "msg_01", "ordinal": 0, "delta": "chunk" }),
    );
    round_trip_event(
        "session.text.ended",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "ordinal": 0,
            "text": "full text",
            "state": { "sig": "abc" }
        }),
    );
}

#[test]
fn event_reasoning_group() {
    round_trip_event(
        "session.reasoning.started",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "ordinal": 1,
            "state": { "encrypted": "..." }
        }),
    );
    round_trip_event(
        "session.reasoning.delta",
        json!({ "sessionID": "ses_01", "assistantMessageID": "msg_01", "ordinal": 1, "delta": "hmm" }),
    );
    round_trip_event(
        "session.reasoning.ended",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "ordinal": 1,
            "text": "full reasoning"
        }),
    );
}

#[test]
fn event_tool_group() {
    round_trip_event(
        "session.tool.input.started",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "name": "shell"
        }),
    );
    round_trip_event(
        "session.tool.input.delta",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "delta": "{\"command\":"
        }),
    );
    round_trip_event(
        "session.tool.input.ended",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "text": "{\"command\":\"ls\"}"
        }),
    );
    round_trip_event(
        "session.tool.called",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "input": { "command": "ls" },
            "executed": true,
            "state": { "openai": { "call": "1" } }
        }),
    );
    round_trip_event(
        "session.tool.progress",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "metadata": { "percent": 50 }
        }),
    );
    round_trip_event(
        "session.tool.success",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "content": [{ "type": "text", "text": "file.rs" }],
            "metadata": { "duration": 12 },
            "executed": true,
            "resultState": { "openai": { "ok": true } }
        }),
    );
    round_trip_event(
        "session.tool.failed",
        json!({
            "sessionID": "ses_01",
            "assistantMessageID": "msg_01",
            "id": "tool_01",
            "error": { "type": "Tool.Failed", "message": "boom" },
            "content": [{ "type": "text", "text": "partial" }],
            "executed": false
        }),
    );
}

#[test]
fn event_compaction_group() {
    round_trip_event(
        "session.compaction.started",
        json!({ "sessionID": "ses_01", "reason": "auto", "recent": "[]", "inputID": "msg_inbox_01" }),
    );
    round_trip_event(
        "session.compaction.delta",
        json!({ "sessionID": "ses_01", "text": "summarizing..." }),
    );
    round_trip_event(
        "session.compaction.ended",
        json!({
            "sessionID": "ses_01",
            "reason": "manual",
            "model": { "id": "k3-256k", "providerID": "cardinal" },
            "text": "summary text",
            "recent": "[]",
            "cost": 0.001,
            "tokens": { "input": 1, "output": 2, "reasoning": 0, "cache": { "read": 0, "write": 0 } }
        }),
    );
    round_trip_event(
        "session.compaction.failed",
        json!({
            "sessionID": "ses_01",
            "reason": "auto",
            "error": { "type": "LLM.ContextOverflow", "message": "overflow" }
        }),
    );
}

// ===========================================================================
// Unknown event types — lossless fallback
// ===========================================================================

#[test]
fn unknown_event_round_trips_losslessly() {
    // A skipped group (revert) with arbitrary data must survive intact.
    let wire = envelope(
        "session.revert.staged",
        json!({
            "sessionID": "ses_01",
            "snapshot": "snp_99",
            "anything": [1, { "nested": true }, null],
            "weirdNumber": 1.5e300
        }),
    );
    let parsed: EventEnvelope = serde_json::from_value(wire.clone()).unwrap();
    let typed = parsed.parse_typed();
    match &typed.data {
        SessionEventData::Unknown { r#type, data } => {
            assert_eq!(r#type, "session.revert.staged");
            assert_eq!(data, &wire["data"]);
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    let back = serde_json::to_value(typed.into_generic()).unwrap();
    assert_eq!(back, wire);

    // Other un-ported groups fall back the same way.
    for kind in [
        "session.skill.started",
        "session.shell.started",
        "session.instructions.updated",
        "session.revert.committed",
        "session.viewed",
        "session.archived",
        "session.idle",
        "some.future.event",
    ] {
        let wire = envelope(kind, json!({ "sessionID": "ses_01", "extra": { "a": 1 } }));
        let parsed: EventEnvelope = serde_json::from_value(wire.clone()).unwrap();
        let typed = parsed.parse_typed();
        assert!(
            matches!(typed.data, SessionEventData::Unknown { .. }),
            "{kind}"
        );
        assert_eq!(
            serde_json::to_value(typed.into_generic()).unwrap(),
            wire,
            "{kind}"
        );
    }
}
