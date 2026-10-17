//! Integration tests for the event bus + in-transaction projector.

use ox::db::Db;
use ox::event_bus::EventBus;
use ox::schema::event::SessionEventData;
use ox::schema::inbox::{Delivery, Item, UserPayload};
use ox::schema::support::{LocationRef, ModelRef, Prompt, TokenCache, TokenUsage};

fn location() -> LocationRef {
    LocationRef {
        directory: "/tmp/ox".to_string(),
        workspace_id: None,
    }
}

fn model() -> ModelRef {
    ModelRef {
        id: "claude-test".to_string(),
        provider_id: "anthropic".to_string(),
        variant: None,
    }
}

fn tokens() -> TokenUsage {
    TokenUsage {
        input: 10,
        output: 5,
        reasoning: 2,
        cache: TokenCache { read: 1, write: 0 },
    }
}

fn user_item(text: &str) -> Item {
    Item::User {
        payload: UserPayload {
            prompt: Prompt {
                text: text.to_string(),
                files: None,
                agents: None,
                skills: None,
            },
            metadata: None,
        },
        delivery: Delivery::Queue,
    }
}

async fn bus() -> EventBus {
    EventBus::new(Db::open_in_memory().await.unwrap())
}

async fn create(bus: &EventBus, sid: &str) {
    bus.publish(SessionEventData::SessionCreated {
        session_id: sid.to_string(),
        project_id: "proj".to_string(),
        location: location(),
        subpath: None,
        parent_id: None,
        slug: sid.to_string(),
        title: Some("Test".to_string()),
        agent: Some("build".to_string()),
        model: Some(model()),
        metadata: None,
        permissions: None,
        version: "1.0.0".to_string(),
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Sequencing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn monotonic_seq_per_aggregate() {
    let bus = bus().await;
    create(&bus, "s1").await;
    create(&bus, "s2").await;

    let e1 = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s1".to_string(),
            title: "a".to_string(),
        })
        .await
        .unwrap();
    let e2 = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s1".to_string(),
            title: "b".to_string(),
        })
        .await
        .unwrap();
    let e3 = bus
        .publish(SessionEventData::SessionRenamed {
            session_id: "s2".to_string(),
            title: "c".to_string(),
        })
        .await
        .unwrap();

    // s1: created=0, rename=1, rename=2. s2: created=0, rename=1.
    assert_eq!(e1.durable.as_ref().unwrap().seq, 1);
    assert_eq!(e2.durable.as_ref().unwrap().seq, 2);
    assert_eq!(e3.durable.as_ref().unwrap().seq, 1);
}

#[tokio::test]
async fn concurrent_publishes_serialize() {
    let bus = std::sync::Arc::new(bus().await);
    create(&bus, "s1").await;

    let n = 16;
    let mut handles = Vec::new();
    for i in 0..n {
        let b = std::sync::Arc::clone(&bus);
        handles.push(tokio::spawn(async move {
            b.publish(SessionEventData::SessionRenamed {
                session_id: "s1".to_string(),
                title: format!("t{i}"),
            })
            .await
            .unwrap()
            .durable
            .unwrap()
            .seq
        }));
    }
    let mut seqs: Vec<i64> = Vec::new();
    for h in handles {
        seqs.push(h.await.unwrap());
    }
    seqs.sort_unstable();
    // created is seq 0, so renames are seqs 1..=n.
    let expected: Vec<i64> = (1..=n as i64).collect();
    assert_eq!(seqs, expected, "no duplicate or missing seqs");
}

// ---------------------------------------------------------------------------
// Rollback on projector error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn projector_error_rolls_back() {
    let bus = bus().await;
    create(&bus, "s1").await;

    // Deliver a nonexistent inbox row → projector error → rollback.
    let res = bus
        .publish(SessionEventData::InboxDelivered {
            session_id: "s1".to_string(),
            inbox_id: "nonexistent".to_string(),
        })
        .await;
    assert!(res.is_err());

    // event_sequence must be untouched (still at created's seq 0).
    let conn = bus.db_conn().clone();
    let seq: i64 = conn
        .call(|c| {
            c.query_row(
                "SELECT seq FROM event_sequence WHERE aggregate_id = 's1'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(seq, 0);

    // No event rows beyond the created one.
    let count: i64 = conn
        .call(|c| {
            c.query_row(
                "SELECT count(*) FROM event WHERE aggregate_id = 's1'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
}

// ---------------------------------------------------------------------------
// Happy-path projection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_projection() {
    let bus = bus().await;
    create(&bus, "s1").await;

    // inbox.enqueued
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        item: user_item("hello"),
    })
    .await
    .unwrap();

    // inbox.delivered → user message row.
    let del = bus
        .publish(SessionEventData::InboxDelivered {
            session_id: "s1".to_string(),
            inbox_id: "ib1".to_string(),
        })
        .await
        .unwrap();
    let del_seq = del.durable.as_ref().unwrap().seq;

    let conn = bus.db_conn().clone();
    let (mtype, mseq): (String, i64) = conn
        .call(move |c| {
            c.query_row(
                "SELECT type, seq FROM session_message WHERE id = 'ib1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(mtype, "user");
    assert_eq!(mseq, del_seq, "promoted message seq = deliver event seq");

    // step.started → assistant row
    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 1000,
    })
    .await
    .unwrap();

    // text.started / ended
    bus.publish(SessionEventData::TextStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::TextEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        ordinal: 0,
        text: "world".to_string(),
        state: None,
    })
    .await
    .unwrap();

    // step.ended → usage accumulated
    bus.publish(SessionEventData::StepEnded {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        finish: ox::schema::support::FinishReason::Stop,
        raw_finish: None,
        provider_state: None,
        cost: ox::schema::support::Finite::Float(0.5),
        tokens: tokens(),
        snapshot: None,
        files: None,
    })
    .await
    .unwrap();

    // execution.succeeded → time_idle + idle_outcome
    bus.publish(SessionEventData::ExecutionSucceeded {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();

    let conn2 = bus.db_conn().clone();
    let (cost, tin, tout, idle_outcome, time_idle): (f64, i64, i64, String, i64) = conn2
        .call(move |c| {
            c.query_row(
                "SELECT cost, tokens_input, tokens_output, idle_outcome, time_idle \
                 FROM session_v2 WHERE id = 's1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })
        .await
        .unwrap();
    assert!((cost - 0.5).abs() < 1e-9);
    assert_eq!(tin, 10);
    assert_eq!(tout, 5);
    assert_eq!(idle_outcome, "succeeded");
    assert!(time_idle > 0);

    // Idle marker message exists.
    let conn3 = bus.db_conn().clone();
    let idle_count: i64 = conn3
        .call(move |c| {
            c.query_row(
                "SELECT count(*) FROM session_message WHERE session_id = 's1' AND type = 'idle'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(idle_count, 1);
}

// ---------------------------------------------------------------------------
// Inbox promote / cancel / delivery-change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_cancel_removes_row() {
    let bus = bus().await;
    create(&bus, "s1").await;
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        item: user_item("hi"),
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::InboxCancelled {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
    })
    .await
    .unwrap();

    let conn = bus.db_conn().clone();
    let n: i64 = conn
        .call(move |c| {
            c.query_row(
                "SELECT count(*) FROM session_inbox WHERE id = 'ib1'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn inbox_delivery_change_updates() {
    let bus = bus().await;
    create(&bus, "s1").await;
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        item: user_item("hi"),
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::InboxDeliveryChanged {
        session_id: "s1".to_string(),
        inbox_id: "ib1".to_string(),
        delivery: Delivery::Steer,
    })
    .await
    .unwrap();

    let conn = bus.db_conn().clone();
    let d: String = conn
        .call(move |c| {
            c.query_row(
                "SELECT delivery FROM session_inbox WHERE id = 'ib1'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(d, "steer");
}

// ---------------------------------------------------------------------------
// event_log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_log_ordering_and_versions() {
    let bus = bus().await;
    create(&bus, "s1").await;
    bus.publish(SessionEventData::SessionRenamed {
        session_id: "s1".to_string(),
        title: "x".to_string(),
    })
    .await
    .unwrap();
    bus.publish(SessionEventData::SessionDeleted {
        session_id: "s1".to_string(),
    })
    .await
    .unwrap();

    let log = bus.event_log("s1", -1).await.unwrap();
    assert_eq!(log.len(), 3);
    let seqs: Vec<i64> = log
        .iter()
        .map(|e| e.durable.as_ref().unwrap().seq)
        .collect();
    assert_eq!(seqs, vec![0, 1, 2], "ordered by seq");

    // created & renamed are v1; deleted is v2.
    assert_eq!(log[0].durable.as_ref().unwrap().version, 1);
    assert_eq!(log[1].durable.as_ref().unwrap().version, 1);
    assert_eq!(log[2].durable.as_ref().unwrap().version, 2);
    assert_eq!(log[2].kind, "session.deleted");

    // after filter
    let after = bus.event_log("s1", 0).await.unwrap();
    assert_eq!(after.len(), 2);
}

// ---------------------------------------------------------------------------
// Ephemeral broadcast
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ephemeral_events_broadcast_not_persisted() {
    let bus = bus().await;
    let mut rx = bus.subscribe();
    create(&bus, "s1").await;
    bus.publish(SessionEventData::StepStarted {
        session_id: "s1".to_string(),
        assistant_message_id: "am1".to_string(),
        agent: "build".to_string(),
        model: model(),
        snapshot: None,
        started: 1,
    })
    .await
    .unwrap();

    // text.delta is ephemeral: no durable envelope, but broadcast.
    let env = bus
        .publish(SessionEventData::TextDelta {
            session_id: "s1".to_string(),
            assistant_message_id: "am1".to_string(),
            ordinal: 0,
            delta: "d".to_string(),
        })
        .await
        .unwrap();
    assert!(env.durable.is_none());

    // It should arrive on the subscriber stream.
    let mut got = None;
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "session.text.delta" {
            got = Some(ev);
        }
    }
    assert!(got.is_some(), "ephemeral event broadcast");

    // And not persisted.
    let conn = bus.db_conn().clone();
    let n: i64 = conn
        .call(move |c| {
            c.query_row(
                "SELECT count(*) FROM event WHERE aggregate_id = 's1' AND type LIKE 'session.text.delta%'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(n, 0);
}
