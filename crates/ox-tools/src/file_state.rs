//! Per-session file stamps: what each session last saw on disk, so a tool that
//! writes a file can refuse when that file changed in the meantime.
//!
//! `read` records a stamp for every text page it serves; `edit`, `write` and
//! `patch` compare the current stamp before they write and record it again
//! after a successful write. A write to a file the session never read has no
//! stamp to compare, so it proceeds: the guard only catches drift away from
//! content the session already has in context.
//!
//! The stamp is `(len, modified)`, not a content hash: `read` pages a file in
//! bounded windows, so hashing would cost a second full pass over a file the
//! caller deliberately avoided loading whole.
//!
//! Paths are keyed by the resolved absolute path the tools already share
//! (`resolve_path`), not a canonicalized one: a symlinked or `..` spelling of
//! the same file gets its own entry, which at worst skips the guard and never
//! rejects a write that is actually fresh.
//!
//! Layering: [`verdict`] is a pure decision function and the store's key
//! access is pure, so the state machine is testable without a tempdir. Only
//! [`FileStates::record`] and [`FileStates::check`] touch the filesystem, and
//! they read metadata *before* taking the lock: no I/O ever runs while the
//! store is locked.
//!
//! The check and the write it guards are two separate syscalls, so a writer
//! outside this process can still land between them. Closing that window would
//! need a per-path lock held across every mutation; the guard is a context
//! check, not a concurrency primitive.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Paths remembered per session. Reads are paged and a session touches a
/// bounded set of files, so this holds a working set rather than history.
const MAX_PATHS_PER_SESSION: usize = 128;
/// Sessions remembered in one store, evicted least recently recorded first.
/// Sessions never unregister: this store outlives individual sessions. One
/// store is shared by a whole server's registry, so this bounds the process.
const MAX_SESSIONS: usize = 64;

/// A file's identity at one point in time: its length and modification time,
/// taken from a single `std::fs::metadata` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// Size in bytes.
    pub len: u64,
    /// Modification time; `None` on a filesystem that does not report one.
    pub modified: Option<SystemTime>,
}

impl Stamp {
    /// The stamp of one file's metadata.
    #[must_use]
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// What the filesystem says about a path right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The path exists and reports this stamp.
    Stamp(Stamp),
    /// The path does not exist: there is nothing to overwrite or lose.
    Missing,
    /// The path exists as far as metadata opinions go, but it cannot be read.
    Unreadable,
}

impl Presence {
    /// `path`'s state as it is on disk now.
    fn read(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(metadata) => Self::Stamp(Stamp::of(&metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Unreadable,
        }
    }
}

/// How a write target stands compared with what the session last saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The session never recorded this path, or the file is gone; there is
    /// nothing in context that a write could contradict.
    Unrecorded,
    /// The file is the one the session saw.
    Unchanged,
    /// The file changed, or its current state cannot be read. Fail closed.
    Changed,
}

/// The decision: does the file's current [`Presence`] still match `recorded`?
///
/// A missing file is [`Verdict::Unrecorded`]: nothing is overwritten, so a write
/// that recreates a deleted path is allowed, and the record is dropped by
/// [`FileStates::check`] so the path does not stay stale forever. An
/// [`Presence::Unreadable`] state counts as [`Verdict::Changed`], never as a
/// pass: a guard that cannot verify the file must not let a write through.
#[must_use]
pub fn verdict(recorded: Option<Stamp>, current: Presence) -> Verdict {
    let Some(recorded) = recorded else {
        return Verdict::Unrecorded;
    };
    match current {
        Presence::Stamp(current) if current == recorded => Verdict::Unchanged,
        Presence::Stamp(_) | Presence::Unreadable => Verdict::Changed,
        Presence::Missing => Verdict::Unrecorded,
    }
}

/// The process-wide store of per-session file stamps.
#[derive(Default)]
pub struct FileStates {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Monotone counter shared by path insertion and session touch order, so
    /// each ordering is a plain integer comparison.
    next_seq: u64,
    sessions: HashMap<String, Session>,
}

#[derive(Default)]
struct Session {
    /// When this session last recorded anything.
    touched: u64,
    entries: HashMap<PathBuf, Record>,
}

struct Record {
    /// When this path was first recorded (its insertion order).
    seq: u64,
    stamp: Stamp,
}

impl FileStates {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record `stamp` for `path` in `session`, evicting the earliest-inserted
    /// path of a full session and the least recently recorded session beyond
    /// [`MAX_SESSIONS`]. Re-recording a known path replaces its stamp and keeps
    /// its insertion position.
    pub fn record_stamp(&self, session: &str, path: &Path, stamp: Stamp) {
        let mut state = self.lock();
        let seq = state.next_seq;
        state.next_seq += 1;
        let entries = {
            let record = state.sessions.entry(session.to_owned()).or_default();
            record.touched = seq;
            &mut record.entries
        };
        match entries.get_mut(path) {
            Some(record) => record.stamp = stamp,
            None => {
                if entries.len() >= MAX_PATHS_PER_SESSION
                    && let Some(oldest) = entries
                        .iter()
                        .min_by_key(|(_, record)| record.seq)
                        .map(|(path, _)| path.clone())
                {
                    entries.remove(&oldest);
                }
                entries.insert(path.to_path_buf(), Record { seq, stamp });
            }
        }
        while state.sessions.len() > MAX_SESSIONS {
            let oldest = state
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.touched)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                break;
            };
            state.sessions.remove(&oldest);
        }
    }

    /// The stamp `session` last recorded for `path`, if any. Pure: records
    /// nothing and does not touch the session's recency.
    #[must_use]
    pub fn recorded(&self, session: &str, path: &Path) -> Option<Stamp> {
        let state = self.lock();
        state
            .sessions
            .get(session)?
            .entries
            .get(path)
            .map(|record| record.stamp)
    }

    /// Record `path`'s current state for `session`. A path whose metadata
    /// cannot be read records nothing, so it stays [`Verdict::Unrecorded`]
    /// rather than becoming a permanently stale entry.
    pub fn record(&self, session: &str, path: &Path) {
        let Presence::Stamp(stamp) = Presence::read(path) else {
            return;
        };
        self.record_stamp(session, path, stamp);
    }

    /// Drop `session`'s stamp for `path`, so a later write treats it as
    /// unrecorded.
    pub fn forget(&self, session: &str, path: &Path) {
        let mut state = self.lock();
        if let Some(records) = state.sessions.get_mut(session) {
            records.entries.remove(path);
        }
    }

    /// Drop `session`'s stamp for `path` only if it is still `expected`, under
    /// one lock: a concurrent writer that recorded a newer stamp in the meantime
    /// keeps it, so [`Self::check`] cannot erase a fresh record.
    fn forget_if(&self, session: &str, path: &Path, expected: Stamp) {
        let mut state = self.lock();
        if let Some(records) = state.sessions.get_mut(session)
            && records
                .entries
                .get(path)
                .is_some_and(|record| record.stamp == expected)
        {
            records.entries.remove(path);
        }
    }

    /// Whether `path` changed since `session` recorded it. A path the session
    /// recorded that has since been deleted reports [`Verdict::Unrecorded`] and
    /// forgets the record: there is nothing left to overwrite, and keeping the
    /// stamp would lock the path against a legitimate recreation.
    #[must_use]
    pub fn check(&self, session: &str, path: &Path) -> Verdict {
        let Some(recorded) = self.recorded(session, path) else {
            return Verdict::Unrecorded;
        };
        let current = Presence::read(path);
        if current == Presence::Missing {
            self.forget_if(session, path, recorded);
        }
        verdict(Some(recorded), current)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Distinguishes this process's fixtures without reading the clock: the pid
    /// separates concurrent `cargo test` runs, the counter separates tests.
    static FIXTURE: AtomicUsize = AtomicUsize::new(0);

    fn stamp(len: u64) -> Stamp {
        Stamp {
            len,
            modified: None,
        }
    }

    /// An isolated fixture directory under the system temp dir.
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ox-file-state-{name}-{}-{}",
            std::process::id(),
            FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("create the isolated file-state fixture");
        root
    }

    #[test]
    fn an_unrecorded_path_is_unrecorded() {
        assert_eq!(
            verdict(None, Presence::Stamp(stamp(1))),
            Verdict::Unrecorded
        );
        assert_eq!(verdict(None, Presence::Missing), Verdict::Unrecorded);
        assert_eq!(verdict(None, Presence::Unreadable), Verdict::Unrecorded);
    }

    #[test]
    fn an_identical_stamp_is_unchanged() {
        assert_eq!(
            verdict(Some(stamp(1)), Presence::Stamp(stamp(1))),
            Verdict::Unchanged
        );
    }

    #[test]
    fn a_different_length_or_time_is_changed() {
        assert_eq!(
            verdict(Some(stamp(1)), Presence::Stamp(stamp(2))),
            Verdict::Changed
        );
        let later = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        assert_eq!(
            verdict(
                Some(Stamp {
                    len: 1,
                    modified: Some(SystemTime::UNIX_EPOCH)
                }),
                Presence::Stamp(Stamp {
                    len: 1,
                    modified: Some(later)
                })
            ),
            Verdict::Changed,
            "a rewrite that keeps the length is caught by the modification time"
        );
    }

    #[test]
    fn an_unreadable_current_state_is_changed() {
        assert_eq!(
            verdict(Some(stamp(1)), Presence::Unreadable),
            Verdict::Changed,
            "a guard that cannot read the file must not pass"
        );
    }

    #[test]
    fn a_missing_file_is_not_a_conflict() {
        assert_eq!(
            verdict(Some(stamp(1)), Presence::Missing),
            Verdict::Unrecorded,
            "a deleted file has nothing to overwrite, so a recreation must not be refused"
        );
    }

    #[test]
    fn a_recorded_stamp_is_read_back() {
        let states = FileStates::new();
        states.record_stamp("ses_a", Path::new("/tmp/a.txt"), stamp(3));

        assert_eq!(
            states.recorded("ses_a", Path::new("/tmp/a.txt")),
            Some(stamp(3))
        );
        assert_eq!(
            states.recorded("ses_b", Path::new("/tmp/a.txt")),
            None,
            "another session's records are not visible"
        );
        assert_eq!(states.recorded("ses_a", Path::new("/tmp/other.txt")), None);
    }

    #[test]
    fn re_recording_replaces_the_stamp() {
        let states = FileStates::new();
        states.record_stamp("ses_a", Path::new("/tmp/a.txt"), stamp(3));
        states.record_stamp("ses_a", Path::new("/tmp/a.txt"), stamp(9));

        assert_eq!(
            states.recorded("ses_a", Path::new("/tmp/a.txt")),
            Some(stamp(9)),
            "the newest stamp wins"
        );
    }

    #[test]
    fn a_full_session_evicts_its_earliest_path() {
        let states = FileStates::new();
        for index in 0..MAX_PATHS_PER_SESSION {
            states.record_stamp(
                "ses_a",
                &PathBuf::from(format!("/tmp/{index}.txt")),
                stamp(index as u64),
            );
        }
        states.record_stamp(
            "ses_a",
            &PathBuf::from(format!("/tmp/{MAX_PATHS_PER_SESSION}.txt")),
            stamp(999),
        );

        assert_eq!(
            states.recorded("ses_a", Path::new("/tmp/0.txt")),
            None,
            "the earliest-inserted path is evicted"
        );
        assert_eq!(
            states.recorded("ses_a", Path::new("/tmp/1.txt")),
            Some(stamp(1)),
            "the next path in insertion order survives"
        );
        assert!(
            states
                .recorded(
                    "ses_a",
                    &PathBuf::from(format!("/tmp/{MAX_PATHS_PER_SESSION}.txt"))
                )
                .is_some(),
            "the new path is recorded"
        );
    }

    #[test]
    fn beyond_the_session_cap_the_least_recently_recorded_is_evicted() {
        let states = FileStates::new();
        let sessions: Vec<String> = (0..MAX_SESSIONS)
            .map(|index| format!("ses_{index}"))
            .collect();
        for (index, session) in sessions.iter().enumerate() {
            states.record_stamp(session, Path::new("/tmp/a.txt"), stamp(index as u64));
        }
        // Touch the first session again: it is now the most recently recorded.
        states.record_stamp("ses_0", Path::new("/tmp/a.txt"), stamp(0));
        states.record_stamp("ses_new", Path::new("/tmp/a.txt"), stamp(0));

        assert_eq!(
            states.recorded("ses_1", Path::new("/tmp/a.txt")),
            None,
            "the least recently recorded session is evicted"
        );
        assert_eq!(
            states.recorded("ses_0", Path::new("/tmp/a.txt")),
            Some(stamp(0)),
            "re-recording kept the touched session alive"
        );
        assert_eq!(
            states.recorded("ses_new", Path::new("/tmp/a.txt")),
            Some(stamp(0)),
            "the newest session is recorded"
        );
    }

    #[test]
    fn record_and_check_track_a_real_file() {
        let root = fixture("real");
        let path = root.join("note.txt");
        std::fs::write(&path, "one\n").expect("write the fixture");
        let states = FileStates::new();

        assert_eq!(
            states.check("ses_a", &path),
            Verdict::Unrecorded,
            "nothing is recorded before the first read"
        );
        states.record("ses_a", &path);
        assert_eq!(states.check("ses_a", &path), Verdict::Unchanged);

        std::fs::write(&path, "two lines\n").expect("rewrite the fixture");
        assert_eq!(
            states.check("ses_a", &path),
            Verdict::Changed,
            "a rewrite with a different length is detected without a content hash"
        );

        std::fs::remove_file(&path).expect("remove the fixture");
        assert_eq!(
            states.check("ses_a", &path),
            Verdict::Unrecorded,
            "a deleted file is not a conflict: there is nothing to overwrite"
        );
        assert_eq!(
            states.recorded("ses_a", &path),
            None,
            "checking a deleted path forgets its stamp so a recreation is not refused"
        );
        std::fs::write(&path, "three\n").expect("recreate the fixture");
        assert_eq!(
            states.check("ses_a", &path),
            Verdict::Unrecorded,
            "the recreated file has no record to compare against"
        );
        states.record("ses_a", &path);
        assert_eq!(states.check("ses_a", &path), Verdict::Unchanged);
        std::fs::remove_dir_all(root).expect("remove the isolated fixture");
    }

    #[test]
    fn recording_a_missing_path_records_nothing() {
        let root = fixture("missing");
        let path = root.join("absent.txt");
        let states = FileStates::new();

        states.record("ses_a", &path);

        assert_eq!(
            states.recorded("ses_a", &path),
            None,
            "an unreadable path must not become a stale entry"
        );
        assert_eq!(states.check("ses_a", &path), Verdict::Unrecorded);
        std::fs::remove_dir_all(root).expect("remove the isolated fixture");
    }
}
