//! Settled token usage for one attempt, and the process-wide prompt-cache
//! ledger the diagnostics read.
//!
//! The provider's own numbers are the only truth about what a prompt cost
//! (`prompt_tokens`) and how much of it its prefix cache served
//! (`cached_tokens`). Both land in [`TokenUsage`] and are accumulated per
//! session in a [`CacheLedger`], so a diagnostic log line can report the cache
//! hit ratio without any request byte depending on it.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use ox_llm::cardinal::wire::{ChatRequest, Usage};
use ox_schema::support::{TokenCache, TokenUsage};

use super::super::drain::DrainCtx;

/// The prompt split the provider reported: `(cache read, uncached input)`,
/// where the uncached half is what it had to prefill. The token shape and the
/// session ledger both derive from this one place, so a reported prompt is
/// accounted identically in the row and in the aggregate.
fn prompt_split(usage: &Usage) -> (i64, i64) {
    let cached = usage.cached_tokens.unwrap_or(0);
    (
        cached as i64,
        usage.prompt_tokens.saturating_sub(cached) as i64,
    )
}

/// How one session's prompts were served, accumulated over its settled
/// attempts.
///
/// A `folds` counter is deliberately absent. A fold reaches a pass as
/// `Action::Hard`, and `PassOutcome` collapses that together with
/// `Action::Soft` into one `BustApplied` (`crate::mctx::cache`), while the
/// runner's only pass call site (`mctx_head`) keeps just the head halves and
/// the floor and never reads `PassResult::outcome`. Counting folds would need a
/// `PassOutcome` that tells a hard rebuild from a soft delta, or a fold mark
/// carried on the pass request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionCache {
    /// Prompt tokens the provider served from its prefix cache.
    pub read: i64,
    /// Input tokens the provider had to prefill (`prompt_tokens - cached`).
    pub uncached: i64,
}

/// Process-wide prompt-cache accounting, per session.
///
/// The sibling of [`crate::mctx::calibration`]'s store: held by the runner, one
/// entry per session, dropped with the rest of that session's process state
/// (`Inner::forget` in the runner). Only provider-reported numbers are folded
/// in — an attempt whose stream carried no usage reported an estimate, and an
/// estimate in this table would be indistinguishable from a cache miss the
/// provider never saw.
#[derive(Debug, Default)]
pub(crate) struct CacheLedger {
    sessions: Mutex<HashMap<String, SessionCache>>,
}

impl CacheLedger {
    /// Fold one settled attempt's reported split into its session.
    pub(crate) fn record_attempt(&self, session: &str, read: i64, uncached: i64) {
        let mut sessions = lock(&self.sessions);
        let entry = sessions.entry(session.to_owned()).or_default();
        entry.read = entry.read.saturating_add(read);
        entry.uncached = entry.uncached.saturating_add(uncached);
    }

    /// Every session `keep` accepts, sorted by session id so a periodic log
    /// line stays stable while unrelated sessions come and go.
    pub(crate) fn snapshot(&self, keep: impl Fn(&str) -> bool) -> Vec<(String, SessionCache)> {
        let mut rows: Vec<(String, SessionCache)> = lock(&self.sessions)
            .iter()
            .filter(|(session, _)| keep(session))
            .map(|(session, cache)| (session.clone(), *cache))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// Drop a session's entry along with the rest of its runner state.
    pub(crate) fn forget(&self, session: &str) {
        lock(&self.sessions).remove(session);
    }
}

fn lock(
    sessions: &Mutex<HashMap<String, SessionCache>>,
) -> MutexGuard<'_, HashMap<String, SessionCache>> {
    sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record one settled attempt in the session ledger. `usage` is the provider's
/// report; `None` means the stream carried none, which is an estimate and
/// therefore records nothing.
fn record_attempt(ledger: &CacheLedger, session: &str, usage: Option<Usage>) {
    let Some(usage) = usage else {
        return;
    };
    let (read, uncached) = prompt_split(&usage);
    ledger.record_attempt(session, read, uncached);
}

/// Upstream token shape (`SessionUsage.tokens`): input excludes cached
/// prompt tokens, cache write is not reported by cardinal's OpenAI dialect.
/// A stream that never carried usage stores `estimate()` as `input` and
/// zeros elsewhere; the estimate is not evaluated when usage was reported.
/// The estimate branch sets `estimated: Some(true)` so estimated rows can be
/// told from reported ones (`json_extract(data, '$.tokens.estimated')`); the
/// reported branch leaves it `None`.
fn tokens_for(usage: Option<Usage>, estimate: impl FnOnce() -> i64) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage {
            input: estimate(),
            output: 0,
            reasoning: 0,
            cache: TokenCache { read: 0, write: 0 },
            estimated: Some(true),
            reasoning_reported: Some(false),
        };
    };
    let (read, input) = prompt_split(&usage);
    let (output, reasoning, reasoning_reported) = match (
        usage.reasoning_tokens_included(),
        usage.reasoning_tokens,
    ) {
        (Some(true), Some(reasoning)) if reasoning <= usage.completion_tokens => (
            (usage.completion_tokens - reasoning) as i64,
            reasoning as i64,
            Some(true),
        ),
        (Some(true), Some(_)) => {
            tracing::warn!(
                "cardinal reported reasoning tokens greater than completion tokens; ignoring reasoning"
            );
            (usage.completion_tokens as i64, 0, Some(false))
        }
        (_, Some(reasoning)) => (
            usage.completion_tokens as i64,
            reasoning as i64,
            Some(false),
        ),
        (_, None) => (usage.completion_tokens as i64, 0, Some(false)),
    };
    TokenUsage {
        input,
        output,
        reasoning,
        cache: TokenCache { read, write: 0 },
        estimated: None,
        reasoning_reported,
    }
}

/// Prompt-size estimate for a request body of `body_len` bytes:
/// `ceil(bytes / 4 * ratio)`. A non-finite or negative `ratio` counts as 1.0;
/// the product saturates at `i64::MAX`.
fn estimate_prompt_tokens(body_len: usize, ratio: f64) -> i64 {
    let ratio = if ratio.is_finite() && ratio >= 0.0 {
        ratio
    } else {
        1.0
    };
    let raw = crate::mctx::tokens::heuristic_bytes(body_len) as f64 * ratio;
    // `as` saturates for out-of-range floats, so this clamps to i64::MAX.
    raw.ceil() as i64
}

/// Settled token usage for one attempt. When the stream carried no usage the
/// prompt size is estimated from the request that was sent (its serialized
/// length is measured, not built); a serialization failure falls back to 0.
///
/// The same settled numbers feed the session's prompt-cache ledger: exactly one
/// accumulation per settled attempt, reported usage only.
pub(super) fn attempt_tokens(
    ctx: &DrainCtx,
    provider_id: &str,
    model_id: &str,
    request: &ChatRequest,
    usage: Option<Usage>,
) -> TokenUsage {
    record_attempt(&ctx.cache_ledger, &ctx.session_id, usage);
    tokens_for(usage, || {
        let body_len = match ox_llm::cardinal::wire::serialized_len(request) {
            Ok(len) => len,
            Err(error) => {
                tracing::warn!(session = %ctx.session_id, error = %error, "prompt estimate skipped");
                return 0;
            }
        };
        let ratio = crate::mctx::calibration::ratio_for(
            &ctx.calibrations,
            &ctx.session_id,
            provider_id,
            model_id,
        );
        estimate_prompt_tokens(body_len, ratio)
    })
}

/// Fold the provider's own count for this request into the session's token
/// calibration ([`crate::mctx::calibration`]).
///
/// The local side is `bytes / 4` over the exact JSON the wire carried, so the
/// ratio absorbs everything a local counter cannot see: the chat template, the
/// tool framing, the vendor's own tokenizer and the cache accounting. The
/// request is post pairing-guard and post every message push, i.e. the bytes the
/// provider was billed for.
///
/// Best effort, and only while it can still move the ratio: the session asks
/// for at most [`crate::mctx::calibration`]'s freeze window of samples, so the
/// per-chunk usage events after that cost a map lookup and nothing else. A
/// serialization failure only skips the sample; the observation never fails or
/// blocks the step.
pub(super) fn observe_request_cost(
    ctx: &DrainCtx,
    provider_id: &str,
    model_id: &str,
    request: &ChatRequest,
    prompt_tokens: u64,
) {
    if !crate::mctx::calibration::wants_sample(
        &ctx.calibrations,
        &ctx.session_id,
        provider_id,
        model_id,
    ) {
        return;
    }
    // Only the body's size is needed, so it is measured rather than built: a
    // transcript-sized copy per sampled step buys nothing here.
    let Ok(body_len) = ox_llm::cardinal::wire::serialized_len(request) else {
        return;
    };
    crate::mctx::calibration::observe(
        &ctx.calibrations,
        &ctx.session_id,
        provider_id,
        model_id,
        prompt_tokens,
        crate::mctx::tokens::heuristic_bytes(body_len),
    );
}
