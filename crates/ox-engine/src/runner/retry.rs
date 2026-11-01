//! Retry policy for transient provider failures.
//!
//! Owns two decisions and nothing else: which [`ChatError`]s are worth
//! retrying ([`classify`]) and how long to wait, or whether to give up
//! ([`Backoff::on_failure`]). Pure, no I/O; the caller (`step.rs`) owns
//! events, sleeping and settlement.
//!
//! [`Backoff`] owns three independent counters for one step:
//!
//! - `network`: consecutive [`RetryKind::Network`] failures; drives the delay
//!   ladder `min(2s * 2^(n-1), 10s)`, never exhausts, and restarts whenever
//!   an attempt received model output (progress was made).
//! - `bounded`: [`RetryKind::Bounded`] failures, capped at
//!   [`MAX_BOUNDED_RETRIES`]; never reset, not even by progress. A long
//!   network outage does not consume this allowance.
//! - `continuations`: attempts that failed after receiving model output,
//!   capped at [`MAX_CONTINUATIONS`], whatever the error kind.
//!
//! Deliberate deviations from upstream `model-retry.ts`: network-class errors
//! retry without limit, the per-step continuation cap, and there is no jitter
//! and no `retry-after` handling (`ChatError::Http` carries no headers;
//! single user, no thundering herd).

use std::time::Duration;

use ox_llm::cardinal::ChatError;

/// Maximum consecutive failures for [`RetryKind::Bounded`] errors (upstream
/// `model-retry.ts` `recurs(10)`).
pub(crate) const MAX_BOUNDED_RETRIES: usize = 10;

/// Maximum attempts per step that fail after receiving model output. A
/// flapping upstream that cuts every stream after a few tokens must not loop
/// forever: each cycle costs a billed request, a partial assistant row and a
/// synthetic continue message.
pub(crate) const MAX_CONTINUATIONS: usize = 10;

const BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(10);

/// How an error should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryKind {
    /// Fail immediately.
    Never,
    /// Retry up to [`MAX_BOUNDED_RETRIES`] consecutive failures.
    Bounded,
    /// Retry until it recovers (or the user interrupts).
    Network,
}

/// Classify a provider error. Exhaustive on purpose: a new variant must get an
/// explicit decision here.
pub(crate) fn classify(error: &ChatError) -> RetryKind {
    match error {
        ChatError::Transport(e) => {
            // `is_decode()` is a body read failure (reqwest wraps every
            // `Response::chunk()` error as decode; ox-llm never parses JSON
            // through reqwest), so it falls through to `Network`.
            if e.is_builder() {
                RetryKind::Never
            } else if e.is_redirect() {
                RetryKind::Bounded
            } else {
                RetryKind::Network
            }
        }
        ChatError::EarlyClose => RetryKind::Network,
        ChatError::Http { status, .. } => match *status {
            408 | 429 | 500.. => RetryKind::Bounded,
            _ => RetryKind::Never,
        },
        ChatError::Upstream(_) | ChatError::Sse(_) => RetryKind::Bounded,
        ChatError::NoRoute { .. } => RetryKind::Never,
    }
}

/// Delay for the `n`-th consecutive failure: `min(2s * 2^(n-1), 10s)`;
/// `n == 0` counts as 1.
fn ladder(n: usize) -> Duration {
    // Clamp the shift: 2s * 2^4 already exceeds the cap.
    let shift = u32::try_from(n.max(1) - 1).unwrap_or(u32::MAX).min(4);
    BASE_DELAY.saturating_mul(1 << shift).min(MAX_DELAY)
}

/// Retry state of one step: decides the delay before the next attempt, or
/// `None` when the step must fail. See the module doc for the counters.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    network: usize,
    bounded: usize,
    continuations: usize,
}

impl Backoff {
    /// Record a failed attempt and return the delay before the next one, or
    /// `None` to give up. `produced` says the attempt received model output
    /// before failing.
    ///
    /// - `Never` => `None`, counters untouched.
    /// - `produced` => `continuations += 1` and the network ladder restarts;
    ///   more than [`MAX_CONTINUATIONS`] => `None`, for any kind.
    /// - `Network` => `ladder(network)`, never exhausts.
    /// - `Bounded` => `ladder(bounded)`; more than [`MAX_BOUNDED_RETRIES`]
    ///   => `None`. Never reset, not even by progress.
    pub(crate) fn on_failure(&mut self, kind: RetryKind, produced: bool) -> Option<Duration> {
        if kind == RetryKind::Never {
            return None;
        }
        if produced {
            self.continuations = self.continuations.saturating_add(1);
            self.network = 0;
            if self.continuations > MAX_CONTINUATIONS {
                return None;
            }
        }
        match kind {
            RetryKind::Never => None,
            RetryKind::Network => {
                self.network = self.network.saturating_add(1);
                Some(ladder(self.network))
            }
            RetryKind::Bounded => {
                self.bounded = self.bounded.saturating_add(1);
                (self.bounded <= MAX_BOUNDED_RETRIES).then(|| ladder(self.bounded))
            }
        }
    }
}
