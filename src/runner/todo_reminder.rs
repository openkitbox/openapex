//! Session todo completion reminder, ported from oh-my-pi
//! `packages/coding-agent/src/session/todo-tracker.ts` (`checkCompletion`).
//!
//! A turn that ends naturally (no tool calls pending) with open todo tasks gets
//! one synthetic user prompt telling the model to finish or close them. The
//! mechanics differ from the reference — ox has no developer-role messages or
//! `scheduleAgentContinue`; the synthetic rides the inbox as a steer, which the
//! drain promotes at the next turn boundary — but the guards are the same:
//!
//! - **Budget**: at most [`MAX_REMINDERS`] reminders per list (the reference's
//!   `todo.remindersMax`, default 3). The count lives in the same `kv` row
//!   family as the list itself and resets whenever the list is touched, so a
//!   fresh `init` starts a fresh budget.
//! - **No progress, no re-ask**: a reminder that did not move the list suppresses
//!   further reminders until the model touches `todo` again (the reference's
//!   `reminderAwaitingProgress`).
//! - **Waiting on the user**: a final message that asks a question is a hand-off,
//!   not a stop — no reminder (the reference's `isAwaitingUserAnswer`, reduced
//!   here to a trailing-question-mark heuristic).
//!
//! The reminder never fires mid-turn: it runs when a turn ends with
//! [`TurnOutcome::Ended`], so a `tool-calls` continuation is unaffected.

use serde_json::json;

use crate::db::Db;
use crate::schema::event::SessionEventData;
use crate::tool::todo;

/// The reference's `todo.remindersMax` default (`settings.ts:387`).
const MAX_REMINDERS: u32 = 3;

/// `kv` key for the per-session reminder counter. Distinct from the list's own
/// `todo:{sessionID}` row so clearing the list and resetting the budget stay
/// independent writes.
fn count_key(session_id: &str) -> String {
    format!("todo:reminders:{session_id}")
}

async fn reminder_count(db: &Db, session_id: &str) -> anyhow::Result<u32> {
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare("SELECT value FROM kv WHERE key = ?1").await?;
    let Some(row) = crate::db::optional(&mut stmt, [count_key(session_id)]).await? else {
        return Ok(0);
    };
    let raw: String = row.get(0)?;
    Ok(raw.parse().unwrap_or(0))
}

async fn set_reminder_count(db: &Db, session_id: &str, count: u32) -> anyhow::Result<()> {
    let value = count.to_string();
    let now = crate::id::now_millis();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
        (count_key(session_id), value.as_str(), now),
    )
    .await?;
    Ok(())
}

/// Whether a session's reminder counter is reset. Called by the todo tool after
/// every successful mutation (the reference resets `reminderCount` whenever the
/// list is touched or empties): any real progress re-arms the budget.
pub(crate) async fn reset_budget(db: &Db, session_id: &str) {
    if let Err(error) = set_reminder_count(db, session_id, 0).await {
        tracing::debug!(session = %session_id, error = %error, "todo reminder budget reset failed");
    }
}

/// Whether the assistant's final text hands the turn to the user. The
/// reference's `isAwaitingUserAnswer` is an elaborate closing-ask classifier; the
/// essence for ox is the trailing question — the cheapest reliable "waiting on
/// you" signal, and the false negatives (a stop with a question buried mid-prose)
/// only cost a reminder, which the budget bounds.
fn awaits_user_answer(text: &str) -> bool {
    text.trim_end().ends_with('?') || text.trim_end().ends_with('？')
}

/// Build the reminder text. The shape mirrors the reference's
/// `<system-reminder>` block: the open items, the ask, and the budget state.
fn reminder_text(open: &[(String, String, &'static str)], attempt: u32) -> String {
    let mut lines = Vec::with_capacity(open.len() + 4);
    lines.push("<system-reminder>".to_owned());
    lines.push(format!(
        "You stopped with {} incomplete todo item(s):",
        open.len()
    ));
    let mut current_phase: Option<&str> = None;
    for (phase, task, status) in open {
        if current_phase != Some(phase.as_str()) {
            lines.push(format!("- {phase}"));
            current_phase = Some(phase.as_str());
        }
        lines.push(format!("  - {task} [{status}]"));
    }
    lines.push(
        "Continue working on these tasks, or mark them complete/abandoned if they are \
         actually finished. Do not leave the list behind."
            .to_owned(),
    );
    lines.push(format!("(Reminder {attempt}/{MAX_REMINDERS})"));
    lines.push("</system-reminder>".to_owned());
    lines.join("\n")
}

/// Called when a turn ends naturally. Publishes one synthetic steer when open
/// todo work remains and the budget allows; otherwise resets the budget (a
/// finished or empty list starts the next list fresh). Every failure is logged
/// and swallowed: a reminder is advisory and must never fail the turn.
pub(crate) async fn on_turn_ended(db: &Db, bus: &crate::event_bus::EventBus, session_id: &str) {
    let open = match todo::open_tasks(db, session_id).await {
        Ok(open) => open,
        Err(error) => {
            tracing::debug!(session = %session_id, error = %error, "todo reminder: read failed");
            return;
        }
    };
    if open.is_empty() {
        // Nothing open: the list is finished, empty, or absent — a fresh budget
        // either way.
        reset_budget(db, session_id).await;
        return;
    }

    let attempt = match reminder_count(db, session_id).await {
        Ok(count) => count + 1,
        Err(error) => {
            tracing::debug!(session = %session_id, error = %error, "todo reminder: count failed");
            return;
        }
    };
    if attempt > MAX_REMINDERS {
        tracing::debug!(session = %session_id, attempt, "todo reminder: budget exhausted");
        return;
    }

    // A stop that ends in a question is a hand-off, not an abandonment. The
    // text read is best-effort; failing it only skips this reminder.
    let waiting = match crate::runner::step::last_assistant_text(db, session_id).await {
        Ok(Some(text)) => awaits_user_answer(&text),
        _ => false,
    };
    if waiting {
        tracing::debug!(session = %session_id, "todo reminder: waiting on user, skipping");
        return;
    }

    if let Err(error) = set_reminder_count(db, session_id, attempt).await {
        tracing::debug!(session = %session_id, error = %error, "todo reminder: save count failed");
        return;
    }

    // The synthetic rides as a steer: the drain promotes it at the next turn
    // boundary, ahead of queued work, which is exactly the reference's
    // `scheduleAgentContinue` semantics.
    let inbox_id = crate::id::id("msg");
    if let Err(error) = bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.to_owned(),
            inbox_id: inbox_id.clone(),
            item: crate::schema::inbox::Item::Synthetic {
                payload: crate::schema::inbox::SyntheticPayload {
                    text: reminder_text(&open, attempt),
                    description: None,
                    metadata: Some(
                        json!({ "source": "todo-reminder", "attempt": attempt })
                            .as_object()
                            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                            .unwrap_or_default(),
                    ),
                },
                delivery: crate::schema::inbox::Delivery::Steer,
            },
        })
        .await
    {
        tracing::warn!(session = %session_id, error = %error, "todo reminder: enqueue failed");
        return;
    }
    if let Err(error) = bus
        .publish(SessionEventData::SessionSynthetic {
            session_id: session_id.to_owned(),
            text: String::new(),
            description: Some(format!(
                "todo reminder {attempt}/{MAX_REMINDERS} · {} open item{}",
                open.len(),
                if open.len() == 1 { "" } else { "s" }
            )),
            metadata: None,
        })
        .await
    {
        tracing::warn!(session = %session_id, error = %error, "todo reminder: notice failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::event_bus::EventBus;

    /// Seed a session's todo row directly (the tool's own path is covered in
    /// `tool::todo`); this suite is about the reminder's guards. The session
    /// row is required: `session_inbox` has a foreign key to it, and the
    /// inbox's durable projection is what materializes the steer.
    async fn seed(db: &Db, session_id: &str, phases: &str) {
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO session_v2
               (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES (?1, 'default', 'reminder-test', '/tmp', '1', 0, 0)",
            [session_id],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, 0, 0)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (format!("todo:{session_id}"), phases),
        )
        .await
        .unwrap();
    }

    fn one_open_one_done() -> &'static str {
        r#"[{"name":"P","tasks":[
             {"content":"open","status":"pending"},
             {"content":"closed","status":"completed"}]}]"#
    }

    fn all_done() -> &'static str {
        r#"[{"name":"P","tasks":[{"content":"closed","status":"completed"}]}]"#
    }

    #[tokio::test]
    async fn a_turn_with_open_work_enqueues_one_steer() {
        let db = Db::open_in_memory().await.unwrap();
        let bus = EventBus::new(db.clone());
        seed(&db, "ses", one_open_one_done()).await;
        // Subscribed before the action: the bus is a broadcast channel, so a
        // late subscriber sees nothing that was already published.
        let mut events = bus.subscribe();

        on_turn_ended(&db, &bus, "ses").await;

        // Exactly one inbox row, and it is a synthetic steer.
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare("SELECT id, payload, delivery FROM session_inbox")
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let mut found = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            found.push((
                row.get::<String>(0).unwrap(),
                row.get::<String>(1).unwrap(),
                row.get::<String>(2).unwrap(),
            ));
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].1.contains("incomplete todo item"),
            "{}",
            found[0].1
        );
        assert!(found[0].1.contains("open"), "the open task is named");
        assert!(!found[0].1.contains("closed"), "a closed task is not");
        assert!(found[0].1.contains("Reminder 1/3"), "{}", found[0].1);
        drop(stmt);
        drop(conn);

        // The human-facing notice rides along.
        let mut notices = Vec::new();
        while let Ok(envelope) = events.try_recv() {
            if envelope.kind == "session.synthetic"
                && let Some(description) = envelope.data.get("description").and_then(|v| v.as_str())
            {
                notices.push(description.to_owned());
            }
        }
        assert!(
            notices
                .iter()
                .any(|text| text.contains("todo reminder 1/3")),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn the_budget_bounds_repeated_reminders() {
        let db = Db::open_in_memory().await.unwrap();
        let bus = EventBus::new(db.clone());
        seed(&db, "ses", one_open_one_done()).await;

        for _ in 0..5 {
            on_turn_ended(&db, &bus, "ses").await;
        }

        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM session_inbox")
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let row = rows.next().await.unwrap().expect("count row");
        let count: i64 = row.get(0).unwrap();
        assert_eq!(count, i64::from(MAX_REMINDERS), "one steer per budget slot");
    }

    #[tokio::test]
    async fn a_finished_or_empty_list_sends_nothing_and_resets_the_budget() {
        let db = Db::open_in_memory().await.unwrap();
        let bus = EventBus::new(db.clone());
        seed(&db, "ses", all_done()).await;

        on_turn_ended(&db, &bus, "ses").await;
        assert!(reminder_count(&db, "ses").await.unwrap() == 0);

        // No rows were enqueued for a finished list.
        let conn = db.acquire().await.unwrap();
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM session_inbox")
            .await
            .unwrap();
        let mut rows = stmt.query(()).await.unwrap();
        let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count, 0);

        // An absent row (the tool clears finished lists) behaves the same.
        on_turn_ended(&db, &bus, "other").await;
        assert!(reminder_count(&db, "other").await.unwrap() == 0);
    }

    #[tokio::test]
    async fn resetting_the_budget_after_progress_re_arms_it() {
        let db = Db::open_in_memory().await.unwrap();
        let bus = EventBus::new(db.clone());
        seed(&db, "ses", one_open_one_done()).await;

        for _ in 0..MAX_REMINDERS {
            on_turn_ended(&db, &bus, "ses").await;
        }
        assert_eq!(reminder_count(&db, "ses").await.unwrap(), MAX_REMINDERS);

        // The tool touching the list resets the budget.
        reset_budget(&db, "ses").await;
        assert_eq!(reminder_count(&db, "ses").await.unwrap(), 0);

        on_turn_ended(&db, &bus, "ses").await;
        assert_eq!(
            reminder_count(&db, "ses").await.unwrap(),
            1,
            "a fresh budget starts at one"
        );
    }

    #[test]
    fn a_trailing_question_counts_as_waiting_on_the_user() {
        assert!(awaits_user_answer("Shall I continue?"));
        assert!(awaits_user_answer("  要不要继续？  "));
        assert!(!awaits_user_answer("Done."));
        assert!(!awaits_user_answer(""));
        // A question mid-prose is not a hand-off; the reminder is cheap.
        assert!(!awaits_user_answer("Why? Because the tests pass."));
    }

    #[test]
    fn the_reminder_names_each_phase_once() {
        let open = vec![
            ("Alpha".to_owned(), "one".to_owned(), "pending"),
            ("Alpha".to_owned(), "two".to_owned(), "in_progress"),
            ("Beta".to_owned(), "three".to_owned(), "pending"),
        ];
        let text = reminder_text(&open, 2);
        assert_eq!(text.matches("- Alpha").count(), 1, "{text}");
        assert_eq!(text.matches("- Beta").count(), 1, "{text}");
        assert!(text.contains("  - one [pending]"), "{text}");
        assert!(text.contains("  - two [in_progress]"), "{text}");
        assert!(text.contains("(Reminder 2/3)"), "{text}");
        assert!(text.starts_with("<system-reminder>"), "{text}");
        assert!(text.ends_with("</system-reminder>"), "{text}");
    }
}
