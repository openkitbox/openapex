use anyhow::{Context as _, Result};

use ox_db::Db;

/// Most user turns one retrospective run reads, newest first (the reference's
/// window, reduced to a fixed bound).
const RETROSPECTIVE_TURNS: usize = 80;

/// The project's user turns newer than `after`, newest first.
///
/// The project key is the session directory (that is what `ctx_memory` stores
/// and what the dreamer enumerates by), so this joins every session of the
/// project. The watermark is the last *successful* run's start, which is what
/// makes a sweep cover exactly the turns that have not been read yet. Rows whose
/// `data` does not decode, or that carry no text, are skipped: a turn with
/// nothing typed into it has nothing to learn from.
pub(super) async fn recent_user_turns(db: &Db, project: &str, after: i64) -> Result<Vec<String>> {
    let context = format!("read recent user turns for project {project}");
    let result: Result<Vec<String>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT m.data FROM session_message m
                   JOIN session_v2 s ON s.id = m.session_id
                  WHERE m.type = 'user' AND s.directory = ?1 AND m.time_created > ?2
                  ORDER BY m.time_created DESC
                  LIMIT ?3",
            )
            .await?;
        let mut rows = stmt
            .query((project, after, RETROSPECTIVE_TURNS as i64))
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
                out.push(text);
            }
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}
