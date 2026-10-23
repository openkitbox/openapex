//! Deterministic m[0]/m[1] head composition.
//!
//! The request head is two blocks the model sees before the live transcript:
//!
//! - **m[0]** — the frozen baseline: `<session-history>`, the decay-rendered
//!   compartment set, measured against the session's history budget.
//! - **m[1]** — the volatile half: the `<project-memory>` pinboard, then the
//!   placeholder that keeps the block non-empty.
//!
//! Everything that folds — and only folding — changes m[0]: it is a pure
//! function of the compartment rows and the budget, so a fold and nothing else
//! moves the cache anchor.
//!
//! The pinboard lives in m[1], not m[0], and that placement is a cache
//! decision: m[0] is the provider's cache anchor, so anything that changes with
//! a memory write must not be part of it. A memory write is a `Soft` bust
//! (the delta changes, the baseline does not), which keeps the cached prefix
//! intact; putting the pinboard in m[0] made every write a `Hard` rebuild for
//! every session of the project — including the dreamer's background writes.
//!
//! Faithful port of the reference `mc-module/src/memory_render.rs` composition
//! (`render_m0` / `assemble_m1` / `render_memory_block`). Pure functions over
//! already-loaded rows — the caller decides what is selected and trimmed; these
//! only assemble bytes, so the same inputs always produce the same bytes.
//!
//! The `<session-history>` block is ALWAYS present, even for empty history
//! ([`M0_EMPTY_BODY`]). That is load-bearing, not cosmetic: the provider prompt
//! cache anchors a breakpoint at this block, and an absent block would shift
//! every byte after it and bust the cache on every turn.
//!
//! Documented deviations from the reference port:
//!
//! - **No `<project-docs>` / `<user-profile>` / `<covered-system-messages>`
//!   sections.** Those carry upstream's docs injection, the user-profile
//!   memory tier and covered system-role messages; ox has no source for them
//!   yet (they land with the historian/system-message work if needed), so the
//!   m0 section list here is `<session-history>` + `<project-memory>`.
//! - **No m[1] `<memory-updates>` corrections block.** The reference freezes the
//!   pinboard inside m[0] and patches it with per-entry mutations in m[1]. ox
//!   instead renders the pinboard once, in m[1], so there is a single
//!   authoritative rendering and no stale-frozen-vs-delta disagreement to
//!   reconcile. The cost of that simplification is paid only when a memory
//!   changes: the delta's bytes shift from the pinboard onwards, one turn.
//! - **No workspace memory union** (the reference's `source_name_by_id` /
//!   `WorkspaceMembership`): ox memories are scoped to one project directory.
//! - **The token estimator is [`crate::mctx::decay_render::estimate_tokens`]**
//!   (see that module's docs).

use crate::mctx::decay_render::{
    DecayRenderCompartment, estimate_tokens, render_decayed_compartments,
};
use crate::mctx::memory::MemoryRow;

/// The m0 body when no compartment exists. The `<session-history>` tag is
/// always present (never omitted) so the provider prompt cache has a stable
/// breakpoint to anchor on — see the module docs.
pub const M0_EMPTY_BODY: &str = "<session-history></session-history>";
/// The non-empty placeholder for the m1 delta block when there is no pinboard.
/// m1 is the volatile half of the cached prefix and must never be fully empty
/// for the same cache-anchoring reason.
pub const M1_PLACEHOLDER: &str =
    "<session-history-since>(no new content since last materialization)</session-history-since>";

/// The five canonical memory categories, in render order.
const MEMORY_CATEGORY_ORDER: [&str; 5] = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
];

fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_xml_content(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render order: canonical categories first (in [`MEMORY_CATEGORY_ORDER`]),
/// then any non-taxonomy category alphabetically; ids ascending inside a
/// category.
fn memory_render_order(left: &MemoryRow, right: &MemoryRow) -> std::cmp::Ordering {
    let rank = |category: &str| {
        MEMORY_CATEGORY_ORDER
            .iter()
            .position(|known| *known == category)
    };
    match (rank(&left.category), rank(&right.category)) {
        (Some(left_rank), Some(right_rank)) => left_rank
            .cmp(&right_rank)
            .then_with(|| numeric_id(&left.id).cmp(&numeric_id(&right.id))),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left
            .category
            .cmp(&right.category)
            .then_with(|| numeric_id(&left.id).cmp(&numeric_id(&right.id)))
            // Ids are numeric today, but a non-numeric one must not make the
            // byte order depend on row order: tie-break on the string.
            .then_with(|| left.id.cmp(&right.id)),
    }
}

/// Sort key for a handle id: numeric ids order numerically, anything else
/// sorts after them.
fn numeric_id(id: &str) -> i64 {
    id.parse().unwrap_or(i64::MAX)
}

/// Render one compact memory fact line. Importance controls selection but is
/// deliberately absent from the wire, so a classification-only update does not
/// change a single byte of the prompt.
fn render_memory_line(memory: &MemoryRow) -> String {
    // Cap at 64 KiB on a char boundary (the reference's own cap) and indent
    // continuation lines so a multi-line fact cannot forge a top-level `#N:`
    // handle.
    let mut end = memory.content.len().min(64 * 1024);
    while !memory.content.is_char_boundary(end) {
        end -= 1;
    }
    let content = escape_xml_content(&memory.content[..end]).replace('\n', "\n  ");
    format!("#{}: {content}", memory.id)
}

/// The `<project-memory>` block, or an empty string when there are no memories.
pub(crate) fn render_memory_block(memories: &[MemoryRow]) -> String {
    if memories.is_empty() {
        return String::new();
    }
    let mut ordered: Vec<&MemoryRow> = memories.iter().collect();
    ordered.sort_by(|left, right| memory_render_order(left, right));

    let mut lines = Vec::with_capacity(memories.len() * 2 + 3);
    lines.push("<project-memory>".to_string());
    let mut open_category: Option<&str> = None;
    for memory in ordered {
        if open_category != Some(memory.category.as_str()) {
            if let Some(category) = open_category {
                lines.push(format!("</{}>", escape_xml_attr(category)));
            }
            open_category = Some(&memory.category);
            lines.push(format!("<{}>", escape_xml_attr(&memory.category)));
        }
        lines.push(render_memory_line(memory));
    }
    if let Some(category) = open_category {
        lines.push(format!("</{}>", escape_xml_attr(category)));
    }
    lines.push("</project-memory>".to_string());
    lines.join("\n")
}

/// Assemble the m1 body: the pinboard, or the placeholder when there is none.
///
/// The placeholder is always present when the pinboard is empty: the m1 half
/// must never vanish, or every byte after it shifts.
pub(crate) fn assemble_m1(memories: &str) -> String {
    if memories.is_empty() {
        return M1_PLACEHOLDER.to_string();
    }
    format!("{memories}\n{M1_PLACEHOLDER}")
}

/// Inputs to [`render_m0`], after the caller has loaded the rows it means to
/// show. This renderer only assembles them in order with framing.
pub(crate) struct M0Inputs<'a> {
    /// The compartment history, chronological (oldest first).
    pub compartments: &'a [DecayRenderCompartment],
    /// The history budget in tokens, before pressure.
    pub history_budget_tokens: f64,
}

/// Compose the m0 baseline: `<session-history>` alone. The history section is
/// always present (empty history uses [`M0_EMPTY_BODY`]) because it is the
/// provider cache's anchor.
pub(crate) fn render_m0(inputs: &M0Inputs<'_>) -> String {
    let history = render_decayed_compartments(
        inputs.compartments,
        inputs.history_budget_tokens,
        estimate_tokens,
    );
    if history.is_empty() {
        return M0_EMPTY_BODY.to_string();
    }
    format!("<session-history>\n{history}\n</session-history>")
}
