//! Read-only worktree inventory for the TUI's Worktrees panel.
//!
//! Upstream (`packages/core/src/worktree.ts` + `worktree/strategies.ts`) keeps a
//! `worktree` table, reconciles it with git through pluggable strategies, and
//! exposes create/remove/refresh over `/api/worktree`. ox has none of that; this
//! module answers the panel's *list* call straight from `git worktree list`, so
//! the panel renders the checkouts that exist on disk instead of failing with
//! "No route for GET /api/worktree".
//!
//! Deliberate deviations, recorded here:
//!
//! - **No inventory table.** git already is the source of truth for checkouts,
//!   so `refresh` acknowledges without reconciling anything (upstream publishes
//!   `worktree.updated` afterwards; ox has no crate-side worktree state to
//!   change).
//! - **Strategy labels are derived, not stored.** The first entry git reports is
//!   the repository root and carries no `strategy`; every linked checkout gets
//!   `"git"` — the only strategy upstream ships.
//! - **Fail-soft.** A non-repository directory, a missing `git` binary or a
//!   hanging git all degrade to the directory itself. The panel is a reader; a
//!   degraded list is better than a 500.
//! - **Create/remove stay unimplemented.** `POST /api/worktree` and
//!   `DELETE /api/worktree` are not routed, so the panel's create and delete
//!   actions still surface an error toast.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;

/// Upstream `Worktree.Directory` (`@opencode/schema/worktree.ts`): the checkout
/// path plus the strategy that created it (absent for the root checkout).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WorktreeDirectory {
    pub directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
}

/// `git worktree list` on a local repository answers in milliseconds; a hung
/// git (network checkout, index lock) must not hold an HTTP request open.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The project's checkouts: the root first (no strategy), then the linked
/// worktrees in git's order.
pub(crate) async fn list(directory: &str) -> Vec<WorktreeDirectory> {
    match git_worktrees(directory).await {
        Some(paths) if !paths.is_empty() => paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| WorktreeDirectory {
                directory: canonical(&path),
                strategy: (index > 0).then(|| "git".to_owned()),
            })
            .collect(),
        _ => vec![WorktreeDirectory {
            directory: canonical(directory),
            strategy: None,
        }],
    }
}

/// `git -C <directory> worktree list --porcelain`, mapped to absolute paths;
/// `None` when git is missing, fails, or times out.
async fn git_worktrees(directory: &str) -> Option<Vec<String>> {
    let running = tokio::process::Command::new("git")
        .args(["-C", directory, "worktree", "list", "--porcelain"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(GIT_TIMEOUT, running)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("worktree ")
                .map(|path| path.trim().to_owned())
        })
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// Absolute, symlink-resolved form (the panel compares these against the
/// locations it already holds, which `directory_of` canonicalizes the same way).
fn canonical(directory: &str) -> String {
    let path = PathBuf::from(directory);
    path.canonicalize().unwrap_or(path).display().to_string()
}
