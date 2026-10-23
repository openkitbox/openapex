//! Provider failure classification, ported from
//! `packages/ai/src/provider-error.ts` (the context-overflow slice).
//!
//! [`is_context_overflow`] matches the curated provider-specific pattern
//! list verbatim, guarded by exclusions (rate-limit phrasing, and Cohere's
//! output-limit `max_tokens must be less than`, which compaction cannot
//! fix) and extended with the no-body fallback
//! (`400 (no body)` / `413 (no body)`). [`is_payload_too_large`] carries the
//! payload-phrasing list. Status scoping (4xx client errors, 413) happens in
//! [`is_overflow_error`], the step's entry point.

use regex::Regex;
use std::sync::OnceLock;

use crate::cardinal::CardinalError;

/// Upstream `patterns`: one [`Regex`] source per line, all case-insensitive.
const PATTERN_SOURCES: &[&str] = &[
    r"prompt is too long",
    r"input is too long for requested model",
    // Cloudflare Workers AI reports this as HTTP 413.
    r"exceeded this model context window limit",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum",
    // Amazon Nova on Bedrock reports this as a mid-stream validationException.
    r"number of input tokens exceeds maximum length",
    r"tokens in request more than max tokens allowed",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    // DeepInfra
    r"requested input length \d+ exceeds maximum input length",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    // Novita omits the token counts.
    r"input(?: \(\d+ tokens\))? is longer than the model'?s context length",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    // Hugging Face Text Generation Inference, e.g. Together
    r"`inputs` tokens \+ `max_new_tokens` must be <= \d+",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"context[_ ]length[_ ]exceeded",
    r"context length is only \d+ tokens",
    r"input length.*exceeds.*context length",
    // Z.ai code 1261 arrives as `Prompt too long` or `Prompt 超长`.
    r"prompt (?:too long|超长)",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"range of input length should be",
    r"too many tokens",
    r"token limit exceeded",
    r"request_too_large",
];

/// Upstream `payloadPatterns`.
const PAYLOAD_SOURCES: &[&str] = &[
    r"request entity too large",
    r"payload too large",
    r"request too large",
];

/// Upstream `exclusions`.
const EXCLUSION_SOURCES: &[&str] = &[
    r"^(throttling error|service unavailable):",
    r"rate limit",
    r"too many requests",
    // Cohere reports an output limit above the model maximum as "too many
    // tokens"; compaction cannot fix it.
    r"max[_ ]tokens must be less than",
];

/// Upstream's no-body fallback: `^4(?:00|13)\s*(status code)?\s*\(no body\)`.
const NO_BODY_SOURCE: &str = r"^4(?:00|13)\s*(status code)?\s*\(no body\)";

fn compiled(sources: &[&str]) -> Vec<Regex> {
    sources
        .iter()
        .filter_map(|source| match Regex::new(&format!(r"(?i){source}")) {
            Ok(regex) => Some(regex),
            Err(error) => {
                tracing::error!(pattern = source, %error, "provider-error pattern failed to compile");
                None
            }
        })
        .collect()
}

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| compiled(PATTERN_SOURCES))
}

fn payload_patterns() -> &'static [Regex] {
    static PAYLOAD: OnceLock<Vec<Regex>> = OnceLock::new();
    PAYLOAD.get_or_init(|| compiled(PAYLOAD_SOURCES))
}

fn exclusions() -> &'static [Regex] {
    static EXCLUSIONS: OnceLock<Vec<Regex>> = OnceLock::new();
    EXCLUSIONS.get_or_init(|| compiled(EXCLUSION_SOURCES))
}

/// The compiled no-body fallback; a compile failure (impossible for the
/// constant, asserted by a unit test) simply never matches.
fn no_body_matches(message: &str) -> bool {
    static NO_BODY: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    NO_BODY
        .get_or_init(|| Regex::new(&format!(r"(?i){NO_BODY_SOURCE}")))
        .as_ref()
        .is_ok_and(|regex| regex.is_match(message))
}

/// Upstream `isContextOverflow`.
pub(crate) fn is_context_overflow(message: &str) -> bool {
    !exclusions().iter().any(|pattern| pattern.is_match(message))
        && (patterns().iter().any(|pattern| pattern.is_match(message)) || no_body_matches(message))
}

/// Upstream `isPayloadTooLarge`.
pub(crate) fn is_payload_too_large(message: &str) -> bool {
    payload_patterns()
        .iter()
        .any(|pattern| pattern.is_match(message))
}

/// Whether a cardinal error is the context-overflow (or payload-too-large)
/// rejection the step recovers from by compacting (upstream
/// `classifyProviderFailure`: the overflow classification is scoped to
/// client-side 4xx rejections; an in-band upstream failure carries no status
/// and matches on the body alone).
pub(crate) fn is_overflow_error(error: &CardinalError) -> bool {
    match error {
        CardinalError::Http { status, body } => {
            (400..=499).contains(status)
                && (is_context_overflow(body) || *status == 413 || is_payload_too_large(body))
        }
        CardinalError::Upstream(body) => is_context_overflow(body) || is_payload_too_large(body),
        _ => false,
    }
}
