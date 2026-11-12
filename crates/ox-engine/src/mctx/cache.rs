//! cache-core integration: the ox-side harness around the
//! `cortexkit-cache-core` per-pass state machine.
//!
//! Division of labor (the kernel's docs are authoritative): **ox classifies
//! each pass into an [`Action`] and renders byte-complete units; the kernel
//! owns freeze/replay/coordinator/durability/version.** This module provides:
//!
//! - [`classify`] — the signal → Action table (MVP: Hard on the first pass,
//!   on boundary loss, or on an explicit forced request; SoftPlus otherwise;
//!   the compartment/coverage `Soft` classifier arrives with the render
//!   tasks).
//! - [`HeadRenderer`] — the render handoff seam (the real prompt renderer
//!   lands with the m[0]/m[1] tasks; [`TrivialRenderer`] keeps the state
//!   machine exercisable end-to-end until then).
//! - [`McCache`] — whole-value CAS persistence of [`CoreState`] in the
//!   `mc_frozen_state` table.
//! - [`run_pass`] — the per-pass driver: load → classify → render → step →
//!   snapshot CAS write-back. A lost race is returned to the caller.

use super::cache_core::{
    Action, CompactionIdentity, CoreState, DurabilityClass, FrozenUnit, M0RenderIdentity,
    PassInput, StepResult,
};
use anyhow::Context as _;

use ox_db::{Db, optional};

/// Full mctx cache error type: `thiserror` plumbing (see `McCacheError`).
#[derive(Debug, thiserror::Error)]
pub enum McCacheError {
    /// The `mc_frozen_state` row moved underneath us between load and save
    /// (another writer won the CAS). The caller must reload its sources and
    /// replan against a fresh snapshot.
    #[error(
        "CAS conflict on mc_frozen_state for session {session_id}: expected version {expected}"
    )]
    CasConflict {
        /// Session whose frozen-state row was contended.
        session_id: String,
        /// The version the loser held when it tried to write.
        expected: u64,
    },
    /// Everything else (IO, serialization, SQL).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Signals one transform pass observes about the live session, as plain
/// data so classification is table-driven and trivially testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// Boundary-presence check for this pass: `Some(id)` when the frozen
    /// coverage boundary `id` is still present in the live message array,
    /// `None` when it is absent (e.g. a revert removed it).
    pub boundary_present: Option<String>,
    /// Explicit refresh/compaction request (wired by later tasks).
    pub forced: bool,
    /// A run/episode boundary was crossed (lineage units reproduce
    /// byte-identical, episode units reset). Reserved for later tasks.
    pub run_started: bool,
    /// The rendered baseline (`m0`) bytes differ from the frozen ones — a
    /// memory write, or any other input the baseline is a function of. The
    /// caller compares bytes because the baseline's inputs are open-ended.
    pub head_changed: bool,
    /// Only the delta (`m1`) differs from the frozen bytes: new coverage
    /// arrived and the frozen baseline is still valid.
    pub delta_changed: bool,
    pub completed_compaction: Option<CompactionIdentity>,
}

/// Sentinel fed to the kernel as `boundary_present` when the boundary is
/// absent from the live array. A NUL byte cannot occur in an ox id, so the
/// sentinel can never compare equal to a minted boundary id.
const BOUNDARY_ABSENT: &str = "\u{0}absent";

/// Classify one pass into the kernel Action.
///
/// `signals.boundary_present` is deliberately NOT a classify input: absence
/// is detected by the kernel at step time, and surfaces on the NEXT pass as
/// the durable `reconcile_pending` flag (rule 2 below).
///
/// The table (evaluated top to bottom; the first hit wins):
///
/// | # | condition                                             | action     | rationale |
/// |---|-------------------------------------------------------|------------|-----------|
/// | 1 | no durable state (first-ever pass)                    | **Hard**   | mint the baseline |
/// | 2 | boundary loss pending — the durable `state.reconcile_pending` flag | **Hard** | rematerialize against the live array |
/// | 3 | `signals.forced` (explicit refresh/compaction)        | **Hard**   | the request demands a bust |
/// | 4 | `signals.head_changed` (the m0 bytes differ)          | **Hard**   | a baseline input changed (memory write, budget) |
/// | 5 | `signals.delta_changed` (only the m1 bytes differ)    | **Soft**   | new coverage rides the delta; the baseline stands |
/// | 6 | otherwise                                             | **SoftPlus** | defer: replay frozen bytes verbatim |
///
/// Rules 4 and 5 are byte comparisons made by the caller ([`crate::mctx::pass`])
/// against the frozen units: the baseline's inputs are open-ended, so "did the
/// head change" is answered by the bytes rather than by enumerating causes.
#[must_use]
pub fn classify(state: Option<&CoreState>, signals: &Signals) -> Action {
    // 1. First-ever pass: nothing frozen yet, mint a baseline.
    let Some(state) = state else {
        return Action::Hard;
    };
    // 2. A boundary was lost on a prior defer: the next pass must
    //    rematerialize against the live array.
    if state.reconcile_pending {
        return Action::Hard;
    }
    // 3. Explicit forced request.
    if signals.forced {
        return Action::Hard;
    }
    // 4. The baseline itself changed.
    if signals.head_changed {
        return Action::Hard;
    }
    // 5. Only the delta changed.
    if signals.delta_changed {
        return Action::Soft;
    }
    // 6. Default: defer.
    Action::SoftPlus
}

/// The ox→kernel render handoff for one pass. The real prompt renderer
/// lands with the m[0]/m[1] tasks; until then [`TrivialRenderer`] feeds the
/// state machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedPass {
    /// Byte-complete units to freeze on a bust pass (Soft/Hard).
    pub rendered_units: Vec<FrozenUnit>,
    /// Drop-queued units, consumed only on a defer pass (they wait in the
    /// kernel's `pending_changes` for the next bust). Ignored on a bust:
    /// drops reach the core by queueing on a defer first.
    pub queued: Vec<FrozenUnit>,
    /// The new coverage boundary minted by this pass, if any.
    pub new_boundary_id: Option<String>,
    /// The m[0] render identity to persist on a HARD rebuild.
    pub new_m0_identity: Option<M0RenderIdentity>,
    /// Durable geometry written with the frozen state; KV mirrors follow it.
    pub new_materialized_floor: Option<i64>,
    pub new_baseline_coverage: Option<i64>,
}

/// The renderer seam: turns (state, signals) into the byte-complete units
/// for this pass. `Send + Sync` so it can sit behind the runner.
pub trait HeadRenderer: Send + Sync {
    /// A defer pass does not render: the kernel replays frozen bytes verbatim.
    fn render(&mut self, state: Option<&CoreState>, signals: &Signals) -> RenderedPass;
}

/// A stand-in renderer: one `m0` unit with a static payload, minting a
/// fixed boundary. Keeps the full load → classify → step → CAS path
/// exercisable before the real prompt renderer exists.
#[derive(Debug, Clone)]
pub struct TrivialRenderer {
    /// Static payload for the `m0` unit.
    pub payload: String,
    /// Boundary id minted on bust passes.
    pub boundary_id: String,
    /// Drop-queued units handed to defer passes.
    pub queued: Vec<FrozenUnit>,
}

impl HeadRenderer for TrivialRenderer {
    fn render(&mut self, _state: Option<&CoreState>, _signals: &Signals) -> RenderedPass {
        RenderedPass {
            rendered_units: vec![FrozenUnit {
                key: "m0".into(),
                kind: "synthesized-region".into(),
                frozen_payload: self.payload.clone(),
                durability_class: DurabilityClass::Lineage,
                reset_rule: String::new(),
            }],
            queued: self.queued.clone(),
            new_boundary_id: Some(self.boundary_id.clone()),
            new_m0_identity: None,
            new_materialized_floor: None,
            new_baseline_coverage: None,
        }
    }
}

/// Build the kernel [`PassInput`] from the classified action and the
/// rendered handoff. This is where the defer discipline is enforced at the
/// ox/kernel seam: a defer pass carries NO rendered units (frozen bytes
/// replay verbatim, nothing re-derives) and never mints a boundary.
fn build_pass_input(action: Action, rendered: RenderedPass, signals: &Signals) -> PassInput {
    let boundary_present = signals
        .boundary_present
        .clone()
        .unwrap_or_else(|| BOUNDARY_ABSENT.to_string());
    match action {
        Action::SoftPlus => PassInput {
            proposed: Some(Action::SoftPlus),
            boundary_present,
            rendered_units: Vec::new(),
            queued: rendered.queued,
            new_boundary_id: None,
            run_started: signals.run_started,
            new_m0_identity: rendered.new_m0_identity,
            new_materialized_floor: rendered.new_materialized_floor,
            new_baseline_coverage: rendered.new_baseline_coverage,
        },
        Action::Soft | Action::Hard => PassInput {
            proposed: Some(action),
            boundary_present,
            rendered_units: rendered.rendered_units,
            // A bust ignores the queued set (see `RenderedPass::queued`).
            queued: Vec::new(),
            new_boundary_id: rendered.new_boundary_id,
            run_started: signals.run_started,
            new_m0_identity: rendered.new_m0_identity,
            new_materialized_floor: rendered.new_materialized_floor,
            new_baseline_coverage: rendered.new_baseline_coverage,
        },
    }
}

/// What one transform pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassOutcome {
    /// Defer pass: the frozen bytes replayed verbatim.
    DeferReplayed {
        /// The cached prefix after the pass.
        prefix: String,
    },
    /// Bust pass (Soft/Hard): new bytes froze into the baseline.
    BustApplied {
        /// The cached prefix after the pass.
        prefix: String,
    },
}

impl PassOutcome {
    /// The cached prefix after the pass.
    #[must_use]
    pub fn prefix(&self) -> Option<&str> {
        match self {
            PassOutcome::DeferReplayed { prefix } | PassOutcome::BustApplied { prefix } => {
                Some(prefix)
            }
        }
    }
}

/// Per-session handle over the frozen-state CAS store.
pub struct McCache {
    db: Db,
    session_id: String,
}

pub(crate) struct Snapshot {
    pub state: Option<CoreState>,
    json: Option<String>,
    column_version: i64,
}

impl McCache {
    /// A cache handle for `session_id` over `db`.
    pub fn new(db: Db, session_id: impl Into<String>) -> Self {
        Self {
            db,
            session_id: session_id.into(),
        }
    }

    /// The session this handle serves.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Load the durable [`CoreState`] for this handle's session, or `None`
    /// when the session has no frozen state yet (the first-ever pass). The
    /// JSON payload is the whole durable value; the `version` column mirrors
    /// its version for CAS indexing — on divergence the payload wins (they
    /// are only ever written together, so divergence means an out-of-band
    /// edit).
    pub async fn load(&self) -> Result<Option<CoreState>, McCacheError> {
        Ok(self.load_snapshot().await?.state)
    }

    pub(crate) async fn load_snapshot(&self) -> Result<Snapshot, McCacheError> {
        let context = || format!("load mc_frozen_state for session {}", self.session_id);
        let conn = self.db.acquire().await.with_context(context)?;
        let mut stmt = conn
            .prepare("SELECT state, version FROM mc_frozen_state WHERE session_id = ?1")
            .await
            .with_context(context)?;
        let row = optional(&mut stmt, [self.session_id.as_str()])
            .await
            .with_context(context)?;
        let Some(row) = row else {
            return Ok(Snapshot {
                state: None,
                json: None,
                column_version: 0,
            });
        };
        let (json, column_version) = (|row: &ox_db::turso::Row| -> anyhow::Result<(String, i64)> {
            Ok((row.get(0)?, row.get(1)?))
        })(&row)
        .with_context(context)?;
        let state: CoreState = serde_json::from_str(&json).with_context(|| {
            format!(
                "corrupt mc_frozen_state payload for session {}",
                self.session_id
            )
        })?;
        if state.version as i64 != column_version {
            tracing::warn!(
                session = %self.session_id,
                column_version,
                payload_version = state.version,
                "mc_frozen_state version column diverges from the JSON payload; trusting the payload"
            );
        }
        Ok(Snapshot {
            state: Some(state),
            json: Some(json),
            column_version,
        })
    }

    async fn save_snapshot(
        &self,
        snapshot: &Snapshot,
        state: &CoreState,
    ) -> Result<(), McCacheError> {
        let json = serde_json::to_string(state).context("serialize mc_frozen_state")?;
        let conn = self.db.acquire().await?;
        let affected = if let Some(previous) = snapshot.json.as_deref() {
            conn.execute(
                "UPDATE mc_frozen_state SET state = ?1, version = ?2 WHERE session_id = ?3 AND state = ?4 AND version = ?5",
                (json.as_str(), state.version as i64, self.session_id.as_str(), previous, snapshot.column_version),
            ).await.context("update mc_frozen_state")?
        } else {
            conn.execute(
                "INSERT INTO mc_frozen_state (session_id, state, version) VALUES (?1, ?2, ?3) ON CONFLICT(session_id) DO NOTHING",
                (self.session_id.as_str(), json.as_str(), state.version as i64),
            ).await.context("insert mc_frozen_state")?
        };
        if affected == 0 {
            return Err(McCacheError::CasConflict {
                session_id: self.session_id.clone(),
                expected: snapshot.column_version as u64,
            });
        }
        Ok(())
    }

    /// Whole-value CAS write-back for this handle's session: replace the row
    /// only when its current version is still `expected_version` — the
    /// version this handle LOADED before the kernel step (the kernel already
    /// bumped `state.version` to the new value). `affected-rows == 0` →
    /// [`McCacheError::CasConflict`]. A missing row inserts (first-ever
    /// save; no writer to race with).
    pub async fn save_cas(
        &self,
        expected_version: u64,
        state: &CoreState,
    ) -> Result<(), McCacheError> {
        let json = serde_json::to_string(state).with_context(|| {
            format!("serialize mc_frozen_state for session {}", self.session_id)
        })?;
        // Versions are small monotonic counters; the i64 narrowing matches
        // the SQLite column type.
        let new_version = state.version as i64;
        let expected = expected_version as i64;
        let conn = self.db.acquire().await?;
        let affected = conn
            .execute(
                "INSERT INTO mc_frozen_state (session_id, state, version) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET state = ?2, version = ?3
                 WHERE mc_frozen_state.version = ?4",
                (
                    self.session_id.as_str(),
                    json.as_str(),
                    new_version,
                    expected,
                ),
            )
            .await
            .with_context(|| format!("save mc_frozen_state for session {}", self.session_id))?;
        if affected == 0 {
            return Err(McCacheError::CasConflict {
                session_id: self.session_id.clone(),
                expected: expected_version,
            });
        }
        Ok(())
    }
}

/// Drive one transform pass end to end: load the durable state, classify,
/// render, step the kernel, and CAS-write the result
/// back. A CAS conflict is returned to the caller. Reusing the failed attempt's
/// state would risk overwriting a concurrent writer's newer state.
///
/// The CAS compares the complete persisted snapshot, including defer changes
/// that do not bump the kernel version.
///
/// The result is always saved, including on defer passes: a defer can queue
/// drops and flip `reconcile_pending` without moving `version`, and both
/// are durable state the next pass must see.
pub async fn run_pass(
    cache: &McCache,
    renderer: &mut dyn HeadRenderer,
    signals: &Signals,
) -> Result<PassOutcome, McCacheError> {
    let snapshot = cache.load_snapshot().await?;
    run_snapshot(cache, snapshot, renderer, signals).await
}

pub(crate) async fn run_snapshot(
    cache: &McCache,
    mut snapshot: Snapshot,
    renderer: &mut dyn HeadRenderer,
    signals: &Signals,
) -> Result<PassOutcome, McCacheError> {
    let mut loaded = snapshot.state.take();
    let action = classify(loaded.as_ref(), signals);

    let rendered = if action == Action::SoftPlus {
        RenderedPass::default()
    } else {
        renderer.render(loaded.as_ref(), signals)
    };
    let (state, outcome) = transition(loaded.take(), action, rendered, signals);

    cache.save_snapshot(&snapshot, &state).await?;
    Ok(outcome)
}

/// Step a locally owned snapshot; persistence belongs exclusively to the caller.
pub(crate) fn transition(
    loaded: Option<CoreState>,
    action: Action,
    rendered: RenderedPass,
    signals: &Signals,
) -> (CoreState, PassOutcome) {
    let input = build_pass_input(action, rendered, signals);
    let mut state = loaded.unwrap_or_default();
    let result: StepResult = state.step(input);
    if result.action == Action::Hard {
        state
            .completed_compaction
            .clone_from(&signals.completed_compaction);
    }
    let prefix = state.cached_prefix_bytes();
    let outcome = match result.action {
        Action::SoftPlus => PassOutcome::DeferReplayed { prefix },
        Action::Soft | Action::Hard => PassOutcome::BustApplied { prefix },
    };
    (state, outcome)
}
