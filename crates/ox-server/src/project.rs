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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn test_root(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "ox-project-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn git(directory: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn create_git_repo(root: &Path) {
        std::fs::create_dir_all(root).expect("create repository");
        git(root, &["init", "-q"]);
        git(root, &["config", "user.name", "ox test"]);
        git(root, &["config", "user.email", "ox@example.test"]);
        std::fs::write(root.join("file"), "content").expect("write repository file");
        git(root, &["add", "file"]);
        git(root, &["commit", "-q", "-m", "initial"]);
    }

    #[tokio::test]
    async fn markerless_projects_have_stable_distinct_ids_and_are_persisted() {
        let db = Db::open_in_memory().await.expect("open test db");
        let root = std::env::temp_dir().join(format!("ox-project-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).expect("create a");
        std::fs::create_dir_all(root.join("b")).expect("create b");

        let first = super::resolve_project(&db, &root.join("a"))
            .await
            .expect("resolve a");
        let again = super::resolve_project(&db, &root.join("a"))
            .await
            .expect("resolve a again");
        let other = super::resolve_project(&db, &root.join("b"))
            .await
            .expect("resolve b");

        assert_eq!(first.id, again.id);
        assert_ne!(first.id, other.id);
        assert_eq!(first.directory, first.canonical);
        assert!(db.table_exists("project").await.expect("project table"));

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn normalizes_https_and_scp_remotes_like_upstream() {
        assert_eq!(
            normalize_remote("https://GitHub.com/acme/ox.git"),
            Some("github.com/acme/ox".to_owned())
        );
        assert_eq!(
            normalize_remote("git@github.com:acme/ox.git"),
            Some("github.com/acme/ox".to_owned())
        );
        assert_eq!(normalize_remote("file:///tmp/ox"), None);
    }

    #[test]
    fn chooses_the_stable_first_hg_root() {
        assert_eq!(
            first_hg_root("b-root\n\na-root\n"),
            Some("a-root".to_owned())
        );
        assert_eq!(first_hg_root("\n  \n"), None);
    }

    #[tokio::test]
    async fn uses_cached_common_directory_id_before_root_commit() {
        let db = Db::open_in_memory().await.expect("open test db");
        let root = test_root("cached");
        create_git_repo(&root);
        std::fs::write(root.join(".git/opencode"), "cached-project-id\n")
            .expect("write cached project id");

        let resolved = resolve_project(&db, &root)
            .await
            .expect("resolve repository");

        assert_eq!(resolved.id, "cached-project-id");
        std::fs::remove_dir_all(root).expect("remove test repository");
    }

    #[tokio::test]
    async fn discovers_linked_worktree_and_root_commit_identity() {
        let db = Db::open_in_memory().await.expect("open test db");
        let root = test_root("worktree");
        let linked = root.with_file_name(format!("{}-linked", root.display()));
        create_git_repo(&root);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked",
                linked.to_str().expect("utf8 path"),
            ],
        );
        let expected_root = {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["rev-list", "--max-parents=0", "HEAD"])
                .output()
                .expect("read root commit");
            String::from_utf8(output.stdout)
                .expect("root commit utf8")
                .trim()
                .to_owned()
        };

        let resolved = resolve_project(&db, &linked)
            .await
            .expect("resolve linked worktree");

        assert_eq!(resolved.id, expected_root);
        assert_eq!(
            resolved.directory,
            canonicalize(&linked).await.expect("canonical linked")
        );
        assert_eq!(
            resolved.canonical,
            canonicalize(&root).await.expect("canonical root")
        );
        std::fs::remove_dir_all(root).expect("remove test repository");
        std::fs::remove_dir_all(linked).expect("remove linked worktree");
    }

    #[tokio::test]
    async fn preserves_existing_canonical_when_it_still_exists() {
        let db = Db::open_in_memory().await.expect("open test db");
        let first = test_root("preserve-first");
        let second = test_root("preserve-second");
        create_git_repo(&first);
        let clone_status = Command::new("git")
            .args(["clone", "-q"])
            .arg(&first)
            .arg(&second)
            .status()
            .expect("clone repository");
        assert!(clone_status.success(), "git clone failed");

        let first_resolved = resolve_project(&db, &first)
            .await
            .expect("resolve first repository");
        let second_resolved = resolve_project(&db, &second)
            .await
            .expect("resolve second repository");

        assert_eq!(first_resolved.id, second_resolved.id);
        let conn = db.acquire().await.expect("acquire test db");
        let mut statement = conn
            .prepare("SELECT worktree FROM project WHERE id = ?1")
            .await
            .expect("prepare project query");
        let row = ox_db::optional(&mut statement, [first_resolved.id.as_str()])
            .await
            .expect("query project");
        let worktree = row
            .expect("persisted project")
            .get::<String>(0)
            .expect("project worktree");
        assert_eq!(
            worktree,
            canonicalize(&first)
                .await
                .expect("canonical first repository")
                .display()
                .to_string()
        );
        assert_eq!(
            second_resolved.canonical,
            canonicalize(&second)
                .await
                .expect("canonical second repository")
        );
        std::fs::remove_dir_all(first).expect("remove first repository");
        std::fs::remove_dir_all(second).expect("remove second repository");
    }

    #[tokio::test]
    async fn replacing_missing_canonical_preserves_activity_and_updated_times() {
        let db = Db::open_in_memory().await.expect("open test db");
        let first = test_root("replacement-first");
        let second = test_root("replacement-second");
        std::fs::create_dir_all(&first).expect("create first directory");
        std::fs::create_dir_all(&second).expect("create second directory");
        let project = ResolvedProject {
            id: "replacement".to_owned(),
            directory: second.clone(),
            canonical: second.clone(),
            vcs: None,
        };
        let original = ResolvedProject {
            canonical: first.clone(),
            ..project.clone()
        };
        upsert_project(&db, &original)
            .await
            .expect("insert project");
        let conn = db.acquire().await.expect("acquire test db");
        conn.execute(
            "UPDATE project SET time_updated = ?2, time_active = ?3 WHERE id = ?1",
            ("replacement", 123_i64, 456_i64),
        )
        .await
        .expect("seed timestamps");
        std::fs::remove_dir_all(&first).expect("remove original directory");

        upsert_project(&db, &project)
            .await
            .expect("replace missing canonical");

        let mut statement = conn
            .prepare("SELECT worktree, time_updated, time_active FROM project WHERE id = ?1")
            .await
            .expect("prepare project query");
        let row = ox_db::optional(&mut statement, ["replacement"])
            .await
            .expect("query project")
            .expect("persisted project");
        assert_eq!(
            row.get::<String>(0).expect("worktree"),
            second.display().to_string()
        );
        assert_eq!(row.get::<i64>(1).expect("updated time"), 123);
        assert_eq!(row.get::<i64>(2).expect("active time"), 456);
        std::fs::remove_dir_all(second).expect("remove replacement directory");
    }

    #[tokio::test]
    async fn activation_updates_only_activity_time() {
        let db = Db::open_in_memory().await.expect("open test db");
        let root = test_root("activation");
        std::fs::create_dir_all(&root).expect("create project directory");
        let resolved = resolve_project(&db, &root).await.expect("resolve project");
        {
            let conn = db.acquire().await.expect("acquire test db");
            conn.execute(
                "UPDATE project SET time_updated = ?2, time_active = ?3 WHERE id = ?1",
                (resolved.id.as_str(), 123_i64, 1_i64),
            )
            .await
            .expect("seed timestamps");
        }

        activate_project(&db, &resolved.id, 60_456)
            .await
            .expect("activate project");

        let conn = db.acquire().await.expect("acquire test db");
        let mut statement = conn
            .prepare("SELECT time_updated, time_active FROM project WHERE id = ?1")
            .await
            .expect("prepare project query");
        let row = ox_db::optional(&mut statement, [resolved.id.as_str()])
            .await
            .expect("query project")
            .expect("persisted project");
        assert_eq!(row.get::<i64>(0).expect("updated time"), 123);
        assert_eq!(row.get::<i64>(1).expect("active time"), 60_456);
        std::fs::remove_dir_all(root).expect("remove project directory");
    }

    #[tokio::test]
    async fn activation_is_throttled_by_the_persisted_activity_time() {
        let db = Db::open_in_memory().await.expect("open test db");
        let root = test_root("activation-throttle");
        std::fs::create_dir_all(&root).expect("create project directory");
        let resolved = resolve_project(&db, &root).await.expect("resolve project");
        {
            let conn = db.acquire().await.expect("acquire test db");
            conn.execute(
                "UPDATE project SET time_updated = ?2, time_active = ?3 WHERE id = ?1",
                (resolved.id.as_str(), 123_i64, 100_000_i64),
            )
            .await
            .expect("seed timestamps");
        }

        activate_project(&db, &resolved.id, 100_001)
            .await
            .expect("throttled activation");
        let conn = db.acquire().await.expect("acquire test db");
        let mut statement = conn
            .prepare("SELECT time_updated, time_active FROM project WHERE id = ?1")
            .await
            .expect("prepare project query");
        let row = ox_db::optional(&mut statement, [resolved.id.as_str()])
            .await
            .expect("query project")
            .expect("persisted project");
        assert_eq!(row.get::<i64>(0).expect("updated time"), 123);
        assert_eq!(row.get::<i64>(1).expect("active time"), 100_000);
        drop(row);
        drop(statement);
        drop(conn);

        activate_project(&db, &resolved.id, 160_000)
            .await
            .expect("activation after throttle interval");
        let conn = db.acquire().await.expect("acquire test db");
        let mut statement = conn
            .prepare("SELECT time_updated, time_active FROM project WHERE id = ?1")
            .await
            .expect("prepare project query after interval");
        let row = ox_db::optional(&mut statement, [resolved.id.as_str()])
            .await
            .expect("query project after interval")
            .expect("persisted project after interval");
        assert_eq!(row.get::<i64>(0).expect("updated time"), 123);
        assert_eq!(row.get::<i64>(1).expect("active time"), 160_000);

        std::fs::remove_dir_all(root).expect("remove project directory");
    }

    #[tokio::test]
    async fn resolution_restores_archived_projects_without_touching_updated_time() {
        let db = Db::open_in_memory().await.expect("open test db");
        let root = test_root("archived");
        std::fs::create_dir_all(&root).expect("create project directory");
        let resolved = resolve_project(&db, &root).await.expect("resolve project");
        let conn = db.acquire().await.expect("acquire test db");
        conn.execute(
            "UPDATE project SET time_created = ?2, time_updated = ?3, time_active = ?4,
             time_archived = ?5
             WHERE id = ?1",
            (resolved.id.as_str(), 789_i64, 123_i64, 1_i64, 456_i64),
        )
        .await
        .expect("archive project");

        resolve_project_canonical(&db, &resolved.directory)
            .await
            .expect("resolve archived project");

        let mut statement = conn
            .prepare(
                "SELECT time_created, time_updated, time_active, time_archived
                 FROM project WHERE id = ?1",
            )
            .await
            .expect("prepare project query");
        let row = ox_db::optional(&mut statement, [resolved.id.as_str()])
            .await
            .expect("query project")
            .expect("persisted project");
        assert_eq!(row.get::<i64>(0).expect("created time"), 789);
        assert_eq!(row.get::<i64>(1).expect("updated time"), 123);
        assert_eq!(row.get::<i64>(2).expect("active time"), 1);
        assert_eq!(row.get::<Option<i64>>(3).expect("archived time"), None);

        std::fs::remove_dir_all(root).expect("remove project directory");
    }
}
