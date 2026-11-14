//! Location-scoped version control reads for API clients and worktree tooling: `server/src/handlers/vcs.ts` over
//! `core/src/plugin/vcs/git.ts` + `core/src/vcs/patch.ts`.
//!
//! Ported in full where it decides *what* the client sees:
//!
//! - `status` = `git status --porcelain=v1 --untracked-files=all --no-renames`,
//!   line counts from `git diff --numstat` against `HEAD`, untracked files
//!   counted with `git diff --no-index /dev/null <file>`;
//! - `diff` modes `working` (HEAD→worktree), `branch` (merge-base→worktree),
//!   `committed` (merge-base→HEAD), with `base` overriding the inferred review
//!   base and `context` defaulting to full-file hunks;
//! - `base` inference from branch-creation reflog entries, falling back to the
//!   repository default branch only when it is checked out;
//! - `branches` from `for-each-ref` over local and remote refs, `search`
//!   interpolated into the ref pattern, `limit` capped at 100;
//! - patch assembly: one `git diff -p` for the whole directory split into
//!   per-file chunks, per-file patches for anything the split misses, and
//!   untracked files built with `git diff --no-index`;
//! - the upstream caps (`MAX_PATCH_BYTES`, `MAX_TOTAL_PATCH_BYTES` = 10 MB):
//!   once the total would overflow, that file and every later one carries an
//!   empty patch instead of a truncated body.
//!
//! Deliberate deviations, recorded here:
//!
//! - **No untracked index batching.** Upstream feeds every untracked file into
//!   a throwaway `GIT_INDEX_FILE` so one `git diff` covers them all. ox runs
//!   one `--no-index` per file; the result is byte-identical, the cost is one
//!   process per untracked file.
//! - **Empty patches are `diff --git a/<f> b/<f>`.** Upstream formats a
//!   header-only hunk through the JS `diff` package (`===` banner). Clients
//!   read `patch` as text; the git-style header is the shape every other entry
//!   already has.
//! - **Non-repository locations degrade instead of erroring.** `status`,
//!   `branches` and `diff` answer empty, `base` answers `null`; a git failure
//!   inside a real repository is still a 503 the way upstream maps `DiffError`.
//! - **No VCS provider table.** `vcs.init` only knows `git` (the built-in
//!   upstream default); any other `provider` is a 400.

use std::process::Stdio;
use std::time::Duration;

use salvo::prelude::*;
use serde::Serialize;
use serde_json::json;

use super::compat::{directory_of, envelope};

/// Full-file hunks, exactly upstream's `PATCH_CONTEXT_LINES`.
const PATCH_CONTEXT_LINES: u32 = i32::MAX as u32;
const MAX_PATCH_BYTES: usize = 10_000_000;
const MAX_TOTAL_PATCH_BYTES: usize = 10_000_000;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// `Vcs.FileStatus` / `FileDiff.Info` share every field but `patch`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct FileDiff {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub status: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct FileStatus {
    pub file: String,
    pub additions: u64,
    pub deletions: u64,
    pub status: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Base {
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub source: &'static str,
}

struct Git {
    directory: String,
    worktree: String,
}

struct Output {
    ok: bool,
    text: String,
    truncated: bool,
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

#[handler]
pub(crate) async fn info(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let (current, default) = match Git::open(&directory).await {
        Some(git) => (
            git.branch(&git.directory).await,
            git.default_branch().await.map(|branch| branch.name),
        ),
        None => (None, None),
    };
    res.render(Json(envelope(
        &directory,
        json!({ "branch": { "current": current.unwrap_or_default(), "default": default.unwrap_or_default() } }),
    )));
}

#[handler]
pub(crate) async fn base(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let Some(git) = Git::open(&directory).await else {
        return res.render(Json(envelope(&directory, serde_json::Value::Null)));
    };
    match git.base().await {
        Ok(found) => res.render(Json(envelope(&directory, json!(found)))),
        Err(message) => unavailable(res, &message),
    }
}

#[handler]
pub(crate) async fn status(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let files = match Git::open(&directory).await {
        Some(git) => git.status().await,
        None => Vec::new(),
    };
    res.render(Json(envelope(&directory, json!(files))));
}

#[handler]
pub(crate) async fn branches(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let search = req.query::<String>("search");
    let limit = req.query::<u32>("limit").unwrap_or(50).min(100);
    let list = match Git::open(&directory).await {
        Some(git) => git.branches(search.as_deref(), limit).await,
        None => Vec::new(),
    };
    res.render(Json(envelope(&directory, json!(list))));
}

#[handler]
pub(crate) async fn diff(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let mode = req
        .query::<String>("mode")
        .unwrap_or_else(|| "working".to_owned());
    let requested_base = req.query::<String>("base");
    let context = req.query::<u32>("context").unwrap_or(PATCH_CONTEXT_LINES);
    let Some(git) = Git::open(&directory).await else {
        return res.render(Json(envelope(&directory, json!([]))));
    };
    match git.diff(&mode, requested_base.as_deref(), context).await {
        Ok(files) => res.render(Json(envelope(&directory, json!(files)))),
        Err(message) => unavailable(res, &message),
    }
}

#[handler]
pub(crate) async fn init(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let provider = req
        .query::<String>("provider")
        .unwrap_or_else(|| "git".to_owned());
    if provider != "git" {
        res.status_code(StatusCode::BAD_REQUEST);
        return res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Unknown VCS provider",
            "field": "provider",
        })));
    }
    if !std::path::Path::new(&directory).is_dir() {
        res.status_code(StatusCode::BAD_REQUEST);
        return res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Project directory does not exist",
            "field": "location",
        })));
    }
    if Git::open(&directory).await.is_some() {
        res.status_code(StatusCode::CONFLICT);
        return res.render(Json(json!({
            "_tag": "ConflictError",
            "message": "Project already has version control",
            "resource": directory,
        })));
    }
    let created = run(&directory, &["init"], MAX_PATCH_BYTES).await;
    if !created.ok {
        return unavailable(res, "VCS initialization failed");
    }
    res.status_code(StatusCode::NO_CONTENT);
}

fn unavailable(res: &mut Response, message: &str) {
    res.status_code(StatusCode::SERVICE_UNAVAILABLE);
    res.render(Json(json!({
        "_tag": "ServiceUnavailableError",
        "service": "vcs",
        "message": message,
    })));
}

// ---------------------------------------------------------------------------
// Git adapter
// ---------------------------------------------------------------------------

impl Git {
    /// The requested location plus the worktree root that owns it.
    async fn open(directory: &str) -> Option<Self> {
        let root = run(directory, &["rev-parse", "--show-toplevel"], 65_536).await;
        if !root.ok {
            return None;
        }
        let worktree = root.text.trim();
        if worktree.is_empty() {
            return None;
        }
        Some(Git {
            directory: directory.to_owned(),
            worktree: worktree.to_owned(),
        })
    }

    async fn branch(&self, cwd: &str) -> Option<String> {
        let out = run(
            cwd,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            MAX_PATCH_BYTES,
        )
        .await;
        let name = out.text.trim();
        (out.ok && !name.is_empty()).then(|| name.to_owned())
    }

    /// `origin` if present, else the only remote, else `upstream`, else the first.
    async fn primary_remote(&self) -> Option<String> {
        let out = run(&self.worktree, &["remote"], MAX_PATCH_BYTES).await;
        let list: Vec<&str> = out
            .text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if list.contains(&"origin") {
            return Some("origin".to_owned());
        }
        if list.len() == 1 {
            return Some(list[0].to_owned());
        }
        if list.contains(&"upstream") {
            return Some("upstream".to_owned());
        }
        list.first().map(|remote| (*remote).to_owned())
    }

    async fn default_branch(&self) -> Option<Base> {
        if let Some(remote) = self.primary_remote().await {
            let reference = format!("refs/remotes/{remote}/HEAD");
            let out = run(
                &self.worktree,
                &["symbolic-ref", &reference],
                MAX_PATCH_BYTES,
            )
            .await;
            if out.ok {
                let target = out.text.trim().trim_start_matches("refs/remotes/");
                let name = target
                    .strip_prefix(&format!("{remote}/"))
                    .unwrap_or_default();
                if !name.is_empty() {
                    return Some(Base {
                        name: name.to_owned(),
                        reference: target.to_owned(),
                        source: "default",
                    });
                }
            }
        }
        let locals = self.local_branches().await;
        let configured = run(
            &self.worktree,
            &["config", "init.defaultBranch"],
            MAX_PATCH_BYTES,
        )
        .await;
        let configured = configured.text.trim();
        if !configured.is_empty() && locals.iter().any(|branch| branch == configured) {
            return Some(Base {
                name: configured.to_owned(),
                reference: configured.to_owned(),
                source: "default",
            });
        }
        for candidate in ["main", "master"] {
            if locals.iter().any(|branch| branch == candidate) {
                return Some(Base {
                    name: candidate.to_owned(),
                    reference: candidate.to_owned(),
                    source: "default",
                });
            }
        }
        None
    }

    async fn local_branches(&self) -> Vec<String> {
        let out = run(
            &self.worktree,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
            MAX_PATCH_BYTES,
        )
        .await;
        out.text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    async fn branches(&self, search: Option<&str>, limit: u32) -> Vec<String> {
        let pattern = search.map(|search| {
            let escaped = search.trim().chars().fold(String::new(), |mut acc, ch| {
                if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
                    acc.push('\\');
                }
                acc.push(ch);
                acc
            });
            (escaped, search.trim().to_owned())
        });
        let mut args: Vec<String> = vec![
            "for-each-ref".into(),
            "--ignore-case".into(),
            "--sort=refname".into(),
            "--sort=-committerdate".into(),
            "--format=%(refname:short)".into(),
            format!("--count={limit}"),
        ];
        match &pattern {
            Some((escaped, _)) => {
                args.push(format!("refs/heads/*{escaped}*"));
                args.push(format!("refs/remotes/*{escaped}*"));
            }
            None => {
                args.push("refs/heads".into());
                args.push("refs/remotes".into());
            }
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&self.worktree, &refs, MAX_PATCH_BYTES).await;
        out.text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.ends_with("/HEAD"))
            .map(str::to_owned)
            .collect()
    }

    async fn has_head(&self) -> bool {
        run(&self.worktree, &["rev-parse", "--verify", "HEAD"], 65_536)
            .await
            .ok
    }

    async fn rev_parse(&self, reference: &str) -> Option<String> {
        let out = run(
            &self.worktree,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
            MAX_PATCH_BYTES,
        )
        .await;
        let commit = out.text.trim();
        (out.ok && !commit.is_empty()).then(|| commit.to_owned())
    }

    async fn is_ancestor(&self, commit: &str, reference: &str) -> bool {
        if !is_object_id(commit) {
            return false;
        }
        run(
            &self.worktree,
            &["merge-base", "--is-ancestor", commit, reference],
            MAX_PATCH_BYTES,
        )
        .await
        .ok
    }

    /// A branch name a creation reflog entry can point at (not `HEAD`, not an
    /// object id, not a revision expression).
    async fn named_ref(&self, input: &str) -> Option<Base> {
        if input == "HEAD"
            || input.ends_with("/HEAD")
            || input.chars().any(|ch| "~^:@{}\\ ".contains(ch))
        {
            return None;
        }
        let out = run(
            &self.worktree,
            &[
                "rev-parse",
                "--symbolic-full-name",
                "--verify",
                "--end-of-options",
                input,
            ],
            MAX_PATCH_BYTES,
        )
        .await;
        let reference = out.text.trim();
        if !(reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/"))
            || reference.ends_with("/HEAD")
            || self.rev_parse(reference).await.is_none()
        {
            return None;
        }
        let name = reference
            .strip_prefix("refs/heads/")
            .or_else(|| {
                reference
                    .strip_prefix("refs/remotes/")
                    .and_then(|rest| rest.split_once('/').map(|(_, name)| name))
            })
            .unwrap_or(reference);
        Some(Base {
            name: name.to_owned(),
            reference: reference.to_owned(),
            source: "reflog",
        })
    }

    async fn base(&self) -> Result<Option<Base>, String> {
        if !self.has_head().await {
            return Ok(None);
        }
        let Some(current) = self.branch(&self.worktree).await else {
            return Err("Choose a review base".to_owned());
        };
        let out = run(
            &self.worktree,
            &[
                "reflog",
                "show",
                "--max-count=256",
                "--format=%H%x00%gs",
                &format!("refs/heads/{current}"),
            ],
            MAX_PATCH_BYTES,
        )
        .await;
        let history: Vec<(String, String)> = out
            .text
            .lines()
            .filter_map(|line| line.split_once('\0'))
            .map(|(commit, message)| (commit.to_owned(), message.to_owned()))
            .collect();
        let renamed = history
            .iter()
            .any(|(_, message)| message.starts_with("Branch: renamed "));
        let creation = (!renamed)
            .then(|| {
                history
                    .iter()
                    .find(|(_, message)| message.starts_with("branch: Created from "))
            })
            .flatten();
        if let Some((commit, message)) = creation {
            let origin = message.trim_start_matches("branch: Created from ");
            if !origin.is_empty()
                && let Some(candidate) = self.named_ref(origin).await
                && candidate.name != current
                && self.is_ancestor(commit, "HEAD").await
                && self.is_ancestor(commit, &candidate.reference).await
            {
                return Ok(Some(candidate));
            }
        }
        let Some(root) = self.default_branch().await else {
            return Err("Choose a review base".to_owned());
        };
        if current != root.name {
            return Err("Choose a review base".to_owned());
        }
        let Some(candidate) = self.named_ref(&root.reference).await else {
            return Err("The default review base is unavailable".to_owned());
        };
        Ok(Some(Base {
            name: root.name,
            reference: candidate.reference,
            source: "default",
        }))
    }

    async fn merge_base(&self, base_ref: &str) -> Option<String> {
        let reference = self.rev_parse(base_ref).await?;
        let out = run(
            &self.worktree,
            &["merge-base", &reference, "HEAD"],
            MAX_PATCH_BYTES,
        )
        .await;
        let commit = out.text.trim();
        (out.ok && !commit.is_empty()).then(|| commit.to_owned())
    }

    async fn status_items(&self) -> Vec<Item> {
        let out = run(
            &self.directory,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
                "-z",
                "--",
                ".",
            ],
            MAX_TOTAL_PATCH_BYTES,
        )
        .await;
        parse_status(&out.text)
    }

    async fn diff_names(&self, reference: &str, target: Option<&str>) -> Result<Vec<Item>, String> {
        let mut args = vec![
            "diff".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-renames".to_owned(),
            "--name-status".to_owned(),
            "-z".to_owned(),
            reference.to_owned(),
        ];
        if let Some(target) = target {
            args.push(target.to_owned());
        }
        args.extend(["--".to_owned(), ".".to_owned()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&self.directory, &refs, MAX_TOTAL_PATCH_BYTES).await;
        if !out.ok {
            return Err("Unable to list Git changes".to_owned());
        }
        Ok(parse_name_status(&out.text))
    }

    async fn stats(&self, reference: &str, target: Option<&str>) -> Vec<(String, Stat)> {
        let mut args = vec![
            "diff".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-renames".to_owned(),
            "--numstat".to_owned(),
            "-z".to_owned(),
            reference.to_owned(),
        ];
        if let Some(target) = target {
            args.push(target.to_owned());
        }
        args.extend(["--".to_owned(), ".".to_owned()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&self.directory, &refs, MAX_TOTAL_PATCH_BYTES).await;
        parse_numstat(&out.text)
    }

    async fn patch_all(&self, reference: &str, target: Option<&str>, context: u32) -> Patch {
        let mut args = vec![
            "diff".to_owned(),
            "--patch".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-renames".to_owned(),
            format!("--unified={context}"),
            reference.to_owned(),
        ];
        if let Some(target) = target {
            args.push(target.to_owned());
        }
        args.extend(["--".to_owned(), ".".to_owned()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&self.directory, &refs, MAX_TOTAL_PATCH_BYTES).await;
        Patch {
            text: out.text,
            truncated: out.truncated,
        }
    }

    async fn patch(
        &self,
        reference: &str,
        file: &str,
        target: Option<&str>,
        context: u32,
    ) -> Patch {
        let mut args = vec![
            "diff".to_owned(),
            "--patch".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-renames".to_owned(),
            format!("--unified={context}"),
            reference.to_owned(),
        ];
        if let Some(target) = target {
            args.push(target.to_owned());
        }
        args.extend(["--".to_owned(), file.to_owned()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&self.worktree, &refs, MAX_PATCH_BYTES).await;
        Patch {
            text: out.text,
            truncated: out.truncated,
        }
    }

    async fn patch_untracked(&self, file: &str, context: u32) -> Patch {
        let out = run(
            &self.worktree,
            &[
                "diff",
                "--no-index",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                &format!("--unified={context}"),
                "--",
                "/dev/null",
                file,
            ],
            MAX_PATCH_BYTES,
        )
        .await;
        Patch {
            text: out.text,
            truncated: out.truncated,
        }
    }

    async fn stat_untracked(&self, file: &str) -> Option<Stat> {
        let out = run(
            &self.worktree,
            &["diff", "--no-index", "--numstat", "--", "/dev/null", file],
            4_096,
        )
        .await;
        if out.truncated {
            return None;
        }
        let line = out.text.lines().next()?;
        let (additions, rest) = line.split_once('\t')?;
        let (deletions, _) = rest.split_once('\t')?;
        Some(Stat {
            additions: count(additions),
            deletions: count(deletions),
        })
    }

    async fn status(&self) -> Vec<FileStatus> {
        let list = self.status_items().await;
        let stats = if self.has_head().await {
            self.stats("HEAD", None).await
        } else {
            Vec::new()
        };
        let map = num_map(stats);
        let mut files = Vec::with_capacity(list.len());
        for mut item in list {
            let mut stat = map.get(&item.file).cloned();
            if stat.is_none() && item.status == "added" && item.code == "??" {
                stat = self.stat_untracked(&item.file).await;
            }
            files.push(FileStatus {
                file: std::mem::take(&mut item.file),
                additions: stat.as_ref().map_or(0, |stat| stat.additions),
                deletions: stat.as_ref().map_or(0, |stat| stat.deletions),
                status: item.status,
            });
        }
        files.sort_by(|a, b| a.file.cmp(&b.file));
        files
    }

    async fn diff(
        &self,
        mode: &str,
        base_ref: Option<&str>,
        context: u32,
    ) -> Result<Vec<FileDiff>, String> {
        match mode {
            "working" => {
                let reference = self.has_head().await.then(|| "HEAD".to_owned());
                self.track(reference.as_deref(), context).await
            }
            "branch" | "committed" => {
                if !self.has_head().await {
                    return if mode == "committed" {
                        Ok(Vec::new())
                    } else {
                        self.track(None, context).await
                    };
                }
                let base_ref = match base_ref {
                    Some(base_ref) => base_ref.to_owned(),
                    None => self
                        .default_branch()
                        .await
                        .map(|branch| branch.reference)
                        .unwrap_or_default(),
                };
                if base_ref.is_empty() {
                    return Err("No review base available".to_owned());
                }
                let Some(reference) = self.merge_base(&base_ref).await else {
                    return Err(format!("No merge base available for {base_ref}"));
                };
                let target = (mode == "committed").then_some("HEAD");
                self.diff_against_ref(&reference, target, context).await
            }
            other => Err(format!("Unknown diff mode: {other}")),
        }
    }

    async fn track(&self, reference: Option<&str>, context: u32) -> Result<Vec<FileDiff>, String> {
        match reference {
            // No `HEAD` yet: every file is new, there is nothing to patch against.
            None => {
                let list = self.status_items().await;
                self.files(
                    None,
                    None,
                    list,
                    std::collections::HashMap::new(),
                    Batch::empty(),
                    context,
                )
                .await
            }
            Some(reference) => self.diff_against_ref(reference, None, context).await,
        }
    }

    async fn diff_against_ref(
        &self,
        reference: &str,
        target: Option<&str>,
        context: u32,
    ) -> Result<Vec<FileDiff>, String> {
        let list = self.diff_names(reference, target).await?;
        let stats = self.stats(reference, target).await;
        let extra = if target.is_some() {
            Vec::new()
        } else {
            self.status_items()
                .await
                .into_iter()
                .filter(|item| item.code == "??")
                .collect()
        };
        let merged = merge(list, extra);
        let batch = self
            .batch_patches(reference, target, &merged, context)
            .await;
        self.files(
            Some(reference),
            target,
            merged,
            num_map(stats),
            batch,
            context,
        )
        .await
    }

    async fn batch_patches(
        &self,
        reference: &str,
        target: Option<&str>,
        list: &[Item],
        context: u32,
    ) -> Batch {
        if list.is_empty() {
            return Batch::empty();
        }
        let patch = self.patch_all(reference, target, context).await;
        Batch {
            patches: chunks_by_file(&patch, list),
            capped: patch.truncated,
        }
    }

    async fn files(
        &self,
        reference: Option<&str>,
        target: Option<&str>,
        list: Vec<Item>,
        stats: std::collections::HashMap<String, Stat>,
        batch: Batch,
        context: u32,
    ) -> Result<Vec<FileDiff>, String> {
        let mut list = list;
        list.sort_by(|a, b| a.file.cmp(&b.file));
        let mut files = Vec::with_capacity(list.len());
        let mut total = 0usize;
        let mut capped = false;
        for item in list {
            let mut stat = stats.get(&item.file).cloned();
            if stat.is_none() && target.is_none() && item.status == "added" {
                stat = self.stat_untracked(&item.file).await;
            }
            let patch = if capped {
                empty_patch(&item.file)
            } else if let Some(batched) = batch.patches.get(&item.file) {
                batched.clone()
            } else if batch.capped && item.code != "??" {
                // The batch output was capped, so this file's patch may have
                // been cut in the middle; upstream leaves it empty instead.
                empty_patch(&item.file)
            } else if item.code == "??" || reference.is_none() {
                let patch = self.patch_untracked(&item.file, context).await;
                if patch.truncated {
                    empty_patch(&item.file)
                } else {
                    patch.text
                }
            } else {
                let patch = self
                    .patch(reference.unwrap_or_default(), &item.file, None, context)
                    .await;
                if patch.truncated || patch.text.is_empty() {
                    empty_patch(&item.file)
                } else {
                    patch.text
                }
            };
            let patch = if !capped && total + patch.len() > MAX_TOTAL_PATCH_BYTES {
                capped = true;
                empty_patch(&item.file)
            } else {
                if !capped {
                    total += patch.len();
                    capped = total >= MAX_TOTAL_PATCH_BYTES;
                }
                patch
            };
            files.push(FileDiff {
                file: item.file,
                patch: Some(patch),
                additions: stat.as_ref().map_or(0, |s| s.additions),
                deletions: stat.as_ref().map_or(0, |s| s.deletions),
                status: item.status,
            });
        }
        Ok(files)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Item {
    file: String,
    code: String,
    status: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
struct Stat {
    additions: u64,
    deletions: u64,
}

struct Patch {
    text: String,
    truncated: bool,
}

struct Batch {
    patches: std::collections::HashMap<String, String>,
    capped: bool,
}

impl Batch {
    fn empty() -> Self {
        Batch {
            patches: std::collections::HashMap::new(),
            capped: false,
        }
    }
}

fn merge(first: Vec<Item>, extra: Vec<Item>) -> Vec<Item> {
    let mut merged: Vec<Item> = Vec::with_capacity(first.len() + extra.len());
    for item in first.into_iter().chain(extra) {
        if !merged.iter().any(|seen| seen.file == item.file) {
            merged.push(item);
        }
    }
    merged
}

fn num_map(stats: Vec<(String, Stat)>) -> std::collections::HashMap<String, Stat> {
    stats.into_iter().collect()
}

fn kind(code: &str) -> &'static str {
    if code == "??" {
        return "added";
    }
    if code.contains('U') {
        return "modified";
    }
    if code.contains('A') && !code.contains('D') {
        return "added";
    }
    if code.contains('D') && !code.contains('A') {
        return "deleted";
    }
    "modified"
}

fn parse_status(text: &str) -> Vec<Item> {
    text.split('\0')
        .filter(|entry| entry.len() > 3)
        .filter_map(|entry| {
            let code = &entry[..2];
            let file = &entry[3..];
            (!file.is_empty()).then(|| Item {
                file: file.to_owned(),
                code: code.to_owned(),
                status: kind(code),
            })
        })
        .collect()
}

fn parse_name_status(text: &str) -> Vec<Item> {
    let parts: Vec<&str> = text.split('\0').filter(|part| !part.is_empty()).collect();
    let mut items = Vec::new();
    let mut index = 0;
    while index + 1 < parts.len() {
        let code = parts[index];
        let file = parts[index + 1];
        index += 2;
        items.push(Item {
            file: file.to_owned(),
            code: code.to_owned(),
            status: kind(code),
        });
    }
    items
}

fn parse_numstat(text: &str) -> Vec<(String, Stat)> {
    text.split('\0')
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let mut parts = entry.splitn(3, '\t');
            let additions = parts.next()?;
            let deletions = parts.next()?;
            let file = parts.next()?;
            Some((
                file.to_owned(),
                Stat {
                    additions: count(additions),
                    deletions: count(deletions),
                },
            ))
        })
        .collect()
}

fn count(value: &str) -> u64 {
    if value == "-" {
        return 0;
    }
    value.parse::<u64>().unwrap_or(0)
}

fn is_object_id(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn empty_patch(file: &str) -> String {
    if file.is_empty() {
        return String::new();
    }
    format!("diff --git a/{file} b/{file}\n")
}

/// Split `git diff` output on `diff --git` boundaries, dropping the partial
/// trailing chunk when the output was capped.
fn split_git_patch(patch: &Patch) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let bytes = patch.text.as_bytes();
    let mut index = 0;
    while index + 11 <= bytes.len() {
        if &bytes[index..index + 11] == b"diff --git " && (index == 0 || bytes[index - 1] == b'\n')
        {
            starts.push(index);
            index += 11;
        } else {
            index += 1;
        }
    }
    for (position, start) in starts.iter().enumerate() {
        let end = starts
            .get(position + 1)
            .copied()
            .unwrap_or(patch.text.len());
        chunks.push(&patch.text[*start..end]);
    }
    if patch.truncated {
        chunks.pop();
    }
    chunks
}

fn file_from_diff_path(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value == "/dev/null" {
        return None;
    }
    let value = value.split('\t').next().unwrap_or(value);
    Some(
        value
            .strip_prefix("a/")
            .or_else(|| value.strip_prefix("b/"))
            .unwrap_or(value)
            .to_owned(),
    )
}

/// The file a `git diff` chunk touches, in the shapes git emits with
/// `core.quotepath=false`: `+++ b/<file>` (or `--- a/<file>` for deletions),
/// falling back to the `diff --git a/<f> b/<f>` header.
fn file_from_patch_chunk(chunk: &str) -> Option<String> {
    for line in chunk.lines() {
        if let Some(rest) = line.strip_prefix("+++ ")
            && let Some(file) = file_from_diff_path(rest)
        {
            return Some(file);
        }
    }
    for line in chunk.lines() {
        if let Some(rest) = line.strip_prefix("--- ")
            && let Some(file) = file_from_diff_path(rest)
        {
            return Some(file);
        }
    }
    let header = chunk.lines().next()?.strip_prefix("diff --git ")?;
    let (_, second) = header.rsplit_once(" b/")?;
    Some(second.to_owned())
}

fn chunks_by_file(patch: &Patch, list: &[Item]) -> std::collections::HashMap<String, String> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (index, chunk) in split_git_patch(patch).into_iter().enumerate() {
        let file =
            file_from_patch_chunk(chunk).or_else(|| list.get(index).map(|item| item.file.clone()));
        let Some(file) = file else { continue };
        map.entry(file).or_default().push_str(chunk);
    }
    map
}

/// One git invocation. `cap` bounds both the bytes we keep and the truncation
/// flag the patch caps read.
async fn run(cwd: &str, args: &[&str], cap: usize) -> Output {
    let running = tokio::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.longpaths=true",
            "-c",
            "core.symlinks=true",
            "-c",
            "core.quotepath=false",
            "-C",
            cwd,
        ])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(output)) = tokio::time::timeout(GIT_TIMEOUT, running).await else {
        return Output {
            ok: false,
            text: String::new(),
            truncated: false,
        };
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let truncated = text.len() > cap;
    if truncated {
        text.truncate(cap);
    }
    Output {
        ok: output.status.success(),
        text,
        truncated,
    }
}
