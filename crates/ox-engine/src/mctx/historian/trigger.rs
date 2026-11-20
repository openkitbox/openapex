use crate::mctx::budget::Budget;

/// `TAIL_SIZE_TRIGGER_MULTIPLIER`: the tail must be this many times its budget
/// before its size alone starts a run.
const TAIL_SIZE_MULTIPLIER: f64 = 3.0;

/// Pressure band the session tail has reached.
///
/// Ported from the reference's escalation order in
/// `boundary.rs::check_compartment_trigger_with_index` (the force band is
/// tested first, the proactive floor last):
///
/// - [`Trigger::Forced`] — `TriggerReason::ForceBand`: usage is at or above the
///   force band (`max(85, execute + 2)`, derived in [`Budget`]).
/// - [`Trigger::Speculative`] — `TriggerReason::ProjectedHeadroom` /
///   `TriggerReason::TailSize`: usage is at or above the proactive floor
///   (`execute − 2`), **or** the unfolded tail alone has reached three times
///   its budget (a fresh session on a huge window can overflow inside one long
///   step before any usage number exists).
/// - [`Trigger::No`] — `HistorianNoFireCause::BelowProactiveFloor` (also
///   `BelowMinimumEligibleContent`): nothing is large enough to be worth a run.
///
/// [`Budget`]: crate::mctx::budget::Budget
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Below both the floor and the tail budget: do not start a run.
    No,
    /// In the proactive band: a run may start before pressure forces one.
    Speculative,
    /// In the force band: a run must start.
    Forced,
}

/// Decide whether a historian run should start.
///
/// Two measurements, because one is not always available: `usage_pct` is what
/// the provider reported for the newest turn as a share of the model window
/// (`None` when the catalogue does not know the window), `tail_tokens` the
/// live tail's own estimate. The tail comparison is what makes the trigger
/// work on the first step of a session, before any completion has reported a
/// usage number; the usage comparison is what catches a window that is filling
/// up with head+tail together rather than with tail alone.
#[must_use]
pub fn trigger(
    usage_pct: Option<f64>,
    tail_tokens: f64,
    tail_budget: u32,
    band: &Budget,
) -> Trigger {
    if !tail_tokens.is_finite() {
        return Trigger::No;
    }
    // The force band is checked first: a tail that is merely large must never
    // downgrade a forced run into a speculative one.
    if let Some(usage) = usage_pct {
        if usage >= band.hard_pct {
            return Trigger::Forced;
        }
        if usage >= band.soft_pct {
            return Trigger::Speculative;
        }
    }
    // Below the floor by usage, or no usage number at all: the tail alone can
    // still be over budget — but by `TAIL_SIZE_MULTIPLIER` times, not once
    // (`TAIL_SIZE_TRIGGER_MULTIPLIER`): usage is the primary signal, and a tail
    // merely at its nominal budget is not yet grounds for a run.
    if tail_budget > 0 && tail_tokens >= f64::from(tail_budget) * TAIL_SIZE_MULTIPLIER {
        Trigger::Speculative
    } else {
        Trigger::No
    }
}
