//! Tmp retention: which entries under `ox_home()/tmp` are old enough to delete.
//!
//! This module owns "which tmp entries expire, and how they are deleted": the
//! selection rule as pure functions over `(name, newest mtime)` pairs, and a
//! thin edge ([`scan`], [`sweep_once`], [`spawn_sweeper`]) that measures and
//! deletes on disk. An entry (a top-level file, directory, or symlink) expires
//! only when nothing inside it, at any depth, was modified since the cutoff.
//! Anything that cannot be measured is kept. The scan never follows symlinks.
//! An entry that turns fresh between the scan and its deletion is still
//! deleted; this task accepts that window.
//!
//! Remaining facts live elsewhere: where `tmp` is and how many days to keep
//! come from the caller (config); the session sweeper is `session_retention`.

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const DAY_MS: i64 = 86_400_000;

/// Same cadence as `session_retention::SWEEP_INTERVAL`.
const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Upper bound on `retention_days`, matching `session_retention`: a
/// nonsensical value must not overflow into a cutoff that deletes everything.
const MAX_RETENTION_DAYS: u32 = 3_650;

/// A top-level tmp entry and the newest mtime found in it or beneath it.
/// `newest_ms` is `None` when any part could not be measured (kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: OsString,
    pub(crate) newest_ms: Option<i64>,
}

/// What one [`sweep_once`] found and did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepReport {
    pub(crate) scanned: usize,
    pub(crate) deleted: usize,
    pub(crate) failed: usize,
}

/// Epoch-millisecond cutoff below which an entry is expired. Days are clamped
/// to a decade and the subtraction saturates, so no input can wrap the cutoff.
pub(crate) fn cutoff_ms(now_ms: i64, days: u32) -> i64 {
    let window = i64::from(days.min(MAX_RETENTION_DAYS)) * DAY_MS;
    now_ms.saturating_sub(window)
}

/// Entries whose newest mtime is strictly older than the cutoff. An entry that
/// could not be measured (`None`) is never selected.
pub(crate) fn expired(entries: &[Entry], cutoff_ms: i64) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|e| e.newest_ms.is_some_and(|m| m < cutoff_ms))
        .collect()
}

/// Measure every direct child of `dir`. Fails only if `dir` cannot be listed.
///
/// `newest_ms` is the maximum mtime over the entry and every node beneath it
/// (files, directories, and symlinks themselves; links are never followed).
/// Traversal stops as soon as the running maximum reaches `cutoff_ms`, since
/// the entry is then certain to be kept. Any error while measuring an entry
/// yields `None` for that entry only.
pub(crate) fn scan(dir: &Path, cutoff_ms: i64) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for child in fs::read_dir(dir)? {
        let child = child?;
        let newest_ms = newest_mtime(&child, cutoff_ms).ok();
        out.push(Entry {
            name: child.file_name(),
            newest_ms,
        });
    }
    Ok(out)
}

/// Newest mtime in `top` and beneath it, or early once it reaches `cutoff_ms`.
/// Iterative: the stack holds one path per pending directory.
fn newest_mtime(top: &fs::DirEntry, cutoff_ms: i64) -> io::Result<i64> {
    let meta = top.metadata()?;
    let mut newest = mtime_ms(&meta)?;
    if newest >= cutoff_ms || !meta.is_dir() {
        return Ok(newest);
    }
    let mut stack = vec![top.path()];
    while let Some(dir) = stack.pop() {
        for child in fs::read_dir(&dir)? {
            let child = child?;
            // `DirEntry::metadata` does not follow symlinks.
            let meta = child.metadata()?;
            newest = newest.max(mtime_ms(&meta)?);
            if newest >= cutoff_ms {
                return Ok(newest);
            }
            if meta.is_dir() {
                stack.push(child.path());
            }
        }
    }
    Ok(newest)
}

/// Milliseconds since the epoch; a pre-epoch or unavailable mtime is an error
/// so the entry is kept.
fn mtime_ms(meta: &Metadata) -> io::Result<i64> {
    let since_epoch = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    i64::try_from(since_epoch.as_millis()).map_err(io::Error::other)
}

/// One retention pass over `dir`: delete expired top-level entries. `dir`
/// itself is never deleted; one entry failing is logged and skipped.
pub(crate) fn sweep_once(dir: &Path, now_ms: i64, days: u32) -> io::Result<SweepReport> {
    let cutoff = cutoff_ms(now_ms, days);
    let entries = scan(dir, cutoff)?;
    let mut report = SweepReport {
        scanned: entries.len(),
        ..SweepReport::default()
    };
    for entry in expired(&entries, cutoff) {
        let path = dir.join(&entry.name);
        match remove_entry(&path) {
            Ok(()) => report.deleted += 1,
            Err(error) => {
                report.failed += 1;
                tracing::warn!(path = %path.display(), error = %error, "tmp retention: deleting entry failed");
            }
        }
    }
    if report.deleted > 0 {
        tracing::info!(
            deleted = report.deleted,
            failed = report.failed,
            days,
            "tmp retention: deleted expired tmp entries"
        );
    }
    Ok(report)
}

/// Real directories are removed recursively; files and symlinks (the link
/// itself, never its target) with `remove_file`.
fn remove_entry(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Sweep once now and then every [`SWEEP_INTERVAL`]. `days == 0` disables
/// retention. Best-effort: a failed sweep is logged and retried next tick.
pub(crate) fn spawn_sweeper(dir: PathBuf, days: u32) {
    if days == 0 {
        return;
    }
    tokio::spawn(async move {
        loop {
            let target = dir.clone();
            let now = ox_schema::id::now_millis();
            match tokio::task::spawn_blocking(move || sweep_once(&target, now, days)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "tmp retention: sweep failed");
                }
                Err(error) => {
                    tracing::warn!(error = %error, "tmp retention: sweep task failed");
                }
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    });
}
