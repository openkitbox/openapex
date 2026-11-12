//! Deterministic m[0]/m[1] head composition.
//!
//! The request head is two blocks the model sees before the live transcript:
//!
//! - **m[0]** — the frozen baseline: `<session-history>`, the decay-rendered
//!   compartment set, measured against the session's history budget.
//! - **m[1]** — the volatile half: the `<project-memory>` pinboard, the
//!   `<user-profile>` the user carries into every project, then the surfaced
//!   `<ready-notes>` and the placeholder that keeps the block non-empty.
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
//! - **No `<project-docs>` / `<covered-system-messages>` sections.** Those
//!   carry upstream's docs injection and covered system-role messages; ox has
//!   no source for them yet (they land with the historian/system-message work
//!   if needed), so the m0 section list here is `<session-history>` alone.
//! - **`<user-profile>` lives in m[1], not m[0]**, for the same cache reason
//!   as the pinboard: it is a memory tier ([`crate::mctx::memory::USER_SCOPE`])
//!   and changes with memory writes.
//! - **No m[1] `<memory-updates>` corrections block.** The reference freezes the
//!   pinboard inside m[0] and patches it with per-entry mutations in m[1]. ox
//!   instead renders the pinboard once, in m[1], so there is a single
//!   authoritative rendering and no stale-frozen-vs-delta disagreement to
//!   reconcile. The cost of that simplification is paid only when a memory
//!   changes: the delta's bytes shift from the pinboard onwards, one turn.
//! - **No workspace memory union** (the reference's `source_name_by_id` /
//!   `WorkspaceMembership`): ox memories are scoped to one project directory.
//! - **A `<memory-index>` after the memory tiers** ([`render_memory_index`]):
//!   a few counts telling the model how much archive exists beyond the blocks
//!   it sees (memories the budget left out, folded compartments, notes), so it
//!   knows when `ctx_search` is worth a call. The reference's equivalent is
//!   the "What's in Memory" index of its memory summary.
//! - **The token estimator is injected** (`impl Fn(&str) -> usize`): the pass
//!   resolves the session's counter once — [`crate::mctx::tokens::heuristic`]
//!   scaled by the measured provider ratio ([`crate::mctx::calibration`]) — and
//!   threads it through [`M0Inputs`], so no vocabulary is ever loaded.

use crate::mctx::decay_render::{DecayRenderCompartment, render_decayed_compartments};
use crate::mctx::memory::{MemoryRow, ReadyNote, numeric_id};

/// The m0 body when no compartment exists. The `<session-history>` tag is
/// always present (never omitted) so the provider prompt cache has a stable
/// breakpoint to anchor on — see the module docs.
pub const M0_EMPTY_BODY: &str = "<session-history></session-history>";
/// The non-empty placeholder for the m1 delta block when there is no pinboard.
/// m1 is the volatile half of the cached prefix and must never be fully empty
/// for the same cache-anchoring reason.
pub const M1_PLACEHOLDER: &str =
    "<session-history-since>(no new content since last materialization)</session-history-since>";

/// Characters of a surfaced note's body the pinboard carries. A note is an
/// intention, not a fact: the title and the opening of the body are enough to
/// recognize it, and the full text is one `ctx_note read` away.
const READY_BODY_CHARS: usize = 400;

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
///
/// Takes the rows the caller selected ([`crate::mctx::memory::select_memories`]
/// decides *who* is in, under a token budget); this function only decides the
/// byte order, so the same included set always renders the same bytes.
pub(crate) fn render_memory_block(memories: &[&MemoryRow]) -> String {
    render_categorized_block("project-memory", memories)
}

/// The `<user-profile>` block: the user's own memories, rendered exactly like
/// the pinboard under their own tag so the model (and the tool description)
/// can tell the two tiers apart. Empty string when there are none.
pub(crate) fn render_user_profile_block(memories: &[&MemoryRow]) -> String {
    render_categorized_block("user-profile", memories)
}

/// One memory tier as `<tag><CATEGORY>#id: fact…</CATEGORY></tag>`.
fn render_categorized_block(tag: &str, memories: &[&MemoryRow]) -> String {
    if memories.is_empty() {
        return String::new();
    }
    let mut ordered: Vec<&MemoryRow> = memories.to_vec();
    ordered.sort_by(|left, right| memory_render_order(left, right));

    let mut lines = Vec::with_capacity(memories.len() * 2 + 3);
    lines.push(format!("<{tag}>"));
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
    lines.push(format!("</{tag}>"));
    lines.join("\n")
}

/// The `<ready-notes>` block, or an empty string when nothing is surfaced.
///
/// One line per note: `#7: <title> — <body>`. A note's body is model-written
/// prose, so it is escaped like memory text, cut at [`READY_BODY_CHARS`] on a
/// character boundary and flattened to one line — a note that was parked on a
/// condition is a reminder, not a document, and the pinboard pays for it on
/// every request while it is surfaced. A one-line note renders its title alone.
pub(crate) fn render_ready_notes_block(notes: &[ReadyNote]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(notes.len() + 2);
    lines.push("<ready-notes>".to_string());
    for note in notes {
        let title = escape_xml_content(&note.title);
        let body = escape_xml_content(head_chars(&note.body, READY_BODY_CHARS)).replace('\n', " ");
        let body = body.trim();
        if body.is_empty() {
            lines.push(format!("#{}: {title}", note.id));
        } else {
            lines.push(format!("#{}: {title} — {body}", note.id));
        }
    }
    lines.push("</ready-notes>".to_string());
    lines.join("\n")
}

/// Render compartments that have not yet entered the frozen baseline.
///
/// New rows stay at P1 until the next hard fold, and are ordered by the
/// chronological order supplied by [`crate::mctx::compartments::load`].
pub(crate) fn render_new_compartments(compartments: &[DecayRenderCompartment<'_>]) -> String {
    let mut rendered = Vec::with_capacity(compartments.len());
    for compartment in compartments {
        let body = crate::mctx::decay_render::render_compartment_at_tier(compartment, 1);
        if !body.is_empty() {
            rendered.push(body);
        }
    }
    if rendered.is_empty() {
        return String::new();
    }
    format!(
        "<new-compartments>\n{}\n</new-compartments>",
        rendered.join("\n\n")
    )
}

/// The first `max` characters of `text`, cut on a character boundary. Borrowed,
/// never copied — the block above is built once per pass from rows the caller
/// already owns.
fn head_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// The rendered parts of m1, each an empty string when absent. Borrowed from
/// the pass's own render results; nothing is copied until the join.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct M1Parts<'a> {
    /// `<project-memory>` ([`render_memory_block`]).
    pub memories: &'a str,
    /// `<user-profile>` ([`render_user_profile_block`]).
    pub user_profile: &'a str,
    /// `<memory-index>` ([`render_memory_index`]).
    pub index: &'a str,
    /// `<ready-notes>` ([`render_ready_notes_block`]).
    pub notes: &'a str,
    /// `<new-compartments>` for covered rows not yet in m0.
    pub new_compartments: &'a str,
}

/// What the `<memory-index>` tells the model about the archive it cannot see:
/// how much there is beyond the rendered blocks, so it knows when
/// `ctx_search` is worth a call. Counts only — a few dozen tokens that change
/// when the archive changes, not per turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemoryIndex {
    /// Active project memories, and how many of them `<project-memory>` shows.
    pub project: Shown,
    /// Active user-profile memories, and how many `<user-profile>` shows.
    pub profile: Shown,
    /// The session's folded compartments.
    pub compartments: usize,
    /// `created_at` of the oldest and newest compartment (epoch ms).
    pub compartment_span: Option<(i64, i64)>,
    /// The session's surfaced notes.
    pub notes: usize,
}

/// An active count beside the part of it the head renders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Shown {
    pub active: usize,
    pub shown: usize,
}

impl Shown {
    /// `N active, M shown` — or `N active, all shown` when nothing is hidden.
    fn describe(self) -> String {
        if self.shown >= self.active {
            format!("{} active, all shown", self.active)
        } else {
            format!("{} active, {} shown", self.active, self.shown)
        }
    }
}

/// The `<memory-index>` block, or an empty string when the archive is empty
/// (an index of nothing would only cost tokens). Dates are local calendar
/// days, like the prompt's own `core/date`.
pub(crate) fn render_memory_index(index: &MemoryIndex) -> String {
    if index.project.active == 0
        && index.profile.active == 0
        && index.compartments == 0
        && index.notes == 0
    {
        return String::new();
    }
    let zone = jiff::tz::TimeZone::system();
    let mut lines = vec!["<memory-index>".to_owned()];
    if index.project.active > 0 {
        lines.push(format!("project memories: {}", index.project.describe()));
    }
    if index.profile.active > 0 {
        lines.push(format!("user profile: {}", index.profile.describe()));
    }
    if index.compartments > 0 {
        let span = index
            .compartment_span
            .map(|(oldest, newest)| {
                let from = crate::prompt::local_date(oldest, &zone);
                let to = crate::prompt::local_date(newest, &zone);
                if from == to {
                    format!(", {from}")
                } else {
                    format!(", {from}..{to}")
                }
            })
            .unwrap_or_default();
        lines.push(format!(
            "session history: {} folded compartments{span}",
            index.compartments
        ));
    }
    if index.notes > 0 {
        lines.push(format!("notes: {} ready", index.notes));
    }
    lines.push("ctx_search reaches all of it, shown or not.".to_owned());
    lines.push("</memory-index>".to_owned());
    lines.join("\n")
}

/// Assemble the m1 body: the pinboard, the user profile, the memory index,
/// then the surfaced notes, or the placeholder when none exists.
///
/// The placeholder is the marker the prefix split looks for at the seam, so it
/// is load-bearing in exactly one case: when nothing but notes is shown it
/// leads the half (`{M1_PLACEHOLDER}\n{notes}`), because a delta opening with
/// `<ready-notes>` would not be recognized as m[1] at all (the two memory
/// tiers are recognized openers). Byte stability is the other half of the
/// contract: with no profile and no surfaced notes the output is byte-for-byte
/// what it was before either existed.
pub(crate) fn assemble_m1(parts: M1Parts<'_>) -> String {
    let memory_tiers: Vec<&str> = [parts.memories, parts.user_profile, parts.index]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
    let mut sections = memory_tiers;
    if !parts.new_compartments.is_empty() {
        sections.push(parts.new_compartments);
    }
    match (sections.is_empty(), parts.notes.is_empty()) {
        (true, true) => M1_PLACEHOLDER.to_string(),
        (true, false) => format!("{M1_PLACEHOLDER}\n{}", parts.notes),
        (false, true) => {
            sections.push(M1_PLACEHOLDER);
            sections.join("\n")
        }
        (false, false) => {
            sections.push(parts.notes);
            sections.join("\n")
        }
    }
}

/// Inputs to [`render_m0`], after the caller has loaded the rows it means to
/// show. This renderer only assembles them in order with framing.
pub(crate) struct M0Inputs<'a> {
    /// The compartment history, chronological (oldest first).
    pub compartments: &'a [DecayRenderCompartment<'a>],
    /// The history budget in tokens, before pressure.
    pub history_budget_tokens: f64,
    /// The session model family's token counter: what the decay guard's
    /// budget honesty is measured in.
    pub count_tokens: &'a dyn Fn(&str) -> usize,
}

/// Compose the m0 baseline: `<session-history>` alone. The history section is
/// always present (empty history uses [`M0_EMPTY_BODY`]) because it is the
/// provider cache's anchor.
pub(crate) fn render_m0(inputs: &M0Inputs<'_>) -> String {
    let history = render_decayed_compartments(
        inputs.compartments,
        inputs.history_budget_tokens,
        inputs.count_tokens,
    );
    if history.is_empty() {
        return M0_EMPTY_BODY.to_string();
    }
    format!("<session-history>\n{history}\n</session-history>")
}
