//! Behavior contract for the built-in development workflow and engineering
//! standards in the static system layer (`ox_engine::prompt::system_static`).
//!
//! The workflow only has teeth if the model actually receives it, so these
//! tests assert the rendered prompt — not the constants — and they pin stable
//! behavioral phrases rather than the whole prompt byte for byte.

use ox_engine::prompt::system_static;

/// Assert every `needle` appears in `haystack`, naming the first missing one.
fn assert_contains_all(haystack: &str, needles: &[&str]) {
    let missing: Vec<&str> = needles
        .iter()
        .copied()
        .filter(|needle| !haystack.contains(needle))
        .collect();
    assert!(missing.is_empty(), "prompt is missing {missing:?}");
}

/// The engineering standards must render even when no tool is registered: they
/// are unconditional harness rules, not a tool-guidance side effect.
#[test]
fn engineering_standards_render_in_the_static_layer() {
    let prompt = system_static(&[]);
    assert_contains_all(
        &prompt,
        &[
            "# Engineering standards",
            "Apply these rules while writing the design document and while implementing it.",
            // Low coupling / high cohesion, with dependencies pointing one way.
            "low-coupling and high-cohesion",
            "dependencies point one way",
            // Smallest satisfying design; no speculative surfaces.
            "smallest design that satisfies the requirement",
            "No speculative abstraction layers",
            "generic frameworks",
            // Explicit module boundaries with owned state and invariants.
            "Make module boundaries explicit",
            "Do not reach across a boundary",
            "add a cache that can drift from its source",
            // Deep copies are the memory rule: borrows, moves, Arc, mem::take.
            "Strictly limit deep copies, especially on hot paths",
            "Prefer borrows (`&str`, `&[T]`)",
            "shared immutable data (`Arc`)",
            "`mem::take`",
            "Do not clone merely to satisfy the borrow checker",
        ],
    );
}

/// The workflow's orchestration, evidence, review, landing, and stop rules all
/// reach the model.
#[test]
fn development_workflow_renders_the_approved_pipeline() {
    let prompt = system_static(&[]);
    assert_contains_all(
        &prompt,
        &[
            "# Development workflow",
            // Multi-step only: a small coherent fix stays single-step.
            "A small coherent fix remains single-step even when it touches several files.",
            "This workflow owns orchestration when a skill or an external instruction file suggests a different execution",
            "explicit user instructions still take precedence",
            // The conversation's native todo list is the required progress surface.
            "Maintain the conversation's native todo list through `todowrite`.",
            "after every completed step or task, call `todowrite` with the full updated list",
            "a prose status message or a subagent's own list does not replace it",
            // 1. analysis before edits, with file:line evidence.
            "Inspect requirements and code, cite file:line evidence, and make no edits.",
            // 2. one plan document, both sections, approval gate.
            ".ox/plans/YYYY-MM-DD-<topic>.md",
            "## 分析与方案",
            "## 实施任务",
            "Obtain user approval before implementation",
            // 3. worktree before the first edit, with worktree-local build artifacts.
            ".ox/worktrees/<branch>",
            "Keep the main checkout clean",
            "Do not use codegraph in a worktree",
            "`CARGO_TARGET_DIR=<worktree>/target`",
            "target directory must live inside the worktree",
            "removing the worktree removes its artifacts",
            // 4. serial, fresh subagent per todo, per-task commit and reviews.
            "Execute todo items serially, one fresh subagent per item.",
            "commits only that task's files in the worktree",
            "spec-compliance and code-quality reviews",
            // 5. whole-diff review plus the domain expert.
            "general code reviewer plus the matching domain expert",
            "`rust-expert` for Rust",
            "fall back to the general review",
            // 6. green suite, rebase, squash, cleanup, and the stop line.
            "run the full suite in the worktree",
            "rebase onto the default branch",
            "squash-merge as one conventional commit",
            "rerun the suite on the merged result",
            "remove the worktree",
            "`git branch -D`",
            "never stash, overwrite, or discard them",
            "do not build, deploy, sync binaries, or restart services",
            // Subagents stay inside their task.
            "Subagents execute only their assigned task.",
        ],
    );
}

/// Committing is scoped, not banned: the old unconditional rule must be gone
/// so the workflow's per-task commits are not contradicted.
#[test]
fn commit_rule_is_scoped_to_the_workflow() {
    let prompt = system_static(&[]);
    assert!(
        prompt.contains(
            "- Outside the built-in development workflow, do not commit unless the user asks. Within an active worktree workflow, per-task commits and the final squash commit are expected."
        ),
        "scoped commit rule is missing"
    );
    assert!(
        !prompt.contains("Do not commit unless the user asks you to."),
        "the old unconditional commit rule still renders"
    );
}

/// Tool guidance still renders into its slot and the placeholder itself is
/// consumed.
#[test]
fn tool_guidance_still_renders_and_the_placeholder_is_gone() {
    let prompt = system_static(&[("shell", None)]);
    assert_contains_all(
        &prompt,
        &[
            "- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.",
            "- Do not chain shell commands with separators",
        ],
    );
    assert!(
        !prompt.contains("${OPENCODE_TOOL_GUIDANCE}"),
        "the tool-guidance placeholder was not substituted"
    );
    // The appended sections come after the harness template, so the workflow
    // cannot be dropped by a template that swallows trailing content.
    let harness = prompt.find("# Verification and claims").unwrap();
    let standards = prompt.find("# Engineering standards").unwrap();
    let workflow = prompt.find("# Development workflow").unwrap();
    assert!(harness < standards, "standards must follow the harness");
    assert!(standards < workflow, "workflow must follow the standards");
}
