//! Deterministic m[0]/m[1] head composition.
//!
//! The request head is two blocks the model sees before the live transcript:
//!
//! - **m[0]** — the frozen baseline: `<session-history>`, the decay-rendered
//!   compartment set, measured against the session's history budget.
//! - **m[1]** — the volatile half: the `<project-memory>` pinboard, then the
//!   surfaced `<ready-notes>` and the placeholder that keeps the block
//!   non-empty.
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
    if memories.is_empty() {
        return String::new();
    }
    let mut ordered: Vec<&MemoryRow> = memories.to_vec();
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

/// The first `max` characters of `text`, cut on a character boundary. Borrowed,
/// never copied — the block above is built once per pass from rows the caller
/// already owns.
fn head_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Assemble the m1 body: the pinboard, then the surfaced notes, or the
/// placeholder when neither exists.
///
/// The placeholder is the marker the prefix split looks for at the seam, so it
/// is load-bearing in exactly one case: when nothing but notes is shown it
/// leads the half (`{M1_PLACEHOLDER}\n{notes}`), because a delta opening with
/// `<ready-notes>` would not be recognized as m[1] at all. Byte stability is the
/// other half of the contract: with no surfaced notes the output is byte-for-
/// byte what it was before notes existed.
pub(crate) fn assemble_m1(memories: &str, notes: &str) -> String {
    match (memories.is_empty(), notes.is_empty()) {
        (true, true) => M1_PLACEHOLDER.to_string(),
        (false, false) => format!("{memories}\n{notes}"),
        (false, true) => format!("{memories}\n{M1_PLACEHOLDER}"),
        (true, false) => format!("{M1_PLACEHOLDER}\n{notes}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mctx::memory::select_memories;

    fn memory(id: &str, category: &str, content: &str) -> MemoryRow {
        MemoryRow {
            id: id.to_string(),
            category: category.to_string(),
            content: content.to_string(),
            importance: 0.0,
            updated_at: 0,
            verified_at: 0,
        }
    }

    #[test]
    fn rendered_bytes_are_unchanged_when_every_memory_fits() {
        // Selection only decides who is included: the framing, category
        // grouping, numeric-id order and escaping stay byte-identical.
        let rows = [
            memory("2", "PROJECT_RULES", "Rule B"),
            memory("1", "PROJECT_RULES", "Rule A & <x>"),
        ];
        // A budget every row fits under keeps the whole set...
        let selected = select_memories(&rows, 1_000);
        assert_eq!(selected.len(), rows.len());
        let expected = "<project-memory>\n\
                        <PROJECT_RULES>\n\
                        #1: Rule A &amp; &lt;x&gt;\n\
                        #2: Rule B\n\
                        </PROJECT_RULES>\n\
                        </project-memory>";
        // ...and its rendering is byte-identical to the pre-budget output.
        assert_eq!(render_memory_block(&selected), expected);
    }

    // -- ready notes --------------------------------------------------------

    fn note(id: &str, title: &str, body: &str) -> ReadyNote {
        ReadyNote {
            id: id.to_string(),
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn m1_bytes_are_unchanged_without_ready_notes() {
        // Byte stability is the whole point of the placeholder rule: a session
        // with no surfaced notes must render exactly what it rendered before
        // ready notes existed.
        assert_eq!(assemble_m1("", ""), M1_PLACEHOLDER);
        assert_eq!(
            assemble_m1("<project-memory>\n</project-memory>", ""),
            format!("<project-memory>\n</project-memory>\n{M1_PLACEHOLDER}")
        );
    }

    #[test]
    fn ready_notes_follow_the_memory_block() {
        let notes = render_ready_notes_block(&[note("7", "Migrate", "then run")]);
        assert_eq!(
            assemble_m1("<project-memory>\n</project-memory>", &notes),
            format!("<project-memory>\n</project-memory>\n{notes}")
        );
    }

    #[test]
    fn the_placeholder_survives_a_notes_only_delta() {
        // A budget can drop every memory while a note is still surfaced; the
        // delta half must still open with one of the markers the prefix split
        // recognises, or the whole m[1] half is lost at the seam.
        let notes = render_ready_notes_block(&[note("7", "Migrate", "then run")]);
        assert_eq!(
            assemble_m1("", &notes),
            format!("{M1_PLACEHOLDER}\n{notes}")
        );
    }

    #[test]
    fn a_ready_note_renders_as_one_escaped_line() {
        let notes = render_ready_notes_block(&[
            note("7", "Migrate <db>", "first line\nsecond & last"),
            note("9", "One-liner", ""),
        ]);
        assert_eq!(
            notes,
            "<ready-notes>\n\
             #7: Migrate &lt;db&gt; — first line second &amp; last\n\
             #9: One-liner\n\
             </ready-notes>"
        );
    }

    #[test]
    fn a_ready_note_body_is_capped_on_a_character_boundary() {
        let body = format!("{}\n{}", "é".repeat(READY_BODY_CHARS), "tail");
        let notes = render_ready_notes_block(&[note("1", "T", &body)]);
        assert!(
            notes.contains(&format!("— {}", "é".repeat(READY_BODY_CHARS))),
            "{notes}"
        );
        assert!(!notes.contains("tail"), "{notes}");
    }

    #[test]
    fn no_ready_notes_renders_nothing() {
        assert_eq!(render_ready_notes_block(&[]), "");
    }
}
