//! Project identity resolution and persistence.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ox_db::Db;
use sha1::Digest;

/// The project identity and the two directory views used by OpenCode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub id: String,
    pub directory: PathBuf,
    pub canonical: PathBuf,
    pub vcs: Option<String>,
}

/// Resolve a directory to a stable project and persist the corresponding row.
pub async fn resolve_project(db: &Db, input: &Path) -> Result<ResolvedProject> {
    let directory = canonicalize(input).await?;
    resolve_project_canonical(db, &directory).await
}

/// Resolve an already canonical directory without canonicalizing it again.
pub async fn resolve_project_canonical(db: &Db, directory: &Path) -> Result<ResolvedProject> {
    let native = match discover_git(directory).await? {
        Some(project) => Some(project),
        None => discover_hg(directory).await?,
    };
    let project = match native {
        Some(project) => project,
        None => ResolvedProject {
            id: digest_id(&format!("directory:{}", directory.display())),
            canonical: directory.to_path_buf(),
            directory: directory.to_path_buf(),
            vcs: None,
        },
    };
    upsert_project(db, &project).await?;
    Ok(project)
}

async fn discover_git(directory: &Path) -> Result<Option<ResolvedProject>> {
    let Some(worktree) = git_output(directory, &["rev-parse", "--show-toplevel"]).await? else {
        return Ok(None);
    };
    let worktree = canonicalize(Path::new(&worktree)).await?;
    let common_directory = git_common_directory(&worktree).await?;
    let remote = git_output(&worktree, &["config", "--get", "remote.origin.url"]).await?;
    let root = git_output(&worktree, &["rev-list", "--max-parents=0", "HEAD"])
        .await?
        .and_then(|roots| {
            roots
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        });
    let cached = cached_project_id(&common_directory).await;
    let id = remote
        .as_deref()
        .and_then(normalize_remote)
        .map(|remote| digest_id(&format!("git-remote:{remote}")))
        .or(cached)
        .or(root);
    let Some(id) = id else {
        return Ok(Some(ResolvedProject {
            id: "global".to_owned(),
            canonical: worktree.clone(),
            directory: worktree,
            vcs: Some("git".to_owned()),
        }));
    };
    let canonical = git_main_worktree(&worktree)
        .await?
        .unwrap_or_else(|| worktree.clone());
    Ok(Some(ResolvedProject {
        id,
        directory: worktree,
        canonical,
        vcs: Some("git".to_owned()),
    }))
}

async fn discover_hg(directory: &Path) -> Result<Option<ResolvedProject>> {
    let Some(worktree) = hg_output(directory, &["root"]).await? else {
        return Ok(None);
    };
    let worktree = canonicalize(Path::new(&worktree)).await?;
    let cached = cached_project_id(&worktree.join(".hg")).await;
    let root = hg_output(&worktree, &["log", "-r", "roots(all())", "-T", "{node}\\n"])
        .await?
        .and_then(|roots| first_hg_root(&roots));
    Ok(Some(ResolvedProject {
        id: cached.or(root).unwrap_or_else(|| "global".to_owned()),
        directory: worktree.clone(),
        canonical: worktree,
        vcs: Some("hg".to_owned()),
    }))
}

async fn canonicalize(input: &Path) -> Result<PathBuf> {
    let input = input.to_owned();
    Ok(tokio::task::spawn_blocking(move || std::fs::canonicalize(input)).await??)
}

async fn git_common_directory(worktree: &Path) -> Result<PathBuf> {
    let Some(common) = git_output(worktree, &["rev-parse", "--git-common-dir"]).await? else {
        return Ok(worktree.join(".git"));
    };
    let common = PathBuf::from(common);
    if common.is_absolute() {
        Ok(common)
    } else {
        Ok(worktree.join(common))
    }
}

async fn cached_project_id(common_directory: &Path) -> Option<String> {
    tokio::fs::read_to_string(common_directory.join("opencode"))
        .await
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

async fn git_main_worktree(directory: &Path) -> Result<Option<PathBuf>> {
    let Some(output) = git_output(directory, &["worktree", "list", "--porcelain"]).await? else {
        return Ok(None);
    };
    Ok(output
        .lines()
        .find_map(|line| line.strip_prefix("worktree ").map(PathBuf::from)))
}

async fn git_output(directory: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(%error, "git command unavailable");
            return Ok(None);
        }
    };
    if !output.status.success() {
        return Ok(None);
    }
    let output = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!output.is_empty()).then_some(output))
}

async fn hg_output(directory: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = match tokio::process::Command::new("hg")
        .arg("--cwd")
        .arg(directory)
        .args(args)
        .env("HGPLAIN", "1")
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let output = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok((!output.is_empty()).then_some(output))
}

fn first_hg_root(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .min()
        .map(str::to_owned)
}

fn digest_id(value: &str) -> String {
    sha1::Sha1::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize_remote(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() || value.starts_with("file:") {
        return None;
    }
    if let Ok(url) = url::Url::parse(value) {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path().trim_matches('/').trim_end_matches(".git");
        return (!path.is_empty()).then(|| format!("{host}/{path}"));
    }
    let (host, path) = value.split_once(':')?;
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let path = path.trim_matches('/').trim_end_matches(".git");
    (!host.is_empty() && !path.is_empty()).then(|| format!("{}/{path}", host.to_ascii_lowercase()))
}

/// Record project activity without changing the project's identity metadata.
pub async fn activate_project(db: &Db, project_id: &str, timestamp: i64) -> Result<()> {
    let conn = db.acquire().await?;
    conn.execute(
        "UPDATE project SET time_active = ?2, time_updated = time_updated
         WHERE id = ?1 AND time_active <= ?2 - 60000",
        (project_id, timestamp),
    )
    .await?;
    Ok(())
}

async fn upsert_project(db: &Db, project: &ResolvedProject) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    let existing = {
        let conn = db.acquire().await?;
        let mut statement = conn
            .prepare("SELECT worktree FROM project WHERE id = ?1")
            .await?;
        ox_db::optional(&mut statement, [project.id.as_str()])
            .await?
            .map(|row| row.get::<String>(0))
            .transpose()?
    };
    let worktree = project.canonical.to_string_lossy();
    let replace = match existing.as_deref() {
        Some(existing) if existing != worktree => {
            !tokio::fs::try_exists(existing).await.unwrap_or(true)
        }
        _ => false,
    };
    let tx = db.begin().await?;
    let current = {
        let mut statement = tx
            .prepare("SELECT worktree FROM project WHERE id = ?1")
            .await?;
        ox_db::optional(&mut statement, [project.id.as_str()])
            .await?
            .map(|row| row.get::<String>(0))
            .transpose()?
    };
    match current.as_deref() {
        None => {
            tx.execute(
                "INSERT INTO project
                 (id, worktree, vcs, time_created, time_updated, time_active, sandboxes)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?4, '[]')",
                (
                    project.id.as_str(),
                    worktree.as_ref(),
                    project.vcs.as_deref(),
                    now,
                ),
            )
            .await?;
        }
        Some(current) if existing.as_deref() == Some(current) => {
            if replace {
                tx.execute(
                    "UPDATE project SET worktree = ?2, vcs = ?3, time_archived = NULL
                     WHERE id = ?1",
                    (
                        project.id.as_str(),
                        worktree.as_ref(),
                        project.vcs.as_deref(),
                    ),
                )
                .await?;
            } else {
                tx.execute(
                    "UPDATE project SET vcs = ?2, time_archived = NULL WHERE id = ?1",
                    (project.id.as_str(), project.vcs.as_deref()),
                )
                .await?;
            }
        }
        Some(_) => {}
    }
    tx.commit().await
}
