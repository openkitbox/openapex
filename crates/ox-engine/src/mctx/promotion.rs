//! Historian facts → project memory: the promotion pass that turns a run's
//! `<facts>` side channel into `mc_memory` writes.
//!
//! The decision ([`plan_promotion`]) is pure: it compares each fact against the
//! project's stored memories under a normalized key and emits a write action.
//! [`apply`] is the thin edge that reads the rows and executes the actions
//! inside the historian's publish transaction, so compartments and promoted
//! memories commit or roll back together. Every `mc_memory` statement lives in
//! [`crate::mctx::memory`]; this module only orchestrates its primitives.
//!
//! Reference: magic-context `memory/promotion.ts::promoteSessionFactsDurable`
//! (`:49-94`) and `storage-memory.ts::getMemoryByHash` (`:347-356`).
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **D1 — no stored hash.** The reference stores an md5 `normalized_hash`
//!   column under `UNIQUE(project_path, category, normalized_hash)`. ox stores
//!   none and normalizes in memory on every pass ([`normalize_fact`]): a
//!   derived column would drift from `content` across the memory tool's
//!   UPDATEs, and a project holds few memories.
//! - **D2 — no TTL.** The reference sets `expires_at` from a per-category TTL
//!   and expires rows in the dreamer. Only its legacy categories carry a TTL,
//!   and ox has none of them, so the path is not ported.
//! - **D3 — archived rows count as hits (same as the reference).** The
//!   reference's hash lookup has no status filter; ox matches `active` and
//!   `archived` rows alike. A hit on an archived row only bumps it: a fact the
//!   user retired is not re-inserted, and reviving it stays the user's call.
//! - **D4 — no embedding.** The reference embeds new rows asynchronously after
//!   commit. ox computes none here, matching `memory::write_memory`; the
//!   historian transaction stays short.
//! - **D5 — the five v2 categories only.** The reference's
//!   `PROMOTABLE_CATEGORIES` also admits legacy categories. ox accepts
//!   [`crate::mctx::memory::CATEGORIES`] only, and the historian parser has
//!   already dropped every other category, so this pass does not re-check.
//!
//! The historian's `<user_observations>` take the same path into the user
//! tier: [`user_candidates`] shapes them as [`FactCandidate`]s of
//! [`crate::mctx::memory::USER_PROFILE`], and [`apply`] runs against
//! [`crate::mctx::memory::USER_SCOPE`] instead of the project. One dedup, one
//! `seen_count`, one confirmation rule for both tiers.

use std::collections::HashSet;

use anyhow::{Context as _, Result};

use crate::mctx::historian_validate::FactCandidate;
use crate::mctx::memory;
use crate::mctx::redact;
use ox_db::WriteTx;

/// One stored memory as the dedup comparison reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExistingMemory<'a> {
    /// `#N` handle body.
    pub id: &'a str,
    /// One of the memory categories.
    pub category: &'a str,
    /// The stored fact text.
    pub content: &'a str,
}

/// One write the promotion pass makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromotionAction<'a> {
    /// The fact is already stored: count the sighting.
    Bump {
        /// The stored memory's id.
        id: &'a str,
    },
    /// The fact is new: insert it.
    Insert {
        /// The fact's category.
        category: &'a str,
        /// The fact text, verbatim.
        content: &'a str,
    },
}

/// What one promotion pass wrote, for the run log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PromotionSummary {
    /// New memories inserted.
    pub inserted: usize,
    /// Existing memories whose `seen_count` was bumped.
    pub bumped: usize,
}

/// The dedup form of a fact: lowercased, whitespace runs collapsed to one
/// space, trimmed — the reference's `normalize-hash.ts` steps before its md5.
#[must_use]
pub(crate) fn normalize_fact(content: &str) -> String {
    let lower = content.to_lowercase();
    let mut normalized = String::with_capacity(lower.len());
    for word in lower.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    normalized
}

/// Decide the write for each fact, in order.
///
/// A fact whose `(category, normalized content)` matches a stored memory bumps
/// that memory; otherwise it is inserted. Facts that normalize to the same key
/// within one batch produce one action: the first decides, the rest are
/// dropped. Blank facts are skipped, as in the reference.
///
/// Stored rows hold redacted text, so each fact is keyed on its redacted
/// form; facts that differ only in a secret share one key.
#[must_use]
pub(crate) fn plan_promotion<'a>(
    facts: &'a [FactCandidate],
    existing: &[ExistingMemory<'a>],
) -> Vec<PromotionAction<'a>> {
    let stored: Vec<(&str, String, &'a str)> = existing
        .iter()
        .map(|memory| (memory.category, normalize_fact(memory.content), memory.id))
        .collect();
    let mut seen: HashSet<(&'a str, String)> = HashSet::new();
    let mut actions = Vec::new();
    for fact in facts {
        let key = normalize_fact(&redact::redact_secrets(&fact.content));
        if key.is_empty() {
            continue;
        }
        let category = fact.category.as_str();
        let hit = stored
            .iter()
            .find(|(stored_category, stored_key, _)| {
                *stored_category == category && *stored_key == key
            })
            .map(|(_, _, id)| *id);
        if !seen.insert((category, key)) {
            continue;
        }
        actions.push(match hit {
            Some(id) => PromotionAction::Bump { id },
            None => PromotionAction::Insert {
                category,
                // Raw text is fine here: `insert_promoted` redacts on store.
                content: &fact.content,
            },
        });
    }
    actions
}

/// The historian's user observations as promotion candidates for the user
/// tier. Observations carry no compartment anchor (they are about the user,
/// not a stretch of transcript). Consumes the strings: the observations are
/// not read again after promotion.
#[must_use]
pub(crate) fn user_candidates(observations: Vec<String>) -> Vec<FactCandidate> {
    observations
        .into_iter()
        .map(|content| FactCandidate {
            category: memory::USER_PROFILE.to_owned(),
            content,
            origin_compartment_index: None,
        })
        .collect()
}

/// Promote `facts` into `owner`'s memories inside the caller's transaction,
/// where `owner` is a project or [`memory::USER_SCOPE`].
///
/// Reads the owner's active and archived memories, plans the writes with
/// [`plan_promotion`] and executes them. A storage failure is returned so the
/// caller's publish rolls back as a whole.
pub(crate) async fn apply(
    tx: &WriteTx<'_>,
    project: &str,
    facts: &[FactCandidate],
    now: i64,
) -> Result<PromotionSummary> {
    let result: Result<PromotionSummary> = async {
        let rows = memory::promotion_rows(tx, project).await?;
        let existing: Vec<ExistingMemory<'_>> = rows
            .iter()
            .map(|row| ExistingMemory {
                id: &row.id,
                category: &row.category,
                content: &row.content,
            })
            .collect();
        let actions = plan_promotion(facts, &existing);

        let mut summary = PromotionSummary::default();
        // Ids come from `memory::next_memory_id` (`MAX(id) + 1`); a batch
        // counts up from one read.
        let mut next_id: Option<i64> = None;
        for action in actions {
            match action {
                PromotionAction::Bump { id } => {
                    memory::bump_seen_count(tx, id, now).await?;
                    summary.bumped += 1;
                }
                PromotionAction::Insert { category, content } => {
                    let id = match next_id {
                        Some(id) => id,
                        None => memory::next_memory_id(tx).await?,
                    };
                    next_id = Some(id + 1);
                    let id = id.to_string();
                    memory::insert_promoted(tx, &id, project, category, content, now).await?;
                    summary.inserted += 1;
                }
            }
        }
        Ok(summary)
    }
    .await;
    result.with_context(|| format!("promote historian facts for project {project}"))
}
