use ox_llm::cardinal::wire::{ChatRequest, Usage};
use ox_schema::support::{TokenCache, TokenUsage};

use super::super::drain::DrainCtx;
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
        };
    };
    let cached = usage.cached_tokens.unwrap_or(0);
    TokenUsage {
        input: (usage.prompt_tokens.saturating_sub(cached)) as i64,
        output: usage.completion_tokens as i64,
        reasoning: usage.reasoning_tokens.unwrap_or(0) as i64,
        cache: TokenCache {
            read: cached as i64,
            write: 0,
        },
        estimated: None,
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
pub(super) fn attempt_tokens(
    ctx: &DrainCtx,
    request: &ChatRequest,
    usage: Option<Usage>,
) -> TokenUsage {
    tokens_for(usage, || {
        let body_len = match ox_llm::cardinal::wire::serialized_len(request) {
            Ok(len) => len,
            Err(error) => {
                tracing::warn!(session = %ctx.session_id, error = %error, "prompt estimate skipped");
                return 0;
            }
        };
        let ratio = crate::mctx::calibration::ratio_for(&ctx.calibrations, &ctx.session_id);
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
    model_id: &str,
    request: &ChatRequest,
    prompt_tokens: u64,
) {
    if !crate::mctx::calibration::wants_sample(&ctx.calibrations, &ctx.session_id, model_id) {
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
        model_id,
        prompt_tokens,
        crate::mctx::tokens::heuristic_bytes(body_len),
    );
}
