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
