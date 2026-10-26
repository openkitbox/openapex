//! Integration tests for skill discovery, frontmatter parsing and the
//! model-facing envelope (`ox_tools::skills`).
//!
//! Everything runs against a tempdir project; assertions select skills by
//! unique `oxt-` ids because the real user's `~/.ox/skills` is always part of
//! the discovery roots (a machine-independent test must not enumerate them).

use std::path::{Path, PathBuf};

use ox_tools::skills;

/// Write `text` at `path`, creating parent directories.
fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A skill directory under `project/.ox/skills/<id>` with the given file text.
fn write_skill(project: &Path, id: &str, text: &str) {
    write(project.join(".ox/skills").join(id).join("SKILL.md"), text);
}

fn ids(skills: &[skills::Skill]) -> Vec<&str> {
    skills.iter().map(|skill| skill.id.as_str()).collect()
}

#[test]
fn only_ox_roots_are_discovered() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write_skill(
        project,
        "oxt-native",
        "---\nname: Native\ndescription: d\n---\nbody\n",
    );
    write(
        project.join(".claude/skills/oxt-claude/SKILL.md"),
        "---\nname: Claude\ndescription: d\n---\nbody\n",
    );
    write(
        project.join(".agents/skills/oxt-agents/SKILL.md"),
        "---\nname: Agents\ndescription: d\n---\nbody\n",
    );

    let discovered = skills::discover(project);
    let ids = ids(&discovered);
    assert!(
        ids.contains(&"oxt-native"),
        "project .ox skill missing: {ids:?}"
    );
    // Ecosystem roots are no longer read (only `.ox` is).
    assert!(
        !ids.contains(&"oxt-claude"),
        ".claude skills leaked in: {ids:?}"
    );
    assert!(
        !ids.contains(&"oxt-agents"),
        ".agents skills leaked in: {ids:?}"
    );
}

#[test]
fn folded_description_block_is_unfolded() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write_skill(
        project,
        "oxt-folded",
        "---\nname: Folded\ndescription: >-\n  Use when the first line\n  continues on the second.\n\n  And a new paragraph.\n---\nBody text.\n",
    );

    let skill = skills::discover(project)
        .into_iter()
        .find(|skill| skill.id == "oxt-folded")
        .expect("folded skill discovered");
    assert_eq!(
        skill.description.as_deref(),
        Some("Use when the first line continues on the second.\nAnd a new paragraph."),
        "the `>-` header must not be read as the description"
    );
    assert_eq!(skill.content, "Body text.\n");
}

#[test]
fn literal_description_block_keeps_line_breaks() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write_skill(
        project,
        "oxt-literal",
        "---\nname: Literal\ndescription: |\n  first line\n  second line\n---\nBody.\n",
    );

    let skill = skills::discover(project)
        .into_iter()
        .find(|skill| skill.id == "oxt-literal")
        .expect("literal skill discovered");
    // Clip chomping: one trailing newline.
    assert_eq!(
        skill.description.as_deref(),
        Some("first line\nsecond line\n")
    );
}

#[test]
fn block_scalar_content_is_not_read_as_frontmatter_keys() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write_skill(
        project,
        "oxt-shadow",
        "---\ndescription: >-\n  description line\n  name: not-the-name\nname: Real Name\n---\nBody.\n",
    );

    let skill = skills::discover(project)
        .into_iter()
        .find(|skill| skill.id == "oxt-shadow")
        .expect("skill discovered");
    assert_eq!(skill.name, "Real Name");
    assert_eq!(
        skill.description.as_deref(),
        Some("description line name: not-the-name")
    );
}

#[test]
fn undescribed_skill_is_unlisted_but_loadable() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write_skill(
        project,
        "oxt-undescribed",
        "---\nname: Undescribed\n---\nBody.\n",
    );

    let listed: Vec<String> = skills::list(project)
        .into_iter()
        .map(|meta| meta.id)
        .collect();
    assert!(
        !listed.contains(&"oxt-undescribed".to_owned()),
        "a skill without a description must stay out of the catalog: {listed:?}"
    );
    assert!(
        skills::prepare(project, "oxt-undescribed", project).is_some(),
        "an explicit id must still load it"
    );
}

#[test]
fn prepare_renders_envelope_with_companion_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path();
    write_skill(
        project,
        "oxt-envelope",
        "---\nname: Envelope Skill\ndescription: d\n---\n\n# Heading\n\nBody text.\n",
    );
    write(project.join(".ox/skills/oxt-envelope/notes.md"), "notes\n");
    write(
        project.join(".ox/skills/oxt-envelope/scripts/run.sh"),
        "#!/bin/sh\n",
    );
    write(
        project.join(".ox/skills/oxt-envelope/node_modules/dep/index.js"),
        "\n",
    );

    let prepared = skills::prepare(project, "oxt-envelope", project).expect("skill prepared");
    assert_eq!(prepared.id, "oxt-envelope");
    assert_eq!(prepared.name, "Envelope Skill");
    assert_eq!(prepared.directory, project.join(".ox/skills/oxt-envelope"));

    let text = &prepared.text;
    assert!(
        text.starts_with("<skill_content name=\"Envelope Skill\">"),
        "{text}"
    );
    assert!(text.contains("# Heading"), "body missing: {text}");
    assert!(text.contains("Base directory for this skill: "), "{text}");
    assert!(text.contains(&format!(
        "<file>{}</file>",
        prepared.directory.join("notes.md").display()
    )));
    assert!(text.contains(&format!(
        "<file>{}</file>",
        prepared.directory.join("scripts/run.sh").display()
    )));
    assert!(
        !text.contains("SKILL.md</file>"),
        "the skill file itself is not a companion: {text}"
    );
    assert!(
        !text.contains("node_modules"),
        "ignored directories are pruned: {text}"
    );
    assert!(text.trim_end().ends_with("</skill_content>"), "{text}");

    assert!(skills::prepare(project, "oxt-missing", project).is_none());
}
