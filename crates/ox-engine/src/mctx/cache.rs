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
//! - [`run_pass`] — the per-pass driver: load → classify → render → step
//!   (panic-contained) → CAS write-back with a single whole-pass retry.
//! - [`Lkg`] — per-session last-known-good prefix (a `kv` row keyed
//!   `mctx_lkg:{session_id}`) backing the refuse-turn semantics wired up by
//!   the integration tasks.

use anyhow::Context as _;
use cortexkit_cache_core::{Action, CoreState, DurabilityClass, FrozenUnit, PassInput, StepResult};

use ox_db::{Db, optional};
use ox_schema::id::now_millis;

/// Full mctx cache error type: `thiserror` plumbing (see `McCacheError`).
#[derive(Debug, thiserror::Error)]
pub enum McCacheError {
    /// The `mc_frozen_state` row moved underneath us between load and save
    /// (another writer won the CAS). The whole pass must retry against the
    /// fresh value.
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
}

/// The renderer seam: turns (state, signals) into the byte-complete units
/// for this pass. `Send + Sync` so it can sit behind the runner.
pub trait HeadRenderer: Send + Sync {
    /// Render the pass handoff for the given state and signals.
    ///
    /// MUST be deterministic per `(state, signals)`: a CAS conflict re-runs
    /// the whole pass, and the kernel requires the retried pass to re-splice
    /// the SAME boundary id (kernel lib.rs boundary-presence contract).
    /// Also runs on defer passes — it is the producer of `queued` (the
    /// drop-queue carrier); only `rendered_units`/`new_boundary_id` are
    /// structurally dropped there by [`build_pass_input`].
    fn render(&self, state: Option<&CoreState>, signals: &Signals) -> RenderedPass;
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
    fn render(&self, _state: Option<&CoreState>, _signals: &Signals) -> RenderedPass {
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
        },
        Action::Soft | Action::Hard => PassInput {
            proposed: Some(action),
            boundary_present,
            rendered_units: rendered.rendered_units,
            // A bust ignores the queued set (see `RenderedPass::queued`).
            queued: Vec::new(),
            new_boundary_id: rendered.new_boundary_id,
            run_started: signals.run_started,
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
    /// The pass panicked mid-render/step. Durable state is untouched (no
    /// poisoning); the caller serves the last-known-good prefix — or
    /// refuses the turn when no LKG exists.
    ModulePanicked,
}

impl PassOutcome {
    /// The cached prefix after the pass, or `None` on a panicked pass.
    pub fn prefix(&self) -> Option<&str> {
        match self {
            PassOutcome::DeferReplayed { prefix } | PassOutcome::BustApplied { prefix } => {
                Some(prefix)
            }
            PassOutcome::ModulePanicked => None,
        }
    }
}

/// Per-session handle over the frozen-state CAS store.
pub struct McCache {
    db: Db,
    session_id: String,
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
            return Ok(None);
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
        Ok(Some(state))
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
/// render, step the kernel (panic-contained), and CAS-write the result
/// back. On a CAS conflict the WHOLE pass retries once against the fresh
/// value (a CAS-retry re-splices the same boundary per the kernel docs); a
/// second conflict is a real contention error for the caller.
///
/// Callers must serialize passes per session (the session turn loop does):
/// the CAS is a crash/stale-handle safety net, not steady-state contention
/// handling — hence the single retry. (Two concurrent defer passes at the
/// same version are last-writer-wins by kernel contract: a defer never
/// bumps `version`, so no guard can distinguish them.)
///
/// On a panic — in the renderer or the kernel — the durable row is left
/// untouched and [`PassOutcome::ModulePanicked`] is returned; the caller
/// applies LKG semantics. Nothing is poisoned.
///
/// The result is always saved, including on defer passes: a defer can queue
/// drops and flip `reconcile_pending` without moving `version`, and both
/// are durable state the next pass must see.
pub async fn run_pass(
    cache: &McCache,
    renderer: &dyn HeadRenderer,
    signals: &Signals,
) -> Result<PassOutcome, McCacheError> {
    let mut attempt: u32 = 0;
    loop {
        let loaded = cache.load().await?;
        let action = classify(loaded.as_ref(), signals);
        // The CAS guard is the version we loaded (see `save_cas`).
        let expected_version = loaded.as_ref().map_or(0, |s| s.version);

        // Panic containment: neither the renderer nor the kernel step may
        // take the server down. `AssertUnwindSafe` is sound here because a
        // panic discards every value produced inside the closure. This
        // REQUIRES the default unwind panic profile — `panic = "abort"`
        // would kill the process instead of containing.
        let stepped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // The renderer runs on defer passes too: it is the producer of
            // `queued` (drop-queued units draining the kernel's
            // `pending_changes` path — ctx_reduce's carrier). Only the
            // `rendered_units`/boundary-mint halves are structurally dropped
            // by `build_pass_input` on SoftPlus. Whether the real Task-5
            // renderer can skip re-deriving the head on defers is its own
            // internal concern.
            let rendered = renderer.render(loaded.as_ref(), signals);
            let input = build_pass_input(action, rendered, signals);
            let mut state = loaded.clone().unwrap_or_default();
            let result: StepResult = state.step(input);
            let prefix = state.cached_prefix_bytes();
            let outcome = match result.action {
                Action::SoftPlus => PassOutcome::DeferReplayed { prefix },
                Action::Soft | Action::Hard => PassOutcome::BustApplied { prefix },
            };
            (state, outcome)
        }));
        let (state, outcome) = match stepped {
            Ok(pair) => pair,
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic payload".to_string());
                tracing::warn!(
                    session = %cache.session_id(),
                    detail = %detail,
                    "mctx transform pass panicked; durable state untouched, LKG applies"
                );
                return Ok(PassOutcome::ModulePanicked);
            }
        };

        match cache.save_cas(expected_version, &state).await {
            Ok(()) => return Ok(outcome),
            // One whole-pass retry after a lost race; a second loss is a
            // real contention error.
            Err(McCacheError::CasConflict { .. }) if attempt == 0 => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

/// Per-session last-known-good prefix: the last prefix bytes the transform
/// pass actually served. On a panicked pass the caller replays the LKG;
/// with no LKG at all the turn is refused. Stored in the existing `kv`
/// table under `mctx_lkg:{session_id}` — a separate table would be
/// overkill for one string per session.
pub struct Lkg {
    db: Db,
}

fn lkg_key(session_id: &str) -> String {
    format!("mctx_lkg:{session_id}")
}

impl Lkg {
    /// An LKG store over `db`.
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The last-known-good prefix for the session, if any.
    pub async fn load_lkg(&self, session_id: &str) -> Result<Option<String>, McCacheError> {
        let context = || format!("load LKG for session {session_id}");
        let key = lkg_key(session_id);
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT value FROM kv WHERE key = ?1")
            .await
            .with_context(context)?;
        let row = optional(&mut stmt, [key.as_str()])
            .await
            .with_context(context)?;
        Ok(row
            .map(|row| row.get(0).map_err(anyhow::Error::from))
            .transpose()?)
    }

    /// Record `prefix` as the session's last-known-good output.
    pub async fn save_lkg(&self, session_id: &str, prefix: &str) -> Result<(), McCacheError> {
        let key = lkg_key(session_id);
        let now = now_millis();
        let conn = self.db.acquire().await?;
        conn.execute(
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
            (key.as_str(), prefix, now),
        )
        .await
        .with_context(|| format!("save LKG for session {session_id}"))?;
        Ok(())
    }
}
