use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::mctx::embed::BATCH_SIZE;
use crate::mctx::redact;
use ox_db::{Db, WriteTx, optional};
use ox_schema::id::now_millis;

use super::{MemorySource, USER_SCOPE, owner_for, owner_match};

/// One active memory as the pinboard render reads it.
///
/// Public so the historian's `<project-memory>` renderer
/// ([`crate::mctx::historian::render_historian_memory_block`]) can take it:
/// that renderer is deliberately a different shape from the pinboard's.
///
/// `PartialEq` but not `Eq`: `importance` is an `f64`. The weight controls
/// selection ([`select_memories`]) and is deliberately absent from the wire, so
/// a classification-only update changes no prompt byte.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRow {
    /// `#N` handle body.
    pub id: String,
    /// One of [`CATEGORIES`](super::CATEGORIES).
    pub category: String,
    /// The fact text.
    pub content: String,
    /// Selection weight from the dreamer's classify task; `0` until it runs,
    /// which ties every row and leaves recency to decide.
    pub importance: f64,
    /// Last write to the row, in epoch milliseconds.
    pub updated_at: i64,
    /// Last verification against the cited files, in epoch milliseconds; `0`
    /// means never verified.
    pub verified_at: i64,
    /// Last retrieval by a model through `ctx_search` or `ctx_memory get`, in
    /// epoch milliseconds; `0` means never retrieved. A selection tiebreak
    /// only: recording a use is neither a write nor a verification.
    pub last_used_at: i64,
    /// How many historian folds have stated this fact (`1` for a row written
    /// on purpose). With [`MemorySource::Historian`] it is the row's
    /// confirmation: see [`select_memories`].
    pub seen_count: i64,
    /// Where the row came from.
    pub source: MemorySource,
}

impl MemoryRow {
    /// Whether the pinboard may carry this row. A historian-promoted fact is a
    /// one-model reading of one fold until a second fold states it again (or
    /// a human writes it); until then it is kept for the historian's own
    /// dedup and for `ctx_search`, but it does not ride every request.
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        self.source == MemorySource::Manual || self.seen_count >= PROMOTED_CONFIRMATION_SIGHTINGS
    }
}

/// Historian sightings before a promoted fact counts as confirmed.
pub(crate) const PROMOTED_CONFIRMATION_SIGHTINGS: i64 = 2;

/// One memory as `get` reads it (every status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryDetail {
    /// `#N` handle body.
    pub id: String,
    /// One of [`CATEGORIES`](super::CATEGORIES).
    pub category: String,
    /// The fact text.
    pub content: String,
    /// `active` or `archived`.
    pub status: String,
    /// `#N` handle of the memory that replaced this row. `None` unless the
    /// row was archived by a merge or supersede.
    pub superseded_by: Option<String>,
}

/// Write a new memory; returns its `#N` handle body. The row's owner follows
/// the category ([`owner_for`]): a [`USER_PROFILE`] memory written from any
/// project lands in the user namespace.
pub(crate) async fn write_memory(
    db: &Db,
    project: &str,
    content: &str,
    category: &str,
) -> Result<String> {
    let context = format!("write memory for project {project}");
    let content = redact::redact_secrets(content);
    let content = content.as_ref();
    let now = now_millis();
    let owner = owner_for(project, category);
    let result: Result<String> = async {
        let tx = db.begin().await?;
        let id = insert_memory(&tx, owner, content, category, now).await?;
        tx.commit().await?;
        Ok(id)
    }
    .await;
    result.with_context(|| context)
}

/// Outcome of publishing an automatic lesson; explicit writes bypass this gate.
pub(crate) enum LessonPublication {
    Recorded(String),
    AlreadyRecorded,
    Ineligible,
}

/// Recheck every contributing session, deduplicate the redacted fact, and insert
/// under the same write transaction. Never hold this transaction across a model call.
pub(crate) async fn publish_lesson(
    db: &Db,
    project: &str,
    content: &str,
    category: &str,
    source_sessions: &[&str],
) -> Result<LessonPublication> {
    let content = redact::redact_secrets(content);
    let tx = db.begin().await?;
    let mut eligibility = tx
        .prepare(
            "SELECT 1 FROM session_v2 s WHERE s.id = ?1 AND s.directory = ?2
         AND NOT EXISTS (SELECT 1 FROM kv WHERE key = ?3 OR key = ?4)",
        )
        .await?;
    if source_sessions.is_empty() {
        tx.rollback().await?;
        return Ok(LessonPublication::Ineligible);
    }
    for session in source_sessions {
        let eligible = optional(
            &mut eligibility,
            (
                *session,
                project,
                ox_schema::support::memory_mode_key(session).as_str(),
                ox_schema::support::session_taint_key(session).as_str(),
            ),
        )
        .await?
        .is_some();
        if !eligible {
            tx.rollback().await?;
            return Ok(LessonPublication::Ineligible);
        }
    }
    let mut recorded = tx.prepare(
        "SELECT 1 FROM mc_memory WHERE project = ?1 AND status = 'active' AND content = ?2 LIMIT 1",
    ).await?;
    if optional(&mut recorded, (project, content.as_ref()))
        .await?
        .is_some()
    {
        tx.rollback().await?;
        return Ok(LessonPublication::AlreadyRecorded);
    }
    let id = insert_memory(&tx, project, &content, category, now_millis()).await?;
    tx.commit().await?;
    Ok(LessonPublication::Recorded(id))
}

async fn insert_memory(
    tx: &WriteTx<'_>,
    owner: &str,
    content: &str,
    category: &str,
    now: i64,
) -> Result<String> {
    let id = next_memory_id(tx).await?.to_string();
    tx.execute(
        "INSERT INTO mc_memory (id, project, content, category, created_at, updated_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'active')",
        (id.as_str(), owner, content, category, now),
    )
    .await?;
    Ok(id)
}

/// The next free memory id: `MAX(id) + 1` over the whole table.
///
/// Read inside the writing transaction; the write gate serializes
/// transactions, so no other writer can claim the id before commit.
pub(crate) async fn next_memory_id(tx: &WriteTx<'_>) -> Result<i64> {
    let mut stmt = tx
        .prepare("SELECT COALESCE(MAX(CAST(id AS INTEGER)), 0) + 1 FROM mc_memory")
        .await?;
    Ok(stmt.query_row(()).await?.get::<i64>(0)?)
}

/// One stored memory as the historian's fact promotion compares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromotionRow {
    /// `#N` handle body.
    pub id: String,
    /// One of [`CATEGORIES`](super::CATEGORIES).
    pub category: String,
    /// The fact text.
    pub content: String,
}

/// The project's active and archived memories, read inside `tx` for
/// [`crate::mctx::promotion`]'s dedup.
pub(crate) async fn promotion_rows(tx: &WriteTx<'_>, project: &str) -> Result<Vec<PromotionRow>> {
    let mut stmt = tx
        .prepare(
            "SELECT id, category, content FROM mc_memory
             WHERE project = ?1 AND status IN ('active', 'archived')",
        )
        .await?;
    let mut cursor = stmt.query([project]).await?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().await? {
        rows.push(PromotionRow {
            id: row.get(0)?,
            category: row.get(1)?,
            content: row.get(2)?,
        });
    }
    Ok(rows)
}

/// Count one more sighting of memory `id`, inside `tx`.
pub(crate) async fn bump_seen_count(tx: &WriteTx<'_>, id: &str, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE mc_memory SET seen_count = seen_count + 1, updated_at = ?2
         WHERE id = ?1",
        (id, now),
    )
    .await?;
    Ok(())
}

/// Insert a historian-promoted memory under `id` (from [`next_memory_id`]),
/// inside `tx`.
pub(crate) async fn insert_promoted(
    tx: &WriteTx<'_>,
    id: &str,
    project: &str,
    category: &str,
    content: &str,
    now: i64,
) -> Result<()> {
    let content = redact::redact_secrets(content);
    tx.execute(
        "INSERT INTO mc_memory
             (id, project, content, category, created_at, updated_at, status, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, 'active', 'historian')",
        (id, project, content.as_ref(), category, now),
    )
    .await?;
    Ok(())
}

/// Rewrite one memory. `category` omitted keeps the stored one. `false` =
/// no such id in this project.
///
/// A rewrite clears `verified_at`: the claims are new text, so the old
/// verification says nothing about them.
///
/// A recategorization is confined to the row's owner: a new category whose
/// owner ([`owner_for`]) differs from the row's matches nothing, so a project
/// fact cannot be turned into a user observation (or back) by rewording.
pub(crate) async fn update_memory(
    db: &Db,
    project: &str,
    id: &str,
    content: &str,
    category: Option<&str>,
) -> Result<bool> {
    let context = format!("update memory #{id} for project {project}");
    let content = redact::redact_secrets(content);
    let content = content.as_ref();
    let now = now_millis();
    let (own, user) = owner_match(project);
    let result: Result<bool> = async {
        let conn = db.acquire().await?;
        let changed = match category {
            Some(category) => {
                conn.execute(
                    "UPDATE mc_memory SET content = ?3, category = ?4, updated_at = ?5,
                         verified_at = 0, embedding = NULL, embedding_model = NULL
                         WHERE id = ?2 AND project IN (?1, ?6) AND project = ?7",
                    (
                        own,
                        id,
                        content,
                        category,
                        now,
                        user,
                        owner_for(project, category),
                    ),
                )
                .await?
            }
            None => {
                conn.execute(
                    "UPDATE mc_memory SET content = ?3, updated_at = ?4, verified_at = 0,
                         embedding = NULL, embedding_model = NULL
                         WHERE id = ?2 AND project IN (?1, ?5)",
                    (own, id, content, now, user),
                )
                .await?
            }
        };
        Ok(changed > 0)
    }
    .await;
    result.with_context(|| context)
}

/// A row's status and owner when it is inside the caller's reach
/// ([`owner_match`]): `(project, id, user)`.
const STATUS_IN_REACH_SQL: &str =
    "SELECT status, project FROM mc_memory WHERE id = ?2 AND project IN (?1, ?3)";

/// Archive the given memories (any status may be given; already-archived
/// rows are left alone). Returns the ids that were actually archived.
pub(crate) async fn archive_memories(
    db: &Db,
    project: &str,
    ids: &[String],
    reason: Option<&str>,
) -> Result<Vec<String>> {
    let context = format!("archive memories for project {project}");
    let now = now_millis();
    let (own, user) = owner_match(project);
    let result: Result<Vec<String>> = async {
        let tx = db.begin().await?;
        let mut archived = Vec::new();
        {
            let mut stmt = tx.prepare(STATUS_IN_REACH_SQL).await?;
            for id in ids {
                let status: Option<String> =
                    match optional(&mut stmt, (own, id.as_str(), user)).await? {
                        Some(row) => Some(row.get(0)?),
                        None => None,
                    };
                if status.as_deref() == Some("active") && !archived.contains(id) {
                    archived.push(id.clone());
                }
            }
        }
        for id in &archived {
            tx.execute(
                "UPDATE mc_memory SET status = 'archived', archive_reason = ?3, updated_at = ?4
                 WHERE id = ?2 AND project IN (?1, ?5)",
                (own, id.as_str(), reason, now, user),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(archived)
    }
    .await;
    result.with_context(|| context)
}

/// Archive every active memory of one owner — a project directory or
/// [`USER_SCOPE`] — with `reason`. The user's one-shot reset: rows are
/// archived, not deleted, so `ctx_memory get` can still read them and a
/// mistake is recoverable. Returns how many rows changed.
pub async fn clear_owner(db: &Db, owner: &str, reason: &str) -> Result<usize> {
    let now = now_millis();
    let conn = db.acquire().await?;
    let changed = conn
        .execute(
            "UPDATE mc_memory SET status = 'archived', archive_reason = ?2, updated_at = ?3
             WHERE project = ?1 AND status = 'active'",
            (owner, reason, now),
        )
        .await
        .with_context(|| format!("clear memories of {owner}"))?;
    Ok(usize::try_from(changed).unwrap_or(usize::MAX))
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
    let content = redact::redact_secrets(content);
    let content = content.as_ref();
    let now = now_millis();
    let (own, user) = owner_match(project);
    let result: Result<Option<Merged>> = async {
        let tx = db.begin().await?;
        // Only ACTIVE rows are merge candidates: an archived row must
        // neither survive (its content would be stranded outside the
        // pinboard) nor lose (its own archive reason would be
        // overwritten).
        // A merge needs at least two live rows of one owner; anything else
        // is reported as "nothing merged" rather than silently rewriting one
        // row or fusing a project fact with a user observation.
        let Some((owner, mut live)) = live_rows_of_one_owner(&tx, (own, user), ids).await? else {
            tx.rollback().await?;
            return Ok(None);
        };
        let project = owner.as_str();
        // The survivor keeps its owner's category set.
        if category.is_some_and(|category| owner_for(project, category) != project) {
            tx.rollback().await?;
            return Ok(None);
        }
        // The survivor is the lowest id (the oldest row).
        live.sort_by_key(|id| id.parse::<i64>().unwrap_or(i64::MAX));
        let Some((survivor, losers)) = live.split_first() else {
            tx.rollback().await?;
            return Ok(None);
        };
        match category {
            Some(category) => {
                tx.execute(
                    "UPDATE mc_memory SET content = ?3, category = ?4, updated_at = ?5,
                         verified_at = 0, embedding = NULL, embedding_model = NULL
                     WHERE project = ?1 AND id = ?2",
                    (project, survivor.as_str(), content, category, now),
                )
                .await?;
            }
            None => {
                tx.execute(
                    "UPDATE mc_memory SET content = ?3, updated_at = ?4, verified_at = 0,
                         embedding = NULL, embedding_model = NULL
                     WHERE project = ?1 AND id = ?2",
                    (project, survivor.as_str(), content, now),
                )
                .await?;
            }
        }
        archive_superseded(&tx, project, losers, survivor.as_str(), "merged", now).await?;
        tx.commit().await?;
        Ok(Some(Merged {
            survivor: survivor.clone(),
            sources: live,
        }))
    }
    .await;
    result.with_context(|| context)
}

/// The active rows among `ids` inside `reach` (`(project, user)`), deduped,
/// with the owner they all share: `None` when fewer than two are live or
/// they span two owners.
async fn live_rows_of_one_owner(
    tx: &WriteTx<'_>,
    reach: (&str, &str),
    ids: &[String],
) -> Result<Option<(String, Vec<String>)>> {
    let (own, user) = reach;
    let mut stmt = tx.prepare(STATUS_IN_REACH_SQL).await?;
    let mut live: Vec<String> = Vec::new();
    let mut owner: Option<String> = None;
    for id in ids {
        let Some(row) = optional(&mut stmt, (own, id.as_str(), user)).await? else {
            continue;
        };
        let status: String = row.get(0)?;
        let row_owner: String = row.get(1)?;
        if status != "active" || live.contains(id) {
            continue;
        }
        if owner.as_deref().is_some_and(|known| known != row_owner) {
            return Ok(None);
        }
        owner.get_or_insert(row_owner);
        live.push(id.clone());
    }
    if live.len() < 2 {
        return Ok(None);
    }
    Ok(owner.map(|owner| (owner, live)))
}

/// Archive active rows of `owner` as replaced by `survivor`, inside `tx`.
/// Eligibility is the caller's job; the `status = 'active'` guard keeps an
/// archived row a terminal state.
async fn archive_superseded(
    tx: &WriteTx<'_>,
    owner: &str,
    ids: &[String],
    survivor: &str,
    reason: &str,
    now: i64,
) -> Result<()> {
    let mut stmt = tx
        .prepare(
            "UPDATE mc_memory SET status = 'archived', archive_reason = ?3,
                 superseded_by = ?4, updated_at = ?5
             WHERE project = ?1 AND id = ?2 AND status = 'active'",
        )
        .await?;
    for id in ids {
        stmt.execute((owner, id.as_str(), reason, survivor, now))
            .await?;
    }
    Ok(())
}

/// Fetch memories by id in every status, in the requested order (unknown
/// ids are skipped).
pub(crate) async fn get_memories(
    db: &Db,
    project: &str,
    ids: &[String],
) -> Result<Vec<MemoryDetail>> {
    let context = format!("get memories for project {project}");
    let (own, user) = owner_match(project);
    let result: Result<Vec<MemoryDetail>> = async {
        let conn = db.acquire().await?;
        let mut out = Vec::new();
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content, status, superseded_by FROM mc_memory
                 WHERE id = ?2 AND project IN (?1, ?3)",
            )
            .await?;
        for id in ids {
            let Some(row) = optional(&mut stmt, (own, id.as_str(), user)).await? else {
                continue;
            };
            let id: String = row.get(0)?;
            let category: String = row.get(1)?;
            let content: String = row.get(2)?;
            let status: String = row.get(3)?;
            let superseded_by: Option<String> = row.get(4)?;
            // The row's own handle: a repeated request id must not
            // print the same memory twice.
            if !out.iter().any(|row: &MemoryDetail| row.id == id) {
                out.push(MemoryDetail {
                    id,
                    category,
                    content,
                    status,
                    superseded_by,
                });
            }
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// Every active memory of `owner` (a project, or [`USER_SCOPE`]), ids
/// ascending (the renderer applies category order).
pub(crate) async fn active_memories(db: &Db, owner: &str) -> Result<Vec<MemoryRow>> {
    let context = format!("read active memories for {owner}");
    let result: Result<Vec<MemoryRow>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content, importance, updated_at, verified_at,
                        last_used_at, seen_count, source
                   FROM mc_memory
                  WHERE project = ?1 AND status = 'active'
                  ORDER BY CAST(id AS INTEGER)",
            )
            .await?;
        let mut rows = stmt.query([owner]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let source: String = row.get(8)?;
            out.push(MemoryRow {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                importance: row.get(3)?,
                updated_at: row.get(4)?,
                verified_at: row.get(5)?,
                last_used_at: row.get(6)?,
                seen_count: row.get(7)?,
                source: MemorySource::parse(&source),
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// The user's active profile memories: what [`active_memories`] reads for
/// [`USER_SCOPE`], named so a caller cannot pass a directory by mistake.
pub(crate) async fn user_profile(db: &Db) -> Result<Vec<MemoryRow>> {
    active_memories(db, USER_SCOPE).await
}

/// Choose which memories the pinboard carries under `budget_tokens`.
///
/// Order: [`MemoryRow::importance`] descending (the classify task's weight,
/// `0` until it runs), then the last touch
/// (`max(verified_at, updated_at, last_used_at)`) descending, then the numeric
/// id ascending. The id tie-break is numeric, not lexicographic — handles are
/// numbers (`#9` before `#10`).
///
/// A retrieval through `ctx_search` or `ctx_memory get` refreshes the touch
/// ([`record_use`]), so a memory the model keeps finding stays on the
/// pinboard. The rendered block orders by category and id, so the change is
/// invisible until the selected *set* differs at the budget edge; when it
/// does, only the m[1] bytes change — a `Soft` bust, never a rebuild of m[0].
///
/// A memory that does not fit is **skipped, not truncated**, and the walk
/// continues: a short fact behind a long one still makes it in. The budget
/// accounts for the fact text only; the framing (`#id: `, category tags) is a
/// constant per included row and not what the budget exists to bound.
///
/// Unconfirmed rows ([`MemoryRow::is_confirmed`]) are not candidates at all:
/// a fact one fold stated once waits for a second sighting before it costs
/// every request of the project.
///
/// Pure: no IO, no DB, and no content copy — the selection is a list of
/// borrows.
pub(crate) fn select_memories<'a>(
    memories: &'a [MemoryRow],
    budget_tokens: usize,
    count_tokens: &dyn Fn(&str) -> usize,
) -> Vec<&'a MemoryRow> {
    let mut ordered: Vec<&MemoryRow> = memories.iter().filter(|m| m.is_confirmed()).collect();
    ordered.sort_by(|left, right| selection_order(left, right));

    let mut used = 0usize;
    let mut selected = Vec::new();
    for memory in ordered {
        let cost = count_tokens(&memory.content);
        if used + cost > budget_tokens {
            continue;
        }
        used += cost;
        selected.push(memory);
    }
    selected
}

/// Selection order (see [`select_memories`]). `total_cmp` keeps the order
/// total even if a row somehow carries a NaN weight.
fn selection_order(left: &MemoryRow, right: &MemoryRow) -> std::cmp::Ordering {
    right
        .importance
        .total_cmp(&left.importance)
        .then_with(|| last_touch(right).cmp(&last_touch(left)))
        .then_with(|| numeric_id(&left.id).cmp(&numeric_id(&right.id)))
        // Ids are numeric today, but a non-numeric one must not make the
        // selection depend on row order.
        .then_with(|| left.id.cmp(&right.id))
}

/// The most recent of the three watermarks: a row is "fresh" when it was
/// rewritten, its citations were re-verified, or a model retrieved it.
fn last_touch(memory: &MemoryRow) -> i64 {
    memory
        .verified_at
        .max(memory.updated_at)
        .max(memory.last_used_at)
}

/// Sort key for a handle id: numeric ids order numerically, anything else
/// sorts after them.
pub(crate) fn numeric_id(id: &str) -> i64 {
    id.parse().unwrap_or(i64::MAX)
}

/// One active memory as the dreamer's gate reads it: the render's shape plus
/// the `updated_at` watermark the verify task compares file mtimes against
/// (the reference's `last_verified_at` stand-in — see
/// [`crate::mctx::dreamer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredMemory {
    /// `#N` handle body.
    pub id: String,
    /// One of [`CATEGORIES`](super::CATEGORIES).
    pub category: String,
    /// The fact text.
    pub content: String,
    /// Last write to the row, in epoch milliseconds.
    pub updated_at: i64,
    /// Last verification against the cited files, in epoch milliseconds; `0`
    /// means never verified.
    pub verified_at: i64,
}

/// [`active_memories`] with both watermarks attached (`updated_at` for the
/// write, `verified_at` for the dreamer's gate). Kept separate from the
/// pinboard read because the dreamer wants the watermark pair without the
/// selection weight: the two shapes differ, and neither grows a field its own
/// caller does not read.
pub(crate) async fn active_memories_with_watermark(
    db: &Db,
    project: &str,
) -> Result<Vec<StoredMemory>> {
    let context = format!("read active memories with updated_at for project {project}");
    let result: Result<Vec<StoredMemory>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content, updated_at, verified_at FROM mc_memory
                 WHERE project = ?1 AND status = 'active'
                 ORDER BY CAST(id AS INTEGER)",
            )
            .await?;
        let mut rows = stmt.query([project]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(StoredMemory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                updated_at: row.get(3)?,
                verified_at: row.get(4)?,
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// Store one memory's vector under `identity` (see [`crate::mctx::embed`]),
/// conditional on the text the vector came from.
///
/// The write is a compare-and-set on `content`: an embedding call can take up to
/// the endpoint's timeout, and an edit in that window rewrites the text and
/// clears the vector. An unconditional write would then stamp the old text's
/// vector onto the new text, and because the row would look covered nothing
/// would ever correct it — the semantic lane would keep ranking that memory by
/// text it no longer contains. Returns whether the row still carried `content`,
/// so a caller can tell a write it made from one a concurrent edit made stale.
///
/// The affected-row count is the value this driver's `execute` returns.
pub(super) async fn store_embedding(
    db: &Db,
    id: &str,
    content: &str,
    identity: &str,
    vector: &[f32],
) -> Result<bool> {
    let context = format!("store the embedding of memory #{id}");
    let blob = crate::mctx::embed::to_blob(vector);
    let result: Result<bool> = async {
        let conn = db.acquire().await?;
        let affected = conn
            .execute(
                "UPDATE mc_memory SET embedding = ?3, embedding_model = ?4
                  WHERE id = ?1 AND content = ?2",
                (id, content, blob, identity),
            )
            .await?;
        Ok(affected > 0)
    }
    .await;
    result.with_context(|| context)
}

/// The rows that still need a vector for a given identity: none stored yet, or
/// one produced under a different model/prefix.
///
/// One definition with two readers — [`memories_needing_embedding`]'s fetch and
/// the pass-end count — because `remaining` is the only coverage signal an
/// operator sees: a copy that drifted would report coverage the sweep never
/// measured. The identity is bound as `?1` by both readers, so this fragment
/// cannot be concatenated ahead of bind parameters in another order.
const EMBEDDING_PENDING: &str =
    "status = 'active' AND (embedding IS NULL OR embedding_model IS NOT ?1)";

/// Active memories of every project whose vector is missing or was produced
/// under another identity, newest first, up to `limit`.
///
/// Project-agnostic on purpose: this feeds [`embed_missing`], the background
/// that owns embedding coverage, and coverage is a property of the whole store
/// rather than of whichever project a search happened to touch. Newest first
/// because the freshest facts are the ones a search is most likely to need.
async fn memories_needing_embedding(
    db: &Db,
    identity: &str,
    limit: i64,
) -> Result<Vec<(String, String)>> {
    let result: Result<Vec<(String, String)>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, content FROM mc_memory
                  WHERE {EMBEDDING_PENDING}
                  ORDER BY CAST(id AS INTEGER) DESC
                  LIMIT ?2"
            ))
            .await?;
        let mut rows = stmt.query((identity, limit)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }
        Ok(out)
    }
    .await;
    result.with_context(|| "find memories to embed")
}

/// What one embedding sweep pass accomplished.
#[derive(Debug, Default)]
struct SweepReport {
    /// Rows that received a vector under the sweep's identity during the pass.
    embedded: usize,
    /// Rows whose embedding or store call failed; they stay pending.
    failed: usize,
    /// Batches the pass actually attempted, whether or not they succeeded.
    batches: usize,
    /// Rows still without a vector for this identity when the pass ended.
    remaining: i64,
}

/// Embed active memories that carry no vector for `embedder`'s identity, newest
/// first, in batches, at most `batch_cap` batches per pass.
///
/// Invariants: idempotent (a row already carrying this identity's vector is
/// never selected, so a repeated pass is a no-op), interruptible (each batch
/// stores its own rows, so a dropped pass leaves completed work behind and the
/// next pass resumes at the head), and bounded (at most `batch_cap` batches of
/// [`BATCH_SIZE`] rows, whatever the store's size).
///
/// Forward progress is guaranteed: a failed batch degrades to per-row attempts,
/// so one text the endpoint rejects no longer strands its batch-mates, and a
/// row that fails alone is skipped for the rest of the pass rather than pinning
/// the head. Every row the pass failed on is remembered, and the query window
/// grows by that count, so a failed row cannot hide the older rows behind it —
/// up to `batch_cap` x [`BATCH_SIZE`] rows, the most one pass can skip, which
/// the window (`BATCH_SIZE + |skipped|`) always covers. `failed` therefore
/// counts distinct rows, never attempts.
///
/// A batch of more than one row whose rows all fail their own single-input
/// attempt is not a batch of poison texts but an endpoint that is down: the pass
/// stops there and leaves the rest for the next one, so a dead endpoint costs
/// one batch of timeouts per pass instead of `batch_cap` of them. A batch of one
/// proves nothing about the endpoint, so it does not stop the pass.
///
/// `remaining` is measured after the pass rather than derived from it: the
/// count is the only thing that says how far behind coverage is, and a caller
/// that logs it can tell a caught-up store from one the cap held back.
#[must_use = "the report says how much of the store the pass covered"]
async fn embed_missing(
    db: &Db,
    embedder: &crate::mctx::embed::Embedder,
    batch_cap: usize,
) -> Result<SweepReport> {
    let identity = embedder.identity();
    let mut report = SweepReport::default();
    // Rows this pass already failed on. They stay pending in the store, so the
    // next query would hand them back as the newest rows and starve the rows
    // behind them; widening the window by their count keeps the batch full of
    // rows the pass has not tried yet.
    let mut skipped: Vec<String> = Vec::new();
    for _ in 0..batch_cap {
        let window = (BATCH_SIZE + skipped.len()) as i64;
        let pending: Vec<(String, String)> = memories_needing_embedding(db, identity, window)
            .await?
            .into_iter()
            .filter(|(id, _)| !skipped.contains(id))
            .take(BATCH_SIZE)
            .collect();
        if pending.is_empty() {
            break;
        }
        report.batches += 1;
        let texts: Vec<&str> = pending
            .iter()
            .map(|(_, content)| content.as_str())
            .collect();
        match embedder.embed_documents(&texts).await {
            Ok(vectors) => {
                for ((id, content), vector) in pending.iter().zip(vectors) {
                    store_swept_row(
                        db,
                        id,
                        content,
                        identity,
                        &vector,
                        &mut report,
                        &mut skipped,
                    )
                    .await;
                }
            }
            Err(error) => {
                tracing::warn!(
                    rows = pending.len(),
                    error = %error,
                    "embed memory batch failed; retrying its rows one at a time"
                );
                // One endpoint call rejects every text it was given, so the
                // batch is retried row by row: a text that is bad on its own
                // must not keep its batch-mates from getting their vectors.
                let mut rejected = 0;
                for (id, content) in &pending {
                    let vectors = match embedder.embed_documents(&[content.as_str()]).await {
                        Ok(vectors) => vectors,
                        Err(error) => {
                            tracing::debug!(memory = %id, error = %error, "embed memory row failed on its own");
                            fail_swept_row(id, &mut report, &mut skipped);
                            rejected += 1;
                            continue;
                        }
                    };
                    let Some(vector) = vectors.first() else {
                        tracing::debug!(
                            memory = %id,
                            "embeddings endpoint returned no vector for one input"
                        );
                        fail_swept_row(id, &mut report, &mut skipped);
                        rejected += 1;
                        continue;
                    };
                    store_swept_row(db, id, content, identity, vector, &mut report, &mut skipped)
                        .await;
                }
                if rejected == pending.len() && pending.len() > 1 {
                    // Not one text of the batch passed on its own, so the texts
                    // are not the problem: the endpoint is. Leave the rest of the
                    // queue for the next pass rather than paying `batch_cap`
                    // batches of timeouts against a dead endpoint. A batch of one
                    // is not evidence of that: one poison row has already been
                    // skipped for the pass and the cap still bounds the work.
                    tracing::warn!(
                        rows = pending.len(),
                        "every row of the batch failed alone; treating the endpoint as down and ending this pass"
                    );
                    break;
                }
            }
        }
    }
    let remaining: i64 = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT count(*) FROM mc_memory WHERE {EMBEDDING_PENDING}"
            ))
            .await?;
        let mut rows = stmt.query([identity]).await?;
        let count: i64 = match rows.next().await? {
            Some(row) => row.get(0)?,
            None => 0,
        };
        Ok::<i64, anyhow::Error>(count)
    }
    .await
    .context("count memories still needing an embedding")?;
    Ok(SweepReport {
        remaining,
        ..report
    })
}

/// Store one swept row's vector under `identity`. A store failure is logged,
/// counted and skipped: the row stays pending for the next pass, and the rest
/// of this pass moves on. A row whose text changed while the batch was being
/// embedded is neither: the compare-and-set declined the write, so the row is
/// already pending with its new text and a later pass picks it up.
async fn store_swept_row(
    db: &Db,
    id: &str,
    content: &str,
    identity: &str,
    vector: &[f32],
    report: &mut SweepReport,
    skipped: &mut Vec<String>,
) {
    match store_embedding(db, id, content, identity, vector).await {
        Ok(true) => report.embedded += 1,
        Ok(false) => {
            tracing::debug!(memory = %id, "memory changed under the sweep; leaving it pending");
        }
        Err(error) => {
            tracing::warn!(memory = %id, error = %error, "store swept embedding failed");
            fail_swept_row(id, report, skipped);
        }
    }
}

/// Count a row this pass failed on and keep the rest of the pass from retrying
/// it, so one bad row cannot pin the head of the queue.
fn fail_swept_row(id: &str, report: &mut SweepReport, skipped: &mut Vec<String>) {
    report.failed += 1;
    skipped.push(id.to_owned());
}

/// Batches one sweep pass may run: `SWEEP_BATCH_CAP` x [`BATCH_SIZE`] rows (640
/// at the sizes these constants carry), whatever the store's size.
const SWEEP_BATCH_CAP: usize = 20;
/// The gap between two sweep passes, measured from the end of the previous one
/// rather than on a fixed clock (see [`spawn_embedding_sweep`]).
const SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// Keep every active memory's embedding current for the configured embedder.
///
/// Coverage belongs to the store, not to query traffic: a search only reads
/// vectors, and the write path embeds new rows inline, so what is left is
/// whatever a failed inline embed or a model change left behind. This sweep is
/// that owner — newest rows first, a bounded number per pass, idempotent, and
/// moving past a failing row so one unreachable endpoint or one rejected text
/// cannot stall coverage. One pass runs at startup, then one every
/// [`SWEEP_INTERVAL`], counted from the end of the previous pass so a slow pass
/// never stacks passes behind it.
///
/// Started from the server's composer rather than from state construction:
/// tests must not acquire a background writer against their database (the same
/// reason the dreamer, the pruners and the retention sweepers are started
/// there).
#[must_use]
pub fn spawn_embedding_sweep(
    db: Db,
    embedder: Option<Arc<crate::mctx::embed::Embedder>>,
    stop: tokio_util::sync::CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let embedder = embedder?;
    Some(tokio::spawn(async move {
        loop {
            match embed_missing(&db, &embedder, SWEEP_BATCH_CAP).await {
                Ok(report) => log_sweep(report, SWEEP_BATCH_CAP),
                Err(error) => tracing::warn!(error = %error, "memory embedding sweep failed"),
            }
            // Sleep after the pass rather than ticking a fixed schedule:
            // `interval`'s default burst behaviour fires every missed tick back
            // to back, so an endpoint that hangs one pass would be hit again
            // immediately instead of once per interval.
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(SWEEP_INTERVAL) => {}
            }
        }
    }))
}

/// Log one sweep pass: nothing when the pass found no work (the common case on a
/// caught-up store), one `info` line when it did, plus a `warn` when the cap the
/// pass was given held coverage back.
fn log_sweep(report: SweepReport, batch_cap: usize) {
    if report.embedded + report.failed == 0 {
        return;
    }
    tracing::info!(
        embedded = report.embedded,
        failed = report.failed,
        remaining = report.remaining,
        "memory embedding sweep"
    );
    if report.batches == batch_cap && report.remaining > 0 {
        tracing::warn!(
            remaining = report.remaining,
            "memory embedding sweep used its whole batch cap; coverage is still behind"
        );
    }
}

/// One active memory with its stored vector, as `ctx_search`'s semantic lane
/// reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EmbeddedMemory {
    /// `#N` handle body.
    pub(crate) id: String,
    /// One of [`CATEGORIES`](super::CATEGORIES).
    pub(crate) category: String,
    /// The fact text.
    pub(crate) content: String,
    /// Creation time (ms), for the search's date filter.
    pub(crate) created_at: i64,
    /// The stored vector, decoded.
    pub(crate) vector: Vec<f32>,
}

/// The project's active memories with the vectors stored under `identity`.
/// Rows embedded under another identity are omitted (they rank as a mismatch
/// anyway, and the sweep re-embeds them).
pub(crate) async fn embedded_memories(
    db: &Db,
    project: &str,
    identity: &str,
) -> Result<Vec<EmbeddedMemory>> {
    let context = format!("load embedded memories for project {project}");
    let result: Result<Vec<EmbeddedMemory>> = async {
        let conn = db.acquire().await?;
        // The inverse of `EMBEDDING_PENDING`: a row is visible to the semantic
        // lane exactly when the sweep would not pick it up.
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content, created_at, embedding FROM mc_memory
                  WHERE project = ?1 AND status = 'active' AND embedding_model = ?2
                    AND embedding IS NOT NULL",
            )
            .await?;
        let mut rows = stmt.query((project, identity)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let blob: Vec<u8> = row.get(4)?;
            out.push(EmbeddedMemory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                created_at: row.get(3)?,
                vector: crate::mctx::embed::from_blob(&blob),
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// Owners with active memories, ascending. Only pool-hygiene tasks use this
/// enumeration; note and retrospective owners come from their own inputs.
pub(crate) async fn projects_with_active_memories(db: &Db) -> Result<Vec<String>> {
    let result: Result<Vec<String>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT project FROM mc_memory WHERE status = 'active' ORDER BY project",
            )
            .await?;
        let mut rows = stmt.query(()).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }
    .await;
    result.context("enumerate projects with active memories")
}

/// Stamp the "last checked against the cited files" watermark of the given
/// active memories: `verified_at = at`, nothing else about the row.
///
/// `at` is the START of the run that produced the verdict, not the moment the
/// verdict is applied: a file changed while the verification was in flight must
/// stay newer than the watermark, or that change would only be noticed after
/// the file changed again. `updated_at` is untouched — a verification is not a
/// write, and conflating them made an edited memory look freshly verified.
///
/// A memory rewritten while the verdict was in flight has `updated_at > at`
/// (and `verified_at` already cleared to 0 by the rewrite); it is not stamped,
/// because the verdict never saw its new text. The count excludes such rows.
pub(crate) async fn touch_memories(
    db: &Db,
    project: &str,
    ids: &[String],
    at: i64,
) -> Result<usize> {
    let context = format!("verify memories for project {project}");
    let result: Result<usize> = async {
        let tx = db.begin().await?;
        let mut touched = 0;
        {
            let mut stmt = tx
                .prepare(
                    "UPDATE mc_memory SET verified_at = ?3
                      WHERE project = ?1 AND id = ?2 AND status = 'active'
                        AND updated_at <= ?3",
                )
                .await?;
            for id in ids {
                touched += stmt.execute((project, id.as_str(), at)).await? as usize;
            }
        }
        tx.commit().await?;
        Ok(touched)
    }
    .await;
    result.with_context(|| context)
}

/// Stamp `last_used_at = at` on the active memories a model just retrieved
/// (`ctx_search` hits, `ctx_memory get` rows). Archived and unknown ids are
/// ignored. `updated_at` and `verified_at` are untouched: a retrieval is
/// neither a write nor a verification, and must not move the verify
/// watermark. No DB access when `ids` is empty.
pub(crate) async fn record_use(db: &Db, project: &str, ids: &[&str], at: i64) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let context = format!("record memory use for project {project}");
    let (own, user) = owner_match(project);
    let result: Result<()> = async {
        let tx = db.begin().await?;
        {
            let mut stmt = tx
                .prepare(
                    "UPDATE mc_memory SET last_used_at = ?3
                      WHERE id = ?2 AND project IN (?1, ?4) AND status = 'active'",
                )
                .await?;
            for &id in ids {
                stmt.execute((own, id, at, user)).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    result.with_context(|| context)
}

/// The cut-offs of one expiry pass (see `dreamer::expire`). Instants, not
/// ages: the policy is decided once at the task's `now`, and the store only
/// compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExpirePolicy<'a> {
    /// An unconfirmed historian row whose every timestamp is older than this
    /// is archived.
    pub unconfirmed_before: i64,
    /// A confirmed historian row whose every timestamp is older than this is
    /// archived.
    pub unused_before: i64,
    /// The `archive_reason` to stamp.
    pub reason: &'a str,
}

/// Archive `owner`'s historian-sourced rows that have aged past `policy`.
/// Returns how many rows were archived. Rows with
/// [`MemorySource::Manual`] are never touched.
///
/// "Touched" is the latest of `updated_at` (a write or a sighting bump),
/// `verified_at` and `last_used_at` (a retrieval or a citation); a row is
/// expired only when all three are older than its cut-off.
pub(crate) async fn expire_memories(
    db: &Db,
    owner: &str,
    policy: &ExpirePolicy<'_>,
    now: i64,
) -> Result<usize> {
    let context = format!("expire memories for {owner}");
    let result: Result<usize> = async {
        let conn = db.acquire().await?;
        let affected = conn
            .execute(
                "UPDATE mc_memory
                    SET status = 'archived', archive_reason = ?2, updated_at = ?3
                  WHERE project = ?1 AND status = 'active' AND source = ?4
                    AND MAX(updated_at, verified_at, last_used_at)
                        < CASE WHEN seen_count >= ?5 THEN ?6 ELSE ?7 END",
                (
                    owner,
                    policy.reason,
                    now,
                    MemorySource::HISTORIAN,
                    PROMOTED_CONFIRMATION_SIGHTINGS,
                    policy.unused_before,
                    policy.unconfirmed_before,
                ),
            )
            .await?;
        Ok(affected as usize)
    }
    .await;
    result.with_context(|| context)
}
