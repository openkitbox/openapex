//! Skills: user-authored instruction files the model can load on demand.
//!
//! The discovery contract mirrors upstream opencode
//! (`packages/core/src/config/plugin/skill-scan.ts`): a directory with a
//! `SKILL.md` is a skill whose id is the directory name, and a top-level
//! `*.md` file is a skill whose id is the file stem. Frontmatter carries the
//! display `name` and `description`; everything after it is the body the
//! `skill` tool loads into the conversation.
//!
//! Deliberate deviation from upstream: the roots are ox-native `.ox` paths
//! (project `.ox/skill(s)` + global `~/.ox/skill(s)`) instead of upstream's
//! bare `skill(s)` dirs and opencode-branded config paths, so ox owns its
//! skills namespace and never reads another tool's config directory.
//!
//! The model-facing envelope has exactly one formatter, `render_content`
//! (upstream `Skill.toModelOutput`), reached through two entry points that
//! differ only in who discovers: `prepare` discovers and then renders one id
//! (what the `skill` tool uses), and `prepare_from` renders one id over an
//! already-discovered slice (what the prompt handler uses, so one scan answers
//! every requested id). `select_files` is their pure companion-file policy, so
//! neither entry point can format differently.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::Serialize;

/// One discovered skill (upstream `Skill.Info`).
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: PathBuf,
    /// The markdown body after frontmatter — what the `skill` tool returns.
    pub content: String,
}

/// The directories skills are discovered under, in precedence order: the
/// project-local `.ox` skill dirs, then the global `~/.ox` ones. Each is
/// scanned shallowly (a skill directory does not contain skills).
fn roots(directory: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        directory.join(".ox").join("skill"),
        directory.join(".ox").join("skills"),
    ];
    if let Some(home) = dirs_home() {
        roots.push(home.join(".ox").join("skill"));
        roots.push(home.join(".ox").join("skills"));
    }
    roots
}

/// The user's home directory (`$HOME`, then `$USERPROFILE` on Windows).
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Discover every skill visible from `directory`. Unreadable roots and files
/// are skipped, never fatal — a broken skill dir must not break the toolset.
pub fn discover(directory: &Path) -> Vec<Skill> {
    discover_in(&roots(directory))
}

/// Discovery over an explicit root set: tests scope themselves to a tempdir
/// so the host's global skill roots never leak in.
pub(crate) fn discover_in(roots: &[PathBuf]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let skill = if path.is_dir() {
                skill_from_directory(&path)
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                skill_from_file(&path)
            } else {
                None
            };
            // First root wins on id collision (ordered discovery, upstream's
            // `Source.equals` dedup).
            if let Some(skill) = skill
                && seen.insert(skill.id.clone())
            {
                skills.push(skill);
            }
        }
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    skills
}

/// A directory skill: `<dir>/SKILL.md`, id = directory name.
fn skill_from_directory(dir: &Path) -> Option<Skill> {
    let file = dir.join("SKILL.md");
    let id = dir.file_name()?.to_str()?.to_owned();
    load(&file, id)
}

/// A file skill: `<root>/<id>.md`, id = file stem.
fn skill_from_file(file: &Path) -> Option<Skill> {
    let id = file.file_stem()?.to_str()?.to_owned();
    load(file, id)
}

/// Read the skill file and parse its frontmatter.
fn load(file: &Path, id: String) -> Option<Skill> {
    let raw = std::fs::read_to_string(file).ok()?;
    let (frontmatter, content) = parse_frontmatter(&raw);
    let name = frontmatter
        .as_ref()
        .and_then(|front| value(front, "name"))
        .unwrap_or_else(|| id.clone());
    let description = frontmatter
        .as_ref()
        .and_then(|front| value(front, "description"));
    Some(Skill {
        id,
        name,
        description,
        path: file.to_path_buf(),
        content,
    })
}

/// Split `---\nname: …\n---\n` frontmatter from the markdown body. Returns
/// `(None, whole file)` when there is none — a plain markdown file is a valid
/// skill.
fn parse_frontmatter(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.strip_prefix("---").unwrap_or(raw);
    if trimmed.len() == raw.len() {
        return (None, raw.to_owned());
    }
    let trimmed = trimmed.trim_start_matches(['\r', '\n']);
    match trimmed.find("\n---") {
        Some(end) => {
            let front = trimmed[..end].to_owned();
            let body = trimmed[end + 4..]
                .trim_start_matches(['\r', '\n'])
                .to_owned();
            (Some(front), body)
        }
        None => (None, raw.to_owned()),
    }
}

/// One `key: value` line of frontmatter. A deliberate subset — skills use
/// `name` and `description`, and a YAML parser dependency is not justified.
fn value(frontmatter: &str, key: &str) -> Option<String> {
    for line in frontmatter.lines() {
        let (head, tail) = line.split_once(':')?;
        if head.trim() == key {
            let text = tail.trim().trim_matches('"').trim_matches('\'');
            if !text.is_empty() {
                return Some(text.to_owned());
            }
        }
    }
    None
}

/// Render the model-facing envelope for one skill (upstream
/// `Skill.toModelOutput`). `directory` is the base directory relative paths
/// resolve against — [`prepare_from`] passes the same value it stores in
/// [`PreparedSkill::directory`], so the envelope and the struct cannot drift.
/// `files` are already selected absolute paths and are emitted in the given
/// order; the `<skill_files>` block is always present, even when empty. This is
/// the only skill format the model ever sees.
pub fn render_content(skill: &Skill, directory: &Path, files: &[PathBuf]) -> String {
    let directory = directory.display();
    let mut lines = vec![
        format!("<skill_content name=\"{}\">", skill.name),
        format!("# Skill: {}", skill.name),
        String::new(),
        skill.content.trim().to_owned(),
        String::new(),
        format!("Base directory for this skill: {directory}"),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory."
            .to_owned(),
        "Note: file list is sampled.".to_owned(),
        String::new(),
        "<skill_files>".to_owned(),
    ];
    for file in files {
        lines.push(format!("<file>{}</file>", file.display()));
    }
    lines.push("</skill_files>".to_owned());
    lines.push("</skill_content>".to_owned());
    lines.join("\n")
}

/// Whether `skill` is a directory skill (`<dir>/SKILL.md`). Upstream `prepare`
/// gates companion files on exactly this basename, so a file skill
/// (`<root>/notes.md`) never lists its siblings.
fn is_directory_skill(skill: &Skill) -> bool {
    skill.path.file_name() == Some(OsStr::new("SKILL.md"))
}

/// The pure policy half of upstream `prepare`'s file handling: no companions
/// for a file skill, drop the `SKILL.md` itself, sort ascending, keep at most
/// ten. Scanning is [`companion_files`]' job, so this stays testable without a
/// filesystem.
pub(crate) fn select_files(skill: &Skill, mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    if !is_directory_skill(skill) {
        return Vec::new();
    }
    files.retain(|file| file.file_name() != Some(OsStr::new("SKILL.md")));
    // Upstream `.toSorted()` sorts path strings in JS UTF-16 code-unit order;
    // UTF-8 byte order is equivalent for ASCII and differs only for astral vs
    // U+E000..U+FFFF code points.
    files.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    files.truncate(10);
    files
}

/// The directory relative paths in a skill resolve against (upstream
/// `path.dirname(skill.path)`): the file's parent, or `.` when the path carries
/// no directory at all (a bare `notes.md`). One derivation so the envelope's
/// `Base directory for this skill:` line and [`PreparedSkill::directory`] can
/// never disagree.
fn base_directory(skill: &Skill) -> &Path {
    match skill.path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The directory names upstream's scan skips at any depth
/// (`FSUtil.ignoredDirectories`, in upstream order).
const IGNORED_DIRECTORIES: [&str; 5] = ["node_modules", ".git", ".venv", "venv", "__pycache__"];

/// The I/O half of upstream `prepare`: walk a directory skill's directory for
/// its companions (upstream `fs.scan("**/*", {absolute: true, include: "file",
/// dot: true, ignore: FSUtil.ignoreGlobs})`) and apply [`select_files`]. A file
/// skill walks nothing.
///
/// Upstream's `ignore` list is only [`IGNORED_DIRECTORIES`], so every
/// gitignore-style filter is off: the walker's defaults would drop companions
/// named by the enclosing repo's `.gitignore` (or the user's global excludes),
/// making the model-visible list machine-dependent. An unreadable or vanished
/// directory yields an empty file list rather than an error — this module's
/// "unreadable is never fatal" contract, where upstream's `fs.scan` would
/// propagate the failure.
pub fn companion_files(skill: &Skill) -> Vec<PathBuf> {
    if !is_directory_skill(skill) {
        return Vec::new();
    }
    let directory = base_directory(skill);
    let files: Vec<PathBuf> = WalkBuilder::new(directory)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .filter_entry(|entry| {
            // Only directories are pruned, so a companion *file* that happens to
            // share one of these names survives (upstream's globs only exclude
            // the directories and their contents).
            entry.depth() == 0
                || !(entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_dir())
                    && IGNORED_DIRECTORIES
                        .iter()
                        .any(|ignored| entry.file_name() == OsStr::new(ignored)))
        })
        .build()
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.path().to_path_buf())
        .collect();
    select_files(skill, files)
}

/// One skill ready to hand to the model: its identity, the directory relative
/// paths resolve against, and the rendered `<skill_content>` envelope.
#[derive(Debug)]
pub struct PreparedSkill {
    pub id: String,
    pub name: String,
    pub directory: PathBuf,
    pub text: String,
}

/// Load `id` from an already-discovered skill set and render it.
///
/// This is the pure half of [`prepare`]: deciding which skill `id` names and
/// how it renders is separable from discovery, so a caller that has already
/// discovered — the prompt path resolves several ids per admission — pays for
/// one scan, and tests can decide without a filesystem. The only I/O left is
/// [`companion_files`], which is per-skill and unavoidable.
pub fn prepare_from(skills: &[Skill], id: &str) -> Option<PreparedSkill> {
    let skill = skills.iter().find(|skill| skill.id == id)?;
    let directory = base_directory(skill).to_path_buf();
    let text = render_content(skill, &directory, &companion_files(skill));
    Some(PreparedSkill {
        id: skill.id.clone(),
        name: skill.name.clone(),
        directory,
        text,
    })
}

/// Discover the skills under `directory` and load `id`.
///
/// This is the formatter entry point the `skill` tool uses; the prompt handler
/// pairs [`discover`] with [`prepare_from`] to answer several ids per scan.
/// Both end in [`render_content`], the only producer of the envelope, so the
/// two can never drift.
pub fn prepare(directory: &Path, id: &str) -> Option<PreparedSkill> {
    prepare_from(&discover(directory), id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_are_ox_native_project_and_global_paths() {
        // Serialize HOME redirection against every other reader in the binary:
        // cargo runs tests on parallel threads and `std::env::set_var` is
        // process-global.
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        unsafe {
            match previous {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(
            roots,
            vec![
                dir.path().join(".ox").join("skill"),
                dir.path().join(".ox").join("skills"),
                home.path().join(".ox").join("skill"),
                home.path().join(".ox").join("skills"),
            ]
        );
    }

    #[test]
    fn directory_and_file_skills_are_discovered_with_ids() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join("skills/review")).unwrap();
        std::fs::write(
            project.join("skills/review/SKILL.md"),
            "---\nname: Code Review\ndescription: Review a PR\n---\nBe thorough.\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.join("skills")).unwrap();
        std::fs::write(
            project.join("skills/notes.md"),
            "Plain body, no frontmatter.",
        )
        .unwrap();
        std::fs::write(project.join("skills/not-a-skill.txt"), "ignored").unwrap();

        let skills = discover_in(&[project.join("skill"), project.join("skills")]);
        let review = skills.iter().find(|skill| skill.id == "review").unwrap();
        assert_eq!(review.name, "Code Review");
        assert_eq!(review.description.as_deref(), Some("Review a PR"));
        assert_eq!(review.content.trim(), "Be thorough.");
        let notes = skills.iter().find(|skill| skill.id == "notes").unwrap();
        assert_eq!(notes.name, "notes");
        assert_eq!(notes.description, None);
        assert_eq!(notes.content.trim(), "Plain body, no frontmatter.");
        assert!(!skills.iter().any(|skill| skill.id == "not-a-skill"));
    }

    #[test]
    fn the_first_root_wins_on_id_collision() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join("skill/dup")).unwrap();
        std::fs::create_dir_all(project.join("skills/dup")).unwrap();
        std::fs::write(project.join("skill/dup/SKILL.md"), "first").unwrap();
        std::fs::write(project.join("skills/dup/SKILL.md"), "second").unwrap();

        let skills = discover_in(&[project.join("skill"), project.join("skills")]);
        let matches: Vec<&Skill> = skills.iter().filter(|skill| skill.id == "dup").collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].content, "first");
    }

    #[test]
    fn frontmatter_without_a_close_is_not_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken.md");
        std::fs::write(&file, "---\nname: x\nbody without a close").unwrap();
        let skill = skill_from_file(&file).unwrap();
        assert_eq!(skill.name, "broken");
        assert!(skill.content.starts_with("---"));
    }

    fn review_skill() -> Skill {
        Skill {
            id: "review".to_owned(),
            name: "Review".to_owned(),
            description: Some("Review a diff".to_owned()),
            path: PathBuf::from("/skills/review/SKILL.md"),
            content: "Look at the diff.\n".to_owned(),
        }
    }

    #[test]
    fn render_content_matches_the_upstream_envelope() {
        let files = [
            PathBuf::from("/skills/review/references/a.md"),
            PathBuf::from("/skills/review/scripts/b.sh"),
        ];
        let rendered = render_content(&review_skill(), Path::new("/skills/review"), &files);
        let expected = r#"<skill_content name="Review">
# Skill: Review

Look at the diff.

Base directory for this skill: /skills/review
Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.
Note: file list is sampled.

<skill_files>
<file>/skills/review/references/a.md</file>
<file>/skills/review/scripts/b.sh</file>
</skill_files>
</skill_content>"#;
        assert_eq!(rendered, expected);
    }

    #[test]
    fn a_bare_relative_path_reports_the_current_directory() {
        // Upstream's `path.dirname("notes.md")` is ".", and the envelope's
        // base-directory line must agree with `PreparedSkill::directory`.
        let skill = Skill {
            id: "notes".to_owned(),
            name: "Notes".to_owned(),
            description: None,
            path: PathBuf::from("notes.md"),
            content: "Plain notes.\n".to_owned(),
        };
        let prepared = prepare_from(std::slice::from_ref(&skill), "notes").unwrap();
        assert_eq!(prepared.directory, PathBuf::from("."));
        assert!(
            prepared.text.contains("Base directory for this skill: .\n"),
            "{}",
            prepared.text
        );
    }

    #[test]
    fn an_empty_file_list_still_emits_the_block() {
        let rendered = render_content(&review_skill(), Path::new("/skills/review"), &[]);
        assert!(rendered.contains("<skill_files>\n</skill_files>"));
        assert!(rendered.ends_with("</skill_content>"));
    }

    fn notes_skill() -> Skill {
        Skill {
            id: "notes".to_owned(),
            name: "Notes".to_owned(),
            description: None,
            path: PathBuf::from("/skills/notes.md"),
            content: "Plain notes.\n".to_owned(),
        }
    }

    #[test]
    fn select_files_drops_skill_md_sorts_and_caps() {
        let mut files: Vec<PathBuf> = (0..11)
            .map(|index| PathBuf::from(format!("/skills/review/f{index:02}.md")))
            .collect();
        files.push(PathBuf::from("/skills/review/SKILL.md"));
        // Shuffled on purpose: ordering must come from `select_files`, not the
        // order the scan happened to produce.
        files.reverse();

        let selected = select_files(&review_skill(), files);

        assert_eq!(selected.len(), 10);
        assert!(
            !selected
                .iter()
                .any(|file| file.file_name().is_some_and(|name| name == "SKILL.md"))
        );
        let expected: Vec<PathBuf> = (0..10)
            .map(|index| PathBuf::from(format!("/skills/review/f{index:02}.md")))
            .collect();
        assert_eq!(selected, expected);
    }

    #[test]
    fn select_files_sorts_by_path_string_not_components() {
        // M1: upstream `.toSorted()` compares path *strings*, where `-` (0x2d)
        // precedes `/` (0x2f), so `notes-1.md` comes first. Sorting `Path`s
        // compares components instead and would put `notes/x.md` first,
        // because the `notes` component sorts before the `notes-1.md` name.
        let files = vec![
            PathBuf::from("/skills/review/notes/x.md"),
            PathBuf::from("/skills/review/notes-1.md"),
        ];
        assert_eq!(
            select_files(&review_skill(), files),
            vec![
                PathBuf::from("/skills/review/notes-1.md"),
                PathBuf::from("/skills/review/notes/x.md"),
            ]
        );
    }

    #[test]
    fn a_file_skill_has_no_companion_files() {
        // Upstream `prepare` gates on `basename(path) === "SKILL.md"`, so a
        // plain `<root>/notes.md` skill lists nothing.
        let siblings = vec![
            PathBuf::from("/skills/alpha.md"),
            PathBuf::from("/skills/beta.md"),
        ];
        assert!(select_files(&notes_skill(), siblings).is_empty());
    }

    #[test]
    fn companion_files_walks_the_skill_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::create_dir_all(skill_dir.join(".git")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        std::fs::write(skill_dir.join("scripts/b.sh"), "echo").unwrap();
        std::fs::write(skill_dir.join("scripts/.hidden.sh"), "echo").unwrap();
        std::fs::write(skill_dir.join("references/a.md"), "ref").unwrap();
        std::fs::write(skill_dir.join(".git/HEAD"), "ref: x").unwrap();

        let skill = Skill {
            id: "review".to_owned(),
            name: "Review".to_owned(),
            description: None,
            path: skill_dir.join("SKILL.md"),
            content: "body".to_owned(),
        };
        let files = companion_files(&skill);

        assert!(files.iter().all(|file| file.is_absolute()));
        let relative: Vec<String> = files
            .iter()
            .map(|file| {
                file.strip_prefix(&skill_dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            relative,
            vec!["references/a.md", "scripts/.hidden.sh", "scripts/b.sh"]
        );
    }

    /// A directory skill rooted at `skill_dir` (which must contain `SKILL.md`).
    fn dir_skill(skill_dir: &Path) -> Skill {
        Skill {
            id: "review".to_owned(),
            name: "Review".to_owned(),
            description: None,
            path: skill_dir.join("SKILL.md"),
            content: "body".to_owned(),
        }
    }

    /// The listed files, relative to `skill_dir`.
    fn relative_files(skill_dir: &Path, files: &[PathBuf]) -> Vec<String> {
        files
            .iter()
            .map(|file| {
                file.strip_prefix(skill_dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn ignore_files_do_not_filter_companions() {
        // Upstream scans with `ignore: FSUtil.ignoreGlobs` only — no
        // `.gitignore`/`.ignore`/global excludes. A `.git` marker is present so
        // ripgrep semantics would apply the `.gitignore`.
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join(".git")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        std::fs::write(skill_dir.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(skill_dir.join("scripts/build.log"), "log").unwrap();

        // `dot: true` lists the dotfiles too, so `.gitignore` is a companion in
        // its own right; the point is that `*.log` did not drop `build.log`.
        assert_eq!(
            relative_files(&skill_dir, &companion_files(&dir_skill(&skill_dir))),
            vec![".gitignore", "scripts/build.log"]
        );
    }

    #[test]
    fn ignored_directories_do_not_consume_the_cap() {
        // Upstream prunes exactly these five directory names at any depth, so
        // their contents can never take one of the ten companion slots.
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("review");
        for ignored in ["node_modules", "venv", ".venv", "__pycache__", ".git"] {
            std::fs::create_dir_all(skill_dir.join(ignored)).unwrap();
        }
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        std::fs::write(skill_dir.join("node_modules/x.js"), "x").unwrap();
        std::fs::write(skill_dir.join("venv/v.py"), "v").unwrap();
        std::fs::write(skill_dir.join(".venv/v.py"), "v").unwrap();
        std::fs::write(skill_dir.join("__pycache__/p.pyc"), "p").unwrap();
        std::fs::write(skill_dir.join(".git/HEAD"), "ref: x").unwrap();
        for index in 0..10 {
            std::fs::write(skill_dir.join(format!("scripts/f{index:02}.sh")), "echo").unwrap();
        }

        let expected: Vec<String> = (0..10)
            .map(|index| format!("scripts/f{index:02}.sh"))
            .collect();
        assert_eq!(
            relative_files(&skill_dir, &companion_files(&dir_skill(&skill_dir))),
            expected
        );
    }

    #[test]
    fn prepare_is_the_single_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        let roots = [dir.path().join(".ox/skills")];
        assert!(prepare_from(&discover_in(&roots), "no-such-skill-xyz").is_none());

        std::fs::create_dir_all(dir.path().join(".ox/skills/review/scripts")).unwrap();
        std::fs::write(
            dir.path().join(".ox/skills/review/SKILL.md"),
            "---\nname: Review\n---\nLook at the diff.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".ox/skills/review/scripts/b.sh"), "echo").unwrap();
        std::fs::write(dir.path().join(".ox/skills/notes.md"), "Plain notes.").unwrap();

        let skills = discover_in(&roots);
        let prepared = prepare_from(&skills, "review").unwrap();
        assert_eq!(prepared.id, "review");
        assert_eq!(prepared.name, "Review");
        assert_eq!(prepared.directory, dir.path().join(".ox/skills/review"));
        assert!(
            prepared.text.contains(&format!(
                "Base directory for this skill: {}",
                prepared.directory.display()
            )),
            "{}",
            prepared.text
        );
        assert!(
            prepared.text.contains(&format!(
                "<file>{}</file>",
                prepared.directory.join("scripts/b.sh").display()
            )),
            "{}",
            prepared.text
        );

        let prepared = prepare_from(&skills, "notes").unwrap();
        assert_eq!(prepared.directory, dir.path().join(".ox/skills"));
        assert!(
            prepared.text.contains("<skill_files>\n</skill_files>"),
            "{}",
            prepared.text
        );
    }
}
