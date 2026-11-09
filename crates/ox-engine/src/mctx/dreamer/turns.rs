use anyhow::{Context as _, Result};

use ox_db::Db;

/// Most user turns one retrospective run reads, newest first (the reference's
/// window, reduced to a fixed bound).
const RETROSPECTIVE_TURNS: usize = 80;

/// Automatic consumers have different provenance requirements: note checks do
/// not create facts, so external-content taint does not exclude their input.
#[derive(Clone, Copy)]
pub(super) enum TurnPurpose {
    Retrospective,
    NoteConditions,
}

pub(super) struct EligibleTurn {
    pub(super) session_id: String,
    pub(super) text: String,
}

/// Directories with retrospective-eligible user activity. Select only owner
/// keys here; the task's persisted watermark gates its bounded input read.
pub(super) async fn projects_with_eligible_activity(db: &Db) -> Result<Vec<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT s.directory FROM session_v2 s
               JOIN session_message m ON m.session_id = s.id
              WHERE m.type = 'user'
                AND NOT EXISTS (SELECT 1 FROM kv WHERE key = ?1 || s.id)
                AND NOT EXISTS (SELECT 1 FROM kv WHERE key = ?2 || s.id)
              ORDER BY s.directory",
        )
        .await?;
    let mut rows = stmt
        .query((
            ox_schema::support::memory_mode_key("").as_str(),
            ox_schema::support::session_taint_key("").as_str(),
        ))
        .await?;
    let mut owners = Vec::new();
    while let Some(row) = rows.next().await? {
        owners.push(row.get(0)?);
    }
    Ok(owners)
}

/// The project's eligible user turns newer than `after`, newest first.
///
/// The project key is the session directory (that is what `ctx_memory` stores
/// and what the dreamer enumerates by), so this joins every session of the
/// project. The watermark is the last *successful* run's start, which is what
/// makes a sweep cover exactly the turns that have not been read yet. Rows whose
/// `data` does not decode, or that carry no text, are skipped: a turn with
/// nothing typed into it has nothing to learn from.
pub(super) async fn recent_user_turns(
    db: &Db,
    project: &str,
    after: i64,
    purpose: TurnPurpose,
) -> Result<Vec<EligibleTurn>> {
    let context = format!("read recent user turns for project {project}");
    let result: Result<Vec<EligibleTurn>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT m.data, m.session_id FROM session_message m
                   JOIN session_v2 s ON s.id = m.session_id
                  WHERE m.type = 'user' AND s.directory = ?1 AND m.time_created > ?2
                    AND NOT EXISTS (SELECT 1 FROM kv WHERE key = ?4 || s.id)
                    AND (?6 = 0 OR NOT EXISTS (SELECT 1 FROM kv WHERE key = ?5 || s.id))
                  ORDER BY m.time_created DESC
                  LIMIT ?3",
            )
            .await?;
        let mut rows = stmt
            .query((
                project,
                after,
                RETROSPECTIVE_TURNS as i64,
                ox_schema::support::memory_mode_key("").as_str(),
                ox_schema::support::session_taint_key("").as_str(),
                i64::from(matches!(purpose, TurnPurpose::Retrospective)),
            ))
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let data: String = row.get(0)?;
            let text = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|value| {
                    value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            if !text.trim().is_empty() {
                out.push(EligibleTurn {
                    session_id: row.get(1)?,
                    text,
                });
            }
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}
