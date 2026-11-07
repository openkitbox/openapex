//! `expire`: time-decay archival of memories nobody has confirmed or used.
//!
//! The one dreamer task that calls no model. Its policy is a pair of ages
//! over columns the store already keeps, so a tick costs one `UPDATE`:
//!
//! - **Unconfirmed historian facts** ([`crate::mctx::memory::MemoryRow::is_confirmed`]
//!   false — one fold stated them once) that no later fold restated and no
//!   model retrieved within [`UNCONFIRMED_TTL`]. They never rode a request;
//!   archiving them is bookkeeping, not forgetting.
//! - **Confirmed historian facts** that nothing has touched — no write, no
//!   verification, no retrieval, no citation — for [`UNUSED_TTL`]. A fact a
//!   project went half a year without needing is a fact the historian can
//!   extract again if the project ever needs it.
//!
//! Rows written on purpose (`ctx_memory`, a retrospective, a merge) never
//! expire: a person or a task decided they matter, and only a person or a
//! verdict should retire them. Archiving is reversible (`archive_reason`
//! names this task), and archived rows still count as dedup hits for the
//! promotion pass, so an expired fact the historian sees again bumps the
//! archived row instead of resurrecting a duplicate.
//!
//! Reference: codex's `memories` prune — a 30-day usage window with the
//! usage counted from the model's own citations — reduced to ox's columns.

use anyhow::Result;

use crate::mctx::memory::{self, ExpirePolicy};
use ox_db::Db;

use super::manifest::ApplyReport;

/// How long an unconfirmed historian fact may wait for a second sighting or a
/// retrieval before it is archived: 30 days.
pub const UNCONFIRMED_TTL: i64 = 30 * DAY_MILLIS;
/// How long a confirmed historian fact may go untouched before it is
/// archived: 180 days.
pub const UNUSED_TTL: i64 = 180 * DAY_MILLIS;

const DAY_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// The archive reason stamped by this task, so an operator can tell an
/// expiry from a curate or verify verdict.
pub const EXPIRE_REASON: &str = "expired: unconfirmed or unused";

/// The policy at `now`: the cut-off instants the store compares against.
#[must_use]
pub(super) fn policy_at(now: i64) -> ExpirePolicy<'static> {
    ExpirePolicy {
        unconfirmed_before: now.saturating_sub(UNCONFIRMED_TTL),
        unused_before: now.saturating_sub(UNUSED_TTL),
        reason: EXPIRE_REASON,
    }
}

/// Run the task for `owner`: one store call, no model. The report counts the
/// rows archived.
pub(super) async fn run_expire(db: &Db, owner: &str, now: i64) -> Result<ApplyReport> {
    let archived = memory::expire_memories(db, owner, &policy_at(now), now).await?;
    Ok(ApplyReport {
        archived,
        ..ApplyReport::default()
    })
}
