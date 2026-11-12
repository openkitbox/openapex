//! Model-window geometry and the four token budgets derived from it.
//!
//! Everything that measures context pressure or decides how much history the
//! head carries reads its numbers from one [`Budget`], derived once per drain
//! from the model catalogue. Four budgets, not one, because they answer four
//! different questions (upstream `derive-budgets.ts`):
//!
//! | field | question | anchor |
//! |---|---|---|
//! | [`Budget::trigger`] | how long may the unfolded tail grow | main model's usable window |
//! | [`Budget::history`] | how much history may `m[0]` carry | main model's usable window |
//! | [`Budget::target`] | how far down must a pressure event fold | main model's window |
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
use ox_core::config::{
    default_execute_threshold_percentage, default_fold_target_percentage,
    default_history_budget_percentage,
};

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
/// Margin between the low-water target and the proactive floor. A target at
/// or above that floor is unreachable while pressure is active: the band stops
/// the event first and immediately arms another one at the next step boundary.
const TARGET_MARGIN_PCT: f64 = 5.0;

/// The bounds of the three config values every number here derives from:
/// `[magic_context] execute_threshold_percentage`, `history_budget_percentage`
/// and `fold_target_percentage`.
/// They live beside the config they constrain (`ox_core::config`).
pub use ox_core::config::{MAX_EXECUTE_PCT, MAX_FOLD_TARGET_PCT, MAX_HISTORY_SHARE};
pub use ox_core::config::{MIN_EXECUTE_PCT, MIN_FOLD_TARGET_PCT, MIN_HISTORY_SHARE};

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
    /// The low-water target in tokens (`window × fold_target_percentage`): a
    /// pressure event keeps folding until its post-fold estimate reaches this.
    /// `None` when the window is unknown — with no target there is nothing to
    /// fold down to, not a single-fold limit. The bands and per-event cap still
    /// bound the event; only a session with no reported usage folds just once.
    pub target: Option<u32>,
    /// The usable working space (`window × execute_threshold`), kept because
    /// the protected tail is a fraction of it.
    usable: u32,
    /// Tokens the request carries before any conversation: the static system
    /// layer and the tools array. Subtracted from both sides of the usage
    /// reading ([`Self::usage_percentage`]) so the percentage measures the
    /// conversation's share of what the conversation can actually use.
    baseline: u32,
}

impl Budget {
    #[must_use]
    pub fn window(&self) -> Option<u32> {
        self.window
    }
    /// Derive the budgets for one session.
    ///
    /// `window` is the main model's context window, `historian` the window of
    /// the model that backs the fold. A missing main window falls back to the
    /// pre-budget numbers ([`FALLBACK_TRIGGER`] and friends) rather than to a
    /// different code path: the "gateway is down" case must not change the
    /// compression strategy, only where its numbers come from.
    #[must_use]
    pub fn derive(config: &MctxConfig, window: Option<u32>, historian: Option<u32>) -> Self {
        // The config is validated where it is loaded, but this is `pub` and
        // `MctxConfig` is plain data: a NaN carried in from a test or a future
        // caller would truncate to `history = 0`, and a zero history budget
        // silently disables decay (`render_m0` renders everything at P1). The
        // clamp belongs to the only place that turns the numbers into budgets.
        let execute = percent(
            config.execute_threshold_percentage,
            default_execute_threshold_percentage(),
            MIN_EXECUTE_PCT,
            MAX_EXECUTE_PCT,
        );
        let soft_pct = (execute - SOFT_OFFSET_PCT).max(0.0);
        let history_share = percent(
            config.history_budget_percentage,
            default_history_budget_percentage(),
            MIN_HISTORY_SHARE,
            MAX_HISTORY_SHARE,
        );
        // The raw percentage is clamped during config validation, but the
        // derived target must also stay below the soft band. Otherwise a
        // reachable stop becomes unreachable: pressure clears before the
        // target is reached, so every step boundary would fold again.
        let fold_target = percent(
            config.fold_target_percentage,
            default_fold_target_percentage(),
            MIN_FOLD_TARGET_PCT,
            MAX_FOLD_TARGET_PCT,
        )
        .min(soft_pct - TARGET_MARGIN_PCT);
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
                soft_pct,
                hard_pct: (execute + 2.0).max(HARD_FLOOR_PCT),
                trigger: FALLBACK_TRIGGER,
                history: FALLBACK_HISTORY,
                historian_chunk,
                target: None,
                usable: 0,
                baseline: 0,
            };
        };
        let usable = usable_tokens(window, execute);
        Self {
            window: Some(window),
            // Upstream derives both bands from the execute threshold, never
            // from each other: a misconfigured pair cannot invert them.
            soft_pct,
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
            // A percentage of a real window, clamped into `1..=window`: a tiny
            // window still gets a target of one token rather than zero, which
            // would read as "already reached" and stop every event early.
            target: Some(clamp_u32(
                (f64::from(window) * fold_target / 100.0).round(),
                1.0,
                f64::from(window.max(1)),
            )),
            usable,
            baseline: 0,
        }
    }

    /// The budget with its fixed-layer baseline set (see [`Self::baseline`]).
    /// Builder-style so [`Self::derive`] stays the one place the numbers come
    /// from and the caller that knows the request's static layers adds the one
    /// it measured.
    #[must_use]
    pub fn with_baseline(mut self, baseline_tokens: u32) -> Self {
        self.baseline = baseline_tokens;
        self
    }

    /// The share of the main model's window the newest assistant turn filled,
    /// or `None` when the window is unknown (the pressure is then measured from
    /// the tail alone).
    ///
    /// `prompt_tokens` is what the provider reported for that turn —
    /// `tokens.input + tokens.cache.read`, the same sum the client's footer
    /// divides by this same window.
    ///
    /// The fixed baseline ([`Self::with_baseline`]) is taken off both sides:
    /// a 20k system layer in a 128k window is 16% "used" before the first
    /// word, and on a small window it can sit above the fold target, so a
    /// reading that counted it would fold early and never reach its stop.
    /// Measured against the room the conversation has, the empty session
    /// reads 0% and the thresholds mean what they say. A baseline at or over
    /// the window leaves no room; the reading then degrades to the raw share.
    #[must_use]
    pub fn usage_percentage(&self, prompt_tokens: i64) -> Option<f64> {
        let window = f64::from(self.window?);
        let baseline = f64::from(self.baseline);
        if baseline <= 0.0 || baseline >= window {
            return Some(prompt_tokens as f64 / window * 100.0);
        }
        let used = (prompt_tokens as f64 - baseline).max(0.0);
        Some(used / (window - baseline) * 100.0)
    }

    /// Conservative token ceiling for the volatile cache head. The fixed
    /// request layers (system/instructions/tools) are `baseline`; the frozen
    /// baseline is bounded by `m0_tokens`; and `trigger` reserves room for the
    /// live tail. The usable window also reserves the configured pressure margin.
    /// An unknown window retains the conservative fallback history ceiling.
    /// The request-budget layer still derives the output cap from
    /// the final serialized request, so this gate never relies on a guessed
    /// output size to admit an over-window input.
    #[must_use]
    pub fn conservative_m1_budget(&self, m0_tokens: usize) -> usize {
        let fixed = u64::from(self.baseline)
            .saturating_add(u64::try_from(m0_tokens).unwrap_or(u64::MAX))
            .saturating_add(u64::from(self.trigger));
        let available = self.window.map_or(u64::from(self.history), |_| {
            u64::from(self.usable).saturating_sub(fixed)
        });
        available.try_into().unwrap_or(usize::MAX)
    }

    /// How many tokens at the end of the tail a run must leave verbatim
    /// (`deriveProtectedTailTokenTarget`): the model keeps what it is working
    /// on, and a fold cannot leave it with only the newest message.
    ///
    /// `usage_pct` shrinks the target as the window fills — at high pressure
    /// the alternative to a big fold is an overflow.
    #[must_use]
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
#[must_use]
pub fn memory_budget(history_tokens: f64) -> usize {
    share_of_history(
        history_tokens,
        MEMORY_BUDGET_SHARE,
        MEMORY_BUDGET_MIN..=MEMORY_BUDGET_MAX,
    )
}

/// Share of the history budget the `<user-profile>` tier may take: a fifth of
/// the pinboard's. A profile is a handful of universal observations, not a
/// project's accumulated knowledge, and it is paid in *every* project.
pub const USER_PROFILE_BUDGET_SHARE: f64 = 0.05;
/// Floor of [`user_profile_budget`]: room for a few observations always.
pub const USER_PROFILE_BUDGET_MIN: usize = 300;
/// Ceiling of [`user_profile_budget`].
pub const USER_PROFILE_BUDGET_MAX: usize = 1_500;

/// The token budget for the `<user-profile>` block, derived like
/// [`memory_budget`] with its own, smaller share and clamp.
#[must_use]
pub fn user_profile_budget(history_tokens: f64) -> usize {
    share_of_history(
        history_tokens,
        USER_PROFILE_BUDGET_SHARE,
        USER_PROFILE_BUDGET_MIN..=USER_PROFILE_BUDGET_MAX,
    )
}

/// `share` of the history budget, clamped to `bounds`. A non-finite history
/// budget takes the floor: the selection is conservative on spend, and a zero
/// budget would silently drop every memory.
fn share_of_history(
    history_tokens: f64,
    share: f64,
    bounds: std::ops::RangeInclusive<usize>,
) -> usize {
    if !history_tokens.is_finite() {
        return *bounds.start();
    }
    ((history_tokens.max(0.0) * share).round() as usize).clamp(*bounds.start(), *bounds.end())
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
