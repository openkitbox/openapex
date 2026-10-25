//! Token estimates for the context budget walks.
//!
//! One shape: `bytes / 4`, scaled by the session's measured provider ratio
//! ([`crate::mctx::calibration`]). Nothing here loads a vocabulary.
//!
//! # Why there is no exact tokenizer any more
//!
//! This module used to hold a per-family exact counter: a local count-only BPE
//! replaying `tokenizer.json` for GLM and DeepSeek (with the HF `tokenizers`
//! crate as its fallback) and a `tiktoken` `CoreBPE` for Kimi. What it bought was
//! a better number in one place — how much history fits the `m[0]` fill budget —
//! which is a *range* decision: the budget is 15% of a usable window that is
//! itself 65% of the model's. An estimate off by a third still cannot overrun
//! the request; it only fills less history than it could.
//!
//! Two independent lines of evidence say the vocabulary was the wrong lever:
//!
//! - **The provider's reported `prompt_tokens` is the only truth**, and no local
//!   vocabulary reproduces it: it carries the chat template, the tool-call
//!   framing and the cache accounting. Measured on the reference harness
//!   (`~/.ox/reference/magic-context`,
//!   `packages/plugin/src/hooks/magic-context/tokenizer-calibration-seeds.json`),
//!   the same tool definitions cost `0.84×` on GPT-5 and `1.55×` on Claude-5
//!   relative to a local count — a spread no tokenizer closes.
//! - **The peers do not count exactly either**: opencode is `len/4`,
//!   deepseek-harness is `chars/4` plus per-block overhead, fx is per-span
//!   `ceil(bytes/4)` corrected by a measured ratio, and the reference this
//!   module tree was ported from ships exactly one vendored vocabulary for *all*
//!   providers, correcting the drift with per-model ratios.
//!
//! So the estimate is `bytes / 4` and the correction lives in
//! [`crate::mctx::calibration`], anchored on the provider's own numbers. Every
//! budget decision takes the calibrated counter, the historian's chunk fence
//! included: its text never reaches the wire verbatim, but both of the fence's
//! ceilings are derived from a model window, so counting it uncalibrated would
//! compare two scales. One caveat, documented rather than hidden: the ratio
//! belongs to the session's model while the fence is measured against the
//! historian model's window; when `magic_context.historian_model` names another
//! model ox has no measurement for it (the hidden completion's usage is not
//! observed), so the session's ratio is the closest available estimate.

/// The estimate: four bytes per token, rounded up.
///
/// Its single owner. The calibrated counter wraps it; so does the observation
/// that measures the ratio (it counts the outbound body, the local side of the
/// division) and the prompt-injection size log, which is a scale signal. Every
/// budget decision goes through [`counter_fn`] instead.
#[must_use]
pub fn heuristic(text: &str) -> usize {
    heuristic_bytes(text.len())
}

/// [`heuristic`] for a length already known.
///
/// The ratio observation measures the outbound request, and the only fact it
/// needs from it is how many bytes it serializes to: building the body as a
/// string to call `len()` on it would cost a transcript-sized copy per sampled
/// step. Same arithmetic, so the two agree by construction.
#[must_use]
pub fn heuristic_bytes(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

/// The session's counter in the shape the budget walks already take.
///
/// Every walk is written against `impl Fn(&str) -> usize` (or `&dyn Fn`) and
/// resolves its counter once at the boundary, so this is that shape: the
/// estimate scaled by `ratio`, the provider ÷ local measurement from
/// [`crate::mctx::calibration`]. With no measurement yet (`ratio == 1.0`) it is
/// exactly [`heuristic`].
///
/// `ratio` is a `Copy` number resolved by the caller, not a cached handle: it
/// cannot change while a walk is in flight, and a non-finite or non-positive
/// ratio falls back to neutral rather than poisoning every budget with a NaN.
///
/// Rounding: each *piece* is rounded up, so a calibrated pass can over-count by
/// at most one token per counted piece (a few dozen on a 25k budget). That is
/// the conservative direction — the fill stops earlier, never later.
pub fn counter_fn(ratio: f64) -> impl Fn(&str) -> usize + Sync + 'static {
    let ratio = if ratio.is_finite() && ratio > 0.0 {
        ratio
    } else {
        1.0
    };
    move |text| {
        let count = heuristic(text);
        if ratio == 1.0 {
            return count;
        }
        ((count as f64) * ratio).ceil() as usize
    }
}
