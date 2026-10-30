//! Repeated tool-call detector for one turn.
//!
//! Owns: a sliding window of recent tool-call fingerprints and the verdict
//! (`Ok` / `Warn` / `WrapUp`) derived from how often a fingerprint recurs in
//! that window. Pure: no I/O, no DB, no events, no globals.
//!
//! Invariants:
//! - State lives and dies with the turn; it is never persisted. Fingerprints
//!   come from `DefaultHasher` and are only comparable within one process.
//! - The tool result is deliberately NOT part of the fingerprint: a call that
//!   is re-issued with identical name and arguments counts as a repeat
//!   regardless of what it returned (plan decision "已定" 3).
//! - A fingerprint is warned at most once per guard, even if its window count
//!   falls below `WARN_AT` and later climbs back.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};

/// Most recent calls retained for counting.
const WINDOW: usize = 20;
/// Window count at which a fingerprint is warned about (once).
const WARN_AT: usize = 3;
/// Window count at which the turn should be wrapped up.
const WRAP_UP_AT: usize = 6;

/// Outcome of observing one step's tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Ok,
    /// First time this fingerprint reached `WARN_AT`.
    Warn,
    /// Some fingerprint reached `WRAP_UP_AT`.
    WrapUp,
}

/// Sliding-window repeat counter.
#[derive(Debug, Default)]
pub(crate) struct LoopGuard {
    recent: VecDeque<u64>,
    warned: HashSet<u64>,
}

/// Hash of a tool call's name and trimmed arguments.
pub(crate) fn fingerprint(name: &str, args: &str) -> u64 {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    // `str::hash` appends a 0xff terminator, so ("ab","c") != ("a","bc").
    args.trim().hash(&mut h);
    h.finish()
}

impl LoopGuard {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one step's call fingerprints in order; return the strongest verdict.
    pub(crate) fn observe(&mut self, calls: &[u64]) -> Verdict {
        let mut verdict = Verdict::Ok;
        for &fp in calls {
            if self.recent.len() == WINDOW {
                self.recent.pop_front();
            }
            self.recent.push_back(fp);
            let count = self.recent.iter().filter(|&&x| x == fp).count();
            if count >= WRAP_UP_AT {
                verdict = Verdict::WrapUp;
            } else if count >= WARN_AT && verdict == Verdict::Ok && self.warned.insert(fp) {
                verdict = Verdict::Warn;
            }
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u32) -> u64 {
        fingerprint("tool", &n.to_string())
    }

    #[test]
    fn third_identical_warns_once_then_ok() {
        let mut g = LoopGuard::new();
        let a = fp(1);
        assert_eq!(g.observe(&[a]), Verdict::Ok);
        assert_eq!(g.observe(&[a]), Verdict::Ok);
        assert_eq!(g.observe(&[a]), Verdict::Warn);
        assert_eq!(g.observe(&[a]), Verdict::Ok);
        assert_eq!(g.observe(&[a]), Verdict::Ok);
    }

    #[test]
    fn sixth_identical_wraps_up() {
        let mut g = LoopGuard::new();
        let a = fp(1);
        for _ in 0..5 {
            g.observe(&[a]);
        }
        assert_eq!(g.observe(&[a]), Verdict::WrapUp);
    }

    #[test]
    fn alternating_fingerprints_warn_each_not_wrap_up() {
        let mut g = LoopGuard::new();
        let (a, b) = (fp(1), fp(2));
        let mut verdicts = Vec::new();
        for _ in 0..3 {
            verdicts.push(g.observe(&[a]));
            verdicts.push(g.observe(&[b]));
        }
        assert_eq!(
            verdicts,
            vec![
                Verdict::Ok,
                Verdict::Ok,
                Verdict::Ok,
                Verdict::Ok,
                Verdict::Warn,
                Verdict::Warn
            ]
        );
    }

    #[test]
    fn window_slide_drops_old_counts() {
        let mut g = LoopGuard::new();
        let a = fp(1);
        g.observe(&[a]);
        g.observe(&[a]);
        let others: Vec<u64> = (100..100 + WINDOW as u32).map(fp).collect();
        assert_eq!(g.observe(&others), Verdict::Ok);
        assert_eq!(g.observe(&[a]), Verdict::Ok);
    }

    #[test]
    fn warned_fingerprint_is_not_rewarned_after_sliding() {
        let mut g = LoopGuard::new();
        let a = fp(1);
        assert_eq!(g.observe(&[a, a, a]), Verdict::Warn);
        let others: Vec<u64> = (100..100 + WINDOW as u32).map(fp).collect();
        assert_eq!(g.observe(&others), Verdict::Ok);
        assert_eq!(g.observe(&[a, a, a]), Verdict::Ok);
    }

    #[test]
    fn full_window_with_same_fingerprint_at_front_does_not_rewarn() {
        let mut g = LoopGuard::new();
        let a = fp(1);
        assert_eq!(g.observe(&[a, a, a]), Verdict::Warn);
        // Window is now [a, a, a, d0..d16] (20 entries, `a` at the front).
        let fillers: Vec<u64> = (100..100 + (WINDOW - 3) as u32).map(fp).collect();
        assert_eq!(g.observe(&fillers), Verdict::Ok);
        // Pop front `a` (3 -> 2), push `a` (2 -> 3): `a` is already warned.
        assert_eq!(g.observe(&[a]), Verdict::Ok);
        assert_eq!(g.observe(&[a]), Verdict::Ok);
    }

    #[test]
    fn short_cycle_does_not_flood_reminders() {
        // Cycles of 7..=9 distinct calls keep every window count between 2
        // and 3 and never reach `WRAP_UP_AT`; the warnings must stay bounded
        // by the number of distinct fingerprints, not by the number of steps.
        for len in 7..=9u32 {
            let mut g = LoopGuard::new();
            let (mut warns, mut wraps) = (0, 0);
            for i in 0..500 {
                match g.observe(&[fp(i % len)]) {
                    Verdict::Warn => warns += 1,
                    Verdict::WrapUp => wraps += 1,
                    Verdict::Ok => {}
                }
            }
            assert!(warns <= len as usize, "cycle {len}: {warns} warns");
            // Known limit: such a cycle never wraps up; the step cap is the
            // only bound.
            assert_eq!(wraps, 0, "cycle {len}");
        }
    }

    #[test]
    fn identical_calls_across_observations_warn_on_third() {
        let mut g = LoopGuard::new();
        let a = fp(1);
        assert_eq!(g.observe(&[a, a]), Verdict::Ok);
        assert_eq!(g.observe(&[a]), Verdict::Warn);
    }

    #[test]
    fn identical_calls_in_one_slice_count_cumulatively() {
        let a = fp(1);
        assert_eq!(LoopGuard::new().observe(&[a, a, a]), Verdict::Warn);
        assert_eq!(LoopGuard::new().observe(&[a; 6]), Verdict::WrapUp);
    }

    #[test]
    fn empty_slice_is_ok() {
        assert_eq!(LoopGuard::new().observe(&[]), Verdict::Ok);
    }

    #[test]
    fn fingerprint_separates_name_and_args() {
        assert_ne!(fingerprint("ab", "c"), fingerprint("a", "bc"));
    }

    #[test]
    fn fingerprint_ignores_surrounding_args_whitespace() {
        assert_eq!(
            fingerprint("t", " {\"x\":1}\n"),
            fingerprint("t", "{\"x\":1}")
        );
        assert_ne!(fingerprint("t", "{\"x\":1}"), fingerprint("t", "{\"x\":2}"));
    }
}
