use anyhow::{Context as _, Result};

use ox_db::{Db, optional};
use ox_schema::id::now_millis;

use super::READY_NOTE_LIMIT;

/// One note as `read` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteRow {
    /// `#N` handle body.
    pub id: String,
    /// First line of the note text.
    pub title: String,
    /// Remainder of the note text (empty when the note is one line).
    pub body: String,
    /// `active`, `pending` or `dismissed`.
    pub status: String,
    /// The smart-note condition, when one was given.
    pub surface_condition: Option<String>,
    /// Last touch: `updated_at` falling back to `created_at`. Age and
    /// staleness are measured from here (the reference's `note_touched_at`),
    /// so editing a note clears its stale marker.
    pub touched_at: i64,
}

/// The `read` view selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteFilter {
    /// `active` notes and parked smart notes (`pending`) — the default view.
    Default_,
    /// Everything, dismissed included.
    All,
    /// `active` only.
    Active,
    /// Parked smart notes (`pending`).
    Pending,
    /// Surfaced smart notes (`ready`): the ones the dreamer's `surface_notes`
    /// task judged in.
    Ready,
    /// Dismissed notes.
    Dismissed,
}

impl NoteFilter {
    /// Parse the `filter` argument; unknown values are the model-facing
    /// error text.
    pub(super) fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(NoteFilter::Default_),
            Some("all") => Ok(NoteFilter::All),
            Some("active") => Ok(NoteFilter::Active),
            Some("pending") => Ok(NoteFilter::Pending),
            Some("ready") => Ok(NoteFilter::Ready),
            Some("dismissed") => Ok(NoteFilter::Dismissed),
            Some(other) => Err(format!(
                "Error: 'filter' must be one of all, active, pending, ready, dismissed (got '{other}')."
            )),
        }
    }

    /// The SQL `status IN (...)` clause body.
    fn statuses(self) -> &'static str {
        match self {
            NoteFilter::Default_ => "'active', 'pending', 'ready'",
            NoteFilter::All => "'active', 'pending', 'ready', 'dismissed'",
            NoteFilter::Active => "'active'",
            NoteFilter::Pending => "'pending'",
            NoteFilter::Ready => "'ready'",
            NoteFilter::Dismissed => "'dismissed'",
        }
    }
}

/// Split note text into `(title, body)`: the first line is the title, the
/// rest (trimmed) the body.
fn split_note(content: &str) -> (String, String) {
    let content = content.trim();
    match content.split_once('\n') {
        Some((title, body)) => (title.trim().to_owned(), body.trim().to_owned()),
        None => (content.to_owned(), String::new()),
    }
}

/// Write a new note; returns its `#N` handle body. A `surface_condition`
/// parks it as a smart note (`pending`).
pub(crate) async fn write_note(
    db: &Db,
    session_id: &str,
    content: &str,
    surface_condition: Option<&str>,
) -> Result<String> {
    let (title, body) = split_note(content);
    let status = if surface_condition.is_some() {
        "pending"
    } else {
        "active"
    };
    let now = now_millis();
    let result: Result<String> = async {
        let tx = db.begin().await?;
        let next = {
            let mut stmt = tx
                .prepare("SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0) + 1 FROM mc_note")
                .await?;
            stmt.query_row(()).await?.get::<i64>(0)?
        };
        let id = next.to_string();
        tx.execute(
            "INSERT INTO mc_note
                 (id, session_id, title, body, status, surface_condition,
                  created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            (
                id.as_str(),
                session_id,
                title.as_str(),
                body.as_str(),
                status,
                surface_condition,
                now,
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }
    .await;
    result.with_context(|| format!("write note for session {session_id}"))
}

/// The `read` page: newest first, for every filter (the reference leads with
/// ready smart notes; ox orders by handle, which is write order, so a note
/// surfaced just now at least does not sink behind older ones).
pub(crate) async fn list_notes(
    db: &Db,
    session_id: &str,
    filter: NoteFilter,
    limit: usize,
    offset: usize,
) -> Result<Vec<NoteRow>> {
    let statuses = filter.statuses();
    let sql = format!(
        "SELECT id, title, body, status, surface_condition,
                COALESCE(updated_at, created_at)
         FROM mc_note
         WHERE session_id = ?1 AND status IN ({statuses})
         ORDER BY CAST(id AS INTEGER) DESC
         LIMIT ?2 OFFSET ?3"
    );
    let result: Result<Vec<NoteRow>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn.prepare(&sql).await?;
        let mut rows = stmt
            .query((session_id, limit as i64, offset as i64))
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(note_from_row(&row)?);
        }
        Ok(out)
    }
    .await;
    result.with_context(|| format!("list notes for session {session_id}"))
}

/// One surfaced smart note as the pinboard render reads it.
///
/// The pinboard is the one place a note reaches a prompt: a `ready` note is
/// work to return to, so it is shown to the model without being asked for. Only
/// the three fields the render prints live here — the status is implied by the
/// query, and the condition has already done its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadyNote {
    /// `#N` handle body.
    pub id: String,
    /// First line of the note text.
    pub title: String,
    /// Remainder of the note text (empty when the note is one line).
    pub body: String,
}

/// The session's surfaced notes for the pinboard: `ready` rows, oldest handle
/// first, at most [`READY_NOTE_LIMIT`].
pub(crate) async fn ready_notes(db: &Db, session_id: &str) -> Result<Vec<ReadyNote>> {
    let context = format!("read ready notes for session {session_id}");
    let result: Result<Vec<ReadyNote>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, body FROM mc_note
                  WHERE session_id = ?1 AND status = 'ready'
                  ORDER BY CAST(id AS INTEGER)
                  LIMIT ?2",
            )
            .await?;
        let mut rows = stmt.query((session_id, READY_NOTE_LIMIT as i64)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(ReadyNote {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// Full note rows for the given ids, in the requested order (unknown ids
/// are skipped).
pub(crate) async fn get_notes(db: &Db, session_id: &str, ids: &[String]) -> Result<Vec<NoteRow>> {
    let result: Result<Vec<NoteRow>> = async {
        let conn = db.acquire().await?;
        let mut out = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT id, title, body, status, surface_condition,
                        COALESCE(updated_at, created_at)
                 FROM mc_note WHERE session_id = ?1 AND id = ?2",
            )
            .await?;
        for id in ids {
            if let Some(row) = optional(&mut stmt, (session_id, id.as_str())).await? {
                out.push(note_from_row(&row)?);
            }
        }
        Ok(out)
    }
    .await;
    result.with_context(|| format!("get notes for session {session_id}"))
}

/// Rewrite one note's text (title/body re-parsed like a write). `false` =
/// no such id in this session.
pub(crate) async fn update_note(
    db: &Db,
    session_id: &str,
    id: &str,
    content: &str,
) -> Result<bool> {
    let context = format!("update note #{id} for session {session_id}");
    let (title, body) = split_note(content);
    let now = now_millis();
    let result: Result<bool> = async {
        let conn = db.acquire().await?;
        let changed = conn
            .execute(
                "UPDATE mc_note SET title = ?3, body = ?4, updated_at = ?5
                 WHERE session_id = ?1 AND id = ?2",
                (session_id, id, title.as_str(), body.as_str(), now),
            )
            .await?;
        Ok(changed > 0)
    }
    .await;
    result.with_context(|| context)
}

/// Dismiss the given notes. Returns how many rows actually moved.
pub(crate) async fn dismiss_notes(db: &Db, session_id: &str, ids: &[String]) -> Result<usize> {
    let now = now_millis();
    let result: Result<usize> = async {
        let tx = db.begin().await?;
        let mut moved = 0;
        for id in ids {
            moved += tx
                .execute(
                    "UPDATE mc_note SET status = 'dismissed', updated_at = ?3
                     WHERE session_id = ?1 AND id = ?2 AND status != 'dismissed'",
                    (session_id, id.as_str(), now),
                )
                .await? as usize;
        }
        tx.commit().await?;
        Ok(moved)
    }
    .await;
    result.with_context(|| format!("dismiss notes for session {session_id}"))
}

/// Row → [`NoteRow`] (shared by the list and get statements).
fn note_from_row(row: &ox_db::turso::Row) -> ox_db::turso::Result<NoteRow> {
    Ok(NoteRow {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        status: row.get(3)?,
        surface_condition: row.get(4)?,
        touched_at: row.get(5)?,
    })
}

/// Humanized age from `created_at` to now: `0m`, `45m`, `3h`, `12d`.
pub(super) fn humanize_age(now: i64, touched_at: i64) -> String {
    let minutes = (now - touched_at).max(0) / 60_000;
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    }
}
