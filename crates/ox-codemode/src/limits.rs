//! Resource ceilings applied to every Code Mode execution.

use std::time::Duration;

/// Bounds for one script run. Production callers use [`Limits::default`];
/// tests shorten `timeout` so a deadline test never waits 30 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock budget for the whole script.
    pub timeout: Duration,
    /// Maximum bytes of captured log output kept in the result.
    pub output_bytes: usize,
    /// QuickJS heap ceiling.
    pub memory_bytes: usize,
    /// QuickJS stack ceiling.
    pub stack_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            output_bytes: 32 * 1024,
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 256 * 1024,
        }
    }
}
