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
//! - **The coverage anchor** — the boundary id carries the ordinal m[0]
//!   covers (`mc:<ordinal>`), which is also the transcript floor for request
//!   assembly: everything at or below it is represented by the head.
//!
//! Freeze/replay discipline is NOT re-implemented here: on a defer the kernel
//! replays the frozen bytes verbatim and this module only plans (it never
//! re-derives what it will not freeze away).

use anyhow::{Context as _, Result};
use cortexkit_cache_core::{CoreState, DurabilityClass, FrozenUnit};
use rusqlite::OptionalExtension;

use crate::db::Db;
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
    /// The head bytes to inject ahead of the live transcript, or `None` when
    /// there is no usable head (first-ever pass with nothing to say, or a
    /// panicked pass with no last-known-good).
    pub head: Option<String>,
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

    // The ordinal m[0] covers. On a pass that must rebuild (no state yet, or
    // an explicit/forced rebuild) m[0] folds the whole compartment set;
    // otherwise it stays pinned to the anchor the frozen m[0] was built at.
    let coverage = compartments::coverage_end(&compartments);
    let m0_coverage = if let Some(state) = state.as_ref() {
        anchor_coverage(&state.boundary_id).unwrap_or(0)
    } else {
        coverage.unwrap_or(0)
    };
    let rebuild_m0 = state.is_none()
        || forced
        || baseline_changed(state.as_ref(), &compartments, &memories, m0_coverage);

    let plan = plan_units(PlanInputs {
        compartments: &compartments,
        memories: &memories,
        m0_coverage: if rebuild_m0 {
            coverage.unwrap_or(0)
        } else {
            m0_coverage
        },
    });

    let (frozen_m0, frozen_m1) = frozen_halves(state.as_ref());
    let signals = Signals {
        boundary_present: boundary_present(state.as_ref(), m0_coverage, last_seq),
        reconcile_pending: false,
        forced,
        run_started,
        head_changed: state.is_none() || forced || plan.m0 != frozen_m0,
        delta_changed: plan.m1 != frozen_m1,
    };

    // Queued drops ride the planned units, not the kernel's defer queue: our
    // durable `mc_drop` rows are the carrier ("queued, not immediate" — the
    // placeholder materializes when a bust freezes the prefix, and until then
    // the frozen bytes replay verbatim).
    let drops = peek_queued_drops(db, session_id).await?;
    let mut units = plan.units();
    units.extend(drop_units(&drops));
    let renderer = PrecomputedRenderer {
        units,
        boundary_id: plan.boundary_id.clone(),
    };

    let outcome = run_pass(&cache, &renderer, &signals).await?;
    let result = match &outcome {
        PassOutcome::DeferReplayed { prefix } => {
            let floor = materialized_floor(db, session_id).await?;
            PassResult {
                head: Some(prefix.clone()),
                floor,
                outcome,
            }
        }
        PassOutcome::BustApplied { prefix } => {
            // The bust froze these bytes, so the queued drops it drained are
            // now reflected in the prefix: mark them applied.
            let tags: Vec<u64> = drops.iter().map(|(tag, _)| *tag).collect();
            if !tags.is_empty() {
                mark_drops_applied(db, session_id, &tags).await?;
            }
            if let Some(coverage) = coverage {
                save_materialized_floor(db, session_id, coverage).await?;
            }
            PassResult {
                head: Some(prefix.clone()),
                floor: coverage.unwrap_or(0),
                outcome,
            }
        }
        PassOutcome::ModulePanicked => {
            // LKG: serve the last bytes the pass actually produced, with the
            // transcript floor that went with them. With no LKG there is
            // nothing coherent to inject, so the request falls back to the
            // raw transcript (the head is an optimization, never a
            // correctness boundary).
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
            PassResult {
                head: lkg,
                floor,
                outcome,
            }
        }
    };

    // Record the head actually served as the session's last-known-good.
    if let Some(head) = result.head.as_deref()
        && !matches!(result.outcome, PassOutcome::ModulePanicked)
    {
        crate::mctx::cache::Lkg::new(db.clone())
            .save_lkg(session_id, head)
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
    m0_coverage: i64,
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
        boundary_id: anchor_id(inputs.m0_coverage),
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
    });
    let (frozen_m0, _) = frozen_halves(Some(state));
    plan.m0 != frozen_m0
}

/// The boundary id for a given m[0] coverage ordinal.
fn anchor_id(coverage: i64) -> String {
    format!("mc:{coverage}")
}

/// The coverage ordinal encoded in an anchor id.
fn anchor_coverage(boundary_id: &str) -> Option<i64> {
    boundary_id.strip_prefix("mc:")?.parse().ok()
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

/// Boundary-presence token: the frozen anchor when the range it covers is
/// still in the live transcript, else the absent sentinel the kernel expects.
fn boundary_present(state: Option<&CoreState>, m0_coverage: i64, last_seq: i64) -> Option<String> {
    let state = state?;
    if m0_coverage <= last_seq && m0_coverage > 0 {
        Some(state.boundary_id.clone())
    } else if m0_coverage == 0 {
        // Nothing folded yet: the anchor describes an empty coverage, which is
        // present by construction.
        Some(state.boundary_id.clone())
    } else {
        None
    }
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
    let key = materialized_key(session_id);
    let value = db
        .conn()
        .call(move |c| -> rusqlite::Result<Option<String>> {
            c.query_row("SELECT value FROM kv WHERE key = ?1", [&key], |row| {
                row.get(0)
            })
            .optional()
        })
        .await
        .context("load mctx materialized floor")?;
    Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
}

async fn save_materialized_floor(db: &Db, session_id: &str, coverage: i64) -> Result<()> {
    let key = materialized_key(session_id);
    let value = coverage.to_string();
    let now = now_millis();
    db.conn()
        .call(move |c| -> rusqlite::Result<()> {
            c.execute(
                "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
                rusqlite::params![key, value, now],
            )?;
            Ok(())
        })
        .await
        .context("save mctx materialized floor")?;
    Ok(())
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

/// The planned head for a session without running a pass — used by tests and
/// tooling that need the bytes without touching durable state.
pub async fn preview(db: &Db, session_id: &str, project: &str) -> Result<String> {
    let compartments = compartments::load(db, session_id).await?;
    let memories = active_memories(db, project).await?;
    let plan = plan_units(PlanInputs {
        compartments: &compartments,
        memories: &memories,
        m0_coverage: compartments::coverage_end(&compartments).unwrap_or(0),
    });
    Ok(format!("{}{}", plan.m0, plan.m1))
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
                facts: &json!({}),
                events: &json!([]),
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
        let head = result.head.unwrap();
        assert!(head.contains("<session-history>"), "m0 history: {head}");
        assert!(
            head.contains("## 1-9 · Alpha\nAlpha full"),
            "compartment: {head}"
        );
        assert!(head.contains("<project-memory>"), "pinboard: {head}");
        assert!(head.contains(M1_PLACEHOLDER), "m1 placeholder: {head}");
    }

    #[tokio::test]
    async fn second_identical_pass_is_a_byte_stable_defer() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        let first = run(request(&db, 9)).await.unwrap();
        let second = run(request(&db, 9)).await.unwrap();

        assert!(matches!(second.outcome, PassOutcome::DeferReplayed { .. }));
        assert_eq!(first.head, second.head, "defer replays the same bytes");
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
        let head = second.head.unwrap();
        let m0 = head.find("<session-history>").unwrap();
        let delta = head.find("<new-compartments>").unwrap();
        assert!(m0 < delta, "delta follows the baseline: {head}");
        assert!(head.contains("## 10-20 · Beta"), "delta body: {head}");
        // The baseline still renders the compartment it was frozen at.
        assert!(
            head.contains("## 1-9 · Alpha\nAlpha full"),
            "frozen m0: {head}"
        );
        assert_eq!(second.floor, 20, "the floor advances with the delta");
    }

    #[tokio::test]
    async fn a_memory_write_rebuilds_the_baseline() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        let first = run(request(&db, 9)).await.unwrap();
        assert!(!first.head.as_deref().unwrap().contains("<project-memory>"));

        write_memory(&db, PROJECT, "new fact", "NAMING")
            .await
            .unwrap();
        let second = run(request(&db, 9)).await.unwrap();
        assert!(matches!(second.outcome, PassOutcome::BustApplied { .. }));
        let head = second.head.unwrap();
        assert!(head.contains("#1: new fact"), "memory lands in m0: {head}");
        assert!(head.contains(M1_PLACEHOLDER), "the delta resets: {head}");
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
    async fn a_queued_drop_waits_for_a_bust_and_is_marked_applied() {
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

        // A defer carries the drop forward and leaves it unapplied.
        let deferred = run(request(&db, 9)).await.unwrap();
        assert!(matches!(
            deferred.outcome,
            PassOutcome::DeferReplayed { .. }
        ));
        let pending = peek_queued_drops(&db, SESSION).await.unwrap();
        assert_eq!(pending.len(), 1, "the drop is still queued after a defer");

        // The next bust drains it into the prefix and marks it applied.
        add_compartment(&db, "mc_2", 10, 20, "Beta").await;
        let busted = run(request(&db, 20)).await.unwrap();
        let head = busted.head.unwrap();
        assert!(
            head.contains(&format!("[dropped §{tag}§]")),
            "placeholder in head: {head}"
        );
        assert!(peek_queued_drops(&db, SESSION).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_revert_beyond_the_anchor_reports_the_boundary_absent() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        run(request(&db, 9)).await.unwrap();

        let state = McCache::new(db.clone(), SESSION).load().await.unwrap();
        let state = state.expect("state after the first pass");
        assert_eq!(anchor_coverage(&state.boundary_id), Some(9));
        // The covered range is still live: the anchor is present.
        assert!(boundary_present(Some(&state), 9, 20).is_some());
        // A revert past the coverage reports it absent → the kernel flags a
        // reconcile, and the next pass rebuilds.
        assert!(boundary_present(Some(&state), 9, 5).is_none());
    }

    #[tokio::test]
    async fn preview_matches_the_planned_head() {
        let db = db().await;
        add_compartment(&db, "mc_1", 1, 9, "Alpha").await;
        let previewed = preview(&db, SESSION, PROJECT).await.unwrap();
        let head = run(request(&db, 9)).await.unwrap().head.unwrap();
        assert_eq!(previewed, head);
    }
}
