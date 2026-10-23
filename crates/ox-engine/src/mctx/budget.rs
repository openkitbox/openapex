//! Model-window geometry and the three token budgets derived from it.
//!
//! Everything that measures context pressure or decides how much history the
//! head carries reads its numbers from one [`Budget`], derived once per drain
//! from the model catalogue. Three budgets, not one, because they answer three
//! different questions (upstream `derive-budgets.ts`):
//!
//! | field | question | anchor |
//! |---|---|---|
//! | [`Budget::trigger`] | how long may the unfolded tail grow | main model's usable window |
//! | [`Budget::history`] | how much history may `m[0]` carry | main model's usable window |
//! | [`Budget::historian_chunk`] | how much may one historian run read | the historian model's own window |
//!
//! Ported from the reference `boundary.rs` and its escalation bands, collapsed
//! to what ox can know: the catalogue reports no output reservation
//! (`ModelEntry::output` is always `None`), so the usable window is the window
//! itself, and cardinal being the only gateway leaves the provider-geometry
//! table (`separate` / `shared_truncating` / `shared_upfront`) nothing to
//! select between. Both constants are documented below so the omission is a
//! decision, not a gap.
//!
//! Pure: no IO, no DB, no config file. Callers pass the two windows and read
//! the fields.

use crate::mctx::MctxConfig;

/// The window assumed for the **historian** model when the catalogue does not
/// name it: the reference's own `BoundaryContext` default (§8.2). Erring small
/// is the safe direction — a run reads less, never more than the model can hold.
const ASSUMED_HISTORIAN_WINDOW: u32 = 128_000;

/// The budgets used when the **session** model's window is unknown: the numbers
/// ox ran with before any of this was derived (60k trigger denominator, 60k
/// history block). A cold catalogue must not change the compression strategy —
/// only a known window may make it model-aware.
const FALLBACK_TRIGGER: u32 = 60_000;
const FALLBACK_HISTORY: u32 = 60_000;

/// The proactive floor sits this far below the execute threshold
/// (`PROACTIVE_TRIGGER_OFFSET_PERCENTAGE`): the gap is what keeps a tail that
/// merely grazed the floor from being treated as execute pressure.
const SOFT_OFFSET_PCT: f64 = 2.0;
/// Floor of the force band (`MIN_FORCE_MATERIALIZE_PERCENTAGE`).
const HARD_FLOOR_PCT: f64 = 85.0;

/// The bounds of the two config values every number here derives from:
/// `[magic_context] execute_threshold_percentage` and `history_budget_percentage`.
/// They live beside the config they constrain (`ox_core::config`).
pub use ox_core::config::{MAX_EXECUTE_PCT, MAX_HISTORY_SHARE, MIN_EXECUTE_PCT, MIN_HISTORY_SHARE};

/// `derive_trigger_budget`: a share of the usable window, clamped so a small
/// window still folds something and a large one cannot let the tail grow
/// without bound.
const TRIGGER_SHARE: f64 = 0.05;
const TRIGGER_MIN: f64 = 5_000.0;
const TRIGGER_MAX: f64 = 50_000.0;

/// One historian run's input, as a share of the **historian model's** window.
const HISTORIAN_CHUNK_SHARE: f64 = 0.25;
const HISTORIAN_CHUNK_MIN: f64 = 8_000.0;
const HISTORIAN_CHUNK_MAX: f64 = 50_000.0;

/// The defaults the config carries when a key is absent (mirrored by
/// `MctxConfig::default`; duplicated here only so a NaN has somewhere to land).
const DEFAULT_EXECUTE_PCT: f64 = 65.0;
const DEFAULT_HISTORY_SHARE: f64 = 0.15;

/// `deriveProtectedTailTokenTarget`: how many tokens at the end of the tail a
/// run must leave alone.
const PROTECTED_ALPHA: f64 = 0.30;
const PROTECTED_FLOOR_RATIO: f64 = 0.08;
const PROTECTED_FLOOR_MIN: f64 = 2_000.0;
const PROTECTED_FLOOR_MAX: f64 = 12_000.0;
const PROTECTED_ABS_CAP: f64 = 96_000.0;
const PROTECTED_MAX_USABLE_RATIO: f64 = 0.40;
const PROTECTED_HEADROOM_MIN: f64 = 1_000.0;
const PROTECTED_HEADROOM_RATIO: f64 = 0.02;

/// The budgets one session runs with. `Copy`: consumers hold it by value or in
/// an `Arc` and never clone the config it was derived from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// The main model's context window; `None` when the catalogue does not
    /// report one, which makes the session's usage percentage unknown.
    window: Option<u32>,
    /// Pressure bands, in percent of `window`.
    pub soft_pct: f64,
    pub hard_pct: f64,
    /// Tokens the unfolded tail may reach before a historian run starts.
    pub trigger: u32,
    /// Tokens the `m[0]` history block may carry (the decay budget).
    pub history: u32,
    /// Tokens one historian run may read.
    pub historian_chunk: u32,
    /// The usable working space (`window × execute_threshold`), kept because
    /// the protected tail is a fraction of it.
    usable: u32,
}

impl Budget {
    /// Derive the budgets for one session.
    ///
    /// `window` is the main model's context window, `historian` the window of
    /// the model that backs the fold. A missing main window falls back to the
    /// pre-budget numbers ([`FALLBACK_TRIGGER`] and friends) rather than to a
    /// different code path: the "gateway is down" case must not change the
    /// compression strategy, only where its numbers come from.
    pub fn derive(config: &MctxConfig, window: Option<u32>, historian: Option<u32>) -> Self {
        // The config is validated where it is loaded, but this is `pub` and
        // `MctxConfig` is plain data: a NaN carried in from a test or a future
        // caller would truncate to `history = 0`, and a zero history budget
        // silently disables decay (`render_m0` renders everything at P1). The
        // clamp belongs to the only place that turns the numbers into budgets.
        let execute = percent(
            config.execute_threshold_percentage,
            DEFAULT_EXECUTE_PCT,
            MIN_EXECUTE_PCT,
            MAX_EXECUTE_PCT,
        );
        let history_share = percent(
            config.history_budget_percentage,
            DEFAULT_HISTORY_SHARE,
            MIN_HISTORY_SHARE,
            MAX_HISTORY_SHARE,
        );
        let historian_chunk = clamp_u32(
            (f64::from(historian.unwrap_or(ASSUMED_HISTORIAN_WINDOW)) * HISTORIAN_CHUNK_SHARE)
                .round(),
            HISTORIAN_CHUNK_MIN,
            HISTORIAN_CHUNK_MAX,
        );
        let Some(window) = window else {
            return Self {
                window: None,
                // The bands are still derived (a reported usage percentage
                // cannot exist without a window, so only `usage_percentage`
                // returning `None` uses this).
                soft_pct: (execute - SOFT_OFFSET_PCT).max(0.0),
                hard_pct: (execute + 2.0).max(HARD_FLOOR_PCT),
                trigger: FALLBACK_TRIGGER,
                history: FALLBACK_HISTORY,
                historian_chunk,
                usable: 0,
            };
        };
        let usable = usable_tokens(window, execute);
        Self {
            window: Some(window),
            // Upstream derives both bands from the execute threshold, never
            // from each other: a misconfigured pair cannot invert them.
            soft_pct: (execute - SOFT_OFFSET_PCT).max(0.0),
            hard_pct: (execute + 2.0).max(HARD_FLOOR_PCT),
            trigger: clamp_u32(
                (f64::from(usable) * TRIGGER_SHARE).round(),
                TRIGGER_MIN,
                TRIGGER_MAX,
            ),
            history: clamp_u32(
                (f64::from(usable) * history_share).floor(),
                1.0,
                f64::from(usable),
            ),
            historian_chunk,
            usable,
        }
    }

    /// The share of the main model's window the newest assistant turn filled,
    /// or `None` when the window is unknown (the pressure is then measured from
    /// the tail alone).
    ///
    /// `prompt_tokens` is what the provider reported for that turn —
    /// `tokens.input + tokens.cache.read`, the same sum the client's footer
    /// divides by this same window.
    pub fn usage_percentage(&self, prompt_tokens: i64) -> Option<f64> {
        let window = self.window?;
        Some(prompt_tokens as f64 / f64::from(window) * 100.0)
    }

    /// How many tokens at the end of the tail a run must leave verbatim
    /// (`deriveProtectedTailTokenTarget`): the model keeps what it is working
    /// on, and a fold cannot leave it with only the newest message.
    ///
    /// `usage_pct` shrinks the target as the window fills — at high pressure
    /// the alternative to a big fold is an overflow.
    pub fn protected_tail(&self, usage_pct: f64) -> u32 {
        if self.usable == 0 {
            // No known window: nothing to protect against, and protecting a
            // share of an invented number would block folds that used to run.
            return 0;
        }
        let usable = f64::from(self.usable);
        let usage = usage_pct.clamp(0.0, 100.0);
        let reserve = (usable * PROTECTED_HEADROOM_RATIO)
            .round()
            .max(PROTECTED_HEADROOM_MIN);
        let raw = (usable * PROTECTED_ALPHA * (1.0 - usage / 100.0)).round();
        let floor = (usable * PROTECTED_FLOOR_RATIO)
            .round()
            .clamp(PROTECTED_FLOOR_MIN, PROTECTED_FLOOR_MAX);
        let headroom = (f64::from(self.trigger) + reserve).min((usable * 0.5).floor());
        let ceiling = 1.0_f64.max(
            PROTECTED_ABS_CAP
                .min((usable * PROTECTED_MAX_USABLE_RATIO).floor())
                .min(usable - headroom),
        );
        ceiling.min(floor.max(raw)).max(0.0) as u32
    }
}

/// The share of the `m[0]` history budget the `<project-memory>` pinboard may
/// spend. The pinboard is a fixed prefix paid on every request, so it is
/// bounded by the history budget it rides beside rather than by a knob of its
/// own (the reference's `trimMemoriesToBudgetV2` shape: a share, clamped).
pub const MEMORY_BUDGET_SHARE: f64 = 0.25;
/// Floor of [`memory_budget`]: a tiny window still injects the most important
/// facts (a bare share would round a small history budget down to nothing, and
/// the pinboard is what makes a fresh session competent).
pub const MEMORY_BUDGET_MIN: usize = 1_000;
/// Ceiling of [`memory_budget`]: no window is large enough to justify paying
/// more than this on project memories every turn.
pub const MEMORY_BUDGET_MAX: usize = 8_000;

/// The token budget for the `<project-memory>` pinboard, derived from the
/// history budget it rides beside ([`MEMORY_BUDGET_SHARE`] of it, clamped to
/// [`MEMORY_BUDGET_MIN`]..=[`MEMORY_BUDGET_MAX`]).
///
/// A non-finite history budget takes the floor: the selection is conservative
/// on spend, and a zero budget would silently drop every memory.
pub fn memory_budget(history_tokens: f64) -> usize {
    if !history_tokens.is_finite() {
        return MEMORY_BUDGET_MIN;
    }
    ((history_tokens.max(0.0) * MEMORY_BUDGET_SHARE).round() as usize)
        .clamp(MEMORY_BUDGET_MIN, MEMORY_BUDGET_MAX)
}

/// `usable = window × execute%`, at least one token and never more than the
/// window itself.
fn usable_tokens(window: u32, execute_pct: f64) -> u32 {
    let usable = (f64::from(window) * execute_pct / 100.0).round();
    clamp_u32(usable, 1.0, f64::from(window.max(1)))
}

/// A configured percentage, brought into its accepted range. `NaN`/`inf` take
/// the default: `f64::clamp` would pass NaN through, and NaN truncates to 0.
fn percent(value: f64, default: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

fn clamp_u32(value: f64, min: f64, max: f64) -> u32 {
    value.clamp(min, max) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_budget_clamps_at_both_ends() {
        // A share of the history budget, clamped so a tiny window still buys a
        // usable pinboard and a large one cannot let the prefix grow forever.
        assert_eq!(memory_budget(0.0), MEMORY_BUDGET_MIN);
        assert_eq!(memory_budget(2_000.0), MEMORY_BUDGET_MIN); // 500 → floored
        assert_eq!(memory_budget(4_000.0), MEMORY_BUDGET_MIN); // exactly 25%
        assert_eq!(memory_budget(20_000.0), 5_000); // 25% inside the band
        assert_eq!(memory_budget(60_000.0), MEMORY_BUDGET_MAX); // 15 000 → capped
        assert_eq!(memory_budget(f64::NAN), MEMORY_BUDGET_MIN);
    }
}
