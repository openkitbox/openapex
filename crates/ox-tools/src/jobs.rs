//! Process-local registry of jobs that are blocking a session, so
//! `session.background` can move them to the background.
//!
//! Ports the blocking half of upstream `packages/core/src/job.ts`: `block`
//! records that a session is waiting on a job, and `backgroundAll` releases
//! every job blocking a session (only entries whose `blockingSessions`
//! contains it and that are not already backgrounded).
//!
//! Deliberate deviations from upstream:
//! - No durable SQLite job table and no restart recovery: entries live only
//!   while a foreground tool call waits on them.
//! - No wait / cancel / consume API: the `shell` and `subagent` tools own how
//!   a job settles and how its completion is reported.
//! - The registry tracks only blocking ↔ backgrounded; once no guard waits on
//!   an entry, it is forgotten.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::watch;

/// Snapshot of one job moved to the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
}

/// Shared registry of jobs that foreground tool calls are waiting on.
#[derive(Default)]
pub struct Jobs {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    entries: HashMap<String, Entry>,
}

struct Entry {
    /// Registration order, so `background_all` reports jobs in creation order.
    seq: u64,
    kind: &'static str,
    title: String,
    /// Blocked session id → number of live guards for it.
    blocking: HashMap<String, usize>,
    backgrounded: watch::Sender<bool>,
}

impl Jobs {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register `session_id` as blocked on job `id`. Creates the entry when
    /// missing; an existing entry that is not backgrounded gains one more
    /// blocking reference for that session. Dropping the guard releases it.
    pub fn block(
        self: &Arc<Self>,
        id: &str,
        kind: &'static str,
        title: &str,
        session_id: &str,
    ) -> BlockGuard {
        let mut state = self.lock();
        if !state.entries.contains_key(id) {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.entries.insert(
                id.to_owned(),
                Entry {
                    seq,
                    kind,
                    title: title.to_owned(),
                    blocking: HashMap::new(),
                    backgrounded: watch::Sender::new(false),
                },
            );
        }
        let entry = state
            .entries
            .get_mut(id)
            .expect("entry was inserted or found under the same lock");
        // A guard on an already-backgrounded entry holds no blocking
        // reference: its wait resolves at once and its drop releases nothing.
        let counted = !*entry.backgrounded.borrow();
        if counted {
            *entry.blocking.entry(session_id.to_owned()).or_insert(0) += 1;
        }
        let seq = entry.seq;
        let backgrounded = entry.backgrounded.subscribe();
        drop(state);
        BlockGuard {
            jobs: Arc::clone(self),
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            seq,
            counted,
            backgrounded,
        }
    }

    /// Upstream `backgroundAll`: every entry blocking `session_id` that is not
    /// yet backgrounded becomes backgrounded (its blocking map is cleared) and
    /// its waiters are woken. Returns those jobs in registration order; a
    /// second call returns nothing.
    pub fn background_all(&self, session_id: &str) -> Vec<JobInfo> {
        let mut state = self.lock();
        let mut moved: Vec<(u64, JobInfo)> = Vec::new();
        for (id, entry) in &mut state.entries {
            if *entry.backgrounded.borrow() || !entry.blocking.contains_key(session_id) {
                continue;
            }
            entry.blocking.clear();
            entry.backgrounded.send_replace(true);
            moved.push((
                entry.seq,
                JobInfo {
                    id: id.clone(),
                    kind: entry.kind,
                    title: entry.title.clone(),
                },
            ));
        }
        moved.sort_by_key(|(seq, _)| *seq);
        moved.into_iter().map(|(_, info)| info).collect()
    }

    /// Release one guard's registration. Only the entry generation the guard
    /// was made on is touched, so a stale guard never releases a newer entry
    /// that reuses the same id.
    fn release(&self, guard: &BlockGuard) {
        let mut state = self.lock();
        let Some(entry) = state.entries.get_mut(&guard.id) else {
            return;
        };
        if entry.seq != guard.seq {
            return;
        }
        if guard.counted
            && let Some(count) = entry.blocking.get_mut(&guard.session_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.blocking.remove(&guard.session_id);
            }
        }
        if entry.blocking.is_empty() {
            state.entries.remove(&guard.id);
        }
    }
}

/// A session's registration as blocked on one job; dropping it releases the
/// registration.
#[must_use = "dropping the guard releases the blocking registration at once"]
pub struct BlockGuard {
    jobs: Arc<Jobs>,
    id: String,
    session_id: String,
    /// The entry generation this guard belongs to.
    seq: u64,
    /// Whether this guard holds a blocking reference on that entry.
    counted: bool,
    backgrounded: watch::Receiver<bool>,
}

impl BlockGuard {
    /// Resolves once the job is backgrounded, including when that happened
    /// before this call. Never resolves for a job that was not backgrounded.
    pub async fn backgrounded(&self) {
        let mut receiver = self.backgrounded.clone();
        // `wait_for` checks the current value before waiting, so an earlier
        // backgrounding is never missed. A closed channel resolves only if the
        // last value says backgrounded; otherwise fail closed and keep waiting.
        if receiver
            .wait_for(|backgrounded| *backgrounded)
            .await
            .is_err()
            && !*receiver.borrow()
        {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for BlockGuard {
    fn drop(&mut self) {
        self.jobs.release(self);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    fn registry() -> Arc<Jobs> {
        Arc::new(Jobs::default())
    }

    async fn resolves(guard: &BlockGuard) {
        timeout(Duration::from_secs(2), guard.backgrounded())
            .await
            .expect("backgrounded() must resolve after background_all");
    }

    #[tokio::test]
    async fn background_all_returns_the_job_and_wakes_its_guard() {
        let jobs = registry();
        let guard = jobs.block("job-1", "shell", "sleep 10", "session-a");

        let moved = jobs.background_all("session-a");
        assert_eq!(
            moved,
            vec![JobInfo {
                id: "job-1".to_owned(),
                kind: "shell",
                title: "sleep 10".to_owned(),
            }],
            "the blocked job must be reported",
        );
        resolves(&guard).await;
    }

    #[tokio::test]
    async fn second_background_all_is_empty() {
        let jobs = registry();
        let _guard = jobs.block("job-1", "shell", "sleep 10", "session-a");
        assert_eq!(jobs.background_all("session-a").len(), 1);
        assert!(
            jobs.background_all("session-a").is_empty(),
            "a backgrounded job must not be reported twice",
        );
    }

    #[tokio::test]
    async fn dropping_the_guard_forgets_the_job() {
        let jobs = registry();
        let guard = jobs.block("job-1", "shell", "sleep 10", "session-a");
        drop(guard);
        assert!(
            jobs.background_all("session-a").is_empty(),
            "a job nobody waits on must be forgotten",
        );
    }

    #[tokio::test]
    async fn other_sessions_do_not_release_the_job() {
        let jobs = registry();
        let _guard = jobs.block("job-1", "shell", "sleep 10", "session-a");
        assert!(
            jobs.background_all("session-b").is_empty(),
            "session B does not block on session A's job",
        );
    }

    #[tokio::test]
    async fn backgrounding_before_the_await_is_not_lost() {
        let jobs = registry();
        let guard = jobs.block("job-1", "subagent", "research", "session-a");
        jobs.background_all("session-a");
        resolves(&guard).await;
    }

    #[tokio::test]
    async fn jobs_are_reported_in_registration_order() {
        let jobs = registry();
        let _second = jobs.block("job-2", "shell", "two", "session-a");
        let _first = jobs.block("job-1", "shell", "one", "session-a");
        let ids: Vec<String> = jobs
            .background_all("session-a")
            .into_iter()
            .map(|info| info.id)
            .collect();
        assert_eq!(ids, vec!["job-2".to_owned(), "job-1".to_owned()]);
    }

    #[tokio::test]
    async fn a_stale_guard_does_not_release_a_newer_entry() {
        let jobs = registry();
        let first = jobs.block("ses_child", "subagent", "one", "session-a");
        jobs.background_all("session-a");
        // The id is reused while the old guard is still alive (a resumed
        // child), then the old guard drops.
        let late = jobs.block("ses_child", "subagent", "one", "session-a");
        drop(first);
        let _second = jobs.block("ses_child", "subagent", "two", "session-a");
        // The stale guard drops while the new generation is live.
        drop(late);
        assert_eq!(
            jobs.background_all("session-a").len(),
            1,
            "the new blocking call must stay registered",
        );
    }

    #[tokio::test]
    async fn an_unbackgrounded_guard_never_resolves() {
        let jobs = registry();
        let guard = jobs.block("job-1", "shell", "sleep 10", "session-a");
        assert!(
            timeout(Duration::from_millis(50), guard.backgrounded())
                .await
                .is_err(),
            "backgrounded() must not resolve without background_all",
        );
    }
}
