//! The per-session transform pass: load the durable state, plan the head
//! bytes, classify the action, step the cache kernel, and write the result
//! back.
//!
//! This is the ox-side orchestration the reference splits across its TS
//! coordinator and Rust module. It owns the three things the kernel cannot
//! know:
//!
//! - **What the head is made of** — m[0] (baseline: the compartment history
//!   at the session's decay budget) and m[1] (delta: the memory pinboard),
//!   rendered by [`crate::mctx::render`]. The kernel treats both as opaque
//!   bytes.
//! - **Classification** — m[0] bytes changed ⇒ `Hard`; only m[1] changed ⇒
//!   `Soft`; nothing changed ⇒ `SoftPlus` defer (see [`Signals`]).
//! - **The coverage anchor and the floor** — the boundary id carries the
//!   *coverage* ordinal (`mc:<ordinal>`): the highest transcript ordinal the
//!   whole prefix (m[0] and m[1] together) represents. That same ordinal is the
//!   transcript floor for request assembly — everything at or below it is
//!   represented by the head and dropped from the tail.
//!
//! ## Why the baseline is rebuilt by every fold
//!
//! A fold changes the compartment set, so it changes the history block, so it
//! busts the cache. There is deliberately no "delta carries the new
//! compartments" half: an earlier ox kept one (a frozen baseline at its own
//! coverage plus a `<new-compartments>` delta), and the extra state was a
//! standing source of bugs — a session whose baseline froze while folds went to
//! the delta renders a *lossy* head (decay never sees the delta, so folds
//! accumulate at full fidelity and older, larger compartments are never
//! demoted), which is the opposite of what folding is for. One rule instead:
//! the head is a function of the current compartment set and the session's
//! history budget, and a fold busts.
//!
//! ## The drop contract, both halves
//!
//! This module produces the head half: a queued drop's `[dropped §N§]`
//! placeholder rides the head at the next bust, and the bust applies the drops.
//! The tail half is [`crate::mctx::tagger::apply_overlay`], which runs at
//! request assembly and replaces an applied drop's content in the live
//! transcript. A queued drop is itself a bust signal ([`Signals::head_changed`]),
//! so a stamp takes effect on the next pass and the signal then converges (the
//! bust empties the queue). A row below the floor is covered by the head alone;
//! a row above it is covered by the overlay — exactly one placeholder either
//! way.
//!
//! Freeze/replay discipline is NOT re-implemented here: on a defer the kernel
//! replays the frozen bytes verbatim and this module only plans (it never
//! re-derives what it will not freeze away).

use anyhow::{Context as _, Result};
use cortexkit_cache_core::{CoreState, DurabilityClass, FrozenUnit};

use crate::mctx::budget::{Budget, memory_budget};
use crate::mctx::cache::{HeadRenderer, McCache, PassOutcome, RenderedPass, Signals, run_pass};
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::memory::{MemoryRow, ReadyNote, active_memories, ready_notes, select_memories};
use crate::mctx::render::{
    M0Inputs, assemble_m1, render_m0, render_memory_block, render_ready_notes_block,
};
use crate::mctx::tagger::{mark_drops_applied, peek_queued_drops};
use ox_db::{Db, optional};
use ox_schema::id::now_millis;

/// What one pass produced for the request builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassResult {
    /// The baseline half of the head (`m[0]`), or `None` when there is no
    /// usable head (a panicked pass with no last-known-good). It is injected
    /// as its own message: the stable baseline is the provider's cache anchor,
    /// so it must stay byte-identical while the delta moves.
    pub head_m0: Option<String>,
    /// The delta half of the head (`m[1]`).
    pub head_m1: Option<String>,
    /// Highest transcript ordinal the head covers: rows at or below it are
    /// dropped from the request (the head represents them).
    pub floor: i64,
    /// What the pass did (for logging/tests).
    pub outcome: PassOutcome,
}

/// Inputs to one pass.
#[derive(Clone)]
pub struct PassRequest<'a> {
    /// Shared database.
    pub db: &'a Db,
    /// Owning session.
    pub session_id: &'a str,
    /// Project scope for the memory pinboard (the canonical directory).
    pub project: &'a str,
    /// Highest transcript ordinal currently live (boundary-presence check).
    pub last_seq: i64,
    /// The session's budgets, derived from the model catalogue at the step
    /// boundary (the history block is rendered at [`Budget::history`]).
    pub budget: Budget,
    /// An explicit refresh/compaction request forces a `Hard` rebuild.
    pub forced: bool,
    /// A new run/episode started.
    pub run_started: bool,
    /// The session's model id: which family's token counter the pass budgets
    /// its guards with (unknown families fall back to the heuristic).
    pub model_id: &'a str,
}

/// Run one transform pass for a session.
pub async fn run(request: PassRequest<'_>) -> Result<PassResult> {
    let PassRequest {
        db,
        session_id,
        project,
        last_seq,
        budget,
        forced,
        run_started,
        model_id,
    } = request;
    let count = crate::mctx::tokens::counter_for_model(model_id);
    let count_tokens = move |s: &str| count.count(s);

    let compartments = compartments::load(db, session_id).await?;
    let memories = active_memories(db, project).await?;
    // Surfaced smart notes ride the same volatile half as the pinboard: a
    // note that becomes `ready` is a `Soft` bust, never a rebuild of m[0].
    let notes = ready_notes(db, session_id).await?;
    let cache = McCache::new(db.clone(), session_id);
    let state = cache.load().await?;

    // The materialized coverage: the highest ordinal the head represents, and
    // therefore the transcript floor. It is the highest compartment end that is
    // still inside the live tail — *not* simply the newest compartment's end: a
    // revert can leave a compartment claiming rows that no longer exist, and
    // the head must stop at the live boundary instead of covering a range it
    // cannot render.
    let claimed = compartments::coverage_end(&compartments).unwrap_or(0);
    let coverage = compartments
        .iter()
        .filter_map(Compartment::end_ordinal)
        .filter(|end| *end <= last_seq)
        .max()
        .unwrap_or(0);
    // A revert removed rows the head already claims to represent: the stored
    // floor and the frozen bytes both describe a range that is gone, and the
    // new rows sit *below* the old floor, so they would be invisible to the
    // model. This is upstream's "reconcile" case: rebuild and re-anchor.
    let stale = claimed > last_seq;

    let plan = plan_units(
        &compartments,
        &memories,
        &notes,
        coverage,
        budget.history,
        &count_tokens,
    );

    // Queued drops ride the planned units, not the kernel's defer queue: our
    // durable `mc_drop` rows are the carrier ("queued, not immediate" — the
    // placeholder materializes when a bust freezes the prefix, and until then
    // the frozen bytes replay verbatim).
    let drops = peek_queued_drops(db, session_id).await?;

    let (frozen_m0, frozen_m1) = frozen_halves(state.as_ref());
    let signals = Signals {
        boundary_present: boundary_present(state.as_ref(), stale),
        forced,
        run_started,
        // A queued drop makes the head itself stale (its placeholder has to
        // appear beside the frozen history), and it must be a bust: a defer
        // would replay the frozen bytes and the mark would never take effect.
        // The signal converges — the bust applies the drops, so the next pass
        // sees an empty queue.
        head_changed: state.is_none()
            || forced
            || stale
            || !drops.is_empty()
            || plan.m0 != frozen_m0,
        delta_changed: plan.m1 != frozen_m1,
    };

    let mut units = plan.units();
    units.extend(drop_units(&drops));
    let renderer = PrecomputedRenderer {
        units,
        boundary_id: plan.boundary_id.clone(),
    };

    let outcome = run_pass(&cache, &renderer, &signals).await?;
    // The prefix is taken by value (it can be hundreds of KB; the outcome
    // rebuilt below carries no bytes).
    let (prefix, floor, outcome) = match outcome {
        PassOutcome::DeferReplayed { prefix } => {
            // A defer replays the frozen bytes: the floor must be the one the
            // frozen head was materialized with, not this pass's coverage.
            let floor = materialized_floor(db, session_id).await?;
            (
                prefix,
                floor,
                PassOutcome::DeferReplayed {
                    prefix: String::new(),
                },
            )
        }
        PassOutcome::BustApplied { prefix } => {
            // The bust froze these bytes, so the queued drops they carry are
            // now reflected in the prefix: mark them applied.
            let tags: Vec<u64> = drops.iter().map(|(tag, _)| *tag).collect();
            if !tags.is_empty() {
                mark_drops_applied(db, session_id, &tags).await?;
            }
            save_materialized_floor(db, session_id, coverage).await?;
            (
                prefix,
                coverage,
                PassOutcome::BustApplied {
                    prefix: String::new(),
                },
            )
        }
        PassOutcome::ModulePanicked => {
            // LKG: serve the last bytes the pass actually produced, with the
            // transcript floor that went with them (both are written by the
            // same bust, so they always agree). With no LKG there is nothing
            // coherent to inject, so the request falls back to the raw
            // transcript — the head is an optimization, never a correctness
            // boundary.
            let lkg = crate::mctx::cache::Lkg::new(db.clone())
                .load_lkg(session_id)
                .await
                .context("load LKG after a panicked pass")?;
            let floor = materialized_floor(db, session_id).await?;
            tracing::warn!(
                session = %session_id,
                has_lkg = lkg.is_some(),
                floor,
                "mctx pass panicked; serving last-known-good head"
            );
            (lkg.unwrap_or_default(), floor, PassOutcome::ModulePanicked)
        }
    };
    // A served floor above the live tail would hide every live row from the
    // model. The pass re-anchors on the next bust; until then the tail wins.
    let floor = floor.min(last_seq);

    // The prefix is the two halves in order; `split_prefix` recovers them (both
    // are non-empty by construction — the baseline always carries the history
    // block, the delta always carries its marker).
    let (head_m0, head_m1) = split_prefix(&prefix);
    let result = if prefix.is_empty() {
        PassResult {
            head_m0: None,
            head_m1: None,
            floor,
            outcome,
        }
    } else {
        PassResult {
            head_m0: Some(head_m0.to_string()),
            head_m1: Some(head_m1.to_string()),
            floor,
            outcome,
        }
    };

    // Record the head actually served as the session's last-known-good.
    if !prefix.is_empty() && !matches!(result.outcome, PassOutcome::ModulePanicked) {
        crate::mctx::cache::Lkg::new(db.clone())
            .save_lkg(session_id, &prefix)
            .await
            .context("save LKG")?;
    }
    Ok(result)
}

/// The head halves as planned for one pass.
struct Plan {
    m0: String,
    m1: String,
    boundary_id: String,
}

impl Plan {
    fn units(&self) -> Vec<FrozenUnit> {
        vec![
            FrozenUnit {
                key: "m0".to_string(),
                kind: "synthesized-region".to_string(),
                frozen_payload: self.m0.clone(),
                durability_class: DurabilityClass::Lineage,
                reset_rule: String::new(),
            },
            FrozenUnit {
                key: "m1".to_string(),
                kind: "synthesized-region".to_string(),
                frozen_payload: self.m1.clone(),
                durability_class: DurabilityClass::Lineage,
                reset_rule: String::new(),
            },
        ]
    }
}

/// Render the two head halves: m[0] over every compartment the head covers,
/// m[1] over the pinboard and the session's surfaced notes.
fn plan_units(
    compartments: &[Compartment],
    memories: &[MemoryRow],
    notes: &[ReadyNote],
    coverage: i64,
    history_budget_tokens: u32,
    count_tokens: &dyn Fn(&str) -> usize,
) -> Plan {
    let covered: Vec<_> = compartments
        .iter()
        .filter(|c| c.end_ordinal().is_some_and(|end| end <= coverage))
        .map(Compartment::render_input)
        .collect();

    let m0 = render_m0(&M0Inputs {
        compartments: &covered,
        history_budget_tokens: f64::from(history_budget_tokens),
        count_tokens,
    });
    // The pinboard rides the volatile half: a memory write must not rebuild the
    // provider's cache anchor. It is also the one section that grows with the
    // project's lifetime, so it is selected under its own budget first — a
    // memory that does not fit is skipped, the rest of the list still renders.
    let selected = select_memories(
        memories,
        memory_budget(f64::from(history_budget_tokens)),
        count_tokens,
    );
    let m1_body = assemble_m1(
        &render_memory_block(&selected),
        &render_ready_notes_block(notes),
    );
    Plan {
        m0,
        // The unit payload carries the blank line that separates it from m[0]:
        // the kernel concatenates frozen payloads verbatim.
        m1: format!("\n\n{m1_body}"),
        boundary_id: anchor_id(coverage),
    }
}

/// The boundary id for a given m[0] coverage ordinal.
fn anchor_id(coverage: i64) -> String {
    format!("mc:{coverage}")
}

/// The frozen `(m0, m1)` payload halves, if the state carries them.
fn frozen_halves(state: Option<&CoreState>) -> (String, String) {
    let Some(state) = state else {
        return (String::new(), String::new());
    };
    let find = |key: &str| {
        state
            .frozen_units
            .iter()
            .find(|unit| unit.key == key)
            .map(|unit| unit.frozen_payload.clone())
    };
    (
        find("m0").unwrap_or_default(),
        find("m1").unwrap_or_default(),
    )
}

/// Boundary-presence token: the anchor when the range it covers is still in
/// the live transcript, else the absent sentinel the kernel expects (which
/// makes it reconcile instead of replaying a head it cannot anchor).
///
/// `stale` is the pass's own verdict — the head claims coverage past the live
/// tail, i.e. a revert removed rows it represents — not a re-derivation here:
/// the coverage was already clamped against `last_seq`, so comparing them again
/// could never detect it.
fn boundary_present(state: Option<&CoreState>, stale: bool) -> Option<String> {
    let state = state?;
    (!stale).then(|| state.boundary_id.clone())
}

/// One queued drop's frozen unit: the placeholder the model sees in place of
/// the dropped content.
fn drop_units(drops: &[(u64, String)]) -> Vec<FrozenUnit> {
    let mut sorted: Vec<&(u64, String)> = drops.iter().collect();
    sorted.sort_by_key(|(tag, _)| *tag);
    sorted
        .into_iter()
        .map(|(tag, _reason)| FrozenUnit {
            key: format!("drop:{tag}"),
            kind: "drop".to_string(),
            frozen_payload: format!("\n[dropped §{tag}§]"),
            durability_class: DurabilityClass::Lineage,
            reset_rule: String::new(),
        })
        .collect()
}

fn materialized_key(session_id: &str) -> String {
    format!("mctx_seq:{session_id}")
}

/// The transcript floor to pair with a last-known-good head.
async fn materialized_floor(db: &Db, session_id: &str) -> Result<i64> {
    read_kv_int(db, &materialized_key(session_id), "mctx materialized floor").await
}

async fn save_materialized_floor(db: &Db, session_id: &str, coverage: i64) -> Result<()> {
    write_kv_int(
        db,
        &materialized_key(session_id),
        coverage,
        "mctx materialized floor",
    )
    .await
}

/// Read a small integer the pass keeps in the session's `kv` row.
async fn read_kv_int(db: &Db, key: &str, what: &str) -> Result<i64> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT value FROM kv WHERE key = ?1")
        .await
        .with_context(|| format!("load {what}"))?;
    let row = optional(&mut stmt, [key])
        .await
        .with_context(|| format!("load {what}"))?;
    let value = row
        .map(|row| row.get::<String>(0))
        .transpose()
        .with_context(|| format!("load {what}"))?;
    Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
}

async fn write_kv_int(db: &Db, key: &str, value: i64, what: &str) -> Result<()> {
    let value = value.to_string();
    let now = now_millis();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
        (key, value.as_str(), now),
    )
    .await
    .with_context(|| format!("save {what}"))?;
    Ok(())
}

/// Split the frozen prefix into its two halves at the delta's opening marker.
///
/// The marker is unambiguous: m[0] content is XML-escaped, so a literal
/// `<session-history-since>` can only appear as the real delta tag. The blank
/// line that separated the halves when they were concatenated is consumed at
/// the seam (the halves travel as their own messages now).
fn split_prefix(prefix: &str) -> (&str, &str) {
    // m[1] opens with the pinboard when there are memories and with the
    // since-marker otherwise; the separator (a blank line) belongs to neither.
    const M1_OPENERS: [&str; 2] = ["\n\n<project-memory>", "\n\n<session-history-since>"];
    let start = M1_OPENERS
        .iter()
        .filter_map(|opener| prefix.find(opener))
        .min();
    match start {
        Some(index) => (prefix[..index].trim_end_matches('\n'), &prefix[index + 2..]),
        None => (prefix.trim_end(), ""),
    }
}

/// Hands the kernel the units planned for this pass. Determinism is structural
/// (the bytes were planned before the step, from the same inputs the plan
/// compared against), which is what the CAS-retry contract needs.
struct PrecomputedRenderer {
    units: Vec<FrozenUnit>,
    boundary_id: String,
}

impl HeadRenderer for PrecomputedRenderer {
    fn render(&self, _state: Option<&CoreState>, _signals: &Signals) -> RenderedPass {
        RenderedPass {
            rendered_units: self.units.clone(),
            queued: Vec::new(),
            new_boundary_id: Some(self.boundary_id.clone()),
        }
    }
}
