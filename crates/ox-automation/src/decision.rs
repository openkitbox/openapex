//! Pure due/settle state machine shared by the scheduler and its tests.
//!
//! Given the same inputs these functions return the same decision: no clock,
//! timer, database or global state is consulted.

use anyhow::{Result, bail, ensure};

use crate::cron::Cron;
use crate::types::{Info, Outcome, Status, Trigger};

/// Grace before a due run is recorded as late, and the point past which a
/// missed run is skipped instead of being executed hours after its deadline.
pub const LATE_AFTER_MS: i64 = 60_000;
pub const MISSED_AFTER_MS: i64 = 5 * 60_000;
pub const RETRY_BASE_MS: i64 = 30_000;
pub const RETRY_MAX_MS: i64 = 10 * 60_000;
pub const MAX_ATTEMPTS: u32 = 3;

/// Snapshot of the persisted fields that decide whether a tick acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueInput {
    pub now_ms: i64,
    pub status: Status,
    /// The `automations` config gate; a disabled feature never dispatches.
    pub enabled: bool,
    pub next_due_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub retry_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip,
    Fire { late: bool },
    Missed,
    Expire,
}

#[must_use]
pub fn due_decision(input: &DueInput) -> Decision {
    if !input.enabled || !matches!(input.status, Status::Active) {
        return Decision::Skip;
    }
    if input
        .expires_at
        .is_some_and(|expires| expires <= input.now_ms)
    {
        return Decision::Expire;
    }
    if input.retry_at.is_some_and(|retry| retry > input.now_ms) {
        return Decision::Skip;
    }
    let Some(due) = input.next_due_at else {
        return Decision::Skip;
    };
    let age = input.now_ms - due;
    if age < 0 {
        Decision::Skip
    } else if age <= LATE_AFTER_MS {
        Decision::Fire { late: false }
    } else if age <= MISSED_AFTER_MS {
        Decision::Fire { late: true }
    } else {
        Decision::Missed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleInput {
    pub now_ms: i64,
    pub outcome: RunOutcome,
    /// Number of failures already recorded on the row.
    pub failures: u32,
    pub next_due_at: Option<i64>,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settle {
    pub status: Status,
    pub next_due_at: Option<i64>,
    pub retry_at: Option<i64>,
    pub outcome: Outcome,
}

#[must_use]
pub fn settle(input: &SettleInput) -> Settle {
    if input
        .expires_at
        .is_some_and(|expires| expires <= input.now_ms)
    {
        return Settle {
            status: Status::Done,
            next_due_at: input.next_due_at,
            retry_at: None,
            outcome: Outcome::Expired,
        };
    }
    match input.outcome {
        RunOutcome::Succeeded => Settle {
            status: Status::Active,
            next_due_at: input.next_due_at,
            retry_at: None,
            outcome: Outcome::Fired,
        },
        RunOutcome::Failed => {
            let delay = RETRY_BASE_MS.saturating_mul(1i64 << input.failures.min(8));
            if input.failures + 1 >= MAX_ATTEMPTS {
                Settle {
                    status: Status::Done,
                    next_due_at: input.next_due_at,
                    retry_at: None,
                    outcome: Outcome::Failed,
                }
            } else {
                Settle {
                    status: Status::Active,
                    next_due_at: input.next_due_at,
                    retry_at: Some(input.now_ms.saturating_add(delay.min(RETRY_MAX_MS))),
                    outcome: Outcome::Failed,
                }
            }
        }
    }
}

/// Compute the next due time from the trigger. `at` and `event` triggers have
/// no periodic due time; `every` accepts milliseconds or a unit suffix.
pub fn next_due(info: &Info, now_ms: i64) -> Result<Option<i64>> {
    match &info.trigger {
        Trigger::At { .. } | Trigger::Event { .. } => Ok(None),
        Trigger::Every { every, .. } => Ok(Some(now_ms.saturating_add(parse_every(every)?))),
        Trigger::Cron { cron, tz } => {
            let zone = tz.as_deref().unwrap_or(&info.tz);
            Ok(Cron::parse(cron)?.next_after(now_ms, zone)?)
        }
    }
}

fn parse_every(every: &str) -> Result<i64> {
    let raw = every.trim();
    ensure!(!raw.is_empty(), "trigger.every must not be empty");
    let (amount, unit_ms) = match raw.chars().last() {
        Some('m') => (&raw[..raw.len() - 1], 60_000),
        Some('h') => (&raw[..raw.len() - 1], 3_600_000),
        Some('d') => (&raw[..raw.len() - 1], 86_400_000),
        Some('s') => (&raw[..raw.len() - 1], 1_000),
        _ if raw.bytes().all(|byte| byte.is_ascii_digit()) => (raw, 1),
        _ => bail!("unsupported every duration: {every}"),
    };
    let count: i64 = amount.parse()?;
    ensure!(count > 0, "trigger.every must be positive");
    Ok(count.saturating_mul(unit_ms))
}
