//! Local estimate → provider-token calibration.
//!
//! Everything in the context budget is written in **provider** tokens (the model
//! window, `usable = window × execute%`, `history = usable × share`), while the
//! fill decisions in `mctx` measure **local** tokens
//! ([`super::tokens::heuristic`]: `bytes / 4`). The two units drift apart by a
//! model-specific factor, so the local count is corrected here before it meets a
//! budget.
//!
//! # Where the ratio comes from
//!
//! The provider's reported `prompt_tokens` for a request ox itself built. It is
//! the only true anchor: it carries the chat template, the tool-call framing and
//! the cache accounting, none of which a local counter can reproduce. Each
//! assistant turn reports one for exactly the bytes we sent, so the observation
//! costs no extra round trip and no vendored vocabulary.
//!
//! Shape ported from `~/.ox/reference/magic-context`
//! (`docs/architecture/calibration.md`): the reference measures three class
//! ratios (`systemRatio` / `toolsRatio` / `proseRatio`, provider ÷ local) with a
//! probe harness, ships them as a seed table and freezes them per session. ox has
//! a better input — its own request and the provider's answer to it — so there is
//! no probe corpus and no per-class split: one blended ratio per session,
//! measured from the session's own turns. Blended is coarser than bucketed and
//! sufficient for the only decision it feeds, "does this much history still fit".
//!
//! # Why it freezes
//!
//! A ratio that kept drifting would change which content fits between two passes
//! that must replay identically, so the first [`SAMPLES_BEFORE_FREEZE`]
//! observations form a running mean and the ratio is fixed from then on. A model
//! switch inside one session starts over: a different model is a different
//! tokenizer and a different chat template.

/// Observations folded into the running mean before the ratio freezes.
const SAMPLES_BEFORE_FREEZE: u32 = 4;

/// The band a single sample may land in. Below it the provider reported
/// implausibly few tokens for the bytes we sent (a truncated request, a vendor
/// that bills differently); above it, the same in the other direction. Both
/// clamp instead of trusting one bad sample.
const MIN_RATIO: f64 = 0.5;
const MAX_RATIO: f64 = 2.0;

/// One session's measured provider ÷ local ratio.
///
/// Pure state: [`observe`](Self::observe) is the whole decision, so it is tested
/// without a session, a database or a clock.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Calibration {
    /// The model the ratio belongs to; a switch invalidates it.
    model: Option<String>,
    ratio: f64,
    samples: u32,
}

impl Calibration {
    /// The ratio to scale local counts by: `1.0` until the first sample.
    #[must_use]
    pub fn ratio(&self, provider: &str, model: &str) -> f64 {
        let identity = format!("{provider}/{model}");
        if self.samples == 0 || self.model.as_deref() != Some(identity.as_str()) {
            1.0
        } else {
            self.ratio
        }
    }

    /// Whether one more observation would still move the ratio for `model`.
    ///
    /// The caller uses this to skip the measurement work entirely once the
    /// ratio is frozen — the observation costs a serialization of the request.
    /// A different model is a fresh measurement and always wants a sample.
    #[must_use]
    pub fn wants_sample(&self, provider: &str, model: &str) -> bool {
        let identity = format!("{provider}/{model}");
        self.samples < SAMPLES_BEFORE_FREEZE || self.model.as_deref() != Some(identity.as_str())
    }

    /// Fold one `(provider prompt_tokens, local estimate)` observation in.
    ///
    /// `model` identifies which model produced the sample. A different model in
    /// the same session starts the measurement over rather than blending two
    /// tokenizers. A zero on either side is not a measurement and is ignored.
    pub fn observe(
        &mut self,
        provider: &str,
        model: &str,
        provider_tokens: u64,
        local_tokens: usize,
    ) {
        let identity = format!("{provider}/{model}");
        if self.model.as_deref() != Some(identity.as_str()) {
            *self = Self {
                model: Some(identity),
                ..Self::default()
            };
            // …and fall through: the triggering observation is the first sample
            // of the new model.
        }
        if self.samples >= SAMPLES_BEFORE_FREEZE || provider_tokens == 0 || local_tokens == 0 {
            return;
        }
        let sample = (provider_tokens as f64 / local_tokens as f64).clamp(MIN_RATIO, MAX_RATIO);
        let taken = f64::from(self.samples);
        self.ratio = (self.ratio * taken + sample) / (taken + 1.0);
        self.samples += 1;
    }
}

/// Process-wide session → calibration table, held by the runner.
///
/// Per **session**, not per model and not global: one session's measurement
/// cannot leak into another's, a fresh session starts neutral and re-measures in
/// one turn, and a forgotten session removes its entry with the rest of its
/// state ([`forget`]).
pub(crate) type Store =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Calibration>>>;

/// The session's ratio, `1.0` when unmeasured.
pub(crate) fn ratio_for(store: &Store, session_id: &str, provider: &str, model: &str) -> f64 {
    lock(store)
        .get(session_id)
        .map_or(1.0, |calibration| calibration.ratio(provider, model))
}

/// Whether the session still needs an observation for `model` (cheap check,
/// taken before the measurement work).
pub(crate) fn wants_sample(store: &Store, session_id: &str, provider: &str, model: &str) -> bool {
    lock(store)
        .get(session_id)
        .is_none_or(|calibration| calibration.wants_sample(provider, model))
}

/// Record one observation for the session.
pub(crate) fn observe(
    store: &Store,
    session_id: &str,
    provider_id: &str,
    model: &str,
    provider_tokens: u64,
    local: usize,
) {
    lock(store)
        .entry(session_id.to_owned())
        .or_default()
        .observe(provider_id, model, provider_tokens, local);
}

/// Drop a session's calibration (idle sweep, session delete, shutdown).
pub(crate) fn forget(store: &Store, session_id: &str) {
    lock(store).remove(session_id);
}

fn lock(
    store: &Store,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Calibration>> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::Calibration;

    #[test]
    fn calibration_resets_when_provider_changes_for_same_model_id() {
        let mut calibration = Calibration::default();
        calibration.observe("cardinal", "shared", 200, 100);

        assert_eq!(calibration.ratio("cardinal", "shared"), 2.0);
        assert_eq!(calibration.ratio("deepseek", "shared"), 1.0);

        calibration.observe("deepseek", "shared", 100, 100);

        assert_eq!(calibration.ratio("deepseek", "shared"), 1.0);
    }
}
