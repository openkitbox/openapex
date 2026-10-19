//! The per-session transform pass: load the durable state, plan the head
//! bytes, classify the action, step the cache kernel, and write the result
//! back.
//!
//! This is the ox-side orchestration the reference splits across its TS
//! coordinator and Rust module. It owns the three things the kernel cannot
//! know:
//!
//! - **What the head is made of** — m[0] (baseline: compartment history +
//!   memory pinboard) and m[1] (delta: compartments folded in since the
//!   baseline), rendered by [`crate::mctx::render`]. The kernel treats both as
//!   opaque bytes.
//! - **Classification** — m[0] bytes changed ⇒ `Hard`; only m[1] changed ⇒
//!   `Soft`; nothing changed ⇒ `SoftPlus` defer (see [`Signals`]).
//! - **The coverage anchor and the floor** — the boundary id carries the
//!   *coverage* ordinal (`mc:<ordinal>`): the highest transcript ordinal the
//!   whole prefix (m[0] and m[1] together) represents. That same ordinal is the
//!   transcript floor for request assembly — everything at or below it is
//!   represented by the head and dropped from the tail.
//!
//! The baseline's own coverage is a *separate*, ox-owned fact (a session `kv`
//! row): a `Soft` folds new coverage into the delta while the baseline stays
//! frozen at the ordinal it was built at, so the two only coincide on a `Hard`.
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

use crate::db::{Db, optional};
use crate::id::now_millis;
use crate::mctx::cache::{HeadRenderer, McCache, PassOutcome, RenderedPass, Signals, run_pass};
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::memory::{MemoryRow, active_memories};
use crate::mctx::render::{
    DEFAULT_HISTORY_BUDGET_TOKENS, M1_PLACEHOLDER, assemble_m1, render_m0, render_new_compartments,
};
use crate::mctx::tagger::{mark_drops_applied, peek_queued_drops};

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
    /// An explicit refresh/compaction request forces a `Hard` rebuild.
    pub forced: bool,
    /// A new run/episode started.
    pub run_started: bool,
}

/// Run one transform pass for a session.
pub async fn run(request: PassRequest<'_>) -> Result<PassResult> {
    let PassRequest {
        db,
        session_id,
        project,
        last_seq,
        forced,
        run_started,
    } = request;

    let compartments = compartments::load(db, session_id).await?;
    let memories = active_memories(db, project).await?;
    let cache = McCache::new(db.clone(), session_id);
    let state = cache.load().await?;

    // The materialized coverage: the highest ordinal the head represents, and
    // therefore the transcript floor. Clamped to the live tail so a revert that
    // removed rows can never leave the head claiming coverage of rows that no
    // longer exist (which would strand a stale head under a live anchor).
    let coverage = compartments::coverage_end(&compartments)
        .unwrap_or(0)
        .min(last_seq);
    // The ordinal the frozen baseline (`m[0]`) covers. It is not the same thing
    // as the coverage: a `Soft` folds new coverage into the delta while the
    // baseline stays put. It is ox-owned state (the kernel's anchor is the
    // coverage token), so it lives in the session's kv row.
    let m0_coverage = materialized_m0_coverage(db, session_id).await?;
    let rebuild_m0 = state.is_none()
        || forced
        || baseline_changed(state.as_ref(), &compartments, &memories, m0_coverage);
    let m0_coverage_after = if rebuild_m0 { coverage } else { m0_coverage };

    let plan = plan_units(PlanInputs {
        compartments: &compartments,
        memories: &memories,
        m0_coverage: m0_coverage_after,
        coverage,
    });

    // Queued drops ride the planned units, not the kernel's defer queue: our
    // durable `mc_drop` rows are the carrier ("queued, not immediate" — the
    // placeholder materializes when a bust freezes the prefix, and until then
    // the frozen bytes replay verbatim).
    let drops = peek_queued_drops(db, session_id).await?;

    let (frozen_m0, frozen_m1) = frozen_halves(state.as_ref());
    let signals = Signals {
        boundary_present: boundary_present(state.as_ref(), coverage, last_seq),
        forced,
        run_started,
        // A queued drop makes the head itself stale (its placeholder has to
        // appear beside the frozen history), and it must be a bust: a defer
        // would replay the frozen bytes and the mark would never take effect.
        // The signal converges — the bust applies the drops, so the next pass
        // sees an empty queue.
        head_changed: state.is_none() || forced || !drops.is_empty() || plan.m0 != frozen_m0,
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
            save_materialized_m0_coverage(db, session_id, m0_coverage_after).await?;
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

struct PlanInputs<'a> {
    compartments: &'a [Compartment],
    memories: &'a [MemoryRow],
    /// The ordinal the baseline half covers.
    m0_coverage: i64,
    /// The ordinal the whole head covers (the floor). The kernel's boundary
    /// anchor is minted from THIS, not from the baseline's own coverage: a
    /// delta extends coverage past the baseline, and a revert of those rows
    /// must invalidate the head.
    coverage: i64,
}

/// Render the two head halves: m[0] over the compartments m[0] covers,
/// m[1] over the ones beyond it.
fn plan_units(inputs: PlanInputs<'_>) -> Plan {
    let m0_compartments: Vec<_> = inputs
        .compartments
        .iter()
        .filter(|c| c.end_ordinal().is_some_and(|end| end <= inputs.m0_coverage))
        .map(Compartment::render_input)
        .collect();
    let delta: Vec<_> = inputs
        .compartments
        .iter()
        .filter(|c| c.end_ordinal().is_some_and(|end| end > inputs.m0_coverage))
        .map(Compartment::render_input)
        .collect();
    let delta_refs: Vec<_> = delta.iter().collect();

    let m0 = render_m0(&crate::mctx::render::M0Inputs {
        compartments: &m0_compartments,
        memories: inputs.memories,
        history_budget_tokens: DEFAULT_HISTORY_BUDGET_TOKENS,
    });
    let new_compartments = render_new_compartments(&delta_refs);
    let m1_body = if new_compartments.is_empty() {
        M1_PLACEHOLDER.to_string()
    } else {
        assemble_m1(&new_compartments)
    };
    Plan {
        m0,
        // The unit payload carries the blank line that separates it from m[0]:
        // the kernel concatenates frozen payloads verbatim.
        m1: format!("\n\n{m1_body}"),
        boundary_id: anchor_id(inputs.coverage),
    }
}

/// Whether the frozen m[0] is stale against the rows it is a function of.
/// m[0] covers the compartments at or below `m0_coverage` plus the active
/// memories, so re-rendering it and comparing bytes answers "must the
/// baseline rebuild" without enumerating causes.
fn baseline_changed(
    state: Option<&CoreState>,
    compartments: &[Compartment],
    memories: &[MemoryRow],
    m0_coverage: i64,
) -> bool {
    let Some(state) = state else {
        return true;
    };
    let plan = plan_units(PlanInputs {
        compartments,
        memories,
        m0_coverage,
        // Only m[0] is compared here; the anchor is irrelevant to the diff.
        coverage: m0_coverage,
    });
    let (frozen_m0, _) = frozen_halves(Some(state));
    plan.m0 != frozen_m0
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

/// Boundary-presence token: the anchor when the range it covers (everything at
/// or below the coverage, i.e. the floor) is still in the live transcript, else
/// the absent sentinel the kernel expects.
///
/// The anchor token is the *coverage*, not the baseline's own coverage: the
/// prefix covers m[0] AND m[1], so a delta that extends coverage past the
/// baseline must still be invalidated by a revert of those rows.
fn boundary_present(state: Option<&CoreState>, coverage: i64, last_seq: i64) -> Option<String> {
    let state = state?;
    (coverage <= last_seq).then(|| state.boundary_id.clone())
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

fn materialized_m0_key(session_id: &str) -> String {
    format!("mctx_m0_cov:{session_id}")
}

/// The ordinal the frozen baseline (`m[0]`) covers; 0 before the first pass.
async fn materialized_m0_coverage(db: &Db, session_id: &str) -> Result<i64> {
    read_kv_int(db, &materialized_m0_key(session_id), "mctx m0 coverage").await
}

async fn save_materialized_m0_coverage(db: &Db, session_id: &str, coverage: i64) -> Result<()> {
    write_kv_int(
        db,
        &materialized_m0_key(session_id),
        coverage,
        "mctx m0 coverage",
    )
    .await
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
    const M1_OPEN: &str = "<session-history-since>";
    match prefix.find(M1_OPEN) {
        Some(index) => (prefix[..index].trim_end_matches('\n'), &prefix[index..]),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mctx::compartments::{NewCompartment, Tiers, insert};
    use crate::mctx::memory::write_memory;
    use crate::mctx::tagger::{QueueOutcome, Tagger, queue_drop_outcome};
    use serde_json::json;

    const SESSION: &str = "ses_pass";
    const PROJECT: &str = "/proj/pass";

    async fn db() -> Db {
        let db = Db::open_in_memory().await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    fn tiers(start: i64, end: i64, title: &str) -> Tiers {
        Tiers {
            title: title.to_string(),
            start_message: start,
            end_message: end,
            p1: Some(format!("{title} full")),
            p2: Some(title.to_string()),
            ..Default::default()
        }
    }

    async fn add_compartment(db: &Db, id: &str, start: i64, end: i64, title: &str) {
        let tiers = tiers(start, end, title);
        insert(
            db,
            &NewCompartment {
                id,
                session_id: SESSION,
                importance: 50.0,
                tiers: &tiers,
            },
        )
        .await
        .unwrap();
    }

    fn request(db: &Db, last_seq: i64) -> PassRequest<'_> {
        PassRequest {
            db,
            session_id: SESSION,
            project: PROJECT,
            last_seq,
            forced: false,
            run_started: false,
        }
    }

    #[tokio::test]
    async fn first_pass_materializes_everything_into_m0() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        write_memory(&db, PROJECT, "spine", "ARCHITECTURE")
            .await
            .unwrap();

        let result = run(request(&db, 9)).await.unwrap();
        assert!(matches!(result.outcome, PassOutcome::BustApplied { .. }));
        assert_eq!(result.floor, 9);
        let m0 = result.head_m0.unwrap();
        let m1 = result.head_m1.unwrap();
        assert!(m0.contains("<session-history>"), "m0 history: {m0}");
        assert!(
            m0.contains("## 1-9 · Alpha\nAlpha full"),
            "compartment: {m0}"
        );
        assert!(m0.contains("<project-memory>"), "pinboard: {m0}");
        assert!(
            !m0.contains("<session-history-since>"),
            "m0 is baseline only"
        );
        assert_eq!(m1, M1_PLACEHOLDER, "m1 is the placeholder");
    }

    #[tokio::test]
    async fn second_identical_pass_is_a_byte_stable_defer() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        let first = run(request(&db, 9)).await.unwrap();
        let second = run(request(&db, 9)).await.unwrap();

        assert!(matches!(second.outcome, PassOutcome::DeferReplayed { .. }));
        assert_eq!(
            first.head_m0, second.head_m0,
            "defer replays the same bytes"
        );
        assert_eq!(first.head_m1, second.head_m1, "both halves stay frozen");
    }

    #[tokio::test]
    async fn a_new_compartment_rides_the_delta_as_a_soft_bust() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        let first = run(request(&db, 9)).await.unwrap();
        assert!(matches!(first.outcome, PassOutcome::BustApplied { .. }));

        add_compartment(&db, "mc_2", 10, 20, "Beta").await;
        let second = run(request(&db, 20)).await.unwrap();

        assert!(
            matches!(second.outcome, PassOutcome::BustApplied { .. }),
            "new coverage is a bust: {:?}",
            second.outcome
        );
        // The baseline half is untouched — that is the whole point of a Soft: the
        // provider's cached anchor survives while the delta moves.
        assert_eq!(second.head_m0, first.head_m0, "m0 stays frozen");
        let delta = second.head_m1.unwrap();
        assert!(delta.contains("## 10-20 · Beta"), "delta body: {delta}");
        assert!(
            delta.contains("<new-compartments>"),
            "delta wrapper: {delta}"
        );
        assert_eq!(second.floor, 20, "the floor advances with the delta");
        // The kernel anchor is the HEAD's coverage, not the baseline's: a revert
        // of the rows the delta covers must invalidate the head.
        let state = McCache::new(db.clone(), SESSION).load().await.unwrap();
        assert_eq!(
            state.expect("state after the delta").boundary_id,
            anchor_id(20),
            "the anchor advances with the delta"
        );
        // ... and the baseline's own coverage stays where it was frozen.
        assert_eq!(materialized_m0_coverage(&db, SESSION).await.unwrap(), 9);
    }

    #[tokio::test]
    async fn a_memory_write_rebuilds_the_baseline() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        let first = run(request(&db, 9)).await.unwrap();
        assert!(!first.head_m0.unwrap().contains("<project-memory>"));

        write_memory(&db, PROJECT, "new fact", "NAMING")
            .await
            .unwrap();
        let second = run(request(&db, 9)).await.unwrap();
        assert!(matches!(second.outcome, PassOutcome::BustApplied { .. }));
        assert!(
            second.head_m0.unwrap().contains("#1: new fact"),
            "memory lands in m0"
        );
        assert_eq!(
            second.head_m1.unwrap(),
            M1_PLACEHOLDER,
            "the delta resets on a Hard"
        );
    }

    #[tokio::test]
    async fn forced_forces_a_rebuild() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        run(request(&db, 9)).await.unwrap();

        let mut forced = request(&db, 9);
        forced.forced = true;
        let result = run(forced).await.unwrap();
        assert!(matches!(result.outcome, PassOutcome::BustApplied { .. }));
    }

    #[tokio::test]
    async fn a_queued_drop_materializes_on_the_next_pass_and_is_marked_applied() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        run(request(&db, 9)).await.unwrap();

        // Drops address tags minted over a real transcript pass.
        let messages = vec![json!({
            "id": "msg_1",
            "type": "user",
            "time": { "created": 1 },
            "text": "hello world",
        })];
        let tagged = Tagger.assign(&db, SESSION, &messages).await.unwrap();
        let tag = tagged[0].tag;
        assert!(matches!(
            queue_drop_outcome(&db, SESSION, tag, "used").await.unwrap(),
            QueueOutcome::Queued
        ));

        // A queued drop makes the head itself stale, so the next pass busts and
        // the placeholder lands beside the frozen history in one go.
        let busted = run(request(&db, 9)).await.unwrap();
        assert!(matches!(busted.outcome, PassOutcome::BustApplied { .. }));
        let head = format!("{}{}", busted.head_m0.unwrap(), busted.head_m1.unwrap());
        assert!(
            head.contains(&format!("[dropped §{tag}§]")),
            "placeholder in head: {head}"
        );
        assert!(
            peek_queued_drops(&db, SESSION).await.unwrap().is_empty(),
            "the bust consumed the queue"
        );

        // ... and the signal converges: with an empty queue the next pass is a
        // plain defer again, so a stamp never keeps the prefix churning.
        let deferred = run(request(&db, 9)).await.unwrap();
        assert!(matches!(
            deferred.outcome,
            PassOutcome::DeferReplayed { .. }
        ));
    }

    #[tokio::test]
    async fn a_revert_beyond_the_anchor_reports_the_boundary_absent() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        run(request(&db, 9)).await.unwrap();

        let state = McCache::new(db.clone(), SESSION).load().await.unwrap();
        let state = state.expect("state after the first pass");
        assert_eq!(state.boundary_id, anchor_id(9));
        // The covered range is still live: the anchor is present.
        assert!(boundary_present(Some(&state), 9, 20).is_some());
        // A revert that removed covered rows reports it absent → the kernel
        // flags a reconcile, and the next pass rebuilds against the live tail.
        assert!(boundary_present(Some(&state), 9, 5).is_none());
    }
}
