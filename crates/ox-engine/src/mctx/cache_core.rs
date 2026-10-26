//! Harness-neutral prompt-cache-stability core.
//!
//! This is the shared per-pass state machine that lets a harness pin a render-config
//! across turns without thrashing the provider prefix cache. It is consumed by the
//! Magic Context observer harness and the llm-runner author harness; both pin the same
//! golden-vector contract (`cache-stability-golden-vectors.json`, schema_version 2).
//!
//! ## The one invariant
//!
//! The harness renders each affected unit to **final bytes on a cache-busting pass**
//! ([`Action::Soft`] / [`Action::Hard`]) and the core **freezes those bytes**. A defer
//! pass ([`Action::SoftPlus`]) **places the frozen bytes verbatim** — there is no render
//! call on a defer pass, so defer-pass re-derivation is structurally impossible. The core
//! never interprets a `frozen_payload`; it only stores and replays it.
//!
//! ## The cache anchor is BOUNDARY-PRESENCE
//!
//! The covered prefix is REPLACED by the frozen bytes; the only per-pass validity check is
//! whether the boundary id is still present in the live array (`boundary_present ==
//! state.boundary_id`). There is NO content fingerprint over the covered prefix, so no
//! collision surface: an in-prefix edit is summarized away (intentional lossiness, not a
//! stale cache), and a revert that removes the boundary keeps replaying the frozen bytes
//! this pass (reconcile_pending) and reconciles on the next cache-busting pass.
//!
//! ## Division of responsibility
//!
//! Trigger causes are a **harness-provided predicate**, never baked into the core: the
//! harness classifies each signal into an [`Action`] and the core applies the
//! freeze/replay/coordinator/durability mechanics for that class. The core owns the
//! boundary-presence branch, the byte-complete freeze-on-bust / replay-on-defer discipline,
//! the deferred-work coordinator (a HARD bust from any cause drains all deferred work), the
//! `durability_class` reset rule across episode boundaries, and the version stamp for the
//! harness's CAS write-back.

use serde::{Deserialize, Serialize};

/// The classifier verdict for one pass. The HARNESS assigns this (its trigger predicate);
/// the core applies the mechanics. `SoftPlus` = defer (replay frozen bytes verbatim, no
/// render). `Soft` = bust at the volatile-delta breakpoint (the stable baseline stays
/// cached). `Hard` = the whole prefix rebuilds into a new frozen baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    #[serde(rename = "SOFT+")]
    SoftPlus,
    #[serde(rename = "SOFT")]
    Soft,
    #[serde(rename = "HARD")]
    Hard,
}

/// Whether a frozen unit survives an episode (`RunStarted`) boundary.
///
/// Every CACHE frozen unit is `Lineage` today: a unit compacts the conversation prefix,
/// which is lineage-cumulative (a drop in episode 1 stays dropped in episode 5; it does not
/// un-compact at a new run). `Episode` is RESERVED for schema-completeness — the per-episode
/// reset state (run_config/usage/completed_steps) is WAL replay-state, a separate structure,
/// not a cache frozen unit — so a future run-scoped frozen unit is expressible without a
/// schema break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurabilityClass {
    Episode,
    Lineage,
}

/// A frozen render unit: an opaque byte-complete payload the harness rendered on a bust
/// pass. The core stores and replays `frozen_payload` verbatim and NEVER interprets it
/// (kind is opaque too — `drop` | `strip` | `skeleton` | `synthesized-region` | `injection`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenUnit {
    pub key: String,
    pub kind: String,
    /// EXACT emitted bytes as of the freeze (bust) pass — NOT inputs to re-render. A defer
    /// pass replays these verbatim; the core never re-derives them.
    pub frozen_payload: String,
    pub durability_class: DurabilityClass,
    #[serde(default)]
    pub reset_rule: String,
}

/// The core's durable per-pass state. One atomic value: the harness CAS-writes it back
/// whole (units + boundary + version), never per-field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CoreState {
    pub version: u64,
    /// The coverage descriptor: the id the covered prefix is spliced out at. A CAS-retry
    /// re-splices the SAME id. Compared for equality against `boundary_present`.
    pub boundary_id: String,
    pub frozen_units: Vec<FrozenUnit>,
    /// Deferred units queued on a `SoftPlus` pass (drop-queued); drained into the next bust.
    #[serde(default)]
    pub pending_changes: Vec<FrozenUnit>,
    /// Set when a defer pass lost the boundary (a revert removed it). The harness reads this
    /// to decide that the NEXT cache-busting pass rematerializes against the live array.
    #[serde(default)]
    pub reconcile_pending: bool,
}

/// One pass into the core. `proposed` is the harness's classification; `boundary_present`
/// is the opaque live-boundary token; `rendered_units` are the byte-complete units the
/// harness rendered for a bust (empty on `SoftPlus`).
#[derive(Debug, Clone, Default)]
pub struct PassInput {
    pub proposed: Option<Action>,
    pub boundary_present: String,
    /// Byte-complete units the harness rendered on a `Soft`/`Hard` bust. Frozen into state.
    pub rendered_units: Vec<FrozenUnit>,
    /// Units to queue into `pending_changes` on a `SoftPlus` defer (drop-queued). They wait
    /// for the next bust.
    pub queued: Vec<FrozenUnit>,
    /// The rematerialized boundary id minted on a `Hard` fold (or a bust that rematerializes
    /// after a boundary loss).
    pub new_boundary_id: Option<String>,
    /// A `RunStarted` / episode boundary: lineage units reproduce byte-identical, episode
    /// units reset.
    pub run_started: bool,
}

impl PassInput {
    /// Construct a pass input with an explicit action and boundary token.
    pub fn new(proposed: Action, boundary_present: impl Into<String>) -> Self {
        PassInput {
            proposed: Some(proposed),
            boundary_present: boundary_present.into(),
            ..Default::default()
        }
    }
}

/// The result of a pass: the executed action and whether reconciliation is now pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult {
    pub action: Action,
    pub reconcile_pending: bool,
}

impl CoreState {
    /// The cached-prefix bytes: the in-order concatenation of every frozen unit's payload.
    /// Two consecutive `SoftPlus` passes MUST produce an identical value (the byte-stability
    /// invariant) — the core proves it structurally by not mutating frozen units on a defer.
    pub fn cached_prefix_bytes(&self) -> String {
        self.frozen_units
            .iter()
            .map(|u| u.frozen_payload.as_str())
            .collect()
    }

    /// Apply one pass. Returns the executed [`StepResult`] and mutates `self` in place
    /// (the harness then CAS-writes the whole value back).
    ///
    /// # Panics
    /// Panics if `input.proposed` is `None` (the harness MUST classify the signal before
    /// stepping — trigger causes are harness-provided).
    pub fn step(&mut self, input: PassInput) -> StepResult {
        let proposed = input
            .proposed
            .expect("harness must classify the signal into an Action before stepping");
        let boundary_match = input.boundary_present == self.boundary_id;

        match proposed {
            Action::SoftPlus => self.step_defer(input, boundary_match),
            Action::Soft => self.step_soft(input, boundary_match),
            Action::Hard => self.step_hard(input),
        }
    }

    /// Defer: no render call, replay frozen bytes verbatim. Queue any drop-queued units into
    /// `pending_changes`. If the boundary is absent (a revert removed it), keep replaying the
    /// frozen bytes this pass and mark `reconcile_pending` — NEVER a blind same-pass rebuild.
    /// On a `RunStarted` boundary, lineage units carry forward byte-identical and episode
    /// units reset.
    fn step_defer(&mut self, input: PassInput, boundary_match: bool) -> StepResult {
        for unit in input.queued {
            self.pending_changes.push(unit);
        }

        if input.run_started {
            // Lineage units survive byte-identical (their frozen_payload is untouched);
            // episode units reset at the run boundary. The cache set is all-lineage today,
            // so this is a no-op in practice, but the reset rule is enforced structurally.
            self.frozen_units
                .retain(|u| u.durability_class == DurabilityClass::Lineage);
        }

        // boundary_match => the covered prefix splices/replaces and frozen bytes replay.
        // boundary absent => reuse this pass, reconcile on the next bust.
        //
        // VACUOUS CASE: an EMPTY boundary_id means no boundary was ever minted (fresh
        // store, nothing covered) — absence then is "never existed", not "reverted
        // away", and there is genuinely nothing to reconcile. Without this, an
        // unseeded session oscillates HARD → defer → HARD forever (each HARD clears
        // the flag, each defer re-sets it against the "" boundary) — cache-neutral
        // (the empty baseline re-materializes byte-identical) but dishonest state.
        // "" is RESERVED as the no-boundary sentinel: a HARD that mints Some("") is
        // legitimately declaring "still no boundary" (fresh store), and the first
        // real compartment mints a non-empty id, exiting the vacuous state.
        self.reconcile_pending = !boundary_match && !self.boundary_id.is_empty();

        StepResult {
            action: Action::SoftPlus,
            reconcile_pending: self.reconcile_pending,
        }
    }

    /// Soft bust: the volatile delta re-renders, the stable baseline (m0 frozen bytes) stays
    /// frozen. Freeze the rendered delta units (replace same-key, append new).
    ///
    /// `boundary_id` is the COVERAGE anchor (the last raw item any summary covers, m0 OR the
    /// delta). A SOFT MAY advance it when `new_boundary_id` is `Some` — used when the volatile
    /// delta now summarizes content past the prior anchor (e.g. a new compartment rides the
    /// delta and extends coverage over raw tail items). The m0 frozen bytes are NEVER mutated
    /// on a SOFT; only the coverage anchor moves, so the byte-stability invariant holds. `None`
    /// leaves the boundary unchanged (the common case: a delta that rides within existing
    /// coverage, e.g. a memory delta). `reconcile_pending` is untouched — a pending reconcile
    /// (m0 itself stale) is cleared only by a HARD rematerialize, never a SOFT.
    ///
    /// The anchor advance is GUARDED: it applies only when the prior anchor is present
    /// (`boundary_match`) AND no reconcile is pending. A coverage-extending SOFT is only
    /// coherent when the current anchor is live and m0 is not already stale — advancing the
    /// anchor while a reconcile is pending would strand the stale m0 under a fresh anchor (the
    /// next defer would clear `reconcile_pending` against the new anchor and the needed HARD
    /// rematerialize would never fire). Under a correct harness classifier this never arises
    /// (`reconcile_pending` routes Defer or HARD, never a coverage-extending SOFT), but this is
    /// a shared cache-stability primitive, so the guard is enforced in the core, not assumed.
    fn step_soft(&mut self, input: PassInput, boundary_match: bool) -> StepResult {
        self.apply_units(input.rendered_units);
        if boundary_match
            && !self.reconcile_pending
            && let Some(new_boundary) = input.new_boundary_id
        {
            self.boundary_id = new_boundary;
        }
        self.version += 1;
        StepResult {
            action: Action::Soft,
            reconcile_pending: self.reconcile_pending,
        }
    }

    /// Hard bust: the whole prefix rebuilds into a new frozen baseline. Freeze the rendered
    /// units AND drain ALL deferred work into this one bust (a HARD from any cause drains the
    /// coordinator). Mint the new boundary id; clear `reconcile_pending` (the rematerialize
    /// reconciles any earlier boundary loss).
    fn step_hard(&mut self, input: PassInput) -> StepResult {
        let mut units = input.rendered_units;
        units.append(&mut self.pending_changes);
        self.apply_units(units);
        if let Some(new_boundary) = input.new_boundary_id {
            self.boundary_id = new_boundary;
        }
        self.reconcile_pending = false;
        self.version += 1;
        StepResult {
            action: Action::Hard,
            reconcile_pending: false,
        }
    }

    /// Freeze a set of rendered units into the frozen set: a unit with an existing key
    /// REPLACES it (a re-materialized region), a new key appends. Order is preserved (existing
    /// keys keep their slot; new keys append in input order) — the cached-prefix byte order
    /// is load-bearing.
    fn apply_units(&mut self, units: Vec<FrozenUnit>) {
        for unit in units {
            if let Some(slot) = self.frozen_units.iter_mut().find(|u| u.key == unit.key) {
                *slot = unit;
            } else {
                self.frozen_units.push(unit);
            }
        }
    }
}
