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

#[cfg(test)]
mod tests {
    use crate::decision::{
        Decision, DueInput, RunOutcome, SettleInput, due_decision, next_due, settle,
    };
    use crate::types::{Action, Info, Outcome, Status, TimeInfo, Trigger};

    fn due(now_ms: i64) -> DueInput {
        DueInput {
            now_ms,
            status: Status::Active,
            enabled: true,
            next_due_at: Some(now_ms),
            expires_at: None,
            retry_at: None,
        }
    }

    #[test]
    fn due_decision_table() {
        let mut input = due(10_000);
        assert_eq!(due_decision(&input), Decision::Fire { late: false });

        input.now_ms = 9_999;
        assert_eq!(
            due_decision(&input),
            Decision::Skip,
            "future due is not ready"
        );

        input = due(10_000);
        input.now_ms = 10_000 + 30_000;
        assert_eq!(due_decision(&input), Decision::Fire { late: false });

        input.now_ms = 10_000 + 90_000;
        assert_eq!(due_decision(&input), Decision::Fire { late: true });

        input.now_ms = 10_000 + 10 * 60_000;
        assert_eq!(due_decision(&input), Decision::Missed);

        input = due(10_000);
        input.expires_at = Some(10_000);
        assert_eq!(
            due_decision(&input),
            Decision::Expire,
            "expiry wins over due"
        );

        input = due(10_000);
        input.retry_at = Some(10_001);
        assert_eq!(
            due_decision(&input),
            Decision::Skip,
            "retry backoff is pending"
        );

        input.retry_at = Some(10_000);
        assert_eq!(due_decision(&input), Decision::Fire { late: false });

        input = due(10_000);
        input.enabled = false;
        assert_eq!(
            due_decision(&input),
            Decision::Skip,
            "disabled feature fails closed"
        );

        input = due(10_000);
        input.status = Status::Paused;
        assert_eq!(due_decision(&input), Decision::Skip);

        input.status = Status::Done;
        assert_eq!(due_decision(&input), Decision::Skip);

        input = due(10_000);
        input.next_due_at = None;
        assert_eq!(
            due_decision(&input),
            Decision::Skip,
            "event triggers have no due time"
        );
    }

    #[test]
    fn due_decision_never_depends_on_the_wall_clock() {
        let input = due(1_700_000_000_000);
        let first = due_decision(&input);
        for _ in 0..100 {
            assert_eq!(due_decision(&input), first);
        }
    }

    #[test]
    fn settle_decides_retry_expire_and_terminal_status() {
        let base = SettleInput {
            now_ms: 1_000,
            outcome: RunOutcome::Failed,
            failures: 0,
            next_due_at: Some(500),
            expires_at: None,
        };
        let retry = settle(&base);
        assert_eq!(retry.status, Status::Active);
        assert_eq!(retry.outcome, Outcome::Failed);
        assert_eq!(retry.retry_at, Some(31_000));
        assert_eq!(
            retry.next_due_at,
            Some(500),
            "retry keeps the original due time"
        );

        let retry2 = settle(&SettleInput {
            failures: 1,
            ..base
        });
        assert_eq!(retry2.retry_at, Some(61_000), "backoff doubles");

        let spent = settle(&SettleInput {
            failures: 3,
            ..base
        });
        assert_eq!(spent.status, Status::Done);
        assert_eq!(spent.retry_at, None);

        let expired = settle(&SettleInput {
            expires_at: Some(1_000),
            ..base
        });
        assert_eq!(expired.status, Status::Done);
        assert_eq!(expired.outcome, Outcome::Expired);
        assert_eq!(expired.retry_at, None, "expired runs are never retried");

        let success = settle(&SettleInput {
            outcome: RunOutcome::Succeeded,
            next_due_at: Some(60_000),
            ..base
        });
        assert_eq!(success.status, Status::Active);
        assert_eq!(success.outcome, Outcome::Fired);
        assert_eq!(success.next_due_at, Some(60_000));
        assert_eq!(success.retry_at, None);
    }

    #[test]
    fn next_due_advances_every_at_and_cron() -> anyhow::Result<()> {
        let now = jiff::civil::date(2024, 6, 3)
            .at(9, 7, 0, 0)
            .in_tz("UTC")?
            .timestamp()
            .as_millisecond();
        let mut info = Info {
            id: "aut_1".to_owned(),
            name: "n".to_owned(),
            project_id: "p".to_owned(),
            session_id: None,
            directory: "/work".to_owned(),
            trigger: Trigger::Every {
                every: "30m".to_owned(),
                tz: None,
            },
            tz: "UTC".to_owned(),
            action: Action::Shell {
                command: "true".to_owned(),
            },
            expires_at: None,
            status: Status::Active,
            next_due_at: Some(now),
            run_count: 0,
            last_run_at: None,
            last_outcome: None,
            last_output: None,
            state: None,
            time: TimeInfo {
                created: now,
                updated: now,
            },
        };
        assert_eq!(next_due(&info, now)?, Some(now + 30 * 60_000));

        info.trigger = Trigger::Cron {
            cron: "*/15 * * * *".to_owned(),
            tz: None,
        };
        assert_eq!(next_due(&info, now)?, Some(now + 8 * 60_000));

        info.trigger = Trigger::At {
            at: "later".to_owned(),
            tz: None,
        };
        assert_eq!(
            next_due(&info, now)?,
            None,
            "one-shot triggers do not reschedule"
        );

        info.trigger = Trigger::Event {
            server: "s".to_owned(),
            event: "e".to_owned(),
            args: None,
        };
        assert_eq!(
            next_due(&info, now)?,
            None,
            "event triggers wait for an event"
        );
        Ok(())
    }
}
