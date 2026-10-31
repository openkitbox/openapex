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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::SystemTime;

    const DAY: i64 = DAY_MS;

    fn entry(name: &str, newest_ms: Option<i64>) -> Entry {
        Entry {
            name: name.into(),
            newest_ms,
        }
    }

    fn names<'a>(entries: &[&'a Entry]) -> Vec<&'a str> {
        entries.iter().map(|e| e.name.to_str().unwrap()).collect()
    }

    #[test]
    fn entries_older_than_the_cutoff_are_selected() {
        let all = [entry("a", Some(1)), entry("b", Some(99))];
        assert_eq!(names(&expired(&all, 100)), vec!["a", "b"]);
    }

    #[test]
    fn entry_exactly_at_the_cutoff_is_kept() {
        assert!(expired(&[entry("a", Some(100))], 100).is_empty());
    }

    #[test]
    fn entry_newer_than_the_cutoff_is_kept() {
        assert!(expired(&[entry("a", Some(101))], 100).is_empty());
    }

    #[test]
    fn unmeasurable_entry_is_kept() {
        assert!(expired(&[entry("a", None)], i64::MAX).is_empty());
    }

    #[test]
    fn cutoff_subtracts_whole_days() {
        assert_eq!(cutoff_ms(100 * DAY, 30), 70 * DAY);
    }

    #[test]
    fn cutoff_clamps_days_to_ten_years() {
        let now = 10_000 * DAY;
        assert_eq!(cutoff_ms(now, u32::MAX), cutoff_ms(now, 3_650));
        assert_eq!(cutoff_ms(now, 3_650), now - 3_650 * DAY);
    }

    #[test]
    fn cutoff_never_wraps() {
        assert_eq!(cutoff_ms(5, u32::MAX), 5 - 3_650 * DAY);
        assert_eq!(cutoff_ms(0, 30), -30 * DAY);
        assert_eq!(cutoff_ms(i64::MIN, u32::MAX), i64::MIN);
        assert_eq!(cutoff_ms(i64::MAX, u32::MAX), i64::MAX - 3_650 * DAY);
    }

    fn now_ms() -> i64 {
        ox_schema::id::now_millis()
    }

    fn set_mtime(path: &Path, ms: i64) {
        let t: SystemTime = UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap());
        File::open(path)
            .unwrap()
            .set_modified(t)
            .unwrap_or_else(|e| panic!("set_modified {}: {e}", path.display()));
    }

    /// A file with the given mtime.
    fn file(path: &Path, ms: i64) {
        fs::write(path, b"x").unwrap();
        set_mtime(path, ms);
    }

    /// `root/sub/f` all stamped `ms`; directories are stamped last because
    /// creating a child bumps its parent's mtime.
    fn old_tree(root: &Path, ms: i64) {
        fs::create_dir_all(root.join("sub")).unwrap();
        file(&root.join("sub").join("f"), ms);
        set_mtime(&root.join("sub"), ms);
        set_mtime(root, ms);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn old_files_and_old_trees_are_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        file(&tmp.path().join("old.txt"), now - 30 * DAY);
        old_tree(&tmp.path().join("old_dir"), now - 30 * DAY);
        file(&tmp.path().join("fresh.txt"), now - DAY);

        let report = sweep_once(tmp.path(), now, 7).unwrap();

        assert_eq!(
            report,
            SweepReport {
                scanned: 3,
                deleted: 2,
                failed: 0
            }
        );
        assert_eq!(names_in(tmp.path()), vec!["fresh.txt"]);
    }

    #[test]
    fn old_directory_holding_a_fresh_file_is_kept_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let root = tmp.path().join("live");
        fs::create_dir_all(root.join("a/b")).unwrap();
        file(&root.join("a/b/fresh"), now - DAY);
        file(&root.join("a/old"), now - 30 * DAY);
        for d in ["a/b", "a", ""] {
            set_mtime(&root.join(d), now - 30 * DAY);
        }

        let entries = scan(tmp.path(), cutoff_ms(now, 7)).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].newest_ms.unwrap() >= cutoff_ms(now, 7));

        let report = sweep_once(tmp.path(), now, 7).unwrap();

        assert_eq!(report.deleted, 0);
        assert!(root.join("a/b/fresh").exists() && root.join("a/old").exists());
    }

    #[test]
    fn old_symlink_is_removed_and_its_target_is_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let now = now_ms();
        old_tree(outside.path(), now - 30 * DAY);
        // The link itself is created now; sweep from a later clock.
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();

        let report = sweep_once(tmp.path(), now + 30 * DAY, 7).unwrap();

        assert_eq!(report.deleted, 1);
        assert!(!tmp.path().join("link").exists());
        assert!(fs::symlink_metadata(tmp.path().join("link")).is_err());
        assert!(outside.path().join("sub/f").exists());
    }

    #[test]
    fn symlink_is_not_followed_when_measuring() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let now = now_ms();
        // The target is newer than the cutoff; the link itself (created now)
        // is older. Following the link would keep it.
        old_tree(outside.path(), now);
        file(&outside.path().join("sub/newest"), now + 40 * DAY);
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();

        let report = sweep_once(tmp.path(), now + 30 * DAY, 7).unwrap();

        assert_eq!(report.deleted, 1, "link was followed and kept");
        assert!(fs::symlink_metadata(tmp.path().join("link")).is_err());
        assert!(outside.path().join("sub/newest").exists());
    }

    #[test]
    fn symlink_inside_an_entry_is_not_followed_either() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let now = now_ms();
        old_tree(outside.path(), now);
        file(&outside.path().join("sub/newest"), now + 40 * DAY);
        let root = tmp.path().join("holder");
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        set_mtime(&root, now - 30 * DAY);

        // The link itself was created at ~now, before the cutoff (now + 23d);
        // following it would reach the file stamped now + 40d and keep holder.
        let report = sweep_once(tmp.path(), now + 30 * DAY, 7).unwrap();

        assert_eq!(report.deleted, 1, "inner link was followed");
        assert!(!root.exists());
        assert!(outside.path().join("sub/newest").exists());
    }

    #[test]
    fn the_swept_directory_itself_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        file(&tmp.path().join("old"), now - 30 * DAY);

        let report = sweep_once(tmp.path(), now, 7).unwrap();

        assert_eq!(report.deleted, 1);
        assert!(tmp.path().is_dir());
        assert!(names_in(tmp.path()).is_empty());
    }

    /// Restores write permission on drop so `tempdir` can clean up even when
    /// an assertion fails.
    struct Unlock(PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn one_failing_entry_does_not_stop_the_others() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let stuck = tmp.path().join("a_stuck");
        old_tree(&stuck, now - 30 * DAY);
        file(&tmp.path().join("b_ok"), now - 30 * DAY);
        // A read-only directory cannot have its children unlinked.
        let _unlock = Unlock(stuck.clone());
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();

        let report = sweep_once(tmp.path(), now, 7).unwrap();

        assert_eq!(
            report,
            SweepReport {
                scanned: 2,
                deleted: 1,
                failed: 1
            }
        );
        assert_eq!(names_in(tmp.path()), vec!["a_stuck"]);
    }

    #[test]
    fn scanning_a_missing_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan(&tmp.path().join("nope"), 0).is_err());
        assert!(sweep_once(&tmp.path().join("nope"), now_ms(), 7).is_err());
    }

    #[test]
    fn disabled_sweeper_does_not_spawn() {
        // No tokio runtime here: `tokio::spawn` would panic if it were called.
        spawn_sweeper(PathBuf::from("/nonexistent"), 0);
    }
}
