//! Skills: user-authored instruction files the model can load on demand.
//!
//! The discovery contract mirrors upstream opencode
//! (`packages/core/src/config/plugin/skill-scan.ts`): every `skill`/`skills`
//! directory in the configured config directories is scanned, a directory with
//! a `SKILL.md` is a skill whose id is the directory name, and a top-level
//! `*.md` file is a skill whose id is the file stem. Frontmatter carries the
//! display `name` and `description`; everything after it is the body the
//! `skill` tool loads into the conversation.

use std::path::{Path, PathBuf};

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
/// project-local skill dirs, then the global config ones. Each is scanned
/// shallowly (a skill directory does not contain skills).
fn roots(directory: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        directory.join("skill"),
        directory.join("skills"),
        directory.join(".opencode").join("skill"),
        directory.join(".opencode").join("skills"),
    ];
    if let Some(home) = dirs_home() {
        roots.push(home.join(".config").join("opencode").join("skill"));
        roots.push(home.join(".config").join("opencode").join("skills"));
        roots.push(home.join(".opencode").join("skill"));
        roots.push(home.join(".opencode").join("skills"));
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
        let Ok(entries) = std::fs::read_dir(&root) else {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
