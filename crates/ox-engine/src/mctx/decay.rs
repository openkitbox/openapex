//! Deterministic tier-decay curve.
//!
//! Pure functions that select which paraphrase tier (P1..P4) renders for each
//! compartment of session history, and which compartments archive (P5, not
//! rendered). Tier demotion is byte-deterministic at render time, driven by
//! compartment age, an emitted importance (semantically a *decay rate*), and
//! live budget pressure — no LLM call.
//!
//! Faithful port of the reference `mc-core/src/decay.rs` (itself a faithful
//! port of the council-validated model). Hyperparameters, tier-cost constants
//! and log-cost boundaries are reproduced exactly, so the model invariants
//! (age/importance monotonicity, finite demotion even at importance 100,
//! append stability, O(H) render cost, budget self-tuning) hold by the same
//! construction, and the ported golden fixture still matches bit-for-bit.
//!
//! Determinism note: the invariant this layer must satisfy is INTRA-module
//! determinism (same inputs → same tiers → same rendered bytes across passes),
//! which f64 gives for free. The ported `testdata/decay-golden.json` is a
//! development cross-check against the reference TS, not a runtime contract.

/// Half-life (in compartments) for importance 50 at pressure 1.
const H50: f64 = 24.0;
/// Importance points needed to double the half-life (imp 75 → 2×, 100 → 4×).
const D: f64 = 25.0;
/// Max extra half-lives of P4 protection from full anchor overlap.
const G: f64 = 2.0;

// Log-cost tier boundaries — geometric means of adjacent measured tier costs.
/// P1→P2 boundary.
const Z1: f64 = 0.201;
/// P2→P3 boundary.
const Z2: f64 = 0.729;
/// P3→P4 boundary.
const Z3: f64 = 1.322;
/// P4→P5 (archive candidate) boundary.
const Z4: f64 = 2.587;

/// Pressure floor: prevents div-by-zero and caps relaxation at 10×.
const P_FLOOR: f64 = 0.1;

/// Per-tier average token cost, indexed by tier number (1..5). Index 0 unused.
const TIER_COST: [u32; 6] = [0, 322, 109, 35, 20, 5];

/// A rendered paraphrase tier: 1 (verbose) .. 5 (archived / not rendered).
type Tier = u8;

/// A compartment's decay inputs for budget-pressure computation.
#[derive(Debug, Clone, Copy)]
pub struct DecayInput {
    /// 1-based position from newest (1 = newest).
    pub index: u32,
    /// 1..100 emitted importance (decay rate).
    pub importance: i32,
}

#[inline]
fn clamp_importance(importance: i32) -> f64 {
    importance.clamp(1, 100) as f64
}

/// The half-life-scaled age `z = a / H` for a compartment, the quantity
/// compared against the tier boundaries. `a` is the 0-based age,
/// `H = H50 · 2^((imp−50)/D) / p`.
#[inline]
fn z_value(compartment_index: u32, importance: i32, budget_pressure: f64) -> f64 {
    let a = (compartment_index.max(1) - 1) as f64;
    let imp = clamp_importance(importance);
    let p = budget_pressure.max(P_FLOOR);
    let f = 2f64.powf((imp - 50.0) / D);
    let h = (H50 * f) / p;
    a / h
}

/// Which paraphrase tier a compartment renders at, ignoring archive
/// protection.
///
/// `compartment_index` is 1-based from newest (1 = newest); `importance` is
/// 1..100; `budget_pressure` is 0.10..∞ (computed once per pass via
/// [`compute_budget_pressure`]).
fn tier(compartment_index: u32, importance: i32, budget_pressure: f64) -> Tier {
    let z = z_value(compartment_index, importance, budget_pressure);
    if z < Z1 {
        1
    } else if z < Z2 {
        2
    } else if z < Z3 {
        3
    } else if z < Z4 {
        4
    } else {
        5
    }
}

/// Whether a compartment should archive (P5, not rendered). Anchor overlap
/// extends P4 protection by up to [`G`] half-lives; with `anchor_overlap =
/// 0.0` (the default today, anchors not yet a first-class storage primitive)
/// this reduces to `z >= Z4`.
fn should_archive(
    compartment_index: u32,
    importance: i32,
    budget_pressure: f64,
    anchor_overlap: f64,
) -> bool {
    let z = z_value(compartment_index, importance, budget_pressure);
    let o = anchor_overlap.clamp(0.0, 1.0);
    z >= Z4 + G * o
}

/// Final rendered tier combining base tier + archive protection. Archived
/// compartments return 5; non-archived ones render at most P4 (the
/// verbose-to-anchor ladder caps at 4 — P5 is reserved for actual archival).
pub fn rendered_tier(
    compartment_index: u32,
    importance: i32,
    budget_pressure: f64,
    anchor_overlap: f64,
) -> Tier {
    if should_archive(
        compartment_index,
        importance,
        budget_pressure,
        anchor_overlap,
    ) {
        return 5;
    }
    tier(compartment_index, importance, budget_pressure).min(4)
}

/// Compute budget pressure for a render pass in a single forward pass.
/// Because `H ∝ 1/p`, the per-tier compartment counts scale as `1/p`, so
/// `C(p) ≈ C(1)/p`; setting `p = C(1)/B` gives `C(p) ≈ B`. Overshoots up to
/// ~30% at very tight budgets (<8K).
pub fn compute_budget_pressure(compartments: &[DecayInput], history_budget: f64) -> f64 {
    if history_budget <= 0.0 {
        return 1.0;
    }
    let mut natural_cost = 0.0;
    for c in compartments {
        let natural_tier = tier(c.index, c.importance, 1.0);
        // Archived compartments render as an empty string, so charging the P5
        // placeholder cost here would create phantom pressure from bytes that
        // never appear in the prompt.
        if natural_tier < 5 {
            natural_cost += TIER_COST[natural_tier as usize] as f64;
        }
    }
    (natural_cost / history_budget).max(P_FLOOR)
}
