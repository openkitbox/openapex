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
//!   `todo.remindersMax`, default 3). The open items come from the transcript's
//!   newest completed `todowrite` call (`tool::todowrite::open_tasks`); only the
//!   reminder counter lives in `kv`, and it resets whenever the list is touched,
//!   so a fresh `todowrite` call starts a fresh budget.
//! - **No progress, no re-ask**: a reminder that did not move the list suppresses
//!   further reminders until the model touches `todo` again (the reference's
//!   `reminderAwaitingProgress`).
//! - **Waiting on the user**: a final message that asks a question is a hand-off,
//!   not a stop — no reminder (the reference's `isAwaitingUserAnswer`, reduced
//!   here to a trailing-question-mark heuristic).
//!
//! The reminder never fires mid-turn: it runs when a turn ends with
//! [`TurnOutcome::Ended`], so a `tool-calls` continuation is unaffected.

use ox_tools::todowrite::{reminder_count, reset_reminder_budget, set_reminder_count};
use serde_json::json;

use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_tools::todowrite;

/// The reference's `todo.remindersMax` default (`settings.ts:387`).
const MAX_REMINDERS: u32 = 3;

/// Whether the assistant's final text hands the turn to the user. The
/// reference's `isAwaitingUserAnswer` is an elaborate closing-ask classifier; the
/// essence for ox is the trailing question — the cheapest reliable "waiting on
/// you" signal, and the false negatives (a stop with a question buried mid-prose)
/// only cost a reminder, which the budget bounds.
fn awaits_user_answer(text: &str) -> bool {
    text.trim_end().ends_with('?') || text.trim_end().ends_with('？')
}

/// Build the reminder text: the open items, the ask, and the budget state.
fn reminder_text(open: &[(String, String)], attempt: u32) -> String {
    let mut lines = Vec::with_capacity(open.len() + 4);
    lines.push("<system-reminder>".to_owned());
    lines.push(format!(
        "You stopped with {} incomplete todo item(s):",
        open.len()
    ));
    for (content, status) in open {
        lines.push(format!("  - {content} [{status}]"));
    }
    lines.push(
        "Continue working on these tasks, or mark them completed/cancelled if they are \
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
pub(crate) async fn on_turn_ended(db: &Db, bus: &ox_core::event_bus::EventBus, session_id: &str) {
    let open = match todowrite::open_tasks(db, session_id).await {
        Ok(open) => open,
        Err(error) => {
            tracing::debug!(session = %session_id, error = %error, "todo reminder: read failed");
            return;
        }
    };
    if open.is_empty() {
        // Nothing open: the list is finished, empty, or absent — a fresh budget
        // either way.
        reset_reminder_budget(db, session_id).await;
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
    let inbox_id = ox_schema::id::id("msg");
    if let Err(error) = bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.to_owned(),
            inbox_id: inbox_id.clone(),
            item: ox_schema::inbox::Item::Synthetic {
                payload: ox_schema::inbox::SyntheticPayload {
                    text: reminder_text(&open, attempt),
                    description: None,
                    metadata: Some(
                        json!({ "source": "todo-reminder", "attempt": attempt })
                            .as_object()
                            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                            .unwrap_or_default(),
                    ),
                },
                delivery: ox_schema::inbox::Delivery::Steer,
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
