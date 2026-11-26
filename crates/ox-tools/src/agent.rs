//! The built-in agent records, including the strict Rust code-review agent.
//!
//! Each record decides what a session created with its id gets: the static
//! system prompt layer (replaced when [`Agent::system`] is set) and the visible
//! toolset ([`Agent::tools`], applied over the registered tools).
//!
//! Invariants:
//! - Tool registries own whether a tool name exists; this table only names which
//!   of the registered tools an agent sees. Every name listed here must exist in
//!   the fully wired registry (the `ctx_*` tools join it at wiring time), and
//!   [`Tools::Only`] is the agent's complete visible toolset, not an addition
//!   to it. Nothing checks these names when the table is built, so a typo in one
//!   of them silently drops that tool, while a call to a name outside the set
//!   fails as an unavailable tool.
//! - The first entry is the default agent ([`default_agent`]).
//! - `rust-expert` is the strict Rust code-review agent: a read-only subagent
//!   using the review toolset with a Rust-specific system layer.
//! - This table is the only authority on which agent ids exist; no other layer
//!   keeps a second list.
//! - Prompt bodies live in `ox-engine::prompt`; this table only selects a layer
//!   through [`SystemLayer`], so a shared rule has a single text owner.
//! - This table is also the only authority on an agent's static permission
//!   rules. [`Agent::permissions`] is the exact slice `agent.list` publishes
//!   and the engine evaluates; no second copy lives in the server. An id
//!   outside the table resolves to [`permissions`]'s deny-all rules, so an
//!   unknown agent can never gain a tool by being misspelled.

use std::sync::LazyLock;

use ox_schema::permission::{Effect, Rule};

/// Whether the user drives the agent or `subagent` spawns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Primary,
    Subagent,
}

/// Which static system layer a built-in agent receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemLayer {
    /// Harness, tool guidance, engineering standards, and mode workflow.
    Default,
    /// The read-only file-search specialist's self-contained layer.
    Explore,
    /// The read-only reviewer's contract and engineering lens.
    Review,
    /// The strict Rust code-review lens, built on the reviewer's contract.
    RustExpert,
}

/// Which registered tools an agent sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tools {
    /// Every registered tool.
    All,
    /// Every registered tool except these names.
    Without(&'static [&'static str]),
    /// Exactly these names: the agent's complete visible toolset.
    Only(&'static [&'static str]),
}

/// One built-in agent.
#[derive(Debug)]
pub struct Agent {
    pub id: &'static str,
    pub mode: Mode,
    /// Selects the static system layer; the text lives in `ox-engine::prompt`.
    pub system: SystemLayer,
    pub tools: Tools,
    /// Static upstream rule set, in evaluation order. Shared by the wire
    /// surface and the engine; never copied into a second table.
    pub permissions: &'static [Rule],
}

/// The agent a session uses when none is named.
pub const DEFAULT_AGENT: &str = APEX_ID;
/// The agent a `subagent` child uses when the call names none.
pub(crate) const SUBAGENT_AGENT: &str = "general";
/// The read-only search agent (`subagent` with `agent: "explore"`), also the
/// engine's fail-closed fallback when a session's agent cannot be read. Named
/// after upstream opencode's read-only `explore` agent.
pub(crate) const READ_ONLY_AGENT: &str = "explore";
/// The primary agent a user session runs (formerly published as `build`): the
/// whole registry, and therefore both direct implementation and dispatching.
pub const APEX_ID: &str = "apex";
/// The review agent (`subagent` with `agent: "review"`): it judges work it did
/// not do and reports a verdict. See [`REVIEW_TOOLS`] for its toolset and the
/// named gap in it.
pub(crate) const REVIEW_AGENT: &str = "review";
/// The strict Rust code-review agent (`subagent` with `agent: "rust-expert"`):
/// it applies a Rust-specific review lens with the same read-only toolset.
pub(crate) const RUST_EXPERT_AGENT: &str = "rust-expert";

/// Tools a subagent session must not have. `question` opens a form only the
/// user can answer; no UI watches a child session and a foreground tool call
/// has no timeout, so the child would wait forever — and the parent's
/// `subagent` call with it. A fail-closed exclusion, not a permission model.
pub(crate) const CHILD_EXCLUDED_TOOLS: &[&str] = &["question"];

/// The toolset of a read-only subagent (`subagent` with `agent: "explore"`).
///
/// Rule: a built-in tool belongs here only if it cannot change the workspace
/// or start other work. Reading files and the web, loading skills, searching
/// the tool catalog, and the `ctx_*` context tools (their writes stay in ox's
/// own database; `ctx_memory` is project-wide, the rest per session) qualify. Excluded: `shell`,
/// `write`, `edit`, `patch` (workspace writes), `todowrite` (parent-facing
/// plan state), `subagent` (a read-only child must not spawn a full one),
/// `question` (no UI watches a child), and every MCP tool, whose side effects
/// cannot be judged statically. Enforced by registry membership, so a call to
/// an excluded tool fails as an unknown tool.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "capability_search",
    "ctx_expand",
    "ctx_memory",
    "ctx_note",
    "ctx_reduce",
    "ctx_search",
    "glob",
    "grep",
    "read",
    "skill",
    "webfetch",
];

/// The complete toolset an automation-run child session may use. `Tools::Only`
/// is an allow-list, not an addition: T8 wiring uses this to derive the
/// unattended tool registry, so a script cannot reach the whole built-in set.
pub const AUTOMATION_TOOLS: Tools = Tools::Only(&[
    "automation_create",
    "automation_list",
    "automation_pause",
    "automation_resume",
    "automation_delete",
]);

/// The toolset of the `review` and `rust-expert` agents.
///
/// Rule: the read paths ([`READ_ONLY_TOOLS`] without the three `ctx_*` writers)
/// plus `shell`. A reviewer must read a committed diff (`git show`, `git log`,
/// `git diff`) and re-run a suite to check a claim instead of trusting it, and
/// that is all `shell` is for by instruction.
///
/// Keeping `shell` is a **named gap, not a guarantee**. `cargo test` compiles
/// and executes code from the tree under review — build scripts, test bodies,
/// procedural macros — so no command allowlist is a boundary, and a real
/// read-only boundary needs a sandbox
/// (`.ox/plans/2026-10-24-codex-sandboxing-port-analysis.md`). What this set
/// does buy is a read constraint by instruction plus the absence of the
/// structured writers: `write`, `edit`, and `patch` are not here, and neither
/// are `ctx_memory`, `ctx_note`, `ctx_reduce` (the review agent must not write
/// memories or notes), `todowrite` (parent-facing plan state), `subagent` (a
/// reviewer must not dispatch the work it judges), `question` (no UI watches a
/// child), or any MCP tool, whose side effects cannot be judged statically.
///
/// One side effect is left on purpose: `ctx_search` records `last_used_at` by
/// calling `record_use`, the tool entry being
/// `crates/ox-engine/src/mctx/search.rs` and the row UPDATE living in
/// `crates/ox-engine/src/mctx/memory/memories.rs`, whose `last_used_at` field is
/// documented as "a selection tiebreak only: recording a use is neither a write
/// nor a verification". It cannot change fact text, so the memory read path
/// stays.
///
/// Enforcement is registry membership: `ToolRegistry::only`
/// (`crates/ox-tools/src/contract.rs`) filters by name, and calling a name
/// outside this set fails as an unknown tool.
pub(crate) const REVIEW_TOOLS: &[&str] = &[
    "capability_search",
    "ctx_expand",
    "ctx_search",
    "glob",
    "grep",
    "read",
    "shell",
    "skill",
    "webfetch",
];

/// Upstream's `Agent.Info.default` rules (`schema/src/agent.ts:38-52`).
const BASE_RULES: &[(&str, &str, Effect)] = &[
    ("*", "*", Effect::Allow),
    ("read", "*.env", Effect::Ask),
    ("read", "*.env.*", Effect::Ask),
    ("read", "*.env.example", Effect::Allow),
];

/// Build a rule set once; strings are owned because `Rule` is the wire type
/// shared with the engine, but this runs only when the static table is first
/// touched, never per request or per tool call.
fn rules(entries: &[(&str, &str, Effect)]) -> Vec<Rule> {
    entries
        .iter()
        .map(|&(action, resource, effect)| Rule {
            action: action.into(),
            resource: resource.into(),
            effect,
        })
        .collect()
}

fn with_rules(base: &[(&str, &str, Effect)], extra: &[(&str, &str, Effect)]) -> Vec<Rule> {
    let mut result = rules(base);
    result.extend(rules(extra));
    result
}

/// The primary agent: upstream's default rules plus `question` (`plugin/agent.ts:56-61`).
static APEX_RULES: LazyLock<Vec<Rule>> =
    LazyLock::new(|| with_rules(BASE_RULES, &[("question", "*", Effect::Allow)]));

/// The general subagent: upstream's default rules minus forms and spawning
/// children (`plugin/agent.ts:63-72`).
static GENERAL_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    with_rules(
        BASE_RULES,
        &[
            ("question", "*", Effect::Deny),
            ("subagent", "*", Effect::Deny),
        ],
    )
});

/// Read-only agents: upstream's explore rules (`plugin/agent.ts:74-93`), with
/// `skill` added for ox's own read-only skill loader. `review` and
/// `rust-expert` use the same set: their toolsets are subsets of these
/// capabilities plus `shell`, which upstream also allows for explore.
static READ_ONLY_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    rules(&[
        ("*", "*", Effect::Deny),
        ("shell", "*", Effect::Allow),
        ("grep", "*", Effect::Allow),
        ("glob", "*", Effect::Allow),
        ("webfetch", "*", Effect::Allow),
        ("websearch", "*", Effect::Allow),
        ("read", "*", Effect::Allow),
        ("read", "*.env", Effect::Ask),
        ("read", "*.env.*", Effect::Ask),
        ("read", "*.env.example", Effect::Allow),
        ("skill", "*", Effect::Allow),
        ("subagent", "*", Effect::Deny),
        ("external_directory", "*", Effect::Allow),
    ])
});

/// No rule may ever match for an agent id outside the table.
static UNKNOWN_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| rules(&[("*", "*", Effect::Deny)]));

/// The built-in agents in display order. The first entry is the default agent.
static AGENTS: LazyLock<[Agent; 5]> = LazyLock::new(|| {
    [
        Agent {
            id: APEX_ID,
            mode: Mode::Primary,
            system: SystemLayer::Default,
            tools: Tools::All,
            permissions: &APEX_RULES,
        },
        Agent {
            id: SUBAGENT_AGENT,
            mode: Mode::Subagent,
            system: SystemLayer::Default,
            tools: Tools::Without(CHILD_EXCLUDED_TOOLS),
            permissions: &GENERAL_RULES,
        },
        Agent {
            id: READ_ONLY_AGENT,
            mode: Mode::Subagent,
            system: SystemLayer::Explore,
            tools: Tools::Only(READ_ONLY_TOOLS),
            permissions: &READ_ONLY_RULES,
        },
        Agent {
            id: REVIEW_AGENT,
            mode: Mode::Subagent,
            system: SystemLayer::Review,
            tools: Tools::Only(REVIEW_TOOLS),
            permissions: &READ_ONLY_RULES,
        },
        Agent {
            id: RUST_EXPERT_AGENT,
            mode: Mode::Subagent,
            system: SystemLayer::RustExpert,
            tools: Tools::Only(REVIEW_TOOLS),
            permissions: &READ_ONLY_RULES,
        },
    ]
});

/// Every built-in agent, in display order.
#[must_use]
pub fn all() -> &'static [Agent] {
    &AGENTS[..]
}

/// The built-in agent with this id, if any.
#[must_use]
pub fn get(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|agent| agent.id == id)
}

/// The static rules for an agent id, or a deny-all rule set for an unknown id.
/// This is the single source consumed by both `agent.list` and the engine.
#[must_use]
pub fn permissions(id: &str) -> &'static [Rule] {
    get(id).map_or(&UNKNOWN_RULES[..], |agent| agent.permissions)
}

/// The agent a session uses when none is named (`apex`).
#[must_use]
pub fn default_agent() -> &'static Agent {
    // The table's first entry is the default agent.
    &AGENTS[0]
}

/// The agent a `subagent` child uses by default (`general`).
#[must_use]
pub fn subagent_default() -> &'static Agent {
    builtin(SUBAGENT_AGENT)
}

/// The read-only subagent (`explore`).
#[must_use]
pub fn read_only() -> &'static Agent {
    builtin(READ_ONLY_AGENT)
}

/// Look up an id the table is known to contain.
fn builtin(id: &str) -> &'static Agent {
    get(id).expect("the built-in agent table always contains its named ids")
}
