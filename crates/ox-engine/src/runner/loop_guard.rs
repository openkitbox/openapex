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
