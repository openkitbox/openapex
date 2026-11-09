//! Provider quota guard: the process-wide memory of the last throttle, and
//! the cool-down it imposes on background completions.
//!
//! Every completion ox makes — a live turn, a fold, a dream — shares the
//! provider's rate limit. When the provider answers `429` (or `529`, the
//! Anthropic "overloaded" status), the user's own turns already back off and
//! retry; a background task that starts right then only competes with them
//! for the quota that is left. So the guard records the throttle
//! ([`observe`]) wherever a completion fails, and the dreamer's tick asks
//! ([`cooling_until`]) before it pays for a task. The historian's fold is
//! not gated: context pressure must clear whether or not the provider is
//! busy, and its own retry ladder already backs off.
//!
//! One atomic for the whole process rather than a table: the information is
//! "a few minutes ago, this provider throttled", worthless across a restart
//! and shared by every session the process serves.
//!
//! Reference: codex `memories/guard.rs` skips memory tasks when the rate-limit
//! snapshot shows under 25% remaining and lets them run when it has none.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use ox_llm::cardinal::ChatError;

/// How long after a throttle the dreamer starts nothing new.
pub const COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Epoch milliseconds of the last observed throttle; `0` = never.
static LAST_THROTTLE_AT: AtomicI64 = AtomicI64::new(0);

/// Whether `error` is the provider saying "not now" rather than "no".
fn is_throttle(error: &ChatError) -> bool {
    matches!(error, ChatError::Http { status, .. } if *status == 429 || *status == 529)
}

/// Note a completion failure at `now`. Only a throttle moves the clock;
/// every other error is the caller's business.
pub fn observe(error: &ChatError, now: i64) {
    if is_throttle(error) {
        LAST_THROTTLE_AT.fetch_max(now, Ordering::Relaxed);
    }
}

/// While the cool-down after the last throttle runs, the instant it ends;
/// `None` when background work may start.
#[must_use]
pub fn cooling_until(now: i64) -> Option<i64> {
    let last = LAST_THROTTLE_AT.load(Ordering::Relaxed);
    if last == 0 {
        return None;
    }
    let until = last.saturating_add(i64::try_from(COOLDOWN.as_millis()).unwrap_or(i64::MAX));
    (now < until).then_some(until)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock is process-wide, so the test works far in the future where
    /// no other test's observation can reach.
    #[test]
    fn only_throttles_move_the_clock() {
        let base = i64::MAX / 2;
        let cooldown = i64::try_from(COOLDOWN.as_millis()).expect("cool-down fits");
        let refusal = ChatError::Http {
            status: 400,
            body: String::new(),
            retry_after: None,
        };
        observe(&refusal, base);
        assert_eq!(cooling_until(base), None);
        let throttle = ChatError::Http {
            status: 429,
            body: String::new(),
            retry_after: None,
        };
        observe(&throttle, base);
        assert_eq!(cooling_until(base + 1), Some(base + cooldown));
        assert_eq!(cooling_until(base + cooldown), None);
    }
}
