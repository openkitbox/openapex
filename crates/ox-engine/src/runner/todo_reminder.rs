//! Advisory todo reminders. Unlike the reference's mutable counter, the budget
//! is derived from completed lists and durable inbox admissions. No waiting
//! table or counter can drift from the transcript. An unfinished epoch ends only
//! at a terminal/empty list; each normalized revision is reminded once, at most
//! three admissions per epoch. Legacy notices count as admissions, not progress.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::{Value, json};

use ox_db::Db;
use ox_schema::event::SessionEventData;
use ox_tools::todowrite;

const MAX_REMINDERS: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    NoWork,
    AwaitExternal,
    AwaitUser,
    NoProgress,
    BudgetExhausted,
    Remind(u32),
}

#[derive(Default)]
struct History {
    list: Vec<(String, String)>,
    epoch: String,
    revision: String,
    admissions: u32,
    reminded: HashSet<String>,
}

fn is_open(status: &str) -> bool {
    matches!(status, "pending" | "in_progress")
}

fn decide(history: &History, external: bool, waiting_user: bool) -> Decision {
    if !history.list.iter().any(|(_, status)| is_open(status)) {
        Decision::NoWork
    } else if external {
        Decision::AwaitExternal
    } else if waiting_user {
        Decision::AwaitUser
    } else if history.reminded.contains(&history.revision) {
        Decision::NoProgress
    } else if history.admissions >= MAX_REMINDERS {
        Decision::BudgetExhausted
    } else {
        Decision::Remind(history.admissions + 1)
    }
}

impl History {
    fn list(&mut self, list: Vec<(String, String)>, identity: String) -> Result<()> {
        let was_open = self.list.iter().any(|(_, status)| is_open(status));
        let open = list.iter().any(|(_, status)| is_open(status));
        if !open || !was_open {
            self.admissions = 0;
            self.reminded.clear();
            self.epoch = identity;
        }
        self.revision = serde_json::to_string(&list)?;
        self.list = list;
        Ok(())
    }

    fn notice(&mut self, metadata: &Value) {
        // Epoch metadata survives promotion, whose message seq can be newer
        // than a later list. Never count such a notice in the new epoch.
        if metadata
            .get("epoch")
            .and_then(Value::as_str)
            .is_some_and(|epoch| epoch != self.epoch)
        {
            return;
        }
        self.admissions = self.admissions.saturating_add(1);
        if let Some(revision) = metadata.get("revision").and_then(Value::as_str) {
            self.reminded.insert(revision.to_owned());
        }
    }
}

/// Each page is bounded; neither the complete transcript nor the event replay
/// log is loaded. Pending/promoted rows are mutually exclusive under the same
/// read transaction, so an admission is counted exactly once.
async fn history(db: &Db, session: &str) -> Result<History> {
    let tx = db.begin().await?;
    let mut history = History::default();
    let mut after = -1_i64;
    loop {
        let mut stmt = tx
            .prepare(
                "SELECT id, seq, type, data FROM session_message
             WHERE session_id = ?1 AND seq > ?2 AND type IN ('assistant','synthetic')
             UNION ALL
             SELECT id, enqueued_seq, type, payload FROM session_inbox
             WHERE session_id = ?1 AND enqueued_seq > ?2 AND type = 'synthetic'
             ORDER BY 2 LIMIT 64",
            )
            .await?;
        let mut rows = stmt.query((session, after)).await?;
        let mut count = 0;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            after = row.get(1)?;
            let kind: String = row.get(2)?;
            let raw: String = row.get(3)?;
            if kind == "assistant" {
                for (index, list) in todowrite::completed_lists(&raw)?.into_iter().enumerate() {
                    history.list(list, format!("{id}:{index}"))?;
                }
            } else {
                let value: Value = serde_json::from_str(&raw)?;
                let metadata = &value["metadata"];
                if metadata["source"] == "todo-reminder" {
                    history.notice(metadata);
                }
            }
            count += 1;
        }
        if count < 64 {
            break;
        }
    }
    tx.commit().await?;
    Ok(history)
}

async fn pending_completion(db: &Db, session: &str) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM session_inbox WHERE session_id = ?1 AND type = 'synthetic'
         AND json_extract(payload, '$.metadata.source') IN ('subagent','shell') LIMIT 1",
        )
        .await?;
    Ok(ox_db::optional(&mut stmt, [session]).await?.is_some())
}

fn awaits_user_answer(text: &str) -> bool {
    text.trim_end().ends_with('?') || text.trim_end().ends_with('？')
}

fn reminder_text(open: &[(String, String)], attempt: u32) -> String {
    let mut lines = vec!["<system-reminder>".to_owned()];
    for (content, status) in open.iter().filter(|(_, status)| is_open(status)) {
        lines.push(format!("  - {content} [{status}]"));
    }
    lines.push(format!("Continue working on these tasks, or mark them completed/cancelled if actually finished. (Reminder {attempt}/{MAX_REMINDERS})"));
    lines.push("</system-reminder>".to_owned());
    lines.join("\n")
}

/// The caller reads live wake obligations BEFORE this function reads durable
/// pending completions: the owner may commit and retire between those reads,
/// but can never fall through the handoff gap.
pub(crate) async fn on_turn_ended(db: &Db, bus: &ox_core::event_bus::EventBus, session_id: &str) {
    if let Err(error) = remind(db, bus, session_id).await {
        tracing::warn!(session = session_id, %error, "todo reminder skipped");
    }
}

async fn remind(db: &Db, bus: &ox_core::event_bus::EventBus, session: &str) -> Result<()> {
    let external = pending_completion(db, session).await?;
    let history = history(db, session).await?;
    let text = crate::runner::step::last_assistant_text(db, session).await?;
    let waiting = text.as_deref().is_some_and(awaits_user_answer);
    let Decision::Remind(attempt) = decide(&history, external, waiting) else {
        return Ok(());
    };
    let metadata = json!({"source":"todo-reminder", "attempt":attempt, "epoch":history.epoch, "revision":history.revision});
    bus.publish(SessionEventData::InboxEnqueued {
        session_id: session.to_owned(),
        inbox_id: ox_schema::id::id("msg"),
        item: ox_schema::inbox::Item::Synthetic {
            payload: ox_schema::inbox::SyntheticPayload {
                text: reminder_text(&history.list, attempt),
                description: Some(format!("todo reminder {attempt}/{MAX_REMINDERS}")),
                metadata: Some(serde_json::from_value(metadata)?),
            },
            delivery: ox_schema::inbox::Delivery::Steer,
        },
    })
    .await?;
    Ok(())
}
