//! The per-session transform pass: load the durable state, plan the head
//! bytes, classify the action, step the cache kernel, and write the result
//! back.
//!
//! This is the ox-side orchestration the reference splits across its TS
//! coordinator and Rust module. It owns the three things the kernel cannot
//! know:
//!
//! - **What the head is made of** — m[0] (baseline: the compartment history
//!   at the session's decay budget) and m[1] (delta: the memory pinboard,
//!   surfaced notes, and compartments published after the baseline),
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
//! ## Frozen baseline and new compartments
//!
//! New compartments ride m[1] while the baseline remains frozen. A HARD bust
//! incorporates all covered compartments into m[0] so decay sees the complete
//! history. The current calibration is recorded only on a HARD bust; calibration
//! changes alone do not invalidate the existing baseline, unless its current
//! token count exceeds the history budget.
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

use super::cache_core::{
    CompactionIdentity, CoreState, DurabilityClass, FrozenUnit, M0RenderIdentity,
};
use anyhow::{Context as _, Result};

use crate::mctx::budget::{Budget, memory_budget, user_profile_budget};
use crate::mctx::cache::{
    HeadRenderer, McCache, McCacheError, PassOutcome, RenderedPass, Signals, run_snapshot,
};
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::memory::{
    MemoryRow, ReadyNote, active_memories, ready_notes, select_memories, user_profile,
};
use crate::mctx::render::{
    M0Inputs, M1Parts, MemoryIndex, Shown, assemble_m1, render_m0, render_memory_block,
    render_memory_index, render_new_compartments, render_ready_notes_block,
    render_user_profile_block,
};
use crate::mctx::tagger::{mark_drops_applied, peek_queued_drops};
use crate::mctx::tokens;
use ox_db::{Db, optional};
use ox_schema::id::now_millis;

/// What one pass produced for the request builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassResult {
    /// The baseline half of the head (`m[0]`), or `None` when there is no
    /// usable head. It is injected as its own message: the stable baseline is
    /// the provider's cache anchor,
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
    /// The session's measured provider ÷ local token ratio
    /// ([`crate::mctx::calibration`]): what every local count below is scaled by
    /// before it meets a budget. `1.0` before the session has a measurement.
    pub calibration: f64,
    /// Provider/model identity for the frozen history render.
    pub provider_id: &'a str,
    pub model_id: &'a str,
}

/// Run one transform pass for a session.
pub async fn run(request: PassRequest<'_>) -> Result<PassResult> {
    execute(request, Execution::Commit)
        .await
        .map(|(result, _)| result)
}

/// Preview the same pass without changing cache, floor or drop lifecycle.
pub(crate) async fn preview(request: PassRequest<'_>) -> Result<(PassResult, Vec<u64>)> {
    execute(request, Execution::Preview).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Execution {
    Commit,
    Preview,
}

async fn execute(
    mut request: PassRequest<'_>,
    execution: Execution,
) -> Result<(PassResult, Vec<u64>)> {
    let first = execute_once(request.clone(), execution).await;
    match first {
        Err(error)
            if execution == Execution::Commit
                && matches!(
                    error.downcast_ref::<McCacheError>(),
                    Some(McCacheError::CasConflict { .. })
                ) =>
        {
            let conn = request.db.acquire().await?;
            let mut stmt = conn
                .prepare("SELECT COALESCE(MAX(seq), 0) FROM session_message WHERE session_id = ?1")
                .await?;
            let mut rows = stmt.query([request.session_id]).await?;
            request.last_seq = match rows.next().await? {
                Some(row) => row.get(0)?,
                None => 0,
            };
            drop(rows);
            drop(stmt);
            drop(conn);
            execute_once(request, execution).await
        }
        result => result,
    }
}

async fn execute_once(
    request: PassRequest<'_>,
    execution: Execution,
) -> Result<(PassResult, Vec<u64>)> {
    let PassRequest {
        db,
        session_id,
        project,
        last_seq,
        budget,
        forced,
        run_started,
        calibration,
        provider_id,
        model_id,
    } = request;
    // One counter resolution per pass: the session's measured ratio scales every
    // local count into the budget's units for every walk below.
    let count_tokens = crate::mctx::tokens::counter_fn(calibration);

    let cache = McCache::new(db.clone(), session_id);
    let mut snapshot = cache.load_snapshot().await?;
    let compartments = compartments::load(db, session_id).await?;
    // A session with memory switched off reads none: both tiers stay empty
    // (and are not even loaded), so the head carries no memory at all.
    let memory_on = crate::mctx::memory_mode::mode_of(db, session_id)
        .await?
        .is_on();
    let memories = if memory_on {
        active_memories(db, project).await?
    } else {
        Vec::new()
    };
    // The user's own tier rides beside the pinboard: same half, same bust
    // class, its own (smaller) budget.
    let profile = if memory_on {
        user_profile(db).await?
    } else {
        Vec::new()
    };
    // Surfaced smart notes ride the same volatile half as the pinboard: a
    // note that becomes `ready` is a `Soft` bust, never a rebuild of m[0].
    let notes = ready_notes(db, session_id).await?;
    let state = &snapshot.state;
    let stored_baseline_coverage = read_kv_int(
        db,
        &baseline_coverage_key(session_id),
        "mctx baseline coverage",
    )
    .await?;
    let latest_usage = latest_assistant_usage(db, session_id).await?;
    let completed_compaction = latest_completed_compaction(db, session_id).await?;
    let checkpoint_changed = completed_compaction.is_some()
        && state
            .as_ref()
            .and_then(|state| state.completed_compaction.as_ref())
            != completed_compaction.as_ref();

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
    let baseline_coverage = state
        .as_ref()
        .and_then(|state| state.baseline_coverage)
        .or_else(|| (stored_baseline_coverage > 0).then_some(stored_baseline_coverage))
        .or_else(|| {
            state
                .as_ref()
                .and_then(|s| parse_baseline_coverage(&s.boundary_id))
        })
        .unwrap_or(0);
    let boundary_ordinal = state
        .as_ref()
        .and_then(|state| parse_baseline_coverage(&state.boundary_id));
    let live_boundary = live_boundary_id(db, session_id, coverage).await?;
    let previous_coverage = state
        .as_ref()
        .and_then(|s| s.materialized_floor)
        .or_else(|| (stored_baseline_coverage > 0).then_some(stored_baseline_coverage))
        .unwrap_or(baseline_coverage);
    // A SOFT pass may move the anchor forward, but only from the exact live
    // boundary it previously froze. Comparing the full boundary id matters:
    // an edit can replace the message at the same ordinal, and ordinal-only
    // matching would incorrectly preserve the old m[0].
    let previous_boundary = live_boundary_id(db, session_id, previous_coverage).await?;
    let forward_boundary = coverage >= previous_coverage
        && baseline_coverage <= coverage
        && boundary_ordinal.is_some_and(|ordinal| ordinal == previous_coverage)
        && state
            .as_ref()
            .is_some_and(|state| state.boundary_id == previous_boundary);
    let boundary_live = match boundary_ordinal {
        Some(ordinal) if ordinal > 0 => boundary_exists(db, session_id, ordinal).await?,
        _ => true,
    };
    let stale = claimed > last_seq
        || baseline_coverage > last_seq
        || !boundary_live
        || state
            .as_ref()
            .is_some_and(|state| !forward_boundary && state.boundary_id != live_boundary);
    let current_identity = M0RenderIdentity::new(
        provider_id,
        model_id,
        budget.window(),
        budget.history,
        calibration,
    );
    let mut plan = plan_units_with_m0_identity(
        &compartments,
        M1Sources {
            memories: &memories,
            profile: &profile,
            notes: &notes,
        },
        coverage,
        budget.history,
        &count_tokens,
        baseline_coverage,
        None,
    );

    // Queued drops ride the planned units, not the kernel's defer queue: our
    // durable `mc_drop` rows are the carrier ("queued, not immediate" — the
    // placeholder materializes when a bust freezes the prefix, and until then
    // the frozen bytes replay verbatim).
    let drops = peek_queued_drops(db, session_id).await?;

    let frozen_m1 = frozen_half(state.as_ref(), "m1");
    // A hard fold must let decay see the whole currently covered history. The
    // first plan is intentionally baseline-only so a new compartment can be
    // classified as a delta; only a real baseline change promotes it into m0.
    let identity_changed = state.as_ref().is_some_and(|state| {
        m0_identity_changed_without_calibration(state.m0_identity.as_ref(), &current_identity)
    });
    let legacy_identity = state
        .as_ref()
        .is_some_and(|state| state.m0_identity.is_none());
    let missing_m0 = state
        .as_ref()
        .is_some_and(|state| frozen_half(Some(state), "m0").is_none());
    let reconcile_pending = state.as_ref().is_some_and(|state| state.reconcile_pending);
    let pressure = requires_hard_rebuild(
        &plan.m1,
        frozen_half(state.as_ref(), "m0").unwrap_or(""),
        &budget,
        &count_tokens,
        latest_usage.and_then(|tokens| budget.usage_percentage(tokens)),
        has_foldable_compartments(&compartments, baseline_coverage, coverage),
    );
    let calibration_pressure = state.as_ref().is_some_and(|state| {
        state.m0_identity.as_ref().is_some_and(|identity| {
            identity.calibration_bits != current_identity.calibration_bits
                && budget.history > 0
                && frozen_half(Some(state), "m0")
                    .is_some_and(|m0| count_tokens(m0) > budget.history as usize)
        })
    });
    let hard_rebuild = state.is_none()
        || calibration_pressure
        || forced
        || checkpoint_changed
        || stale
        || reconcile_pending
        || !drops.is_empty()
        || identity_changed
        || legacy_identity
        || missing_m0
        || pressure;
    if hard_rebuild {
        plan = plan_units_with_m0_identity(
            &compartments,
            M1Sources {
                memories: &memories,
                profile: &profile,
                notes: &notes,
            },
            coverage,
            budget.history,
            &count_tokens,
            coverage,
            Some(&current_identity),
        );
    }
    let boundary_present = if forward_boundary {
        state.as_ref().map(|state| state.boundary_id.clone())
    } else {
        (!stale).then_some(live_boundary.clone())
    };
    let signals = Signals {
        boundary_present,
        forced,
        run_started,
        // A queued drop makes the head itself stale (its placeholder has to
        // appear beside the frozen history), and it must be a bust: a defer
        // would replay the frozen bytes and the mark would never take effect.
        // The signal converges — the bust applies the drops, so the next pass
        // sees an empty queue.
        head_changed: hard_rebuild || forced || stale || !drops.is_empty() || identity_changed,
        delta_changed: Some(plan.m1.as_str()) != frozen_m1,
        completed_compaction,
    };

    // SOFT updates only m1; the core retains the existing m0 unit in place.
    // A defer does not submit units at all.
    let Plan { m0, m1 } = plan;
    let mut units = Vec::new();
    if hard_rebuild {
        units.push(FrozenUnit {
            key: "m0".to_string(),
            kind: "synthesized-region".to_string(),
            frozen_payload: m0,
            durability_class: DurabilityClass::Lineage,
            reset_rule: String::new(),
        });
    }
    if hard_rebuild || signals.delta_changed {
        units.push(FrozenUnit {
            key: "m1".to_string(),
            kind: "synthesized-region".to_string(),
            frozen_payload: m1,
            durability_class: DurabilityClass::Lineage,
            reset_rule: String::new(),
        });
    }
    units.extend(drop_units(&drops));
    let mut renderer = PrecomputedRenderer {
        units,
        boundary_id: live_boundary,
        m0_identity: hard_rebuild.then(|| current_identity.clone()),
        materialized_floor: Some(coverage),
        baseline_coverage: hard_rebuild.then_some(coverage),
    };
    let state_is_initial = state.is_none();
    let frozen_floor = state
        .as_ref()
        .and_then(|state| state.materialized_floor)
        .or_else(|| (stored_baseline_coverage > 0).then_some(stored_baseline_coverage));

    let outcome = match execution {
        Execution::Commit => run_snapshot(&cache, snapshot, &mut renderer, &signals).await?,
        Execution::Preview => preview_outcome(snapshot.state.take(), &signals, &mut renderer),
    };
    if execution == Execution::Commit {
        tracing::debug!(
            session = %session_id,
            action = if hard_rebuild { "hard" } else if signals.delta_changed { "soft" } else { "defer" },
            initial = state_is_initial,
            forced,
            reconcile = stale || reconcile_pending,
            pressure,
            calibration_pressure,
            identity_changed,
            legacy_identity,
            missing_m0,
            queued_drop = !drops.is_empty(),
            delta_changed = signals.delta_changed,
            delta_source = delta_diagnostic(signals.delta_changed, coverage > previous_coverage),
            coverage_advanced = coverage > previous_coverage,
            "mctx cache pass"
        );
    }
    let mut applied = Vec::new();
    // The prefix is taken by value (it can be hundreds of KB; the outcome
    // rebuilt below carries no bytes).
    let (prefix, floor, outcome) = match outcome {
        PassOutcome::DeferReplayed { prefix } => {
            // A defer replays the frozen bytes: the floor must be the one the
            // frozen head was materialized with, not this pass's coverage.
            let floor = match frozen_floor {
                Some(floor) => floor,
                None => materialized_floor(db, session_id).await?,
            };
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
            applied = drops.iter().map(|(tag, _)| *tag).collect();
            if execution == Execution::Commit {
                if !applied.is_empty() {
                    mark_drops_applied(db, session_id, &applied).await?;
                }
                save_materialized_floor(db, session_id, coverage).await?;
                if hard_rebuild {
                    save_baseline_coverage(db, session_id, coverage).await?;
                }
            }
            (
                prefix,
                coverage,
                PassOutcome::BustApplied {
                    prefix: String::new(),
                },
            )
        }
    };
    // A served floor above the live tail would hide every live row from the
    // model. The pass re-anchors on the next bust; until then the tail wins.
    let floor = floor.min(last_seq);

    // The prefix is the two halves in order; `split_prefix` recovers them (both
    // are non-empty by construction — the baseline always carries the history
    // block, the delta always carries its marker).
    let result = if prefix.is_empty() {
        PassResult {
            head_m0: None,
            head_m1: None,
            floor,
            outcome,
        }
    } else {
        let (head_m0, head_m1) = split_owned_prefix(prefix);
        PassResult {
            head_m0: Some(head_m0),
            head_m1: Some(head_m1),
            floor,
            outcome,
        }
    };

    Ok((result, applied))
}

async fn latest_completed_compaction(
    db: &Db,
    session_id: &str,
) -> Result<Option<CompactionIdentity>> {
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(
        "SELECT id, seq FROM session_message WHERE session_id = ?1 AND type = 'compaction' AND json_extract(data, '$.status') = 'completed' ORDER BY seq DESC, id DESC LIMIT 1"
    ).await?;
    let row = optional(&mut stmt, [session_id]).await?;
    row.map(|row| {
        Ok(CompactionIdentity {
            id: row.get(0)?,
            seq: row.get(1)?,
        })
    })
    .transpose()
}

/// The head halves as planned for one pass.
struct Plan {
    m0: String,
    m1: String,
}

/// The loaded rows m[1] is rendered from, borrowed for one pass.
#[derive(Clone, Copy)]
struct M1Sources<'a> {
    /// The project's active memories.
    memories: &'a [MemoryRow],
    /// The user's active profile memories.
    profile: &'a [MemoryRow],
    /// The session's surfaced notes.
    notes: &'a [ReadyNote],
}

fn plan_units_with_m0_identity(
    compartments: &[Compartment],
    sources: M1Sources<'_>,
    coverage: i64,
    history_budget_tokens: u32,
    count_tokens: &dyn Fn(&str) -> usize,
    baseline_coverage: i64,
    m0_identity: Option<&M0RenderIdentity>,
) -> Plan {
    let new_compartments: Vec<_> = compartments
        .iter()
        .filter(|c| {
            c.end_ordinal()
                .is_some_and(|end| end > baseline_coverage && end <= coverage)
        })
        .map(Compartment::render_input)
        .collect();

    let m0 = if let Some(m0_identity) = m0_identity {
        let baseline: Vec<_> = compartments
            .iter()
            .filter(|c| {
                c.end_ordinal()
                    .is_some_and(|end| end <= coverage && end <= baseline_coverage)
            })
            .map(Compartment::render_input)
            .collect();

        render_m0(&M0Inputs {
            compartments: &baseline,
            history_budget_tokens: f64::from(m0_identity.history_budget),
            count_tokens: &tokens::counter_fn(m0_identity.calibration()),
        })
    } else {
        String::new()
    };
    // The pinboard rides the volatile half: a memory write must not rebuild the
    // provider's cache anchor. It is also the one section that grows with the
    // project's lifetime, so it is selected under its own budget first — a
    // memory that does not fit is skipped, the rest of the list still renders.
    let history = f64::from(history_budget_tokens);
    let selected = select_memories(sources.memories, memory_budget(history), count_tokens);
    let profile = select_memories(sources.profile, user_profile_budget(history), count_tokens);
    // The index counts what the model cannot see from the blocks alone: the
    // memories the budget (or the confirmation rule) left out, and the folded
    // history `ctx_search` can reach.
    let index = MemoryIndex {
        project: Shown {
            active: sources.memories.len(),
            shown: selected.len(),
        },
        profile: Shown {
            active: sources.profile.len(),
            shown: profile.len(),
        },
        compartments: compartments.len(),
        compartment_span: compartment_span(compartments),
        notes: sources.notes.len(),
    };
    let m1_body = assemble_m1(M1Parts {
        memories: &render_memory_block(&selected),
        user_profile: &render_user_profile_block(&profile),
        index: &render_memory_index(&index),
        notes: &render_ready_notes_block(sources.notes),
        new_compartments: &render_new_compartments(&new_compartments),
    });
    Plan {
        m0,
        // The unit payload carries the blank line that separates it from m[0]:
        // the kernel concatenates frozen payloads verbatim.
        m1: format!("\n\n{m1_body}"),
    }
}

/// `created_at` of the oldest and newest compartment, `None` when there are
/// none.
fn compartment_span(compartments: &[Compartment]) -> Option<(i64, i64)> {
    let oldest = compartments.iter().map(|c| c.created_at).min()?;
    let newest = compartments.iter().map(|c| c.created_at).max()?;
    Some((oldest, newest))
}

/// The boundary id for a given m[0] coverage ordinal.
fn anchor_id(coverage: i64) -> String {
    format!("mc:{coverage}")
}

/// Extract the baseline coverage from the durable cache boundary.
fn parse_baseline_coverage(boundary_id: &str) -> Option<i64> {
    boundary_id
        .strip_prefix("mc:")?
        .split(':')
        .next()?
        .parse()
        .ok()
}

fn has_foldable_compartments(compartments: &[Compartment], baseline: i64, coverage: i64) -> bool {
    compartments.iter().any(|compartment| {
        compartment
            .end_ordinal()
            .is_some_and(|end| end > baseline && end <= coverage)
    })
}

fn requires_hard_rebuild(
    m1: &str,
    m0: &str,
    budget: &Budget,
    count_tokens: &dyn Fn(&str) -> usize,
    usage_pct: Option<f64>,
    has_foldable_delta: bool,
) -> bool {
    // Reserve known fixed layers and tail against usable window space. This is
    // not admission: final request assembly measures the complete payload.
    let m1_budget = budget.conservative_m1_budget(count_tokens(m0));
    has_foldable_delta
        && (usage_pct.is_some_and(|usage| usage >= budget.hard_pct) || count_tokens(m1) > m1_budget)
}

fn m0_identity_changed_without_calibration(
    frozen: Option<&M0RenderIdentity>,
    current: &M0RenderIdentity,
) -> bool {
    frozen.is_some_and(|identity| {
        identity.provider_id != current.provider_id
            || identity.model_id != current.model_id
            || identity.window != current.window
            || identity.history_budget != current.history_budget
            || identity.render_version != current.render_version
    })
}

/// The frozen `(m0, m1)` payload halves, if the state carries them.
fn frozen_half<'state>(state: Option<&'state CoreState>, key: &str) -> Option<&'state str> {
    let state = state?;
    state
        .frozen_units
        .iter()
        .find(|unit| unit.key == key)
        .map(|unit| unit.frozen_payload.as_str())
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

fn baseline_coverage_key(session_id: &str) -> String {
    format!("mctx_baseline_seq:{session_id}")
}

/// The transcript floor to pair with a materialized head.
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

async fn save_baseline_coverage(db: &Db, session_id: &str, coverage: i64) -> Result<()> {
    write_kv_int(
        db,
        &baseline_coverage_key(session_id),
        coverage,
        "mctx baseline coverage",
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

async fn latest_assistant_usage(db: &Db, session_id: &str) -> Result<Option<i64>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT data FROM session_message
             WHERE session_id = ?1 AND type = 'assistant'
             ORDER BY seq DESC LIMIT 1",
        )
        .await
        .context("load latest assistant usage")?;
    let row = optional(&mut stmt, [session_id])
        .await
        .context("load latest assistant usage")?;
    let Some(row) = row else {
        return Ok(None);
    };
    #[derive(serde::Deserialize)]
    struct UsageRow {
        tokens: Option<serde_json::Value>,
    }
    let data: String = row.get(0)?;
    let data: UsageRow = serde_json::from_str(&data).context("decode latest assistant usage")?;
    let Some(tokens) = data.tokens else {
        return Ok(None);
    };
    let Some(input) = tokens.get("input").and_then(serde_json::Value::as_i64) else {
        return Ok(None);
    };
    let cached = tokens
        .get("cache")
        .and_then(|cache| cache.get("read"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Ok(Some(input.saturating_add(cached)))
}

async fn boundary_exists(db: &Db, session_id: &str, ordinal: i64) -> Result<bool> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM session_message
             WHERE session_id = ?1 AND seq = ?2 LIMIT 1",
        )
        .await
        .context("check mctx boundary")?;
    Ok(optional(&mut stmt, (session_id, ordinal))
        .await
        .context("check mctx boundary")?
        .is_some())
}

async fn live_boundary_id(db: &Db, session_id: &str, coverage: i64) -> Result<String> {
    if coverage <= 0 {
        return Ok(anchor_id(0));
    }
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT id FROM session_message WHERE session_id = ?1 AND seq = ?2")
        .await
        .context("load mctx boundary identity")?;
    let row = optional(&mut stmt, (session_id, coverage))
        .await
        .context("load mctx boundary identity")?;
    Ok(row
        .map(|row| row.get::<String>(0).map(|id| format!("mc:{coverage}:{id}")))
        .transpose()?
        .unwrap_or_else(|| anchor_id(coverage)))
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
    // m[1] opens with a memory tier when there is one (the pinboard, else the
    // user profile) and with the since-marker otherwise; the separator (a
    // blank line) belongs to neither.
    const M1_OPENERS: [&str; 5] = [
        "\n\n<project-memory>",
        "\n\n<user-profile>",
        "\n\n<memory-index>",
        "\n\n<new-compartments>",
        "\n\n<session-history-since>",
    ];
    let start = M1_OPENERS
        .iter()
        .filter_map(|opener| prefix.find(opener))
        .min();
    match start {
        Some(index) => (prefix[..index].trim_end_matches('\n'), &prefix[index + 2..]),
        None => (prefix.trim_end(), ""),
    }
}

fn split_owned_prefix(mut prefix: String) -> (String, String) {
    let (m0_len, m1_offset) = {
        let (m0, m1) = split_prefix(&prefix);
        (m0.len(), prefix.len() - m1.len())
    };
    let m1 = prefix.split_off(m1_offset);
    prefix.truncate(m0_len);
    (prefix, m1)
}

fn preview_outcome(
    loaded: Option<CoreState>,
    signals: &Signals,
    renderer: &mut dyn HeadRenderer,
) -> PassOutcome {
    let action = crate::mctx::cache::classify(loaded.as_ref(), signals);
    let rendered = if action == crate::mctx::cache_core::Action::SoftPlus {
        RenderedPass::default()
    } else {
        renderer.render(loaded.as_ref(), signals)
    };
    crate::mctx::cache::transition(loaded, action, rendered, signals).1
}

/// Hands the kernel the units planned for this pass. Determinism is structural
/// (the bytes were planned before the step, from the same inputs the plan
/// compared against), which is what the CAS-retry contract needs.
struct PrecomputedRenderer {
    units: Vec<FrozenUnit>,
    boundary_id: String,
    m0_identity: Option<M0RenderIdentity>,
    materialized_floor: Option<i64>,
    baseline_coverage: Option<i64>,
}

impl HeadRenderer for PrecomputedRenderer {
    fn render(&mut self, _state: Option<&CoreState>, _signals: &Signals) -> RenderedPass {
        RenderedPass {
            rendered_units: std::mem::take(&mut self.units),
            queued: Vec::new(),
            new_boundary_id: Some(self.boundary_id.clone()),
            new_m0_identity: self.m0_identity.clone(),
            new_materialized_floor: self.materialized_floor,
            new_baseline_coverage: self.baseline_coverage,
        }
    }
}

fn delta_diagnostic(changed: bool, coverage_advanced: bool) -> &'static str {
    match (changed, coverage_advanced) {
        (false, _) => "none",
        (true, true) => "coverage_and_other_or_unknown",
        (true, false) => "other_or_unknown",
    }
}
