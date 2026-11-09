//! Project memories (`ctx_memory`) and session notes (`ctx_note`): storage,
//! the two tools, and the store reads the pinboard render uses.
//!
//! # Two kinds of durable text
//!
//! - **Memories** are project-scoped facts that stay true for months: a
//!   project rule, an architectural fact, a hard-won constraint, a config
//!   value, a naming convention. Every active memory is rendered into the
//!   `<project-memory>` block of the request head ([`crate::mctx::render`]),
//!   so it reaches every future session of the project without being asked
//!   for.
//! - **Notes** are session-scoped pending intentions: work to return to,
//!   with its findings attached. They are pull-based (the model asks with
//!   `ctx_note read`) — with one exception: a note parked on a
//!   `surface_condition` is surfaced into the session's pinboard by the
//!   dreamer once the condition holds (see the deviation below).
//!
//! # Owners: project and user
//!
//! `mc_memory.project` is the row's **owner key**, not only a directory. A
//! project memory's owner is the canonical working directory; a memory about
//! the user themself — how they like to work, what they know, what they keep
//! correcting — is owned by [`USER_SCOPE`] and rendered into every session of
//! every project (`<user-profile>`, see [`crate::mctx::render`]). The owner is
//! decided by the category ([`owner_for`]): [`USER_PROFILE`] rows are user
//! memories, every category in [`CATEGORIES`] is a project memory. Handles are
//! global, so an id-addressed operation from a project may reach that
//! project's rows *and* the user's ([`owner_match`]) — never another
//! project's.
//!
//! # Handles and lifecycles
//!
//! Ids are small ascending integers (`#17`) from a single sequence per table,
//! minted by `MAX(id) + 1` inside the writing transaction (the reference
//! addresses memories and notes as `#N`). A handle is a unique address; the
//! owning scope (project or user for memories, session for notes) is an
//! ordinary column, not part of the handle. The column is `text`; ordering
//! casts to `INTEGER`.
//!
//! Memories: `active` → `archived` (retirable, still `get`-able; the
//! optional archive reason is kept). Notes: `active`, `pending` (a smart
//! note parked on a `surface_condition`), `ready` (the condition holds; it is
//! in the pinboard) and `dismissed`.
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **A condition is checked by a periodic sweep, and its verdict is rendered
//!   in the pinboard.** The reference compiles a note's condition into a check
//!   (`evaluate-n-notes`), surfaces it at natural boundaries, and keeps ready
//!   notes out of m[1] on purpose (`m1_compose.rs`: "Ready smart notes are
//!   deliberately absent from m1 … the host's deferred-notes reminder, then
//!   `ctx_note read`"). ox has one trigger form — the dreamer's
//!   `surface_notes` task, on the dreamer interval — and its verdict is a
//!   status flip to `ready`, which the session's next pinboard rebuild renders
//!   as a `<ready-notes>` line ([`crate::mctx::render`]). Nothing is appended
//!   to the live transcript, so a surfacing never writes a prompt mid-turn; it
//!   costs the same one-turn `Soft` prefix shift a memory write does. `read`
//!   still orders newest first for every filter: the reference's "ready first"
//!   page order is not ported, because handle order already puts a
//!   just-surfaced note near the front (ids ascend with write time).
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
//! - **Memory text is redacted before it is stored.** Every store write
//!   ([`write_memory`], [`update_memory`], [`merge_memories`],
//!   [`insert_promoted`]) passes the content through
//!   [`crate::mctx::redact::redact_secrets`]; rows stored earlier are not
//!   backfilled.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::mctx::embed::BATCH_SIZE;
use crate::mctx::redact;
use crate::runner::{Tool, ToolContext, ToolOutput};
use ox_db::{Db, WriteTx, optional};
use ox_schema::id::now_millis;
use ox_schema::wire::ToolDef;
use ox_tools::{object_param, ok_text};

/// The closed project category set, in pinboard render order.
pub(crate) const CATEGORIES: [&str; 5] = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
];

/// The owner key of memories about the user (see the module docs). Not a path
/// — `@` cannot open a canonical directory — so it can never collide with a
/// project.
pub const USER_SCOPE: &str = "@user";
/// The one user-owned category: universal observations about how the user
/// works, independent of any project (the historian's `<user_observations>`).
pub(crate) const USER_PROFILE: &str = "USER_PROFILE";
/// The user-owned category set.
pub(crate) const USER_CATEGORIES: [&str; 1] = [USER_PROFILE];

/// Whether `owner` is the user namespace rather than a project directory.
#[must_use]
pub(crate) fn is_user_scope(owner: &str) -> bool {
    owner == USER_SCOPE
}

/// Whether `category` is one a memory may carry, in either owner.
#[must_use]
pub(crate) fn is_known_category(category: &str) -> bool {
    CATEGORIES.contains(&category) || USER_CATEGORIES.contains(&category)
}

/// The owner a memory of `category` written from `project` belongs to: the
/// user namespace for [`USER_CATEGORIES`], the project otherwise.
#[must_use]
pub(crate) fn owner_for<'a>(project: &'a str, category: &str) -> &'a str {
    if USER_CATEGORIES.contains(&category) {
        USER_SCOPE
    } else {
        project
    }
}

/// The categories `owner` admits.
#[must_use]
pub(crate) fn categories_of(owner: &str) -> &'static [&'static str] {
    if is_user_scope(owner) {
        &USER_CATEGORIES
    } else {
        &CATEGORIES
    }
}

/// The id-addressed reach of a caller working in `project`: its own rows and
/// the user's. Bound as two parameters, never interpolated.
fn owner_match(project: &str) -> (&str, &'static str) {
    (project, USER_SCOPE)
}

/// Where a memory row came from (the `source` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// Written on purpose: `ctx_memory`, the dreamer's retrospective, a merge.
    Manual,
    /// Promoted from a historian fold's side channel, unconfirmed by a human.
    Historian,
}

impl MemorySource {
    /// The column spelling, as the historian's promotion writes it.
    pub(crate) const HISTORIAN: &'static str = "historian";

    fn parse(value: &str) -> Self {
        if value == Self::HISTORIAN {
            Self::Historian
        } else {
            Self::Manual
        }
    }
}

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
/// Most surfaced notes one session's pinboard carries. A surfaced note is an
/// intention to return to, and the pinboard pays for it on every request, so
/// the list is bounded; the oldest handles win, because they have waited
/// longest.
const READY_NOTE_LIMIT: usize = 10;

// ---------------------------------------------------------------------------
// Store: memories
// ---------------------------------------------------------------------------

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
    /// One of [`CATEGORIES`].
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
    /// One of [`CATEGORIES`].
    pub category: String,
    /// The fact text.
    pub content: String,
    /// `active` or `archived`.
    pub status: String,
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
    /// One of [`CATEGORIES`].
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
        for id in losers {
            tx.execute(
                "UPDATE mc_memory SET status = 'archived', archive_reason = 'merged',
                     updated_at = ?3
                 WHERE project = ?1 AND id = ?2",
                (project, id.as_str(), now),
            )
            .await?;
        }
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
                "SELECT id, category, content, status FROM mc_memory
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
    /// One of [`CATEGORIES`].
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
async fn store_embedding(
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
    /// One of [`CATEGORIES`].
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
    /// Surfaced smart notes (`ready`): the ones the dreamer's `surface_notes`
    /// task judged in.
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
/// a silent no-op). Shared with [`crate::mctx::search`], which reports the
/// same way.
pub(crate) fn internal_error(what: &str, error: &anyhow::Error) -> ToolOutput {
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
    /// `None` when semantic search is off: writes then skip embedding.
    embedder: Option<Arc<crate::mctx::embed::Embedder>>,
}

impl CtxMemoryTool {
    /// The tool over `db` (the `mc_memory` table).
    #[must_use]
    pub fn new(db: Db, embedder: Option<Arc<crate::mctx::embed::Embedder>>) -> Self {
        Self { db, embedder }
    }
}

impl Tool for CtxMemoryTool {
    fn name(&self) -> &str {
        "ctx_memory"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(super::CTX_GUIDANCE)
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
             (\"do X later, here is what we know\") is ctx_note, not memory. A fact about \
             the user rather than the project — how they like to work, what they keep \
             correcting — is category USER_PROFILE: it follows them into every project \
             (<user-profile>), so never put project specifics there.",
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
                            "enum": tool_categories(),
                            "description": "Kind of fact (required for write; optional on \
                                            update/merge to recategorize within the same \
                                            owner: a project fact stays a project fact, \
                                            a USER_PROFILE memory stays one).",
                        }),
                    ),
                    (
                        "ids",
                        json!({
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Memory ids from a `#id: fact` line or a \
                                            `<project-memory>` / `<user-profile>` entry.",
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
        Box::pin(async move { run_memory(&self.db, self.embedder.as_deref(), &ctx, &input).await })
    }
}

async fn run_memory(
    db: &Db,
    embedder: Option<&crate::mctx::embed::Embedder>,
    ctx: &ToolContext,
    input: &Value,
) -> ToolOutput {
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
            let content = redact::redact_secrets(&content);
            match write_memory(db, &project, &content, &category).await {
                Ok(id) => {
                    embed_inline(db, embedder, &id, &content).await;
                    ok_text(format!(
                        "Wrote memory #{id}{}.",
                        redaction_note(matches!(content, Cow::Owned(_)))
                    ))
                }
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
            let content = redact::redact_secrets(&content);
            match update_memory(db, &project, id, &content, category.as_deref()).await {
                Ok(true) => {
                    embed_inline(db, embedder, id, &content).await;
                    ok_text(format!(
                        "Updated memory #{id}{}.",
                        redaction_note(matches!(content, Cow::Owned(_)))
                    ))
                }
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
            let content = redact::redact_secrets(&content);
            match merge_memories(db, &project, &ids, &content, category.as_deref()).await {
                Ok(Some(merged)) => {
                    embed_inline(db, embedder, &merged.survivor, &content).await;
                    ok_text(format!(
                        "Merged {} into #{}{}.",
                        handles(&merged.sources),
                        merged.survivor,
                        redaction_note(matches!(content, Cow::Owned(_)))
                    ))
                }
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
                    // Usage is a by-product: a failed write must not fail the read.
                    let used: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
                    if let Err(error) = record_use(db, &project, &used, now_millis()).await {
                        tracing::warn!(error = %error, "memory get: record use failed");
                    }
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

/// The success-message suffix telling the model its text was redacted, so it
/// does not retry a write whose secret came back as `[REDACTED_SECRET]`.
/// `redacted` is whether [`redact::redact_secrets`] returned `Cow::Owned`.
fn redaction_note(redacted: bool) -> &'static str {
    if redacted { " (secrets redacted)" } else { "" }
}

/// Embed a just-written fact and store its vector.
///
/// Best effort: a failed call logs and leaves the row without a vector, which
/// the background embedding sweep ([`embed_missing`]) picks up. A write must
/// never fail because the embedding endpoint is down.
async fn embed_inline(
    db: &Db,
    embedder: Option<&crate::mctx::embed::Embedder>,
    id: &str,
    content: &str,
) {
    let Some(embedder) = embedder else {
        return;
    };
    match embedder.embed_documents(&[content]).await {
        Ok(mut vectors) => {
            if let Some(vector) = vectors.pop()
                && let Err(error) =
                    store_embedding(db, id, content, embedder.identity(), &vector).await
            {
                tracing::warn!(memory = %id, error = %error, "store memory embedding failed");
            }
        }
        Err(error) => {
            tracing::warn!(memory = %id, error = %error, "embed memory failed; the embedding sweep backfills it");
        }
    }
}

/// Every category the tool accepts: the project set, then the user set.
fn tool_categories() -> Vec<&'static str> {
    CATEGORIES.iter().chain(&USER_CATEGORIES).copied().collect()
}

/// One category check for every arm: the closed sets live in [`CATEGORIES`]
/// and [`USER_CATEGORIES`], and the rejection message is written once.
fn check_category(category: &str) -> Result<(), String> {
    if is_known_category(category) {
        Ok(())
    } else {
        Err(format!(
            "Error: 'category' must be one of {}.",
            tool_categories().join(", ")
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
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Tool for CtxNoteTool {
    fn name(&self) -> &str {
        "ctx_note"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(super::CTX_GUIDANCE)
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
                                            newest first; rows \
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
                            "description": "A condition that must hold before the note is \
                                            worth surfacing. The note is parked as pending and \
                                            a background task re-checks the condition against \
                                            this project's recent user turns; once it holds, \
                                            the note becomes ready and appears in the session \
                                            pinboard without being asked for. Omit it for an \
                                            ordinary note you read yourself.",
                        }),
                    ),
                    (
                        "filter",
                        json!({
                            "type": "string",
                            "enum": ["all", "active", "pending", "ready", "dismissed"],
                            "description": "Which notes read returns (default: active, parked \
                                            and surfaced smart notes).",
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
    let result: Result<(i64, Option<i64>)> = async {
        let conn = db.acquire().await?;
        // An aggregate without GROUP BY always yields exactly one row.
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(*), MIN(COALESCE(updated_at, created_at)) FROM mc_note
                 WHERE session_id = ?1 AND status = 'active'",
            )
            .await?;
        let row = stmt.query_row([session_id]).await?;
        Ok((row.get(0)?, row.get(1)?))
    }
    .await;
    let (count, oldest) = result.context("active note summary")?;
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
