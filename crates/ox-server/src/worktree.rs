//! Read-only worktree inventory for the server's worktree API.
//!
//! Upstream (`packages/core/src/worktree.ts` + `worktree/strategies.ts`) keeps a
//! `worktree` table, reconciles it with git through pluggable strategies, and
//! exposes create/remove/refresh over `/api/worktree`. ox has none of that; this
//! module answers the list call straight from `git worktree list`, so clients
//! can inspect the checkouts that exist on disk instead of failing with
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
//!   hanging git all degrade to the directory itself. The endpoint is read-only; a degraded list is better than a 500.
//! - **No strategy table.** `POST /api/worktree` always uses the built-in git
//!   strategy (`git worktree add --detach -- <directory> <ref|HEAD>`, upstream
//!   `core/src/git.ts:1254`); there is no configured strategy list to select
//!   from and no setup command to run afterwards.
//! - **API and agent shell share one strategy.** Both run plain `git worktree add`,
//!   so a checkout never carries ignored build caches (`target/`, `node_modules/`).
//! - **Default location is `<repo>/.worktrees/`.** Upstream joins the project
//!   configuration's directory (or its data directory) with the name. ox has no
//!   project worktree settings, so unnamed roots land beside the checkout in
//!   the same `.worktrees/` directory this repository's own workflow uses.

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

/// `git worktree add` copies a checkout and may touch the object store; give it
/// the same generous budget the rest of the server's git plumbing gets.
const GIT_MUTATE_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Absolute, symlink-resolved form used by the endpoint and `directory_of`.
fn canonical(directory: &str) -> String {
    let path = PathBuf::from(directory);
    path.canonicalize().unwrap_or(path).display().to_string()
}

/// Upstream `Worktree.CreateInput` minus the strategy/config fields ox has no
/// storage for.
#[derive(Debug, Default, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct CreateInput {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// A `WorktreeError` body (`{ name, data: { message, forceRequired? } }`) with
/// the HTTP status the caller should answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Failure {
    pub message: String,
    pub force_required: bool,
}

impl Failure {
    fn message(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            force_required: false,
        }
    }
}

/// Create a linked checkout: `git worktree add --detach -- <directory> <ref>`
/// where `ref` defaults to `HEAD`, and the directory is the requested one or
/// `<repo>/.worktrees/<name>` with `-2`..`-10` suffixes past a collision.
pub(crate) async fn create(root: &str, input: CreateInput) -> Result<String, Failure> {
    let source = input.from.clone().unwrap_or_else(|| root.to_owned());
    let repository = git_toplevel(&source)
        .await
        .ok_or_else(|| Failure::message(format!("Not a git repository: {source}")))?;
    let parent = input
        .directory
        .clone()
        .filter(|directory| !directory.trim().is_empty())
        .unwrap_or_else(|| format!("{}/.worktrees", repository.trim_end_matches('/')));
    if let Err(error) = tokio::fs::create_dir_all(&parent).await {
        return Err(Failure::message(format!(
            "Could not create {parent}: {error}"
        )));
    }

    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(ox_schema::slug::create);
    let mut directory = PathBuf::from(&parent).join(&name);
    for suffix in 2..=10 {
        if !tokio::fs::try_exists(&directory).await.unwrap_or(false) {
            break;
        }
        if suffix == 10 {
            return Err(Failure::message(format!(
                "Worktree destination already exists: {}",
                directory.display()
            )));
        }
        directory = PathBuf::from(&parent).join(format!("{name}-{suffix}"));
    }

    let reference = input.branch.as_deref().unwrap_or("HEAD");
    let target = directory.display().to_string();
    let (ok, _, stderr) = git(
        &repository,
        &["worktree", "add", "--detach", "--", &target, reference],
        GIT_MUTATE_TIMEOUT,
    )
    .await
    .ok_or_else(|| Failure::message("git worktree add did not run"))?;
    if !ok {
        return Err(Failure::message(if stderr.is_empty() {
            "Creating the worktree failed".to_owned()
        } else {
            stderr
        }));
    }
    Ok(canonical(&target))
}

/// Remove a linked checkout, reporting `forceRequired` when git refuses because
/// the checkout carries modified or untracked files (upstream
/// `core/src/git.ts:1237`).
pub(crate) async fn remove(directory: &str, force: bool) -> Result<(), Failure> {
    let repository = git_toplevel(directory)
        .await
        .ok_or_else(|| Failure::message(format!("Not a git repository: {directory}")))?;
    let target = canonical(directory);
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push("--");
    args.push(&target);
    let (ok, _, stderr) = git(&repository, &args, GIT_MUTATE_TIMEOUT)
        .await
        .ok_or_else(|| Failure::message("git worktree remove did not run"))?;
    if ok {
        return Ok(());
    }
    let message = if stderr.is_empty() {
        "Removing the worktree failed".to_owned()
    } else {
        stderr
    };
    let dirty = message.contains("contains modified or untracked files")
        || message.to_ascii_lowercase().contains("is dirty");
    Err(Failure {
        message,
        force_required: dirty,
    })
}

/// The worktree root that owns `directory`.
async fn git_toplevel(directory: &str) -> Option<String> {
    let (ok, stdout, _) = git(directory, &["rev-parse", "--show-toplevel"], GIT_TIMEOUT).await?;
    let root = stdout.trim();
    (ok && !root.is_empty()).then(|| root.to_owned())
}

/// Run git with `directory` as the working root, returning
/// `(success, stdout, trimmed stderr)`.
async fn git(directory: &str, args: &[&str], timeout: Duration) -> Option<(bool, String, String)> {
    let running = tokio::process::Command::new("git")
        .args(["--no-optional-locks", "-C", directory])
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, running).await.ok()?.ok()?;
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}
