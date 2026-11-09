//! Retry decisions for one step, independent of I/O and settlement.
//!
//! Transport and bounded provider errors allow ten retries; timeouts allow
//! three. None of these budgets reset on partial output. Published failures
//! additionally share a ten-continuation cap. The consecutive network delay
//! ladder alone restarts on progress.
//!
//! Deliberate OpenCode deviations: no jitter, and no one-hour offline special
//! case (offline cannot be reliably distinguished from other connect failures).
//! Provider advice is a minimum delay, normalized and capped by the transport.

use std::time::Duration;

use ox_llm::cardinal::ChatError;

pub(crate) const MAX_BOUNDED_RETRIES: usize = 10;
pub(crate) const MAX_CONTINUATIONS: usize = 10;
const MAX_TIMEOUT_RETRIES: usize = 3;
const BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryKind {
    Never,
    Bounded,
    Network,
    Timeout,
}

/// Every error variant must make an explicit retry decision.
pub(crate) fn classify(error: &ChatError) -> RetryKind {
    match error {
        ChatError::Transport(error) => {
            if error.is_builder() {
                RetryKind::Never
            } else if error.is_timeout() {
                RetryKind::Timeout
            } else if error.is_redirect() {
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

fn ladder(n: usize) -> Duration {
    let shift = u32::try_from(n.max(1) - 1).unwrap_or(u32::MAX).min(4);
    BASE_DELAY.saturating_mul(1 << shift).min(MAX_DELAY)
}

#[derive(Debug, Default)]
pub(crate) struct Backoff {
    network_ladder: usize,
    network: usize,
    timeouts: usize,
    bounded: usize,
    continuations: usize,
}

impl Backoff {
    /// Record one failed attempt; progress resets only the delay ladder.
    pub(crate) fn on_failure(&mut self, kind: RetryKind, produced: bool) -> Option<Duration> {
        if kind == RetryKind::Never {
            return None;
        }
        if produced {
            self.continuations = self.continuations.saturating_add(1);
            self.network_ladder = 0;
            if self.continuations > MAX_CONTINUATIONS {
                return None;
            }
        }
        match kind {
            RetryKind::Never => None,
            RetryKind::Network | RetryKind::Timeout => {
                self.network_ladder = self.network_ladder.saturating_add(1);
                let (count, cap) = if kind == RetryKind::Timeout {
                    (&mut self.timeouts, MAX_TIMEOUT_RETRIES)
                } else {
                    (&mut self.network, MAX_BOUNDED_RETRIES)
                };
                *count = count.saturating_add(1);
                (*count <= cap).then(|| ladder(self.network_ladder))
            }
            RetryKind::Bounded => {
                self.bounded = self.bounded.saturating_add(1);
                (self.bounded <= MAX_BOUNDED_RETRIES).then(|| ladder(self.bounded))
            }
        }
    }
}
