//! External-content taint: the per-session mark that keeps a fetched page's
//! claims out of project memory.
//!
//! A session that has run an external-content tool (`[magic_context]
//! external_content_tools`, `webfetch` by default) carries text that nobody
//! on the project wrote. The historian still folds it — the transcript is
//! the transcript — but its `<facts>` and `<user_observations>` are no
//! longer promoted: a "rule" a web page stated would otherwise ride every
//! future request of the project as if the team had decided it. The facts
//! stay on the run row (`mc_historian_run.facts`) for inspection; only the
//! automatic write into `mc_memory` is withheld. `ctx_memory` is untouched:
//! a model writing a memory on purpose is a decision, not an extraction.
//!
//! The mark is one `kv` row per session, `mctx_tainted:{session}`, written
//! once by the tool step ([`mark`]) and read by the historian's publish
//! ([`is_tainted`]). It is sticky for the session's life: the fetched text
//! stays in the transcript (and in the compartments folded from it), so
//! there is no later point at which the session is clean again. Forks inherit
//! the mark atomically, even when their boundary precedes the external fetch.
//!
//! Reference: codex marks a thread `polluted` on web/MCP content and skips
//! its memory extraction; ox skips the promotion step instead so the fold
//! itself is unaffected.

use anyhow::{Context as _, Result};
use ox_db::{Db, optional};
use ox_schema::id::now_millis;

/// Mark `session_id` as carrying external content, naming the tool that
/// brought it. Idempotent: a second mark only refreshes the timestamp.
pub async fn mark(db: &Db, session_id: &str, tool: &str) -> Result<()> {
    let now = now_millis();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
        (
            ox_schema::support::session_taint_key(session_id).as_str(),
            tool,
            now,
        ),
    )
    .await
    .with_context(|| format!("mark session {session_id} as tainted by {tool}"))?;
    Ok(())
}

/// Whether `session_id` has been marked.
pub async fn is_tainted(db: &Db, session_id: &str) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT 1 FROM kv WHERE key = ?1")
        .await
        .context("read session taint")?;
    let row = optional(
        &mut stmt,
        [ox_schema::support::session_taint_key(session_id).as_str()],
    )
    .await
    .context("read session taint")?;
    Ok(row.is_some())
}
