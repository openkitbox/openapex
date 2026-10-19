//! Project memories (`ctx_memory`) and session notes (`ctx_note`): storage,
//! the two tools, and the store reads the pinboard render uses.
//!
//! # Two kinds of durable text
//!
//! - **Memories** are project-scoped facts that stay true for months: a
//!   project rule, an architectural fact, a hard-won constraint, a config
//!   value, a naming convention. Every active memory is rendered into the
//!   system prompt's tail by [`crate::mctx::render::render_memory_pinboard`],
//!   so it reaches every future session of the project without being asked
//!   for.
//! - **Notes** are session-scoped pending intentions: work to return to,
//!   with its findings attached. They are pull-based (the model asks with
//!   `ctx_note read`); they never enter a prompt on their own.
//!
//! # Handles and lifecycles
//!
//! Ids are small ascending integers (`#17`) from a single sequence per table,
//! minted by `MAX(id) + 1` inside the writing transaction (the reference
//! addresses memories and notes as `#N`). A handle is a unique address; the
//! owning scope (project for memories, session for notes) is an ordinary
//! column, not part of the handle. The column is `text`; ordering casts to
//! `INTEGER`.
//!
//! Memories: `active` → `archived` (retirable, still `get`-able; the
//! optional archive reason is kept). Notes: `active`, `pending` (a smart
//! note parked on a `surface_condition`) and `dismissed`.
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **No outside checker (MVP).** A note written with `surface_condition`
//!   is stored as `pending` and stays parked; the reference's periodic
//!   condition check is not part of ox yet, so `read filter=ready` returns
//!   nothing, `filter=pending` is where smart notes live, and the
//!   reference's "ready smart notes first" page order is vacuous — `read`
//!   orders newest first.
//! - **Merge candidates are active rows only.** An archived row can be
//!   neither survivor (its content would be stranded outside the pinboard)
//!   nor loser (its archive reason would be overwritten), so a merge needs
//!   at least two *active* rows among the ids; repeats are rejected. The
//!   survivor is the lowest id (the oldest row), which is updated to the
//!   merged content; the other rows are archived with reason `"merged"`.
//! - **Age and staleness come from the last touch** (`updated_at`, falling
//!   back to `created_at` — the reference's `note_touched_at`), so editing a
//!   stale note clears its marker.
//! - **Failure convention** follows the module's other tools: rejections
//!   are model-facing `Error: …` *content* (the reference tools return
//!   strings), not tool failures.
//! - Model-facing text is ox-authored where the reference string was not
//!   recoverable; the tool *interfaces* (actions, fields, handle shapes) are
//!   the reference's.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context as _, Result};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::cardinal::wire::ToolDef;
use crate::db::Db;
use crate::id::now_millis;
use crate::runner::{Tool, ToolContext, ToolOutput};
use crate::tool::{object_param, ok_text};

/// The closed category set, in pinboard render order.
pub(crate) const CATEGORIES: [&str; 5] = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
];

/// A note whose age reaches this many days is flagged stale in `read`
/// (reference: "rows untouched 30+ days are marked stale").
const STALE_DAYS: i64 = 30;

/// Most ids one `ctx_memory get` may ask for.
const MEMORY_GET_LIMIT: usize = 20;
/// Most ids one `ctx_memory` archive/merge may carry (ox's own bound: the
/// reference states no numeric cap).
const MEMORY_MUTATE_LIMIT: usize = 50;
/// Most ids one `ctx_note read`/`update`/`dismiss` may carry.
const NOTE_ID_LIMIT: usize = 50;
/// Cap on the `read` page size: `limit` is a page knob, so it is clamped
/// rather than allowed to reach SQLite as an unbounded (or negative) LIMIT.
const MAX_NOTE_PAGE: usize = 200;
/// Default `read` page size.
const NOTE_PAGE: usize = 25;

// ---------------------------------------------------------------------------
// Store: memories
// ---------------------------------------------------------------------------

/// One active memory as the pinboard render reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryRow {
    /// `#N` handle body.
    pub id: String,
    /// One of [`CATEGORIES`].
    pub category: String,
    /// The fact text.
    pub content: String,
}

/// One memory as `get` reads it (every status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryDetail {
    /// `#N` handle body.
    pub id: String,
    /// One of [`CATEGORIES`].
    pub category: String,
    /// The fact text.
    pub content: String,
    /// `active` or `archived`.
    pub status: String,
}

/// Write a new memory; returns its `#N` handle body.
pub(crate) async fn write_memory(
    db: &Db,
    project: &str,
    content: &str,
    category: &str,
) -> Result<String> {
    let context = format!("write memory for project {project}");
    let project = project.to_owned();
    let content = content.to_owned();
    let category = category.to_owned();
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<String> {
            let tx = c.transaction()?;
            let next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0) + 1 FROM mc_memory",
                [],
                |row| row.get(0),
            )?;
            let id = next.to_string();
            tx.execute(
                "INSERT INTO mc_memory
                     (id, project, content, category, created_at, updated_at, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'active')",
                rusqlite::params![id, project, content, category, now],
            )?;
            tx.commit()?;
            Ok(id)
        })
        .await
        .with_context(|| context)
}

/// Rewrite one memory. `category` omitted keeps the stored one. `false` =
/// no such id in this project.
pub(crate) async fn update_memory(
    db: &Db,
    project: &str,
    id: &str,
    content: &str,
    category: Option<&str>,
) -> Result<bool> {
    let context = format!("update memory #{id} for project {project}");
    let project = project.to_owned();
    let id = id.to_owned();
    let content = content.to_owned();
    let category = category.map(str::to_owned);
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<bool> {
            let changed = match category {
                Some(category) => c.execute(
                    "UPDATE mc_memory SET content = ?3, category = ?4, updated_at = ?5
                     WHERE project = ?1 AND id = ?2",
                    rusqlite::params![project, id, content, category, now],
                )?,
                None => c.execute(
                    "UPDATE mc_memory SET content = ?3, updated_at = ?4
                     WHERE project = ?1 AND id = ?2",
                    rusqlite::params![project, id, content, now],
                )?,
            };
            Ok(changed > 0)
        })
        .await
        .with_context(|| context)
}

/// Archive the given memories (any status may be given; already-archived
/// rows are left alone). Returns the ids that were actually archived.
pub(crate) async fn archive_memories(
    db: &Db,
    project: &str,
    ids: &[String],
    reason: Option<&str>,
) -> Result<Vec<String>> {
    let context = format!("archive memories for project {project}");
    let project = project.to_owned();
    let ids = ids.to_vec();
    let reason = reason.map(str::to_owned);
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<Vec<String>> {
            let tx = c.transaction()?;
            let mut archived = Vec::new();
            {
                let mut stmt =
                    tx.prepare("SELECT status FROM mc_memory WHERE project = ?1 AND id = ?2")?;
                for id in &ids {
                    let status: Option<String> = stmt
                        .query_row(rusqlite::params![project, id], |row| row.get(0))
                        .optional()?;
                    if status.as_deref() == Some("active") && !archived.contains(id) {
                        archived.push(id.clone());
                    }
                }
            }
            for id in &archived {
                tx.execute(
                    "UPDATE mc_memory SET status = 'archived', archive_reason = ?3, updated_at = ?4
                     WHERE project = ?1 AND id = ?2",
                    rusqlite::params![project, id, reason, now],
                )?;
            }
            tx.commit()?;
            Ok(archived)
        })
        .await
        .with_context(|| context)
}

/// The result of a merge: the surviving handle and every handle the merge
/// actually touched (survivor first, then the archived losers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Merged {
    /// `#N` handle body of the surviving row.
    pub survivor: String,
    /// Handles that were merged, survivor included.
    pub sources: Vec<String>,
}

/// Collapse duplicates into one: the lowest id among the *active* rows
/// survives (rewritten to the merged content), the rest are archived with
/// reason `"merged"`. `None` = fewer than two active rows among the ids, so
/// nothing was merged.
pub(crate) async fn merge_memories(
    db: &Db,
    project: &str,
    ids: &[String],
    content: &str,
    category: Option<&str>,
) -> Result<Option<Merged>> {
    let context = format!("merge memories for project {project}");
    let project = project.to_owned();
    let ids = ids.to_vec();
    let content = content.to_owned();
    let category = category.map(str::to_owned);
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<Option<Merged>> {
            let tx = c.transaction()?;
            // Only ACTIVE rows are merge candidates: an archived row must
            // neither survive (its content would be stranded outside the
            // pinboard) nor lose (its own archive reason would be
            // overwritten).
            let mut live: Vec<String> = Vec::new();
            {
                let mut stmt =
                    tx.prepare("SELECT status FROM mc_memory WHERE project = ?1 AND id = ?2")?;
                for id in &ids {
                    let status: Option<String> = stmt
                        .query_row(rusqlite::params![project, id], |row| row.get(0))
                        .optional()?;
                    if status.as_deref() == Some("active") && !live.contains(id) {
                        live.push(id.clone());
                    }
                }
            }
            // A merge needs at least two live rows; fewer is reported as
            // "nothing merged" rather than silently rewriting one row.
            if live.len() < 2 {
                return Ok(None);
            }
            // The survivor is the lowest id (the oldest row).
            live.sort_by_key(|id| id.parse::<i64>().unwrap_or(i64::MAX));
            let Some((survivor, losers)) = live.split_first() else {
                return Ok(None);
            };
            match category {
                Some(category) => tx.execute(
                    "UPDATE mc_memory SET content = ?3, category = ?4, updated_at = ?5
                     WHERE project = ?1 AND id = ?2",
                    rusqlite::params![project, survivor, content, category, now],
                )?,
                None => tx.execute(
                    "UPDATE mc_memory SET content = ?3, updated_at = ?4
                     WHERE project = ?1 AND id = ?2",
                    rusqlite::params![project, survivor, content, now],
                )?,
            };
            for id in losers {
                tx.execute(
                    "UPDATE mc_memory SET status = 'archived', archive_reason = 'merged',
                         updated_at = ?3
                     WHERE project = ?1 AND id = ?2",
                    rusqlite::params![project, id, now],
                )?;
            }
            tx.commit()?;
            Ok(Some(Merged {
                survivor: survivor.clone(),
                sources: live,
            }))
        })
        .await
        .with_context(|| context)
}

/// Fetch memories by id in every status, in the requested order (unknown
/// ids are skipped).
pub(crate) async fn get_memories(
    db: &Db,
    project: &str,
    ids: &[String],
) -> Result<Vec<MemoryDetail>> {
    let context = format!("get memories for project {project}");
    let project = project.to_owned();
    let ids = ids.to_vec();
    db.conn()
        .call(move |c| -> rusqlite::Result<Vec<MemoryDetail>> {
            let mut out = Vec::new();
            let mut stmt = c.prepare(
                "SELECT id, category, content, status FROM mc_memory
                 WHERE project = ?1 AND id = ?2",
            )?;
            for id in &ids {
                let row: Option<(String, String, String, String)> = stmt
                    .query_row(rusqlite::params![project, id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .optional()?;
                if let Some((id, category, content, status)) = row {
                    // The row's own handle: a repeated request id must not
                    // print the same memory twice.
                    if !out.iter().any(|row: &MemoryDetail| row.id == id) {
                        out.push(MemoryDetail {
                            id,
                            category,
                            content,
                            status,
                        });
                    }
                }
            }
            Ok(out)
        })
        .await
        .with_context(|| context)
}

/// Every active memory of the project, ids ascending (the renderer applies
/// category order).
pub(crate) async fn active_memories(db: &Db, project: &str) -> Result<Vec<MemoryRow>> {
    let context = format!("read active memories for project {project}");
    let project = project.to_owned();
    db.conn()
        .call(move |c| -> rusqlite::Result<Vec<MemoryRow>> {
            let mut stmt = c.prepare(
                "SELECT id, category, content FROM mc_memory
                 WHERE project = ?1 AND status = 'active'
                 ORDER BY CAST(id AS INTEGER)",
            )?;
            let mapped = stmt.query_map([&project], |row| {
                Ok(MemoryRow {
                    id: row.get(0)?,
                    category: row.get(1)?,
                    content: row.get(2)?,
                })
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .with_context(|| context)
}

// ---------------------------------------------------------------------------
// Store: notes
// ---------------------------------------------------------------------------

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
    /// Surfaced smart notes. MVP: always empty (no outside checker).
    Ready,
    /// Dismissed notes.
    Dismissed,
}

impl NoteFilter {
    /// Parse the `filter` argument; unknown values are the model-facing
    /// error text.
    fn parse(raw: Option<&str>) -> Result<Self, String> {
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

    /// The SQL `status IN (...)` clause body, or `None` for an always-empty
    /// view (`ready`).
    fn statuses(self) -> Option<&'static str> {
        match self {
            NoteFilter::Default_ => Some("'active', 'pending'"),
            NoteFilter::All => Some("'active', 'pending', 'dismissed'"),
            NoteFilter::Active => Some("'active'"),
            NoteFilter::Pending => Some("'pending'"),
            NoteFilter::Ready => None,
            NoteFilter::Dismissed => Some("'dismissed'"),
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
    let session = session_id.to_owned();
    let (title, body) = split_note(content);
    let condition = surface_condition.map(str::to_owned);
    let status = if condition.is_some() {
        "pending"
    } else {
        "active"
    };
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<String> {
            let tx = c.transaction()?;
            let next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0) + 1 FROM mc_note",
                [],
                |row| row.get(0),
            )?;
            let id = next.to_string();
            tx.execute(
                "INSERT INTO mc_note
                     (id, session_id, title, body, status, surface_condition,
                      created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                rusqlite::params![id, session, title, body, status, condition, now],
            )?;
            tx.commit()?;
            Ok(id)
        })
        .await
        .with_context(|| format!("write note for session {session_id}"))
}

/// The `read` page: newest first (parked smart notes would lead the page only
/// once an outside checker can mark them ready — tox has none yet, so the
/// reference's "ready first" ordering is vacuous here).
pub(crate) async fn list_notes(
    db: &Db,
    session_id: &str,
    filter: NoteFilter,
    limit: usize,
    offset: usize,
) -> Result<Vec<NoteRow>> {
    let Some(statuses) = filter.statuses() else {
        return Ok(Vec::new());
    };
    let session = session_id.to_owned();
    let sql = format!(
        "SELECT id, title, body, status, surface_condition,
                COALESCE(updated_at, created_at)
         FROM mc_note
         WHERE session_id = ?1 AND status IN ({statuses})
         ORDER BY CAST(id AS INTEGER) DESC
         LIMIT ?2 OFFSET ?3"
    );
    db.conn()
        .call(move |c| -> rusqlite::Result<Vec<NoteRow>> {
            let mut stmt = c.prepare(&sql)?;
            let mapped = stmt.query_map(
                rusqlite::params![session, limit as i64, offset as i64],
                note_from_row,
            )?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .with_context(|| format!("list notes for session {session_id}"))
}

/// Full note rows for the given ids, in the requested order (unknown ids
/// are skipped).
pub(crate) async fn get_notes(db: &Db, session_id: &str, ids: &[String]) -> Result<Vec<NoteRow>> {
    let session = session_id.to_owned();
    let ids = ids.to_vec();
    db.conn()
        .call(move |c| -> rusqlite::Result<Vec<NoteRow>> {
            let mut out = Vec::new();
            let mut stmt = c.prepare(
                "SELECT id, title, body, status, surface_condition,
                        COALESCE(updated_at, created_at)
                 FROM mc_note WHERE session_id = ?1 AND id = ?2",
            )?;
            for id in &ids {
                let row = stmt
                    .query_row(rusqlite::params![session, id], note_from_row)
                    .optional()?;
                if let Some(row) = row {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await
        .with_context(|| format!("get notes for session {session_id}"))
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
    let session = session_id.to_owned();
    let id = id.to_owned();
    let (title, body) = split_note(content);
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<bool> {
            let changed = c.execute(
                "UPDATE mc_note SET title = ?3, body = ?4, updated_at = ?5
                 WHERE session_id = ?1 AND id = ?2",
                rusqlite::params![session, id, title, body, now],
            )?;
            Ok(changed > 0)
        })
        .await
        .with_context(|| context)
}

/// Dismiss the given notes. Returns how many rows actually moved.
pub(crate) async fn dismiss_notes(db: &Db, session_id: &str, ids: &[String]) -> Result<usize> {
    let session = session_id.to_owned();
    let ids = ids.to_vec();
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<usize> {
            let tx = c.transaction()?;
            let mut moved = 0;
            for id in &ids {
                moved += tx.execute(
                    "UPDATE mc_note SET status = 'dismissed', updated_at = ?3
                     WHERE session_id = ?1 AND id = ?2 AND status != 'dismissed'",
                    rusqlite::params![session, id, now],
                )?;
            }
            tx.commit()?;
            Ok(moved)
        })
        .await
        .with_context(|| format!("dismiss notes for session {session_id}"))
}

/// Row → [`NoteRow`] (shared by the list and get statements).
fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
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
fn humanize_age(now: i64, touched_at: i64) -> String {
    let minutes = (now - touched_at).max(0) / 60_000;
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Parse an `ids` / `note_ids` argument into canonical decimal-id strings.
/// Accepts an integer, a digit string, or an array of either. Errors are
/// the model-facing text.
fn parse_int_ids(value: Option<&Value>, field: &str, max: usize) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Err(format!("Error: '{field}' must be provided."));
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    if items.is_empty() {
        return Err(format!("Error: '{field}' must not be empty."));
    }
    if items.len() > max {
        return Err(format!(
            "Error: '{field}' takes at most {max} ids (got {}).",
            items.len()
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let parsed = match item {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse::<i64>().ok(),
            _ => None,
        };
        match parsed {
            Some(id) if id > 0 => out.push(id.to_string()),
            _ => {
                return Err(format!(
                    "Error: '{field}' must contain positive integer ids (got {item})."
                ));
            }
        }
    }
    Ok(out)
}

/// A non-empty required string argument, already trimmed.
fn required_text(input: &Value, field: &str) -> Result<String, String> {
    match input.get(field) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
        _ => Err(format!("Error: '{field}' must be provided.")),
    }
}

/// An optional non-empty string argument, already trimmed.
fn optional_text(input: &Value, field: &str) -> Result<Option<String>, String> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim().to_owned())),
        Some(other) => Err(format!("Error: '{field}' must be a string (got {other}).")),
    }
}

/// Log an internal failure and render it for the model (never a panic, never
/// a silent no-op).
fn internal_error(what: &str, error: &anyhow::Error) -> ToolOutput {
    // The operator gets the whole chain (and its context) from the log; the
    // model gets the root cause, not ox's internal call path.
    tracing::error!(error = %error, "mctx {what} failed");
    ok_text(format!("Error: {what} failed: {}", error.root_cause()))
}

// ---------------------------------------------------------------------------
// ctx_memory
// ---------------------------------------------------------------------------

/// `ctx_memory` — the project pinboard (see the module docs).
pub struct CtxMemoryTool {
    db: Db,
}

impl CtxMemoryTool {
    /// The tool over `db` (the `mc_memory` table).
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxMemoryTool {
    fn name(&self) -> &str {
        "ctx_memory"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_memory",
            "Durable facts about this project, shared with every agent working on it and \
             kept for the months this work lasts. Your active memories are already in \
             <project-memory> as `#id: fact` lines. Write one when you learn something \
             that must not have to be found again — a project rule, an architectural \
             fact, a hard-won constraint, a config value, a naming convention — and \
             especially when it cost you turns to find. One standalone fact per memory, \
             phrased to make sense on its own. A pending intention with its evidence \
             (\"do X later, here is what we know\") is ctx_note, not memory.",
            object_param(
                vec![
                    (
                        "action",
                        json!({
                            "type": "string",
                            "enum": ["write", "update", "archive", "merge", "get"],
                            "description": "write: new memory (content + category). \
                                            update: rewrite one memory whose fact changed \
                                            (ids: [one], content; category optional to \
                                            recategorize). archive: retire wrong or \
                                            obsolete memories (ids: [one or more], \
                                            optional reason). merge: collapse duplicates \
                                            into one active memory (ids: [two or more] \
                                            active, content; the lowest id survives and \
                                            the rest are archived). get: fetch by id \
                                            (ids: 1-20), readable in every status.",
                        }),
                    ),
                    (
                        "content",
                        json!({
                            "type": "string",
                            "description": "The fact text (write, update, merge).",
                        }),
                    ),
                    (
                        "category",
                        json!({
                            "type": "string",
                            "enum": CATEGORIES,
                            "description": "Kind of fact (required for write; optional on \
                                            update/merge to recategorize).",
                        }),
                    ),
                    (
                        "ids",
                        json!({
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Memory ids from a `#id: fact` line or a \
                                            `<project-memory>` entry.",
                        }),
                    ),
                    (
                        "reason",
                        json!({
                            "type": "string",
                            "description": "Why the memory is being archived (optional, \
                                            archive).",
                        }),
                    ),
                ],
                &["action"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_memory(&self.db, &ctx, &input).await })
    }
}

async fn run_memory(db: &Db, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let Some(action) = input.get("action").and_then(Value::as_str) else {
        return ok_text("Error: 'action' must be provided.");
    };
    // The project key is the working directory the client sent: every
    // session of the same project shares its pinboard.
    let project = ctx.directory.to_string_lossy().into_owned();

    match action {
        "write" => {
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let category = match required_category(input) {
                Ok(category) => category,
                Err(message) => return ok_text(message),
            };
            match write_memory(db, &project, &content, &category).await {
                Ok(id) => ok_text(format!("Wrote memory #{id}.")),
                Err(error) => internal_error("memory write", &error),
            }
        }
        "update" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_MUTATE_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            let [id] = ids.as_slice() else {
                return ok_text("Error: 'ids' must carry exactly one id for update.");
            };
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let category = match optional_category(input) {
                Ok(category) => category,
                Err(message) => return ok_text(message),
            };
            match update_memory(db, &project, id, &content, category.as_deref()).await {
                Ok(true) => ok_text(format!("Updated memory #{id}.")),
                Ok(false) => ok_text(format!("Error: no memory #{id}.")),
                Err(error) => internal_error("memory update", &error),
            }
        }
        "archive" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_MUTATE_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            if has_duplicates(&ids) {
                return ok_text("Error: 'ids' must not repeat an id.");
            }
            let reason = match optional_text(input, "reason") {
                Ok(reason) => reason,
                Err(message) => return ok_text(message),
            };
            match archive_memories(db, &project, &ids, reason.as_deref()).await {
                Ok(archived) if archived.is_empty() => ok_text("Error: no such active memory."),
                Ok(archived) => ok_text(format!(
                    "Archived {}: {}.",
                    plural(archived.len(), "memory", "memories"),
                    handles(&archived)
                )),
                Err(error) => internal_error("memory archive", &error),
            }
        }
        "merge" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_MUTATE_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            if ids.len() < 2 {
                return ok_text("Error: 'ids' must carry at least two ids for merge.");
            }
            if has_duplicates(&ids) {
                return ok_text("Error: 'ids' must not repeat an id.");
            }
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let category = match optional_category(input) {
                Ok(category) => category,
                Err(message) => return ok_text(message),
            };
            match merge_memories(db, &project, &ids, &content, category.as_deref()).await {
                Ok(Some(merged)) => ok_text(format!(
                    "Merged {} into #{}.",
                    handles(&merged.sources),
                    merged.survivor
                )),
                Ok(None) => ok_text("Error: merge needs at least two active memories among 'ids'."),
                Err(error) => internal_error("memory merge", &error),
            }
        }
        "get" => {
            let ids = match parse_int_ids(input.get("ids"), "ids", MEMORY_GET_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            match get_memories(db, &project, &ids).await {
                Ok(rows) if rows.is_empty() => ok_text("Error: no such memory."),
                Ok(rows) => {
                    let lines: Vec<String> = rows
                        .iter()
                        .map(|row| {
                            format!(
                                "#{} [{} {}]\n{}",
                                row.id, row.category, row.status, row.content
                            )
                        })
                        .collect();
                    ok_text(lines.join("\n\n"))
                }
                Err(error) => internal_error("memory get", &error),
            }
        }
        other => ok_text(format!(
            "Error: unknown action '{other}' (write, update, archive, merge, get)."
        )),
    }
}

/// One category check for every arm: the closed set lives in [`CATEGORIES`],
/// and its rejection message is written once.
fn check_category(category: &str) -> Result<(), String> {
    if CATEGORIES.contains(&category) {
        Ok(())
    } else {
        Err(format!(
            "Error: 'category' must be one of {}.",
            CATEGORIES.join(", ")
        ))
    }
}

/// A required `category` argument, validated.
fn required_category(input: &Value) -> Result<String, String> {
    let category = required_text(input, "category")?;
    check_category(&category)?;
    Ok(category)
}

/// An optional (omitted ⇒ `None`) `category` argument, validated when given.
fn optional_category(input: &Value) -> Result<Option<String>, String> {
    let category = optional_text(input, "category")?;
    if let Some(category) = category.as_deref() {
        check_category(category)?;
    }
    Ok(category)
}

/// Whether the same id appears twice (merge rejects repeats: merging an id
/// into itself is meaningless, and the reference reports `DuplicateSourceId`).
fn has_duplicates(ids: &[String]) -> bool {
    let mut seen = std::collections::HashSet::new();
    ids.iter().any(|id| !seen.insert(id))
}

/// `#a, #b, #c` from handle bodies.
fn handles(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `1 note` / `3 notes` (matching the module's other reporting).
fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

// ---------------------------------------------------------------------------
// ctx_note
// ---------------------------------------------------------------------------

/// `ctx_note` — session-scoped pending intentions (see the module docs).
pub struct CtxNoteTool {
    db: Db,
}

impl CtxNoteTool {
    /// The tool over `db` (the `mc_note` table).
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxNoteTool {
    fn name(&self) -> &str {
        "ctx_note"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "ctx_note",
            "Session notes are pending intentions: work you intend to return to, with its \
             findings attached. Use them for a finding to revisit when you return to the \
             intended work; a decision with its reasoning, when follow-up work remains; a \
             backlog item with evidence already found; or something the user explicitly \
             asks you to note. Do not use them for: the next few steps; a plan you are \
             actively executing; restart/fold insurance; a record of how things stand \
             (world-state, a design at a point in time) with nothing you intend to do \
             about it; a fact worth keeping is memory, the rest is nothing. First line is \
             the title (under 80 chars), followed by detail. If the detail already lives \
             in a file, record the path and what to inspect — don't copy the file into a \
             note. Durable project facts belong in ctx_memory, not notes.",
            object_param(
                vec![
                    (
                        "action",
                        json!({
                            "type": "string",
                            "enum": ["write", "read", "update", "dismiss"],
                            "description": "write: save a new note (content required). \
                                            read: one row per note — `#id · age · title` — \
                                            ready smart notes first, then newest; rows \
                                            untouched 30+ days are marked stale. Pass \
                                            note_ids to read full bodies; limit/offset \
                                            page; filter selects other statuses. update: \
                                            change one note (note_ids=[N]). dismiss: \
                                            retire 1-50 notes (note_ids).",
                        }),
                    ),
                    (
                        "content",
                        json!({
                            "type": "string",
                            "description": "Note text for write/update: first line is the \
                                            title (under 80 chars), then the detail.",
                        }),
                    ),
                    (
                        "note_ids",
                        json!({
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Note ids: one for update, 1-50 for read full \
                                            bodies or dismiss.",
                        }),
                    ),
                    (
                        "surface_condition",
                        json!({
                            "type": "string",
                            "description": "Deprecated: this build stores the condition and \
                                            parks the note as pending, but ox does not run \
                                            the outside checker yet, so it will never \
                                            surface on its own. Do not rely on it.",
                        }),
                    ),
                    (
                        "filter",
                        json!({
                            "type": "string",
                            "enum": ["all", "active", "pending", "ready", "dismissed"],
                            "description": "Which notes read returns (default: active and \
                                            parked smart notes).",
                        }),
                    ),
                    (
                        "limit",
                        json!({ "type": "integer", "description": "Rows per read (default 25, max 200)." }),
                    ),
                    (
                        "offset",
                        json!({ "type": "integer", "description": "Skip this many newest rows (default 0)." }),
                    ),
                ],
                &["action"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run_note(&self.db, &ctx, &input).await })
    }
}

async fn run_note(db: &Db, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let Some(action) = input.get("action").and_then(Value::as_str) else {
        return ok_text("Error: 'action' must be provided.");
    };
    let session = ctx.session_id.as_str();

    match action {
        "write" => {
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            let condition = match optional_text(input, "surface_condition") {
                Ok(condition) => condition,
                Err(message) => return ok_text(message),
            };
            match write_note(db, session, &content, condition.as_deref()).await {
                Ok(id) => {
                    let summary = match active_summary(db, session).await {
                        Ok(summary) => summary,
                        Err(error) => {
                            tracing::warn!(error = %error, "note summary after write failed");
                            String::new()
                        }
                    };
                    ok_text(format!("Saved session note #{id}.{summary}"))
                }
                Err(error) => internal_error("note write", &error),
            }
        }
        "read" => {
            let filter = match NoteFilter::parse(input.get("filter").and_then(Value::as_str)) {
                Ok(filter) => filter,
                Err(message) => return ok_text(message),
            };
            let ids = match input.get("note_ids") {
                None | Some(Value::Null) => None,
                other => match parse_int_ids(other, "note_ids", NOTE_ID_LIMIT) {
                    Ok(ids) => Some(ids),
                    Err(message) => return ok_text(message),
                },
            };
            let now = now_millis();
            match ids {
                // Full bodies: the page arguments are not read for this
                // branch, so a bad `limit` cannot reject a valid read.
                Some(ids) => match get_notes(db, session, &ids).await {
                    Ok(rows) if rows.is_empty() => ok_text("Error: no such note."),
                    Ok(rows) => {
                        let lines: Vec<String> =
                            rows.iter().map(|row| full_note(row, now)).collect();
                        ok_text(lines.join("\n\n"))
                    }
                    Err(error) => internal_error("note read", &error),
                },
                // The `#id · age · title` page.
                None => {
                    let limit = match parse_page(input.get("limit"), "limit", NOTE_PAGE) {
                        Ok(limit) => limit,
                        Err(message) => return ok_text(message),
                    };
                    let offset = match parse_page(input.get("offset"), "offset", 0) {
                        Ok(offset) => offset,
                        Err(message) => return ok_text(message),
                    };
                    match list_notes(db, session, filter, limit, offset).await {
                        Ok(rows) if rows.is_empty() => ok_text("No notes."),
                        Ok(rows) => {
                            let lines: Vec<String> =
                                rows.iter().map(|row| note_row_line(row, now)).collect();
                            ok_text(lines.join("\n"))
                        }
                        Err(error) => internal_error("note read", &error),
                    }
                }
            }
        }
        "update" => {
            let ids = match parse_int_ids(input.get("note_ids"), "note_ids", NOTE_ID_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            let [id] = ids.as_slice() else {
                return ok_text("Error: 'note_ids' must carry exactly one id for update.");
            };
            let content = match required_text(input, "content") {
                Ok(content) => content,
                Err(message) => return ok_text(message),
            };
            match update_note(db, session, id, &content).await {
                Ok(true) => ok_text(format!("Updated note #{id}.")),
                Ok(false) => ok_text(format!("Error: no note #{id}.")),
                Err(error) => internal_error("note update", &error),
            }
        }
        "dismiss" => {
            let ids = match parse_int_ids(input.get("note_ids"), "note_ids", NOTE_ID_LIMIT) {
                Ok(ids) => ids,
                Err(message) => return ok_text(message),
            };
            match dismiss_notes(db, session, &ids).await {
                Ok(0) => ok_text("Error: no such note."),
                Ok(moved) => ok_text(format!("Dismissed {}.", plural(moved, "note", "notes"))),
                Err(error) => internal_error("note dismiss", &error),
            }
        }
        other => ok_text(format!(
            "Error: unknown action '{other}' (write, read, update, dismiss)."
        )),
    }
}

/// An optional non-negative integer page argument, clamped to
/// [`MAX_NOTE_PAGE`]. A huge value would otherwise reach SQLite as a negative
/// LIMIT, which SQLite reads as "no limit".
fn parse_page(value: Option<&Value>, field: &str, default: usize) -> Result<usize, String> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) => Ok(usize::try_from(value)
                .unwrap_or(usize::MAX)
                .min(MAX_NOTE_PAGE)),
            None => Err(format!("Error: '{field}' must be a non-negative integer.")),
        },
        Some(other) => Err(format!(
            "Error: '{field}' must be a non-negative integer (got {other})."
        )),
    }
}

/// `#12 · 3d · title` (+ ` (stale)` past [`STALE_DAYS`]).
fn note_row_line(row: &NoteRow, now: i64) -> String {
    let age = humanize_age(now, row.touched_at);
    let stale = if (now - row.touched_at) / 86_400_000 >= STALE_DAYS {
        " (stale)"
    } else {
        ""
    };
    let marker = if row.status == "pending" {
        " [parked]"
    } else {
        ""
    };
    format!("#{} · {age} · {}{marker}{stale}", row.id, row.title)
}

/// Full body view for `read` with `note_ids`.
fn full_note(row: &NoteRow, now: i64) -> String {
    let age = humanize_age(now, row.touched_at);
    let mut out = format!("#{} [{}] ({age})\n{}", row.id, row.status, row.title);
    if !row.body.is_empty() {
        out.push_str("\n\n");
        out.push_str(&row.body);
    }
    out
}

/// ` 3 active, oldest 3d.` — the write confirmation's tail; empty when the
/// session has no active notes. "Oldest" is the earliest last touch, matching
/// how rows are aged everywhere else.
async fn active_summary(db: &Db, session_id: &str) -> Result<String> {
    let session = session_id.to_owned();
    let (count, oldest): (i64, Option<i64>) = db
        .conn()
        .call(move |c| -> rusqlite::Result<(i64, Option<i64>)> {
            // An aggregate without GROUP BY always yields exactly one row.
            c.query_row(
                "SELECT COUNT(*), MIN(COALESCE(updated_at, created_at)) FROM mc_note
                 WHERE session_id = ?1 AND status = 'active'",
                [&session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
        .context("active note summary")?;
    Ok(match oldest {
        Some(oldest) if count > 0 => {
            format!(
                " {count} active, oldest {}.",
                humanize_age(now_millis(), oldest)
            )
        }
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    const PROJECT: &str = "/proj/a";
    const OTHER_PROJECT: &str = "/proj/b";
    const SESSION: &str = "ses_test";
    const OTHER_SESSION: &str = "ses_other";

    async fn db() -> Db {
        Db::open_in_memory().await.unwrap()
    }

    fn ctx(session: &str, project: &str) -> ToolContext {
        ToolContext {
            session_id: session.to_owned(),
            directory: std::path::PathBuf::from(project),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    fn text(output: &ToolOutput) -> String {
        output.content.as_str().unwrap_or_default().to_owned()
    }

    /// Run a `ctx_memory` call.
    async fn memory(db: &Db, input: Value) -> String {
        memory_in(db, input, SESSION, PROJECT).await
    }

    async fn memory_in(db: &Db, input: Value, session: &str, project: &str) -> String {
        let tool = CtxMemoryTool::new(db.clone());
        let output = tool.execute(ctx(session, project), input).await;
        assert!(output.error.is_none(), "ctx_memory must not fail the tool");
        text(&output)
    }

    /// Run a `ctx_note` call.
    async fn note(db: &Db, input: Value) -> String {
        note_in(db, input, SESSION).await
    }

    async fn note_in(db: &Db, input: Value, session: &str) -> String {
        let tool = CtxNoteTool::new(db.clone());
        let output = tool.execute(ctx(session, PROJECT), input).await;
        assert!(output.error.is_none(), "ctx_note must not fail the tool");
        text(&output)
    }

    // ---- memory store ----

    #[tokio::test]
    async fn write_mints_ascending_ids_and_update_recategorizes() {
        let db = db().await;
        let first = write_memory(&db, PROJECT, "one", "NAMING").await.unwrap();
        let second = write_memory(&db, PROJECT, "two", "NAMING").await.unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("1", "2"));

        assert!(
            update_memory(&db, PROJECT, "1", "one'", Some("CONSTRAINTS"))
                .await
                .unwrap()
        );
        let rows = active_memories(&db, PROJECT).await.unwrap();
        assert_eq!(rows[0].content, "one'");
        assert_eq!(rows[0].category, "CONSTRAINTS");

        // Omitted category keeps the stored one.
        assert!(
            update_memory(&db, PROJECT, "1", "one''", None)
                .await
                .unwrap()
        );
        let rows = active_memories(&db, PROJECT).await.unwrap();
        assert_eq!(rows[0].category, "CONSTRAINTS");
        assert_eq!(rows[0].content, "one''");

        // Unknown id is a no-op, not an error.
        assert!(!update_memory(&db, PROJECT, "99", "x", None).await.unwrap());
    }

    #[tokio::test]
    async fn archive_keeps_the_row_readable_with_its_reason() {
        let db = db().await;
        write_memory(&db, PROJECT, "wrong", "NAMING").await.unwrap();
        write_memory(&db, PROJECT, "right", "NAMING").await.unwrap();

        let archived = archive_memories(&db, PROJECT, &["1".to_owned()], Some("outdated"))
            .await
            .unwrap();
        assert_eq!(archived, vec!["1".to_owned()]);

        // Gone from the pinboard...
        assert_eq!(active_memories(&db, PROJECT).await.unwrap().len(), 1);
        // ...but still readable, with its status and reason.
        let rows = get_memories(&db, PROJECT, &["1".to_owned()]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "archived");
        assert_eq!(rows[0].content, "wrong");
        let reason: Option<String> = db
            .conn()
            .call(|c| {
                c.query_row(
                    "SELECT archive_reason FROM mc_memory WHERE id = '1'",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(reason.as_deref(), Some("outdated"));

        // Archiving an already-archived row reports nothing archived.
        let again = archive_memories(&db, PROJECT, &["1".to_owned()], None)
            .await
            .unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn merge_keeps_the_lowest_id_and_archives_the_losers() {
        let db = db().await;
        for content in ["a", "b", "c"] {
            write_memory(&db, PROJECT, content, "NAMING").await.unwrap();
        }
        let merged = merge_memories(
            &db,
            PROJECT,
            &["3".to_owned(), "1".to_owned(), "2".to_owned()],
            "merged",
            Some("ARCHITECTURE"),
        )
        .await
        .unwrap()
        .expect("two or more active rows");
        assert_eq!(merged.survivor, "1");
        assert_eq!(merged.sources, ["1", "2", "3"]);

        let active = active_memories(&db, PROJECT).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "1");
        assert_eq!(active[0].content, "merged");
        assert_eq!(active[0].category, "ARCHITECTURE");

        let losers = get_memories(&db, PROJECT, &["2".to_owned(), "3".to_owned()])
            .await
            .unwrap();
        assert!(losers.iter().all(|row| row.status == "archived"));

        // Too few live rows: nothing is merged (no silent single-row
        // rewrite).
        assert!(
            merge_memories(&db, PROJECT, &["42".to_owned(), "43".to_owned()], "x", None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            merge_memories(&db, PROJECT, &["1".to_owned(), "9".to_owned()], "x", None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn merge_never_touches_an_archived_row() {
        let db = db().await;
        write_memory(&db, PROJECT, "wrong", "NAMING").await.unwrap();
        write_memory(&db, PROJECT, "right", "NAMING").await.unwrap();
        archive_memories(&db, PROJECT, &["1".to_owned()], Some("outdated"))
            .await
            .unwrap();

        // #1 is archived: it is neither the survivor nor a loser.
        assert!(
            merge_memories(&db, PROJECT, &["1".to_owned(), "2".to_owned()], "x", None)
                .await
                .unwrap()
                .is_none()
        );
        let active = active_memories(&db, PROJECT).await.unwrap();
        assert_eq!(active.len(), 1, "the live row must survive untouched");
        assert_eq!(active[0].id, "2");
        assert_eq!(active[0].content, "right");
        // The archived row keeps its own reason.
        let reason: Option<String> = db
            .conn()
            .call(|c| {
                c.query_row(
                    "SELECT archive_reason FROM mc_memory WHERE id = '1'",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(reason.as_deref(), Some("outdated"));
    }

    #[tokio::test]
    async fn memories_are_project_scoped() {
        let db = db().await;
        write_memory(&db, PROJECT, "a", "NAMING").await.unwrap();
        write_memory(&db, OTHER_PROJECT, "b", "NAMING")
            .await
            .unwrap();
        assert_eq!(active_memories(&db, PROJECT).await.unwrap().len(), 1);
        assert_eq!(active_memories(&db, OTHER_PROJECT).await.unwrap().len(), 1);
        // Handles come from one sequence, so the second project's first
        // memory is #2, not #1.
        let id = write_memory(&db, OTHER_PROJECT, "c", "NAMING")
            .await
            .unwrap();
        assert_eq!(id, "3");
        // Another project's ids are invisible.
        assert!(
            get_memories(&db, PROJECT, &["2".to_owned()])
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ---- note store ----

    #[tokio::test]
    async fn write_splits_title_and_body() {
        let db = db().await;
        let id = write_note(&db, SESSION, "  Title line  \n\n body line \n", None)
            .await
            .unwrap();
        let rows = get_notes(&db, SESSION, std::slice::from_ref(&id))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Title line");
        assert_eq!(rows[0].body, "body line");
        assert_eq!(rows[0].status, "active");
        assert_eq!(rows[0].surface_condition, None);
    }

    #[tokio::test]
    async fn note_handles_are_unique_across_sessions_and_scoped_reads() {
        let db = db().await;
        let a = write_note(&db, SESSION, "a", None).await.unwrap();
        let b = write_note(&db, OTHER_SESSION, "b", None).await.unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("1", "2"));
        assert_eq!(
            get_notes(&db, SESSION, &["1".to_owned()]).await.unwrap()[0].title,
            "a"
        );
        // A note of another session is not readable from this one.
        assert!(
            get_notes(&db, SESSION, &["2".to_owned()])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            get_notes(&db, OTHER_SESSION, &["2".to_owned()])
                .await
                .unwrap()[0]
                .title,
            "b"
        );
    }

    #[tokio::test]
    async fn a_surface_condition_parks_the_note_as_pending() {
        let db = db().await;
        let id = write_note(&db, SESSION, "revisit", Some("ci is green"))
            .await
            .unwrap();
        let rows = get_notes(&db, SESSION, &[id]).await.unwrap();
        assert_eq!(rows[0].status, "pending");
        assert_eq!(rows[0].surface_condition.as_deref(), Some("ci is green"));

        // A parked note is in neither `active` nor `ready` (no checker yet),
        // but it is in `pending` and in the default view.
        let active = list_notes(&db, SESSION, NoteFilter::Active, 25, 0)
            .await
            .unwrap();
        assert!(active.is_empty());
        let ready = list_notes(&db, SESSION, NoteFilter::Ready, 25, 0)
            .await
            .unwrap();
        assert!(ready.is_empty());
        let pending = list_notes(&db, SESSION, NoteFilter::Pending, 25, 0)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        let default = list_notes(&db, SESSION, NoteFilter::Default_, 25, 0)
            .await
            .unwrap();
        assert_eq!(default.len(), 1);
    }

    #[tokio::test]
    async fn list_view_orders_newest_first_and_pages() {
        let db = db().await;
        // The parked note is the OLDEST: newest-first ordering must place it
        // last (a parked note leads the page only once a checker can mark it
        // ready).
        write_note(&db, SESSION, "parked", Some("cond"))
            .await
            .unwrap();
        write_note(&db, SESSION, "one", None).await.unwrap();
        write_note(&db, SESSION, "two", None).await.unwrap();
        write_note(&db, SESSION, "three", None).await.unwrap();

        let rows = list_notes(&db, SESSION, NoteFilter::Default_, 25, 0)
            .await
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["4", "3", "2", "1"], "newest first");

        let page = list_notes(&db, SESSION, NoteFilter::Default_, 2, 1)
            .await
            .unwrap();
        let ids: Vec<&str> = page.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["3", "2"]);
    }

    #[tokio::test]
    async fn dismiss_moves_a_note_out_of_the_default_view() {
        let db = db().await;
        write_note(&db, SESSION, "one", None).await.unwrap();
        write_note(&db, SESSION, "two", None).await.unwrap();
        assert_eq!(
            dismiss_notes(&db, SESSION, &["1".to_owned()])
                .await
                .unwrap(),
            1
        );
        // Dismissing twice moves nothing the second time.
        assert_eq!(
            dismiss_notes(&db, SESSION, &["1".to_owned()])
                .await
                .unwrap(),
            0
        );

        let default = list_notes(&db, SESSION, NoteFilter::Default_, 25, 0)
            .await
            .unwrap();
        assert_eq!(default.len(), 1);
        let dismissed = list_notes(&db, SESSION, NoteFilter::Dismissed, 25, 0)
            .await
            .unwrap();
        assert_eq!(dismissed.len(), 1);
        let all = list_notes(&db, SESSION, NoteFilter::All, 25, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn update_reparses_title_and_body() {
        let db = db().await;
        write_note(&db, SESSION, "old", None).await.unwrap();
        assert!(
            update_note(&db, SESSION, "1", "new title\n\nnew body")
                .await
                .unwrap()
        );
        let rows = get_notes(&db, SESSION, &["1".to_owned()]).await.unwrap();
        assert_eq!(rows[0].title, "new title");
        assert_eq!(rows[0].body, "new body");
        assert!(!update_note(&db, SESSION, "9", "x").await.unwrap());
    }

    #[test]
    fn split_note_handles_the_single_line_and_whitespace_cases() {
        assert_eq!(split_note("only"), ("only".to_owned(), String::new()));
        assert_eq!(split_note("  t\n  b  "), ("t".to_owned(), "b".to_owned()));
        assert_eq!(split_note("t\n\n"), ("t".to_owned(), String::new()));
    }

    #[test]
    fn humanize_age_scales_minutes_hours_days() {
        let now = 10_000_000_000;
        assert_eq!(humanize_age(now, now), "0m");
        assert_eq!(humanize_age(now, now - 45 * 60_000), "45m");
        assert_eq!(humanize_age(now, now - 3 * 3_600_000), "3h");
        assert_eq!(humanize_age(now, now - 12 * 86_400_000), "12d");
        // Clock skew never reports a negative age.
        assert_eq!(humanize_age(now, now + 1_000), "0m");
    }

    #[test]
    fn parse_int_ids_accepts_numbers_and_digit_strings_and_rejects_junk() {
        assert_eq!(
            parse_int_ids(Some(&json!([1, "2"])), "ids", 20).unwrap(),
            vec!["1".to_owned(), "2".to_owned()]
        );
        // A lone scalar is accepted too.
        assert_eq!(
            parse_int_ids(Some(&json!(3)), "ids", 20).unwrap(),
            vec!["3".to_owned()]
        );
        for bad in [json!(["x"]), json!([0]), json!([-1]), json!([]), json!(1.5)] {
            assert!(
                parse_int_ids(Some(&bad), "ids", 20).is_err(),
                "{bad} must be rejected"
            );
        }
        assert!(parse_int_ids(None, "ids", 20).is_err());
        assert!(parse_int_ids(Some(&json!([1, 2, 3])), "ids", 2).is_err());
    }

    // ---- tools ----

    #[tokio::test]
    async fn memory_tool_round_trips_write_get_archive() {
        let db = db().await;
        assert_eq!(
            memory(
                &db,
                json!({"action": "write", "content": "fact", "category": "ARCHITECTURE"})
            )
            .await,
            "Wrote memory #1."
        );
        assert_eq!(
            memory(&db, json!({"action": "get", "ids": [1]})).await,
            "#1 [ARCHITECTURE active]\nfact"
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "update", "ids": [1], "content": "fact2"})
            )
            .await,
            "Updated memory #1."
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "archive", "ids": [1], "reason": "wrong"})
            )
            .await,
            "Archived 1 memory: #1."
        );
        assert_eq!(
            memory(&db, json!({"action": "get", "ids": [1]})).await,
            "#1 [ARCHITECTURE archived]\nfact2"
        );
        assert_eq!(
            memory(&db, json!({"action": "archive", "ids": [1]})).await,
            "Error: no such active memory."
        );
        assert_eq!(
            memory(&db, json!({"action": "get", "ids": [7]})).await,
            "Error: no such memory."
        );
    }

    #[tokio::test]
    async fn memory_tool_validates_action_category_and_id_cardinality() {
        let db = db().await;
        assert_eq!(
            memory(&db, json!({"action": "write", "content": "x"})).await,
            "Error: 'category' must be provided."
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "write", "content": "x", "category": "WRONG"})
            )
            .await,
            "Error: 'category' must be one of PROJECT_RULES, ARCHITECTURE, CONSTRAINTS, CONFIG_VALUES, NAMING."
        );
        assert_eq!(
            memory(&db, json!({"action": "nope"})).await,
            "Error: unknown action 'nope' (write, update, archive, merge, get)."
        );
        assert_eq!(
            memory(&db, json!({"action": "update", "content": "x"})).await,
            "Error: 'ids' must be provided."
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "update", "ids": [1, 2], "content": "x"})
            )
            .await,
            "Error: 'ids' must carry exactly one id for update."
        );
        assert_eq!(
            memory(&db, json!({"action": "merge"})).await,
            "Error: 'ids' must be provided."
        );
        assert_eq!(
            memory(&db, json!({"action": "merge", "ids": [1], "content": "x"})).await,
            "Error: 'ids' must carry at least two ids for merge."
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "write", "content": "  ", "category": "NAMING"})
            )
            .await,
            "Error: 'content' must be provided."
        );
    }

    #[tokio::test]
    async fn memory_tool_merge_reports_the_survivor() {
        let db = db().await;
        memory(
            &db,
            json!({"action": "write", "content": "a", "category": "NAMING"}),
        )
        .await;
        memory(
            &db,
            json!({"action": "write", "content": "b", "category": "NAMING"}),
        )
        .await;
        assert_eq!(
            memory(
                &db,
                json!({"action": "merge", "ids": [2, 1], "content": "ab"})
            )
            .await,
            "Merged #1, #2 into #1."
        );
    }

    #[tokio::test]
    async fn memory_tool_merge_validates_category_and_source_ids() {
        let db = db().await;
        memory(
            &db,
            json!({"action": "write", "content": "a", "category": "NAMING"}),
        )
        .await;
        memory(
            &db,
            json!({"action": "write", "content": "b", "category": "NAMING"}),
        )
        .await;

        // An out-of-enum category would drop the merged fact out of the
        // pinboard, so it is rejected exactly like update's.
        assert_eq!(
            memory(
                &db,
                json!({"action": "merge", "ids": [1, 2], "content": "ab", "category": "GARBAGE"})
            )
            .await,
            "Error: 'category' must be one of PROJECT_RULES, ARCHITECTURE, CONSTRAINTS, CONFIG_VALUES, NAMING."
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "merge", "ids": [1, 1], "content": "ab"})
            )
            .await,
            "Error: 'ids' must not repeat an id."
        );
        assert_eq!(
            memory(
                &db,
                json!({"action": "merge", "ids": [1, 7], "content": "ab"})
            )
            .await,
            "Error: merge needs at least two active memories among 'ids'."
        );
        // Nothing was merged by those rejections.
        assert_eq!(active_memories(&db, PROJECT).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn memory_tool_merge_names_only_the_rows_it_merged() {
        let db = db().await;
        for content in ["a", "b", "c"] {
            memory(
                &db,
                json!({"action": "write", "content": content, "category": "NAMING"}),
            )
            .await;
        }
        // #1 is archived, so it is not part of the merge and must not be
        // named in the confirmation.
        memory(
            &db,
            json!({"action": "archive", "ids": [1], "reason": "wrong"}),
        )
        .await;
        assert_eq!(
            memory(
                &db,
                json!({"action": "merge", "ids": [1, 3, 2], "content": "bc"})
            )
            .await,
            "Merged #2, #3 into #2."
        );
    }

    #[tokio::test]
    async fn memory_tool_rejects_repeated_archive_ids_and_dedupes_get() {
        let db = db().await;
        memory(
            &db,
            json!({"action": "write", "content": "a", "category": "NAMING"}),
        )
        .await;
        // A repeat is rejected rather than counted twice.
        assert_eq!(
            memory(
                &db,
                json!({"action": "archive", "ids": [1, 1], "reason": "dup"})
            )
            .await,
            "Error: 'ids' must not repeat an id."
        );
        assert_eq!(active_memories(&db, PROJECT).await.unwrap().len(), 1);
        assert_eq!(
            memory(&db, json!({"action": "archive", "ids": [1]})).await,
            "Archived 1 memory: #1."
        );
        // A repeated get prints the memory once.
        assert_eq!(
            memory(&db, json!({"action": "get", "ids": [1, 1]})).await,
            "#1 [NAMING archived]\na"
        );
    }

    #[tokio::test]
    async fn note_tool_writes_reads_updates_and_dismisses() {
        let db = db().await;
        assert_eq!(
            note(
                &db,
                json!({"action": "write", "content": "Do later\nwith evidence"})
            )
            .await,
            "Saved session note #1. 1 active, oldest 0m."
        );
        assert_eq!(
            note(&db, json!({"action": "read"})).await,
            "#1 · 0m · Do later"
        );
        assert_eq!(
            note(&db, json!({"action": "read", "note_ids": [1]})).await,
            "#1 [active] (0m)\nDo later\n\nwith evidence"
        );
        assert_eq!(
            note(
                &db,
                json!({"action": "update", "note_ids": [1], "content": "Renamed"})
            )
            .await,
            "Updated note #1."
        );
        assert_eq!(
            note(&db, json!({"action": "read"})).await,
            "#1 · 0m · Renamed"
        );
        assert_eq!(
            note(&db, json!({"action": "dismiss", "note_ids": [1]})).await,
            "Dismissed 1 note."
        );
        assert_eq!(note(&db, json!({"action": "read"})).await, "No notes.");
        assert_eq!(
            note(&db, json!({"action": "read", "note_ids": [9]})).await,
            "Error: no such note."
        );
        assert_eq!(
            note(
                &db,
                json!({"action": "update", "note_ids": [1, 2], "content": "x"})
            )
            .await,
            "Error: 'note_ids' must carry exactly one id for update."
        );
    }

    #[tokio::test]
    async fn note_tool_pages_and_filters() {
        let db = db().await;
        note(
            &db,
            json!({"action": "write", "content": "parked", "surface_condition": "cond"}),
        )
        .await;
        for title in ["one", "two", "three"] {
            note(&db, json!({"action": "write", "content": title})).await;
        }

        let all = note(&db, json!({"action": "read"})).await;
        assert_eq!(
            all,
            "#4 · 0m · three\n#3 · 0m · two\n#2 · 0m · one\n#1 · 0m · parked [parked]"
        );
        assert_eq!(
            note(&db, json!({"action": "read", "limit": 2, "offset": 2})).await,
            "#2 · 0m · one\n#1 · 0m · parked [parked]"
        );
        assert_eq!(
            note(&db, json!({"action": "read", "filter": "pending"})).await,
            "#1 · 0m · parked [parked]"
        );
        assert_eq!(
            note(&db, json!({"action": "read", "filter": "active"})).await,
            "#4 · 0m · three\n#3 · 0m · two\n#2 · 0m · one"
        );
        assert_eq!(
            note(&db, json!({"action": "read", "filter": "ready"})).await,
            "No notes."
        );
        assert_eq!(
            note(&db, json!({"action": "read", "filter": "nope"})).await,
            "Error: 'filter' must be one of all, active, pending, ready, dismissed (got 'nope')."
        );
        assert_eq!(note(&db, json!({"action": "write", "content": "maybe later", "surface_condition": "deprecated"})).await, "Saved session note #5. 3 active, oldest 0m.");
        assert_eq!(
            note(&db, json!({"action": "bad"})).await,
            "Error: unknown action 'bad' (write, read, update, dismiss)."
        );
    }

    #[tokio::test]
    async fn note_rows_are_marked_stale_past_thirty_days_and_editing_clears_it() {
        let db = db().await;
        note(&db, json!({"action": "write", "content": "old"})).await;
        // Age the row's last touch past the threshold.
        let age = |db: &Db, days: i64| {
            let db = db.clone();
            async move {
                db.conn()
                    .call(move |c| {
                        c.execute(
                            "UPDATE mc_note
                             SET created_at = created_at - ?1, updated_at = updated_at - ?1",
                            [days * 86_400_000_i64],
                        )
                    })
                    .await
                    .unwrap();
            }
        };
        age(&db, 31).await;
        let read = note(&db, json!({"action": "read"})).await;
        assert!(read.ends_with(" · old (stale)"), "got: {read}");

        // Editing the note resets the last touch, so the marker clears.
        note(
            &db,
            json!({"action": "update", "note_ids": [1], "content": "old but current"}),
        )
        .await;
        let read = note(&db, json!({"action": "read"})).await;
        assert_eq!(read, "#1 · 0m · old but current");
    }

    #[tokio::test]
    async fn tool_definitions_are_model_facing_and_serialize() {
        let tool = CtxMemoryTool::new(db().await);
        assert_eq!(tool.name(), "ctx_memory");
        let definition = tool.definition();
        assert_eq!(definition.function.name, "ctx_memory");
        assert!(!definition.function.description.is_empty());
        assert_eq!(
            definition.function.parameters["properties"]["action"]["enum"][0],
            json!("write")
        );
        assert_eq!(
            definition.function.parameters["required"][0],
            json!("action")
        );

        let tool = CtxNoteTool::new(db().await);
        assert_eq!(tool.name(), "ctx_note");
        let definition = tool.definition();
        assert_eq!(definition.function.name, "ctx_note");
        assert_eq!(
            definition.function.parameters["properties"]["filter"]["enum"][4],
            json!("dismissed")
        );
    }
}
