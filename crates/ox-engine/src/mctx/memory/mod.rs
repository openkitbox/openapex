//! Project memories (`ctx_memory`) and session notes (`ctx_note`): storage,
//! the two tools, and the store reads the pinboard render uses.
//!
//! # Two kinds of durable text
//!
//! - **Memories** are project-scoped facts that stay true for months: a
//!   project rule, an architectural fact, a hard-won constraint, a config
//!   value, a naming convention. Every active memory is rendered into the
//!   `<project-memory>` block of the request head ([`crate::mctx::render`]),
//!   so it reaches every future session of the project without being asked
//!   for.
//! - **Notes** are session-scoped pending intentions: work to return to,
//!   with its findings attached. They are pull-based (the model asks with
//!   `ctx_note read`) — with one exception: a note parked on a
//!   `surface_condition` is surfaced into the session's pinboard by the
//!   dreamer once the condition holds (see the deviation below).
//!
//! # Owners: project and user
//!
//! `mc_memory.project` is the row's **owner key**, not only a directory. A
//! project memory's owner is the canonical working directory; a memory about
//! the user themself — how they like to work, what they know, what they keep
//! correcting — is owned by [`USER_SCOPE`] and rendered into every session of
//! every project (`<user-profile>`, see [`crate::mctx::render`]). The owner is
//! decided by the category ([`owner_for`]): [`USER_PROFILE`] rows are user
//! memories, every category in [`CATEGORIES`] is a project memory. Handles are
//! global, so an id-addressed operation from a project may reach that
//! project's rows *and* the user's ([`owner_match`]) — never another
//! project's.
//!
//! # Handles and lifecycles
//!
//! Ids are small ascending integers (`#17`) from a single sequence per table,
//! minted by `MAX(id) + 1` inside the writing transaction (the reference
//! addresses memories and notes as `#N`). A handle is a unique address; the
//! owning scope (project or user for memories, session for notes) is an
//! ordinary column, not part of the handle. The column is `text`; ordering
//! casts to `INTEGER`.
//!
//! Memories: `active` → `archived` (retirable, still `get`-able; the
//! optional archive reason is kept). Notes: `active`, `pending` (a smart
//! note parked on a `surface_condition`), `ready` (the condition holds; it is
//! in the pinboard) and `dismissed`.
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **A condition is checked by a periodic sweep, and its verdict is rendered
//!   in the pinboard.** The reference compiles a note's condition into a check
//!   (`evaluate-n-notes`), surfaces it at natural boundaries, and keeps ready
//!   notes out of m[1] on purpose (`m1_compose.rs`: "Ready smart notes are
//!   deliberately absent from m1 … the host's deferred-notes reminder, then
//!   `ctx_note read`"). ox has one trigger form — the dreamer's
//!   `surface_notes` task, on the dreamer interval — and its verdict is a
//!   status flip to `ready`, which the session's next pinboard rebuild renders
//!   as a `<ready-notes>` line ([`crate::mctx::render`]). Nothing is appended
//!   to the live transcript, so a surfacing never writes a prompt mid-turn; it
//!   costs the same one-turn `Soft` prefix shift a memory write does. `read`
//!   still orders newest first for every filter: the reference's "ready first"
//!   page order is not ported, because handle order already puts a
//!   just-surfaced note near the front (ids ascend with write time).
//! - **Merge candidates are active rows only.** An archived row can be
//!   neither survivor (its content would be stranded outside the pinboard)
//!   nor loser (its archive reason would be overwritten), so a merge needs
//!   at least two *active* rows among the ids; repeats are rejected. The
//!   survivor is the lowest id (the oldest row), which is updated to the
//!   merged content; the other rows are archived with reason `"merged"`.
//! - **Age and staleness come from the last touch** (`updated_at`, falling
//!   back to `created_at` — the reference's `note_touched_at`), so editing a
//!   stale note clears its marker.
//! - **Failure convention** follows the module's other tools: rejections
//!   are model-facing `Error: …` *content* (the reference tools return
//!   strings), not tool failures.
//! - Model-facing text is ox-authored where the reference string was not
//!   recoverable; the tool *interfaces* (actions, fields, handle shapes) are
//!   the reference's.
//! - **Memory text is redacted before it is stored.** Every store write
//!   ([`write_memory`], [`update_memory`], [`merge_memories`],
//!   [`insert_promoted`]) passes the content through
//!   [`crate::mctx::redact::redact_secrets`]; rows stored earlier are not
//!   backfilled.

/// The closed project category set, in pinboard render order.
pub(crate) const CATEGORIES: [&str; 5] = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
];

/// The owner key of memories about the user (see the module docs). Not a path
/// — `@` cannot open a canonical directory — so it can never collide with a
/// project.
pub const USER_SCOPE: &str = "@user";
/// The one user-owned category: universal observations about how the user
/// works, independent of any project (the historian's `<user_observations>`).
pub(crate) const USER_PROFILE: &str = "USER_PROFILE";
/// The user-owned category set.
pub(crate) const USER_CATEGORIES: [&str; 1] = [USER_PROFILE];

/// Whether `owner` is the user namespace rather than a project directory.
#[must_use]
pub(crate) fn is_user_scope(owner: &str) -> bool {
    owner == USER_SCOPE
}

/// Whether `category` is one a memory may carry, in either owner.
#[must_use]
pub(crate) fn is_known_category(category: &str) -> bool {
    CATEGORIES.contains(&category) || USER_CATEGORIES.contains(&category)
}

/// The owner a memory of `category` written from `project` belongs to: the
/// user namespace for [`USER_CATEGORIES`], the project otherwise.
#[must_use]
pub(crate) fn owner_for<'a>(project: &'a str, category: &str) -> &'a str {
    if USER_CATEGORIES.contains(&category) {
        USER_SCOPE
    } else {
        project
    }
}

/// The categories `owner` admits.
#[must_use]
pub(crate) fn categories_of(owner: &str) -> &'static [&'static str] {
    if is_user_scope(owner) {
        &USER_CATEGORIES
    } else {
        &CATEGORIES
    }
}

/// The id-addressed reach of a caller working in `project`: its own rows and
/// the user's. Bound as two parameters, never interpolated.
fn owner_match(project: &str) -> (&str, &'static str) {
    (project, USER_SCOPE)
}

/// Where a memory row came from (the `source` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// Written on purpose: `ctx_memory`, the dreamer's retrospective, a merge.
    Manual,
    /// Promoted from a historian fold's side channel, unconfirmed by a human.
    Historian,
}

impl MemorySource {
    /// The column spelling, as the historian's promotion writes it.
    pub(crate) const HISTORIAN: &'static str = "historian";

    fn parse(value: &str) -> Self {
        if value == Self::HISTORIAN {
            Self::Historian
        } else {
            Self::Manual
        }
    }
}

/// A note whose age reaches this many days is flagged stale in `read`
/// (reference: "rows untouched 30+ days are marked stale").
const STALE_DAYS: i64 = 30;

/// Most ids one `ctx_memory get` may ask for.
const MEMORY_GET_LIMIT: usize = 20;
/// Most ids one `ctx_memory` archive/merge may carry (ox's own bound: the
/// reference states no numeric cap).
const MEMORY_MUTATE_LIMIT: usize = 50;
/// Most ids one `ctx_note read`/`update`/`dismiss` may carry.
const NOTE_ID_LIMIT: usize = 50;
/// Cap on the `read` page size: `limit` is a page knob, so it is clamped
/// rather than allowed to reach SQLite as an unbounded (or negative) LIMIT.
const MAX_NOTE_PAGE: usize = 200;
/// Default `read` page size.
const NOTE_PAGE: usize = 25;
/// Most surfaced notes one session's pinboard carries. A surfaced note is an
/// intention to return to, and the pinboard pays for it on every request, so
/// the list is bounded; the oldest handles win, because they have waited
/// longest.
const READY_NOTE_LIMIT: usize = 10;

mod ctx_memory_tool;
mod ctx_note_tool;
mod memories;
mod notes;
mod tool_args;

pub use self::ctx_memory_tool::*;
pub use self::ctx_note_tool::*;
pub use self::memories::*;
pub(crate) use self::notes::*;
pub(crate) use self::tool_args::*;
