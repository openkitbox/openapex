//! Dreamer: the scheduled memory-maintenance loop (Task 8).
//!
//! The dreamer is the only place in ox where a model is asked to *maintain*
//! the memory store instead of answering a user turn. A tick enumerates the
//! projects that have memories, and for every due `(project, task)` pair runs
//! one task: `verify` (memory vs. code), `curate` (duplicate/stale
//! consolidation), `classify` (importance scoring for the injection budget),
//! `retrospective` (durable lessons from the user's own turns) and
//! `surface_notes` (parked smart notes whose condition now holds). Results are
//! applied host-side through [`crate::mctx::memory`]'s store functions, never by
//! the model.
//!
//! # Port map (reference → ox)
//!
//! - **Timer and tick semantics** — `docs/architecture/dreamer.md`
//!   ("Scheduling") and `dreamer/task-scheduler.ts`: a process-wide timer
//!   ticks every 15 minutes; each tick runs every due task whose cheap gate
//!   passes; tick stages are isolated so one project's failure cannot stop
//!   the scheduler. ox keeps the tick and the isolation, drops the cron
//!   schedules and the lease machinery (see the deviations).
//! - **`last_run_at` from the START of a successful run** —
//!   `task-scheduler.ts::advanceAfterRun` (line 292): it records "the start
//!   of the last SUCCESSFUL run", not the end, so work that landed *during*
//!   the run re-triggers the gate next slot instead of being skipped; and it
//!   "never advances on failure".
//! - **Retry-after-N-failures bound** — `task-scheduler.ts::MAX_TASK_RETRIES`
//!   (line 39, = 3) with `recordTransientFailure` (line 340): a failed run
//!   keeps its next slot so it is retried on the following ticks, up to the
//!   cap, then advances to the next scheduled slot. ox expresses this as
//!   "once [`MAX_DREAM_FAILURES`] failures are consecutive, each further
//!   failure doubles the wait since the failed attempt (2, 4, 8 ... intervals),
//!   capped at `MAX_DREAM_BACKOFF_MILLIS`"; a success or skip resets it.
//! - **Idle-project cost** — `dreamer/task-gates.ts` (gates are "cheap
//!   counts" so "an idle project costs no model calls") and
//!   `idle-schedule-prune.ts`. ox's enumeration of projects *with active
//!   memories* is that pruning, reduced to the one identity form ox has.
//! - **`verify` gate: only memories whose backing files are missing or
//!   changed** — `dreamer/verify-gate.ts` (a candidate is in scope if a mapped
//!   file changed since that memory's verification, or was deleted) plus
//!   `dreamer.md` ("re-checks mapped memories whose backing files changed
//!   since that memory was last verified"). The reference learns the mapping
//!   with its `map-memories` task (a model call); ox's MVP extracts the paths
//!   the memory text already names, host-side and free.
//! - **`verify` safety** — `dreamer/memory-claim-safety.ts` (32 lines) ported
//!   rule for rule: `isDirectiveShapedProjectRule` gates update AND archive,
//!   and an update that loses more than half the text is refused unless the
//!   model flagged it as a consolidation (`verify.ts` line ~640: "Losing more
//!   than half the claim is the characteristic denaturing shape"). Refusals
//!   are logged and are not failures ("refusals are logged and do not fail
//!   the run").
//! - **Manifest contract, fail CLOSED** — `dreamer.md` ("manifest tasks …
//!   output without the expected root element is rejected, never applied as a
//!   truncated prefix"), `dreamer/manifest-parser.ts::extractCompleteManifestBody`
//!   (a missing closing root is truncation and rejects the whole output), the
//!   `parse*Manifest` functions in `verify-prompt.ts` (a well-formed root with
//!   no recognized entries is a format miss, not success;
//!   `assertNoDuplicateManifestIds` rejects duplicates) and
//!   `verify.ts::applyParsedVerifyManifest` (unknown ids are dropped with a
//!   warning; a manifest covering fewer than half the batch is rejected as
//!   "mostly-wrong").
//! - **Verdicts and their application** — `verify.ts::applyParsedVerifyManifest`
//!   (verified banks progress, update rewrites, archive retires; directive and
//!   content-loss verdicts are refused *before* any write; an empty or
//!   oversized update falls back to a plain re-verify rather than wiping a
//!   memory).
//! - **`curate` scope and rotation** —
//!   `dreamer/curate-category-rotation.ts::peekCurateCategoryScope` (scan from
//!   the persisted cursor for the first category with work, wrapping through
//!   the five canonical categories; the cursor advances past the used category
//!   only on success, `curateTaskStateAfterSuccess`) and
//!   `getCurateCategoryScopeRefusal` (a merge/archive whose category or ids
//!   fall outside the scoped category is refused). The category list is
//!   `memory::CATEGORIES`, the same five the store closes over.
//! - **`curate` prompt** — `dreamer/task-prompts.ts::CURATE_SYSTEM_PROMPT`
//!   (line 40) and the curate user prompt (line ~110): pool QUALITY only, the
//!   pool is assumed accurate, never mint new facts, same-category merges
//!   only, be conservative with archives.
//! - **`verify` prompt** — `dreamer/verify-prompt.ts::VERIFY_SYSTEM_PROMPT`
//!   (line 23): three of its four verdicts, the present-tense update rule, the
//!   directive restriction, and "be conservative about archiving".
//! - **`classify` scores without rendering** — `dreamer.md` ("classify") and the
//!   reference's classify task: one hidden completion scores the pool's
//!   importance, and its only effect is a column update ("do not disturb the
//!   live prompt cache"). ox writes `mc_memory.importance`, which the
//!   pinboard's *selection* reads and its *render* never prints, so a
//!   classification changes no prompt byte.
//! - **`retrospective` reads the user's own turns** — `dreamer.md`
//!   ("retrospective"): the durable-lesson pass. ox scopes it to at most one
//!   lesson per run over the project's recent user messages, and writes it
//!   through [`memory::write_memory`] — the same row `ctx_memory` writes, so
//!   there is one memory path, not two.
//! - **`surface_notes` judges conditions against the user's turns** —
//!   `dreamer.md` ("`evaluate-n-notes` compiles n-note conditions into checks,
//!   runs them in a sandbox and surfaces notes whose condition holds") and
//!   `memory-and-search.md` ("when the condition holds, the note is surfaced to
//!   the session"). The host side is the same: parked notes in, a list of ids
//!   out, applied through the note store. The gate is a cheap count and the
//!   evidence is the same user-turn window `retrospective` reads, so a project
//!   with no parked note and a project with no new activity both cost zero
//!   model calls.
//! - **Failure classes** — `provider-output-failure.ts` /
//!   `dreamer.md` ("Failure records": `provider_timeout`, `provider_error`,
//!   `empty_completion`, `no_models`, `child_aborted`, `parse_failed`,
//!   `unknown`), reduced to [`FailureClass`].
//! - **Transport** — the hidden sub-completion is
//!   [`crate::mctx::historian`]'s pair (`historian_model` + `collect_completion`)
//!   on ox's existing cardinal client; no second transport is added. The
//!   reference's dreamer also shares the historian's provider-output path.
//!
//! # Deviations from the reference (documented, not accidental)
//!
//! - **Interval, not cron.** The reference stores one cron string per task
//!   (`dreamer/task-config.ts`) and runs each task at its own cadence (most
//!   nightly, `curate` weekly). ox runs every task on the single
//!   `dreamer_interval_minutes` tick. Cron parsing, per-task schedules and the
//!   `/ctx-dream` manual trigger are out of scope for the MVP.
//! - **One in-process task at a time, no leases.** The reference takes a
//!   two-minute cross-process lease (`lease.ts`) so two plugin processes cannot
//!   read-modify-write the same memory pool. ox is a single process (a
//!   designed invariant, not a limitation to work around), so the equivalent
//!   is one in-process claim set: a `(project, task)` currently running is
//!   skipped by any overlapping tick. A crash still loses the claim, which is
//!   safe — the claim protects *concurrency*, and the durable state row is
//!   what a crash recovery needs.
//! - **No hidden child session.** The reference runs each task in a hidden
//!   OpenCode session with tools; ox has no session plumbing for that, so each
//!   task is a single tool-free completion whose whole evidence the host
//!   renders into the prompt. That is why `verify` prints file excerpts
//!   instead of letting the model read files.
//! - **`curate` has no staleness signal.** The reference's curate judges
//!   duplicate *and stale* pools; staleness needs a freshness input ox does not
//!   carry (its memories have no age signal the model can weigh beyond the
//!   `updated_at` the prompt already prints). ox scopes curate to what the pool
//!   itself shows: same-category merges of duplicates, and archives of wording
//!   or redundancy. The task keeps its slot and its name.
//! - **A skip does not push the next due time.** `is_due` looks at
//!   `last_run_at` (successes only), so a task that skips is re-evaluated on
//!   every tick instead of at a skip cadence. That costs one state-row read
//!   plus a `stat` per cited file, and issues no model call when nothing is a
//!   candidate, so the reference's skip-cadence bookkeeping is not ported.
//! - **`last_run_at` is the tick start**, not that task's own start (the
//!   reference stamps the task). A tick starts before its tasks, so the next
//!   due time is never earlier than the reference's would be — the safe
//!   direction.
//! - **No `map-memories`.** The reference spends a model call per memory
//!   mapping it to the files that back it; ox MVP-extracts the paths the memory
//!   text already names. A memory that names no path is therefore never a
//!   verify candidate (the reference's "file-independent" case, which its
//!   gate also excludes).
//! - **`verified_at` is its own column** (V9), like the reference's: a
//!   verification is banked there and drops the memory out of the gate until a
//!   cited file changes again. The write watermark (`updated_at`) is a
//!   different fact and does not move on a verification; a content rewrite
//!   clears `verified_at` to 0, because new text is unverified text. `0` also
//!   means "this memory has never been checked", which is what makes the first
//!   slot check it — banking into `updated_at` used to make a freshly written
//!   memory look already verified, and an edit looked like a fresh check.
//!   What is *not* ported is per-memory *resume* after a dead batch: ox
//!   re-checks the whole remaining batch next slot, so a run's progress is
//!   all-or-nothing at the batch level. The verify prompt asks for
//!   a verdict on every id. A `skip` ("code cannot decide it") is STAMPED like
//!   `verified`, so the memory leaves the candidate set until a cited file
//!   changes or the memory is rewritten. This deviates from the reference,
//!   which does not stamp `skip`: without it, undecidable memories (those that
//!   cite a missing file) would hold the first-20 batch forever. An id the
//!   manifest omits still keeps its old watermark and stays a candidate.
//! - **The watermark is the run's start** ([`memory::touch_memories`]'s `at`),
//!   not the moment the verdict lands: a file written while the completion was
//!   in flight stays newer than the watermark and is re-checked next slot
//!   instead of being silently covered by a verification that never saw it.
//! - **`verify`'s gate is mtime-vs-`verified_at`, not mtime-vs-git.** The
//!   reference's incremental mode compares file mtimes against a *git* commit
//!   watermark; ox compares the file mtime against the memory's own
//!   `verified_at`, which needs no git and no history scan.
//! - **No `ctx_memory` tool calls by the model.** The reference lets the
//!   curate child call the memory tool under a category scope;
//!   ox parses a `curate` manifest and applies it through the same store
//!   functions the tool uses. The cross-category refusal is therefore
//!   host-side, and *stricter*: an id outside the scoped category rejects that
//!   verdict instead of being impossible to name.
//! - **Curate's archive needs no named survivor.** The reference refuses an
//!   archive without an active same-category survivor
//!   (`curate-memory-safety.ts::missing-active-same-category-successor`)
//!   because its `supersede` flow names one statically. ox has no `superseded_by`
//!   concept, so curate's archive carries a reason instead — and the same
//!   directive/content-loss refusals still apply. Archives there are also
//!   targeted at the *category* pool, which is what curate is for.
//! - **No user-profile rule.** `curate-memory-safety.ts`'s
//!   `user-profile-is-not-project-memory` guards a cross-project profile ox
//!   does not have (explicitly out of scope).
//! - **`surface_notes` is one completion, not a compiled check.** The
//!   reference compiles a note's condition into a check and runs it in a
//!   sandbox (`evaluate-n-notes`), standing down in favour of a scheduled-wake
//!   daemon when one exists. ox has neither a sandbox nor a daemon: the
//!   condition is natural-language, the evidence is the project's recent user
//!   turns, and the judge is the same hidden completion the other tasks use. A
//!   condition phrased as something a shell command could decide is therefore
//!   judged by reading, not by running — which is why the prompt tells the
//!   checker to leave anything it cannot decide parked.
//! - **A backlog larger than the batch can outrun its evidence.** The batch is
//!   the 20 lowest handles, and the evidence window is the turns since the last
//!   completed sweep, so a note queued behind twenty others is judged only
//!   against turns from after the sweep that skipped it. A condition whose
//!   evidence was one specific past turn can therefore go unjudged; the failure
//!   direction is safe — the note stays parked — and a sweep that did not run at
//!   all never advances the window, so the evidence survives a failure.
//! - **A project with notes but no memories is not swept.** The tick enumerates
//!   the projects that hold active memories
//!   ([`memory::projects_with_active_memories`]), so a project whose only
//!   durable text is parked notes never reaches `surface_notes`. Enumerating
//!   note projects as well would put every task's cheap gate into projects the
//!   dreamer has never seen — a scheduling change this task does not make, so
//!   the limitation is recorded instead.
//! - **Not ported at all:** the reference's remaining tasks (doc proposals,
//!   primers, the mural, user-memory review), `scope`/`shareable` on classify
//!   (single project model, no consumer), token budgets, model fallback chains,
//!   and the `MC-D09`/`MC-S05` status reporting. `retrospective` uses the same
//!   `last_run_at` watermark as every other task (one unswept window per run)
//!   rather than the reference's per-task cursor set.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use regex::Regex;

use crate::mctx::historian::{collect_completion, historian_model};
use crate::mctx::memory::{self, CATEGORIES};
use ox_core::config::Config;
use ox_db::Db;
use ox_llm::cardinal::chat::Chat;
use ox_llm::cardinal::wire::{ChatMessage, ChatRequest};
use ox_schema::id::now_millis;

/// Consecutive failures of one `(project, task)` after which the task stops
/// being retried on every tick and waits for its next interval slot instead
/// (the reference's `MAX_TASK_RETRIES` in `dreamer/task-scheduler.ts`).
pub const MAX_DREAM_FAILURES: u32 = 3;

/// The longest a repeatedly failing `(project, task)` waits between attempts
/// (24 hours): the ceiling of the exponential wait in [`failure_backoff_millis`].
const MAX_DREAM_BACKOFF_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// Most memories one `verify` completion carries (the reference's
/// `VERIFY_BATCH_SIZE` in `dreamer/verify.ts`, chosen there for prompt-token
/// cost; ox keeps the same number so a batch stays a readable prompt).
const VERIFY_MEMORY_LIMIT: usize = 20;

/// Characters of each referenced file the host shows the verifier. The
/// reference reads short line ranges through a tool; ox has no tools, so it
/// prints an excerpt instead. File heads carry the declarations (paths,
/// signatures, constants) that most claims are about.
const VERIFY_EXCERPT_CHARS: usize = 4_000;

/// Total excerpt budget for one `verify` prompt, across every cited file. Not
/// a model limit — a cost guard, so one task never assembles an arbitrarily
/// large prompt from a memory that cites many files.
const VERIFY_EXCERPT_TOTAL_CHARS: usize = 40_000;

/// The task's own prefixed completion prompt (see the port map). Adapts
/// `dreamer/verify-prompt.ts::VERIFY_SYSTEM_PROMPT` to the tool-free host.
const VERIFY_SYSTEM_PROMPT: &str = "You are a memory verifier for the magic-context system. You verify project memories against the CURRENT code.

The host has already read the files each memory names and printed short excerpts below; you have no tools and must not ask for more. Judge each memory on that evidence alone.

Decide ONE of four outcomes per memory:
- VERIFIED — still accurate. Keep it as-is.
- UPDATE — a CODE FACT is still true but a file-falsifiable DETAIL drifted (a renamed symbol, a moved file, a changed number or name). Provide the corrected content in terse present tense (\"X uses Y\", not \"X was changed to Y\"). Only update for genuine drift, not style. If a legitimate update intentionally consolidates the old claim into much shorter content, set consolidation=\"true\" explicitly.
- ARCHIVE — a CODE FACT is positively falsified: the code clearly contradicts it, or the API/symbol/path it describes no longer exists.
- SKIP — the claim is not a code fact, or code cannot decide it. Keep it unchanged and do not claim verification.

UPDATE and ARCHIVE are ONLY for claims a repository file can falsify. Behavioral directives (when to act, how to work, who decides, tool-usage discipline) can only be VERIFIED or SKIPPED — a named path inside a directive does not make the directive a code fact, and a file's failure to corroborate a behavioral rule is never grounds to update or archive it. A memory whose referenced file is simply gone is NOT automatically falsified: if the claim is behavioral, or the excerpt cannot decide it, mark it VERIFIED or SKIPPED and leave it alone.

BE CONSERVATIVE ABOUT ARCHIVING. Wrong archival of a TRUE memory is the worst possible outcome — far worse than leaving a slightly-stale memory. If you are unsure, or the excerpt cannot decide it: mark it VERIFIED or SKIPPED, never archived. Archive ONLY with positive evidence the code contradicts a code fact.

Every memory below must appear in your manifest: each input memory id appears exactly once, in exactly one element.

Output ONE XML manifest at the very end and NOTHING else — no narration, no per-memory commentary, no reasoning:
<verify>
<verified id=\"N\"/>
<update id=\"M\">corrected present-tense content</update>
<update id=\"C\" consolidation=\"true\">intentionally consolidated content</update>
<archive id=\"K\" reason=\"specific evidence the code contradicts it\"/>
<skip id=\"S\" reason=\"behavioral directive; code cannot decide it\"/>
</verify>

Rules:
- Every input memory id MUST appear exactly once, in exactly one of verified/update/archive/skip. Ids must come from the list below.
- Default to VERIFIED. skip when code cannot decide the claim. update and archive are the exceptions, not the norm.";

/// The curate task's prefixed completion prompt, adapting
/// `dreamer/task-prompts.ts::CURATE_SYSTEM_PROMPT` to a manifest instead of
/// `ctx_memory` tool calls.
const CURATE_SYSTEM_PROMPT: &str = "You are a memory-pool curator for the magic-context system. You keep a project's cross-session memory store lean and well-formed.

## Rules
1. **Assume the pool is accurate.** A separate verify task checks memories against code. You handle QUALITY only — duplicates, wording, low-value entries — never correctness, and you do NOT read the codebase.
2. **Never mint new facts.** Merging and tightening only, never adding knowledge.
3. **Present-tense operational language.** \"X uses Y\", not \"X was changed to use Y\".
4. **One fact per memory.**
5. **Be conservative with archives.** Archive a memory only when another memory in the SAME category already preserves its information. A bare \"redundant\" verdict loses knowledge and is refused.
6. Every id in one merge must be in the category shown; cross-category merges are refused.

Output ONE XML manifest at the very end and NOTHING else — no narration, no per-memory commentary, no reasoning:
<curate>
<merge ids=\"1,2\" category=\"ARCHITECTURE\">consolidated canonical text</merge>
<archive id=\"3\" reason=\"fully restated by #1\"/>
</curate>

Rules:
- Each id you mention must appear exactly once, and must come from the list below.
- <merge> needs at least two ids and the consolidated text; <archive> needs a reason.
- A run that changes nothing is a valid answer: emit an empty <curate></curate>.";

/// The classify task's prefixed completion prompt. The output is a strict JSON
/// object because the only effect is a numeric column: there is no manifest to
/// apply, no text to store, and nothing the model may write itself.
const CLASSIFY_SYSTEM_PROMPT: &str = "You are a memory classifier for the magic-context system. You score a project's memories by importance.

IMPORTANCE is how much the fact deserves a permanent slot in every future request's prefix:
- 90-100: a rule the agent must never violate (a hard constraint, a safety rule, a stated directive).
- 60-89: an architectural fact or a hard-won gotcha that prevents real mistakes.
- 30-59: a useful convention, a config value, a path.
- 1-29: background that is nice to know.
- 0: trivia, restated context, or something the code already says.

Judge the fact itself, not how recently it was written, and use the whole range — a pool where everything scores 80 tells the host nothing.

Output ONE JSON object at the very end and NOTHING else — no narration, no reasoning:
{ \"12\": 90, \"13\": 20 }

Rules:
- Every id listed below appears exactly once, with a number between 0 and 100.
- Ids you were not given are ignored.";

/// Most memories one `classify` completion carries. Larger than the verify
/// batch because a score is one number per row and the prompt is the whole
/// cost: forty short facts still fit comfortably.
const CLASSIFY_BATCH: usize = 40;

/// Characters of one memory the classifier sees. A memory is already one fact;
/// the cap is a cost guard against a single verbose row eating the batch.
const CLASSIFY_CONTENT_CHARS: usize = 400;

/// The retrospective task's prefixed completion prompt. One element, or an
/// empty one: the whole task is "at most one durable lesson", so an answer that
/// emits a list is out of contract and rejected.
const RETROSPECTIVE_SYSTEM_PROMPT: &str = "You are a memory curator for the magic-context system, reviewing the user's own turns from a recent working session.

Your job: find AT MOST ONE durable lesson — a correction the user made, a constraint they stated twice, or a convention they set — that every future session of this project should know. Ignore task chatter, one-off instructions, and anything that only makes sense for the work of the moment.

Rules:
1. One fact, phrased to stand alone: present tense, no pronouns pointing back at the conversation, no \"the user said\".
2. The category must be one of: PROJECT_RULES (how work is done here), ARCHITECTURE (how the system is built), CONSTRAINTS (a hard limit or a gotcha), CONFIG_VALUES (a specific value or path), NAMING (what something is called).
3. Only a lesson the user actually stated or corrected. Never infer one, never generalize from a single request, and never restate something ordinary.
4. Nothing durable in these turns is a valid answer.
5. Distill, do not transcribe: never quote the user, never include dates, and never preserve session-local anger.

Output ONE element at the very end and NOTHING else — no narration, no reasoning:
<memory category=\"CONSTRAINTS\">one self-contained fact</memory>

Or, when nothing is durable:
<memory/>";

/// Most user turns one retrospective run reads, newest first (the reference's
/// window, reduced to a fixed bound).
const RETROSPECTIVE_TURNS: usize = 80;

/// Characters of one user turn the retrospective sees. A correction is usually
/// one or two sentences; the cap keeps a pasted stack trace out of the prompt.
const RETROSPECTIVE_TURN_CHARS: usize = 600;

/// Total characters of transcript one retrospective prompt carries. A cost
/// guard, not a model limit: the newest turns win the budget, so the oldest are
/// the ones dropped.
const RETROSPECTIVE_TOTAL_CHARS: usize = 8_000;

/// Cap on the one fact a run may record: a memory is injected into every future
/// request, so a chatty lesson must not become a wall of text.
const RETROSPECTIVE_MAX_CHARS: usize = 2_000;

/// The surface-notes task's prefixed completion prompt. The answer is a list of
/// ids and nothing else: a note's text is the model's own writing from an
/// earlier turn, so the only decision here is "is the condition satisfied", and
/// nothing the model says about a note may be stored.
const SURFACE_SYSTEM_PROMPT: &str = "You are a note-surfacing checker for the magic-context system. Each note below was parked with a condition that must hold before the note is worth showing again. You read the user's own recent turns from this project and decide which conditions hold NOW.

Rules:
1. A condition holds only on evidence in the turns below: something the user did, said, asked for, or finished. Do not infer progress from the note's own text, and do not surface a note because its topic came up.
2. A condition that is still ahead of the work, or that the turns cannot decide, does not hold. Leaving a note parked costs nothing; surfacing it early intrudes on work that is not ready for it.
3. A condition is a plain-language statement about state (\"after the schema migration lands\", \"when the user asks about pricing\"). Judge it as written.

Output ONE element at the very end for each satisfied note and NOTHING else — no narration, no per-note commentary:
<ready id=\"7\"/>
<ready id=\"9\"/>

Or, when no condition holds, emit no <ready> element at all — a one-line answer saying so is fine.";

/// Most parked notes one `surface_notes` completion carries. A note is a
/// paragraph rather than a fact, so the batch is half the classify batch.
const SURFACE_BATCH: usize = 20;

/// Characters of one note's body the checker sees. The condition is what is
/// judged; the body only says what would be surfaced.
const SURFACE_BODY_CHARS: usize = 300;

/// Characters of one user turn the checker sees: the same cap the retrospective
/// uses, for the same reason (a correction is a sentence; a paste is not).
const SURFACE_TURN_CHARS: usize = 600;

// ---------------------------------------------------------------------------
// Task, failure and report types
// ---------------------------------------------------------------------------

/// The tasks the MVP schedules, in the order a tick runs them. The order is
/// the reference's: `task-registry.ts::CANONICAL_DREAM_TASKS` lists `verify`
/// before `curate`, and `compareTaskOrder` orders due tasks that way; the tasks
/// added for context management follow them, cheapest first. It is fixed so a
/// tick is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DreamTask {
    /// Re-check memories whose referenced files are newer (or missing, if never verified).
    Verify,
    /// Duplicate/stale consolidation of one memory category.
    Curate,
    /// Score unclassified memories for the pinboard's injection budget.
    Classify,
    /// Record at most one durable lesson from the project's recent user turns.
    Retrospective,
    /// Move parked smart notes whose condition holds to `ready`.
    SurfaceNotes,
}

impl DreamTask {
    /// Every task, in tick order.
    pub const ALL: [DreamTask; 5] = [
        DreamTask::Verify,
        DreamTask::Curate,
        DreamTask::Classify,
        DreamTask::Retrospective,
        DreamTask::SurfaceNotes,
    ];

    /// The `task` column spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            DreamTask::Verify => "verify",
            DreamTask::Curate => "curate",
            DreamTask::Classify => "classify",
            DreamTask::Retrospective => "retrospective",
            DreamTask::SurfaceNotes => "surface_notes",
        }
    }
}

/// Build a dreamer's hidden completion request. Dreamer has no parent session,
/// so the request deliberately leaves reasoning effort unset.
pub fn dreamer_request(model: &str, system: &str, user_prompt: &str) -> ChatRequest {
    ChatRequest::new(
        model.to_owned(),
        vec![
            ChatMessage::System {
                content: system.to_owned(),
            },
            ChatMessage::User {
                content: user_prompt.to_owned().into(),
            },
        ],
    )
}

/// Why a dream run failed, in the reference's vocabulary
/// (`provider-output-failure.ts`, `dreamer.md` "Failure records"). Recorded on
/// the state row so a stuck task is diagnosable from the database alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The output had no complete expected root element (truncation or drift).
    ParseFailed,
    /// The completion succeeded but carried no text.
    EmptyCompletion,
    /// The transport failed, or the stream ended in an error.
    ProviderError,
    /// No model could be resolved for the task.
    NoModels,
    /// A host-side failure (the store, or applying a valid manifest).
    Unknown,
}

impl FailureClass {
    /// The spelling recorded in `mc_dream_state.last_error`.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::ParseFailed => "parse_failed",
            FailureClass::EmptyCompletion => "empty_completion",
            FailureClass::ProviderError => "provider_error",
            FailureClass::NoModels => "no_models",
            FailureClass::Unknown => "unknown",
        }
    }
}

/// A failed task run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskFailure {
    class: FailureClass,
    message: String,
}

impl TaskFailure {
    /// A host-side failure: the class is [`FailureClass::Unknown`] (the
    /// reference's catch-all) and the message keeps the root cause.
    fn host(what: &str, error: &anyhow::Error) -> Self {
        Self {
            class: FailureClass::Unknown,
            message: format!("{what}: {}", error.root_cause()),
        }
    }

    /// The `last_error` text: the class first, so the column is greppable.
    fn recorded(&self) -> String {
        format!("{}: {}", self.class.as_str(), self.message)
    }
}

/// What one task run did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskOutcome {
    /// The task ran to completion. `cursor` patches the rotation cursor when
    /// the task owns one.
    Completed { detail: String, cursor: Option<u32> },
    /// The moment was due, but the cheap gate found no work. Not a failure:
    /// the reference advances a skipped slot without touching `last_run_at`.
    Skipped { detail: String },
    /// The run failed; see [`TaskFailure`].
    Failed(TaskFailure),
}

/// The scheduler's persisted view of one `(project, task)`.
#[derive(Debug, Clone, Default)]
struct DreamState {
    /// The start of the last successful run, or `None` if it never succeeded.
    last_run_at: Option<i64>,
    /// The start of the last run of any outcome (the retry cap's clock).
    last_attempt_at: i64,
    /// `completed` / `skipped` / `failed` of the last run.
    last_status: Option<String>,
    /// `<class>: <message>` of the last failure.
    last_error: Option<String>,
    /// How many failures have happened in a row without a success or skip.
    consecutive_failures: u32,
    /// The `curate` rotation cursor (an index into [`CATEGORIES`]).
    rotation_cursor: u32,
}

/// What one `(project, task)` did in this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEntry {
    /// The project the task ran for.
    pub project: String,
    /// The task that ran.
    pub task: DreamTask,
    /// How it ended.
    pub status: TickStatus,
}

/// The terminal status of one task in one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickStatus {
    /// Ran and applied its result.
    Completed { detail: String },
    /// Due, but the gate found nothing to do.
    Skipped { detail: String },
    /// Failed; the failure is on the state row and the tick continues.
    Failed {
        class: FailureClass,
        message: String,
    },
    /// Another task of the same `(project, task)` is still running: the
    /// re-entrancy guard refused this one.
    Busy,
}

/// What one tick did. Tasks that were not due are not listed — the report is
/// "what the tick acted on", and a task that was neither due nor busy was not
/// acted on at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// One entry per task the tick considered and skipped (busy) or ran.
    pub entries: Vec<TaskEntry>,
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

/// The dreamer: the store, the config (interval and model), and the hidden
/// completion seam. Construct one and [`Dreamer::tick`] it, or
/// [`Dreamer::spawn`] the interval loop.
pub struct Dreamer {
    db: Db,
    /// Shared with the server: the dreamer outlives no config reload, so a
    /// handle is all it needs.
    config: Arc<Config>,
    chat: Arc<dyn Chat>,
    /// Default-model resolution for the hidden completion.
    catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
    /// The `(project, task)` pairs currently running in this process. The
    /// re-entrancy guard: a tick that fires while another is still inside a
    /// task sees the key and leaves it alone. See the module docs for why this
    /// replaces the reference's cross-process lease.
    in_flight: Mutex<HashSet<(String, DreamTask)>>,
}

/// Releases a claim when it leaves scope, whatever path it leaves by — an
/// early `?`, an applied verdict, or a panic. A leaked claim would wedge that
/// `(project, task)` for the life of the process.
struct TaskGuard<'a> {
    dreamer: &'a Dreamer,
    project: String,
    task: DreamTask,
}

impl Drop for TaskGuard<'_> {
    fn drop(&mut self) {
        self.dreamer
            .in_flight
            .lock()
            .expect("dreamer in-flight lock")
            .remove(&(self.project.clone(), self.task));
    }
}

impl Dreamer {
    /// A dreamer over `db`, configured by `config.magic_context`, issuing its
    /// hidden completions through `chat`.
    ///
    /// Returns `Arc<Self>`: the tick loop outlives its spawner and holds the
    /// object weakly, so nobody may own a bare `Dreamer`.
    pub fn new(
        db: Db,
        config: Arc<Config>,
        chat: Arc<dyn Chat>,
        catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            config,
            chat,
            catalog,
            in_flight: Mutex::new(HashSet::new()),
        })
    }

    /// The dreamer tick interval in milliseconds. Floored at one minute, so a
    /// config that skipped [`crate::mctx::MctxConfig::validated`] still cannot
    /// turn the loop into a hot reseed.
    fn interval_millis(&self) -> i64 {
        (self.config.magic_context.dreamer_interval_minutes.max(1) as i64).saturating_mul(60_000)
    }

    /// Spawn the interval loop. The first tick fires immediately, then every
    /// interval. The handle is detached on drop, which is what a process-wide
    /// background loop wants.
    ///
    /// Each tick runs in its own task, so a panic is contained to that tick
    /// (the join reports it) instead of ending the loop: memory consolidation
    /// is background work, and a bug in one pass must not stop every later
    /// pass for the life of the process.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let every = Duration::from_millis(self.interval_millis() as u64);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            loop {
                ticker.tick().await;
                let dreamer = Arc::clone(&self);
                let joined = tokio::spawn(async move { dreamer.tick(now_millis()).await }).await;
                match joined {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::error!(error = %error, "dreamer tick failed"),
                    Err(join) => tracing::error!(error = %join, "dreamer tick panicked"),
                }
            }
        })
    }

    /// One scheduler pass at `now` (epoch milliseconds; injected so a test can
    /// drive several slots without sleeping).
    ///
    /// Projects are visited in ascending order and tasks in [`DreamTask::ALL`]
    /// order. Every task is isolated: a failure is recorded on its state row
    /// and never fails the tick, the project, or a sibling task.
    pub async fn tick(&self, now: i64) -> Result<TickReport> {
        let mut report = TickReport::default();
        if !self.config.magic_context.enabled || !self.config.magic_context.dreamer_enabled {
            return Ok(report);
        }
        let interval = self.interval_millis();
        let projects = memory::projects_with_active_memories(&self.db).await?;
        for project in projects {
            for task in DreamTask::ALL {
                // A host failure on one task is that task's failure: it must not
                // cancel the rest of the tick (the other tasks, the other
                // projects, and the results already gathered).
                let state = match load_state(&self.db, &project, task).await {
                    Ok(state) => state,
                    Err(error) => {
                        tracing::warn!(
                            project = %project,
                            task = task.as_str(),
                            error = %error,
                            "dreamer could not read its schedule state"
                        );
                        report.entries.push(TaskEntry {
                            project: project.clone(),
                            task,
                            status: TickStatus::Failed {
                                class: FailureClass::Unknown,
                                message: error.root_cause().to_string(),
                            },
                        });
                        continue;
                    }
                };
                if !is_due(&state, now, interval) {
                    continue;
                }
                tracing::debug!(
                    project = %project,
                    task = task.as_str(),
                    last_status = state.last_status.as_deref().unwrap_or("never"),
                    last_error = state.last_error.as_deref().unwrap_or("none"),
                    consecutive_failures = state.consecutive_failures,
                    "dreamer task is due"
                );
                let Some(guard) = self.claim(&project, task) else {
                    report.entries.push(TaskEntry {
                        project: project.clone(),
                        task,
                        status: TickStatus::Busy,
                    });
                    continue;
                };
                let outcome = self.run_task(&project, task, &state, now).await;
                let status = match self.settle(&project, task, now, outcome).await {
                    Ok(status) => status,
                    Err(error) => {
                        // The task's own work is done; only recording it failed.
                        tracing::warn!(
                            project = %project,
                            task = task.as_str(),
                            error = %error,
                            "dreamer could not settle a task"
                        );
                        TickStatus::Failed {
                            class: FailureClass::Unknown,
                            message: error.root_cause().to_string(),
                        }
                    }
                };
                drop(guard);
                report.entries.push(TaskEntry {
                    project: project.clone(),
                    task,
                    status,
                });
            }
        }
        Ok(report)
    }

    /// Claim `(project, task)` for this process, or `None` when it is already
    /// running. The guard releases the claim when it drops.
    fn claim(&self, project: &str, task: DreamTask) -> Option<TaskGuard<'_>> {
        let key = (project.to_owned(), task);
        let mut in_flight = self.in_flight.lock().expect("dreamer in-flight lock");
        if !in_flight.insert(key) {
            return None;
        }
        drop(in_flight);
        Some(TaskGuard {
            dreamer: self,
            project: project.to_owned(),
            task,
        })
    }

    /// Record a settled task and render its report entry.
    async fn settle(
        &self,
        project: &str,
        task: DreamTask,
        now: i64,
        outcome: TaskOutcome,
    ) -> Result<TickStatus> {
        match outcome {
            TaskOutcome::Completed { detail, cursor } => {
                settle_success(&self.db, project, task, now, cursor).await?;
                tracing::info!(
                    project = %project,
                    task = task.as_str(),
                    detail = %detail,
                    "dreamer task completed"
                );
                Ok(TickStatus::Completed { detail })
            }
            TaskOutcome::Skipped { detail } => {
                settle_skip(&self.db, project, task, now).await?;
                tracing::debug!(
                    project = %project,
                    task = task.as_str(),
                    detail = %detail,
                    "dreamer task skipped"
                );
                Ok(TickStatus::Skipped { detail })
            }
            TaskOutcome::Failed(failure) => {
                // The recorded error is never a token or a raw provider body:
                // a class plus the root cause is enough to diagnose and
                // cheap enough to keep in the row.
                settle_failure(&self.db, project, task, now, &failure).await?;
                tracing::warn!(
                    project = %project,
                    task = task.as_str(),
                    class = failure.class.as_str(),
                    error = %failure.message,
                    "dreamer task failed"
                );
                Ok(TickStatus::Failed {
                    class: failure.class,
                    message: failure.message,
                })
            }
        }
    }

    /// Run one task. Errors are values, never `?`: a task failing is a normal
    /// tick outcome.
    async fn run_task(
        &self,
        project: &str,
        task: DreamTask,
        state: &DreamState,
        started_at: i64,
    ) -> TaskOutcome {
        match task {
            DreamTask::Verify => self.run_verify(project, started_at).await,
            DreamTask::Curate => self.run_curate(project, state).await,
            DreamTask::Classify => self.run_classify(project).await,
            DreamTask::Retrospective => self.run_retrospective(project, state).await,
            DreamTask::SurfaceNotes => self.run_surface_notes(project, state, started_at).await,
        }
    }

    /// One hidden, tool-free completion through the historian's resolved model
    /// and transport (one route to the model, not two).
    async fn complete(&self, system: &str, user_prompt: &str) -> Result<String, TaskFailure> {
        let model = historian_model(&self.config, &self.catalog).map_err(|error| TaskFailure {
            class: FailureClass::NoModels,
            message: error.root_cause().to_string(),
        })?;
        let request = dreamer_request(&model, system, user_prompt);
        // Dreamer has no parent session policy; the request carries no
        // reasoning effort deliberately, leaving the provider's default.
        let text = collect_completion(&self.chat, &request)
            .await
            .map_err(|error| TaskFailure {
                class: FailureClass::ProviderError,
                message: error.root_cause().to_string(),
            })?;
        // A stop with no text is an outage shape, not an answer: classify it
        // apart from a parse failure so a retry can be reasoned about
        // (`provider-output-failure.ts`).
        if text.trim().is_empty() {
            return Err(TaskFailure {
                class: FailureClass::EmptyCompletion,
                message: "the completion returned no text".to_owned(),
            });
        }
        Ok(text)
    }

    // -- verify -------------------------------------------------------------

    /// `verify`: candidates are memories whose referenced files are newer than
    /// the memory's last check, or missing for a never-verified memory; only
    /// then is one completion issued.
    async fn run_verify(&self, project: &str, started_at: i64) -> TaskOutcome {
        let memories = match memory::active_memories_with_watermark(&self.db, project).await {
            Ok(memories) => memories,
            Err(error) => return TaskOutcome::Failed(TaskFailure::host("read memories", &error)),
        };
        let candidates: Vec<Candidate> = memories
            .iter()
            .filter_map(|memory| candidate(project, memory))
            .collect();
        let batch: Vec<Candidate> = candidates.into_iter().take(VERIFY_MEMORY_LIMIT).collect();
        if batch.is_empty() {
            return TaskOutcome::Skipped {
                detail: "no memory cites a file that is missing or newer than it".to_owned(),
            };
        }
        let prompt = render_verify_prompt(project, &batch);
        let text = match self.complete(VERIFY_SYSTEM_PROMPT, &prompt).await {
            Ok(text) => text,
            Err(failure) => return TaskOutcome::Failed(failure),
        };
        let manifest = match parse_verify_manifest(&text, &batch) {
            Ok(manifest) => manifest,
            Err(message) => {
                return TaskOutcome::Failed(TaskFailure {
                    class: FailureClass::ParseFailed,
                    message,
                });
            }
        };
        match apply_verify_manifest(&self.db, project, &batch, &manifest, started_at).await {
            Ok(report) => TaskOutcome::Completed {
                detail: report.describe(),
                cursor: None,
            },
            Err(error) => TaskOutcome::Failed(TaskFailure::host("apply verify manifest", &error)),
        }
    }

    // -- curate -------------------------------------------------------------

    /// `curate`: one category per run, scanning from the persisted cursor for
    /// the first category with at least two memories. A category with a single
    /// memory is passed over (it cannot contain a duplicate pair), so the scan
    /// never wedges, and the cursor advances only past a category that
    /// actually ran (the reference's `curateTaskStateAfterSuccess`).
    async fn run_curate(&self, project: &str, state: &DreamState) -> TaskOutcome {
        let memories = match memory::active_memories_with_watermark(&self.db, project).await {
            Ok(memories) => memories,
            Err(error) => return TaskOutcome::Failed(TaskFailure::host("read memories", &error)),
        };
        let Some(scope) = curate_scope(state.rotation_cursor, &memories) else {
            return TaskOutcome::Skipped {
                detail: "no category holds two or more memories".to_owned(),
            };
        };
        let prompt = render_curate_prompt(project, scope.category, &scope.memories);
        let text = match self.complete(CURATE_SYSTEM_PROMPT, &prompt).await {
            Ok(text) => text,
            Err(failure) => return TaskOutcome::Failed(failure),
        };
        let manifest = match parse_curate_manifest(&text, &scope.memories) {
            Ok(manifest) => manifest,
            Err(message) => {
                return TaskOutcome::Failed(TaskFailure {
                    class: FailureClass::ParseFailed,
                    message,
                });
            }
        };
        match apply_curate_manifest(&self.db, project, &scope, &manifest).await {
            Ok(report) => {
                // Advance the cursor past the category that ran, so the next
                // run covers the next one (the reference's
                // `curateTaskStateAfterSuccess`). The modulo keeps it inside
                // the canonical five however the list grows.
                let len = CATEGORIES.len() as u32;
                let used = CATEGORIES
                    .iter()
                    .position(|category| *category == scope.category)
                    .unwrap_or(0) as u32;
                TaskOutcome::Completed {
                    detail: report.describe(),
                    cursor: Some((used + 1) % len),
                }
            }
            Err(error) => TaskOutcome::Failed(TaskFailure::host("apply curate manifest", &error)),
        }
    }

    // -- classify -----------------------------------------------------------

    /// `classify`: score memories that have never been scored, so the
    /// pinboard's budget walks the pool by weight instead of by recency alone.
    ///
    /// The only write is `importance`, which no render reads: a classification
    /// changes no prompt byte. Score zero memories and the task is skipped
    /// without a model call (`dreamer/task-gates.ts`).
    async fn run_classify(&self, project: &str) -> TaskOutcome {
        let pending = match unclassified_count(&self.db, project).await {
            Ok(pending) => pending,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host(
                    "count unclassified memories",
                    &error,
                ));
            }
        };
        if pending == 0 {
            return TaskOutcome::Skipped {
                detail: "no unclassified memories".to_owned(),
            };
        }
        let batch = match unclassified_batch(&self.db, project).await {
            Ok(batch) => batch,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host(
                    "read unclassified memories",
                    &error,
                ));
            }
        };
        // A row written between the count and the read can leave the batch
        // empty; the gate's answer is still "nothing to do".
        if batch.is_empty() {
            return TaskOutcome::Skipped {
                detail: "no unclassified memories".to_owned(),
            };
        }
        let prompt = render_classify_prompt(project, &batch);
        let text = match self.complete(CLASSIFY_SYSTEM_PROMPT, &prompt).await {
            Ok(text) => text,
            Err(failure) => return TaskOutcome::Failed(failure),
        };
        let manifest = match parse_classify_manifest(&text, &batch) {
            Ok(manifest) => manifest,
            Err(message) => {
                return TaskOutcome::Failed(TaskFailure {
                    class: FailureClass::ParseFailed,
                    message,
                });
            }
        };
        match apply_classify_manifest(&self.db, project, &manifest).await {
            Ok(applied) => TaskOutcome::Completed {
                detail: format!("classified {applied} memories"),
                cursor: None,
            },
            Err(error) => TaskOutcome::Failed(TaskFailure::host("apply classify manifest", &error)),
        }
    }

    // -- retrospective ------------------------------------------------------

    /// `retrospective`: the user's own turns since the last successful sweep,
    /// distilled into at most one durable memory.
    ///
    /// "No recent user messages" is a skip: it is decided before the hidden
    /// completion, so the tick has spent nothing and the slot can be retired
    /// without moving the watermark.
    ///
    /// "Nothing durable" and "already recorded" are completed, not skipped: the
    /// completion has already been paid for, and a skip would leave
    /// `last_run_at` where it was — every later tick would re-read the same
    /// turns and pay for the same answer again. Advancing the watermark is what
    /// makes a sweep a sweep.
    ///
    /// The write goes through [`memory::write_memory`] — the same path
    /// `ctx_memory` uses — so a lesson is an ordinary memory row (and changes
    /// the pinboard exactly once, by design).
    async fn run_retrospective(&self, project: &str, state: &DreamState) -> TaskOutcome {
        let turns = match recent_user_turns(&self.db, project, state.last_run_at.unwrap_or(0)).await
        {
            Ok(turns) => turns,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("read recent user turns", &error));
            }
        };
        if turns.is_empty() {
            return TaskOutcome::Skipped {
                detail: "no recent user messages".to_owned(),
            };
        }
        let prompt = render_retrospective_prompt(project, &turns);
        let text = match self.complete(RETROSPECTIVE_SYSTEM_PROMPT, &prompt).await {
            Ok(text) => text,
            Err(failure) => return TaskOutcome::Failed(failure),
        };
        let lesson = match parse_retrospective(&text) {
            Ok(Some(lesson)) => lesson,
            Ok(None) => {
                return TaskOutcome::Completed {
                    detail: "nothing durable".to_owned(),
                    cursor: None,
                };
            }
            Err(message) => {
                return TaskOutcome::Failed(TaskFailure {
                    class: FailureClass::ParseFailed,
                    message,
                });
            }
        };
        // "Distill, do not transcribe" (upstream
        // dreamer/retrospective-learnings.ts:164-168): a lesson that is really
        // a transcription of these turns is dropped and logged, never written.
        // That is a correct outcome, not a parse failure, so the tick
        // completes — and completes-without-write is also what advances the
        // watermark past a rejected window.
        let source_turns: Vec<&str> = turns.iter().map(String::as_str).collect();
        if let Err(reason) = validate_retrospective_lesson(&lesson.content, &source_turns) {
            tracing::info!(
                target: "mctx::dreamer",
                project = %project,
                "retrospective rejected: {reason:?}"
            );
            return TaskOutcome::Completed {
                detail: format!("rejected: {reason:?}"),
                cursor: None,
            };
        }
        match fact_recorded(&self.db, project, &lesson.content).await {
            Ok(false) => {}
            Ok(true) => {
                return TaskOutcome::Completed {
                    detail: "already recorded".to_owned(),
                    cursor: None,
                };
            }
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("check recorded facts", &error));
            }
        }
        match memory::write_memory(&self.db, project, &lesson.content, lesson.category).await {
            Ok(id) => TaskOutcome::Completed {
                detail: format!("recorded memory #{id}"),
                cursor: None,
            },
            Err(error) => TaskOutcome::Failed(TaskFailure::host("write memory", &error)),
        }
    }

    // -- surface_notes ------------------------------------------------------

    /// `surface_notes`: judge the parked notes' conditions against the user's
    /// own turns from the unswept window, and move the satisfied ones to
    /// `ready`.
    ///
    /// Two cheap gates run before the completion is paid for: no parked note
    /// with a usable condition, and no recent user turns to check against. Both
    /// are skips, because nothing was spent.
    ///
    /// "No condition holds" is `Completed`, not `Skipped` — the completion has
    /// already been paid for, and a skip would freeze `last_run_at`, so every
    /// later tick would re-read the same turns and pay for the same answer
    /// again (the same reasoning as `retrospective`).
    ///
    /// `at` is the run's start, stamped on every surfaced row: the reference's
    /// natural-boundary triggers (after a commit, after the historian, after a
    /// todo completes) have no equivalent here, so the watermark is what moves
    /// a sweep forward.
    async fn run_surface_notes(&self, project: &str, state: &DreamState, at: i64) -> TaskOutcome {
        let parked = match parked_note_count(&self.db, project).await {
            Ok(parked) => parked,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("count parked notes", &error));
            }
        };
        if parked == 0 {
            return TaskOutcome::Skipped {
                detail: "no parked notes with a condition".to_owned(),
            };
        }
        let batch = match parked_notes(&self.db, project).await {
            Ok(batch) => batch,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("read parked notes", &error));
            }
        };
        // A note dismissed between the count and the read can leave the batch
        // empty; the gate's answer is still "nothing to do".
        if batch.is_empty() {
            return TaskOutcome::Skipped {
                detail: "no parked notes with a condition".to_owned(),
            };
        }
        let turns = match recent_user_turns(&self.db, project, state.last_run_at.unwrap_or(0)).await
        {
            Ok(turns) => turns,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("read recent user turns", &error));
            }
        };
        if turns.is_empty() {
            return TaskOutcome::Skipped {
                detail: "no recent activity to check conditions against".to_owned(),
            };
        }
        let prompt = render_surface_prompt(project, &batch, &turns);
        let text = match self.complete(SURFACE_SYSTEM_PROMPT, &prompt).await {
            Ok(text) => text,
            // This is the one task whose contract allows an empty answer ("no
            // condition holds"), so an empty completion is a verdict, not the
            // outage shape the other tasks read it as. Nothing is surfaced
            // either way — the fail-closed direction — but recording a failure
            // would freeze `last_run_at` and re-pay for the same window on
            // every tick.
            Err(failure) if failure.class == FailureClass::EmptyCompletion => String::new(),
            Err(failure) => return TaskOutcome::Failed(failure),
        };
        let batch_ids: Vec<&str> = batch.iter().map(|note| note.id.as_str()).collect();
        let ids = match parse_ready_ids(&text, &batch_ids) {
            Ok(ids) => ids,
            Err(message) => {
                return TaskOutcome::Failed(TaskFailure {
                    class: FailureClass::ParseFailed,
                    message,
                });
            }
        };
        match apply_surfaced(&self.db, &ids, at).await {
            Ok(0) => TaskOutcome::Completed {
                detail: "no conditions satisfied".to_owned(),
                cursor: None,
            },
            Ok(applied) => TaskOutcome::Completed {
                detail: format!("surfaced {applied} notes"),
                cursor: None,
            },
            Err(error) => TaskOutcome::Failed(TaskFailure::host("apply surfaced notes", &error)),
        }
    }
}

/// Whether a task should run at `now`.
///
/// Due means `now - last_run_at >= interval`, with `last_run_at` the start of
/// the last *successful* run (so work landing during a run is not skipped).
/// A row that has failed [`MAX_DREAM_FAILURES`] times in a row is not
/// retried until [`failure_backoff_millis`] has elapsed *since the failed
/// attempt*: the first retries are hot (next tick), then the wait doubles per
/// further failure up to [`MAX_DREAM_BACKOFF_MILLIS`]. The scheduler ticks once
/// per interval, so a flat one-interval wait would never throttle anything.
fn is_due(state: &DreamState, now: i64, interval_millis: i64) -> bool {
    if let Some(last_run_at) = state.last_run_at
        && now - last_run_at < interval_millis
    {
        return false;
    }
    if state.consecutive_failures >= MAX_DREAM_FAILURES
        && now - state.last_attempt_at
            < failure_backoff_millis(state.consecutive_failures, interval_millis)
    {
        return false;
    }
    true
}

/// How long a row with `consecutive_failures` failures in a row waits after its
/// last attempt: zero below [`MAX_DREAM_FAILURES`], then
/// `interval * 2^(failures - MAX_DREAM_FAILURES + 1)`, saturating and capped at
/// [`MAX_DREAM_BACKOFF_MILLIS`]. A non-positive interval yields no wait.
fn failure_backoff_millis(consecutive_failures: u32, interval_millis: i64) -> i64 {
    if consecutive_failures < MAX_DREAM_FAILURES {
        return 0;
    }
    let exponent = (consecutive_failures - MAX_DREAM_FAILURES + 1).min(20);
    interval_millis
        .max(0)
        .saturating_mul(1i64 << exponent)
        .min(MAX_DREAM_BACKOFF_MILLIS)
}

// ---------------------------------------------------------------------------
// Schedule state (SQL)
// ---------------------------------------------------------------------------

/// The state row for `(project, task)`, or defaults when it has none.
async fn load_state(db: &Db, project: &str, task: DreamTask) -> Result<DreamState> {
    let context = format!("load dream state for {project}");
    let task = task.as_str();
    let state: Result<DreamState> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT last_run_at, last_attempt_at, last_status, last_error,
                        consecutive_failures, rotation_cursor
                   FROM mc_dream_state WHERE project = ?1 AND task = ?2",
            )
            .await?;
        let Some(row) = ox_db::optional(&mut stmt, (project, task)).await? else {
            return Ok(DreamState::default());
        };
        Ok(DreamState {
            last_run_at: row.get::<Option<i64>>(0)?,
            last_attempt_at: row.get::<i64>(1)?,
            last_status: row.get::<Option<String>>(2)?,
            last_error: row.get::<Option<String>>(3)?,
            consecutive_failures: row.get::<i64>(4)?.max(0) as u32,
            rotation_cursor: row.get::<i64>(5)?.max(0) as u32,
        })
    }
    .await;
    state.with_context(|| context)
}

/// Record a successful run. `last_run_at` is stamped with `started_at` — the
/// run's start, not its end (the reference's `advanceAfterRun`): anything that
/// landed while the task ran is newer than this cutoff, so the gate sees it
/// next slot instead of never.
async fn settle_success(
    db: &Db,
    project: &str,
    task: DreamTask,
    started_at: i64,
    cursor: Option<u32>,
) -> Result<()> {
    let context = format!("record dream success for {project}");
    let cursor = cursor.map(i64::from);
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO mc_dream_state
                 (project, task, last_run_at, last_attempt_at, last_status, last_error,
                  consecutive_failures, rotation_cursor)
             VALUES (?1, ?2, ?3, ?3, 'completed', NULL, 0, COALESCE(?4, 0))
             ON CONFLICT (project, task) DO UPDATE SET
                 last_run_at = excluded.last_run_at,
                 last_attempt_at = excluded.last_attempt_at,
                 last_status = 'completed',
                 last_error = NULL,
                 consecutive_failures = 0,
                 rotation_cursor = COALESCE(?4, mc_dream_state.rotation_cursor)",
        (project, task.as_str(), started_at, cursor),
    )
    .await
    .with_context(|| context)?;
    Ok(())
}

/// Record a due task that found no work. `last_run_at` deliberately does NOT
/// move — a skip is not a successful pass over the pool — and `last_error`
/// stays NULL because nothing failed; the gate's reason is logged, not stored
/// (the reference's `advanceAfterRun(…, "skipped", null)`).
async fn settle_skip(db: &Db, project: &str, task: DreamTask, now: i64) -> Result<()> {
    let context = format!("record dream skip for {project}");
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO mc_dream_state
                 (project, task, last_attempt_at, last_status, last_error,
                  consecutive_failures)
             VALUES (?1, ?2, ?3, 'skipped', NULL, 0)
             ON CONFLICT (project, task) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 last_status = 'skipped',
                 last_error = NULL,
                 consecutive_failures = 0",
        (project, task.as_str(), now),
    )
    .await
    .with_context(|| context)?;
    Ok(())
}

/// Record a failed run: the attempt is stamped, the failure count grows, and
/// `last_run_at` stays where the last success left it.
async fn settle_failure(
    db: &Db,
    project: &str,
    task: DreamTask,
    now: i64,
    failure: &TaskFailure,
) -> Result<()> {
    let context = format!("record dream failure for {project}");
    let error = failure.recorded();
    let conn = db.acquire().await?;
    conn.execute(
        "INSERT INTO mc_dream_state
                 (project, task, last_attempt_at, last_status, last_error,
                  consecutive_failures)
             VALUES (?1, ?2, ?3, 'failed', ?4, 1)
             ON CONFLICT (project, task) DO UPDATE SET
                 last_attempt_at = excluded.last_attempt_at,
                 last_status = 'failed',
                 last_error = excluded.last_error,
                 consecutive_failures = mc_dream_state.consecutive_failures + 1",
        (project, task.as_str(), now, error.as_str()),
    )
    .await
    .with_context(|| context)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// verify: the host-side gate
// ---------------------------------------------------------------------------

/// One memory that needs a re-check (a cited file is newer, or missing for a
/// never-verified memory), with every path its text named and what the host
/// found there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    id: String,
    category: String,
    content: String,
    /// When the memory was last written; the verifier sees it so it can judge
    /// "the file is newer than the claim".
    updated_at: i64,
    paths: Vec<PathState>,
}

/// What the filesystem says about one referenced path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathState {
    /// The path as resolved for this project (absolute), or `None` when the
    /// written path leaves the project — those are never read.
    path: Option<PathBuf>,
    /// The path as the memory wrote it, for the prompt.
    written: String,
    /// False when the file does not exist (the reference's "was deleted"), or
    /// when the path is outside the project. A missing path only makes a
    /// never-verified memory a candidate (see [`needs_check`]).
    exists: bool,
    /// The file's mtime in epoch milliseconds, when readable.
    mtime: Option<i64>,
    /// `mtime > verified_at`: the file changed after the memory was last
    /// checked against it (a never-verified memory has `verified_at = 0`, so
    /// every existing file counts as newer and it is checked once).
    newer: bool,
}

/// The verify gate: `Some` when this memory must be re-checked, `None` when it
/// is free to leave alone. A memory that names no path is never a candidate
/// (the reference's "file-independent" exclusion); [`needs_check`] decides the
/// rest from what the filesystem said.
fn candidate(project_dir: &str, memory: &memory::StoredMemory) -> Option<Candidate> {
    let paths: Vec<PathState> = referenced_paths(&memory.content)
        .into_iter()
        .map(|written| inspect(project_dir, &written, memory.verified_at))
        .collect();
    if !needs_check(&paths, memory.verified_at) {
        return None;
    }
    Some(Candidate {
        id: memory.id.clone(),
        category: memory.category.clone(),
        content: memory.content.clone(),
        updated_at: memory.updated_at,
        paths,
    })
}

/// Whether the inspected paths make a memory worth a verify completion.
///
/// A stamped memory (`verified_at > 0`) has already been judged against its
/// missing file, so re-sending it can never change the outcome, and the
/// deletion time is not recorded. Deviation: a file deleted AFTER stamping is
/// not re-checked until the memory is rewritten (which resets `verified_at` to
/// 0 in memory.rs) or another cited file changes.
fn needs_check(paths: &[PathState], verified_at: i64) -> bool {
    paths
        .iter()
        .any(|state| (state.exists && state.newer) || (!state.exists && verified_at == 0))
}

/// Resolve one written path against the project directory and read the
/// filesystem's answer. A path that cannot be stat'ed counts as missing — the
/// safe direction, because the model is then asked and may say "still fine".
fn inspect(project_dir: &str, written: &str, verified_at: i64) -> PathState {
    let path = resolve_path(project_dir, written);
    let (exists, mtime) = match path.as_deref().map(std::fs::metadata) {
        Some(Ok(meta)) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_millis() as i64);
            (true, mtime)
        }
        _ => (false, None),
    };
    PathState {
        exists: path.is_some() && exists,
        path,
        written: written.to_owned(),
        mtime,
        // Strictly newer: a file whose mtime equals the watermark is not a
        // change. The watermark is the run's START, so anything written during
        // the verification stays newer and is picked up next time.
        newer: mtime.is_some_and(|mtime| mtime > verified_at),
    }
}

/// The absolute path a memory's written path refers to, if it stays inside the
/// project.
///
/// Confined to the project on purpose: the verify prompt prints file contents
/// to a model, and a memory is model-writable, so `~/.ssh/id_rsa.pub`, a
/// `../../etc/passwd` climb, or a symlink pointing out of the tree must not
/// turn the verify task into a file-disclosure primitive. `None` means "outside
/// the project — never read".
fn resolve_path(project_dir: &str, written: &str) -> Option<PathBuf> {
    if written.starts_with('~') {
        return None;
    }
    let root = normalize_lexically(Path::new(project_dir))?;
    let joined = normalize_lexically(&root.join(written))?;
    if !joined.starts_with(&root) {
        return None;
    }
    // A symlink inside the tree can still point out of it, so the real path
    // decides. A path that does not exist has no real path; it stays a
    // "missing" state, which never reads anything.
    match std::fs::canonicalize(&joined) {
        Ok(real) => {
            let real_root = std::fs::canonicalize(&root).unwrap_or(root);
            real.starts_with(&real_root).then_some(real)
        }
        Err(_) => Some(joined),
    }
}

/// Resolve `.` and `..` without touching the filesystem. `None` when `..`
/// climbs above the root, which no valid project path does.
fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// The file paths a memory's text names.
///
/// The reference learns these with `map-memories` (a model call per memory);
/// the MVP reads what the text already says. Two shapes are recognized: a
/// slash path that ends in an extension (`src/api.rs`, `~/.ox/ox.toml`,
/// `/abs/path`) and a bare filename with a known extension (`Cargo.toml`).
/// Both then lose a `:line[:col]` suffix and trailing punctuation. A false
/// positive costs one extra memory in one batch, never a write, so the
/// recognizer errs toward finding paths.
fn referenced_paths(content: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?:~|\.\.?|[\w._\-]+)?/[\w._\-/@]+\.[A-Za-z0-9]{1,8}|\b[\w._\-]+\.(?:rs|toml|lock|md|json|yaml|yml|ts|tsx|js|jsx|sql|go|py|sh|cfg|ini|txt|proto|graphql)",
        )
        .expect("memory path regex")
    });
    let mut seen: Vec<String> = Vec::new();
    for found in re.find_iter(content) {
        let path = found.as_str().trim_end_matches(|c: char| {
            // Trailing punctuation of prose, not part of an extension-bearing
            // path. `:` also ends a `path:line` reference.
            matches!(
                c,
                '.' | ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\'' | '`'
            )
        });
        // `./src/x.rs` and `../src/x.rs` are spelled relative to the project
        // root's parent; the leading dot segments are noise for the store.
        let path = path.strip_prefix("./").unwrap_or(path);
        if path.is_empty() || seen.iter().any(|known| known == path) {
            continue;
        }
        seen.push(path.to_owned());
    }
    seen
}

/// Read at most `chars` characters' worth of bytes (UTF-8 is at most 4 bytes
/// per character) and report whether the file had more. One byte past the
/// bound is read to tell "exactly the bound" from "more".
fn read_head(path: &Path, chars: usize) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read as _;
    let cap = chars.saturating_mul(4);
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    let cut = bytes.len() > cap;
    bytes.truncate(cap);
    Ok((bytes, cut))
}

/// The `verify` user prompt: the candidate memories with their path states,
/// then the excerpts of the files that exist.
fn render_verify_prompt(project: &str, batch: &[Candidate]) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "## Project: {project}\n\nThe memories below cite files that are missing or newer than the memory. The host read each existing file and printed an excerpt.\n\n<memories>"
    ));
    for candidate in batch {
        parts.push(format!(
            "<memory id=\"{}\" category=\"{}\" updated_at=\"{}\">",
            candidate.id, candidate.category, candidate.updated_at
        ));
        for state in &candidate.paths {
            let verdict = if !state.exists {
                "missing"
            } else if state.newer {
                "newer"
            } else {
                "unchanged"
            };
            parts.push(format!(
                "  <path state=\"{verdict}\">{}</path>",
                state.written
            ));
        }
        parts.push(candidate.content.clone());
        parts.push("</memory>".to_owned());
    }
    parts.push("</memories>".to_owned());

    // One excerpt per unique file, in first-seen order, inside the total
    // budget: a memory citing an unchanged sibling file must not pay for it
    // twice.
    let mut files: Vec<String> = Vec::new();
    let mut seen: Vec<&Path> = Vec::new();
    let mut budget = VERIFY_EXCERPT_TOTAL_CHARS;
    for candidate in batch {
        for state in &candidate.paths {
            let Some(path) = state.path.as_deref() else {
                continue;
            };
            if !state.exists || seen.contains(&path) {
                continue;
            }
            seen.push(path);
            if budget == 0 {
                continue;
            }
            match read_head(path, budget) {
                Ok((bytes, cut)) => {
                    // Bounded before decoding: a cited multi-gigabyte file must
                    // not be read into memory just to keep 4k characters.
                    let text = String::from_utf8_lossy(&bytes);
                    // Char-counted, never byte-indexed: lossy replacement
                    // characters are multi-byte, so a byte slice can split one
                    // and panic.
                    let mut excerpt: String = text
                        .chars()
                        .take(VERIFY_EXCERPT_CHARS.min(budget))
                        .collect();
                    let taken = excerpt.chars().count();
                    if cut || taken < text.chars().count() {
                        excerpt.push_str("\n… (truncated)");
                    }
                    budget = budget.saturating_sub(taken);
                    files.push(format!(
                        "<file path=\"{}\">\n{excerpt}\n</file>",
                        path.display()
                    ));
                }
                Err(error) => {
                    files.push(format!(
                        "<file path=\"{}\">\n(unreadable: {error})\n</file>",
                        path.display()
                    ));
                }
            }
        }
    }
    parts.push("<files>".to_owned());
    parts.extend(files);
    parts.push("</files>".to_owned());
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// verify: the manifest
// ---------------------------------------------------------------------------

/// The parsed `<verify>` manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VerifyManifest {
    verified: Vec<String>,
    updates: Vec<VerifyUpdate>,
    archives: Vec<VerifyArchive>,
    /// `<skip>` ids ("code cannot decide it"): counted toward coverage and
    /// duplicate checks, and stamped like `verified` so an undecidable memory
    /// leaves the candidate set instead of holding the batch forever.
    skipped: Vec<String>,
}

/// One `<update>`: the replacement text and the model's consolidation flag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyUpdate {
    id: String,
    content: String,
    consolidation: bool,
}

/// One `<archive>`: the id and the evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyArchive {
    id: String,
    reason: String,
}

/// Parse a `<verify>` manifest, failing CLOSED: no complete root element means
/// the output is truncated or the wrong task's, and nothing is applied (never
/// a prefix). A well-formed but empty manifest is a format miss, a duplicate
/// id is a rejected manifest (across verified/update/archive/skip), ids
/// outside the batch are dropped, and a
/// manifest covering fewer than half the batch is rejected as mostly-wrong —
/// all four are the reference's rules (`manifest-parser.ts`, `verify.ts`).
fn parse_verify_manifest(text: &str, batch: &[Candidate]) -> Result<VerifyManifest, String> {
    let body = complete_manifest_body(text, "verify")?;
    let mut manifest = VerifyManifest::default();
    for captures in update_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <update> is missing an id")?;
        manifest.updates.push(VerifyUpdate {
            id,
            content: captures.get(2).map_or("", |m| m.as_str()).trim().to_owned(),
            consolidation: attr_of(attrs, "consolidation")
                .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        });
    }
    for captures in archive_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <archive> is missing an id")?;
        manifest.archives.push(VerifyArchive {
            id,
            reason: attr_of(attrs, "reason").unwrap_or_default(),
        });
    }
    for captures in verified_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <verified> is missing an id")?;
        manifest.verified.push(id);
    }
    for captures in skip_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("verify manifest <skip> is missing an id")?;
        manifest.skipped.push(id);
    }

    let batch_ids: Vec<&str> = batch.iter().map(|memory| memory.id.as_str()).collect();
    let mut known: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut error: Option<String> = None;
    manifest
        .verified
        .retain(|id| keep_batch_id(id, &batch_ids, &mut known, &mut unknown, &mut error));
    manifest
        .skipped
        .retain(|id| keep_batch_id(id, &batch_ids, &mut known, &mut unknown, &mut error));
    manifest.updates.retain(|update| {
        keep_batch_id(&update.id, &batch_ids, &mut known, &mut unknown, &mut error)
    });
    manifest.archives.retain(|archive| {
        keep_batch_id(
            &archive.id,
            &batch_ids,
            &mut known,
            &mut unknown,
            &mut error,
        )
    });
    if let Some(error) = error {
        return Err(error);
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "verify manifest named memories outside the batch; dropped"
        );
    }
    if known.is_empty() {
        return Err(format!(
            "verify manifest carried no verdicts for the {} memories of the batch",
            batch_ids.len()
        ));
    }
    // Fewer than half the batch is the shape of a manifest answered for some
    // other prompt; the prompt asks for every id (skip is how "leave it
    // alone" is said), so a real answer covers at least half.
    if known.len() * 2 < batch_ids.len() {
        return Err(format!(
            "verify manifest covers {}/{} batch ids; rejecting a mostly-missing manifest",
            known.len(),
            batch_ids.len()
        ));
    }
    Ok(manifest)
}

/// Keep an id only when it belongs to the batch, rejecting a repeat — the
/// reference's `assertNoDuplicateManifestIds`. Unknown ids are dropped and
/// counted (the reference logs and drops them); a duplicate makes the whole
/// manifest inconsistent, so it is recorded in `error` and the caller turns it
/// into a `parse_failed`.
fn keep_batch_id(
    id: &str,
    batch_ids: &[&str],
    known: &mut Vec<String>,
    unknown: &mut usize,
    error: &mut Option<String>,
) -> bool {
    if !batch_ids.contains(&id) {
        *unknown += 1;
        return false;
    }
    if known.iter().any(|seen| seen == id) {
        *error = Some(format!("manifest contains duplicate id {id}"));
        return false;
    }
    known.push(id.to_owned());
    true
}

/// Apply a parsed `<verify>` manifest through the store, with the safety rules
/// of `dreamer/memory-claim-safety.ts` and `verify.ts` in front of every write:
///
/// - a directive-shaped `PROJECT_RULES` memory refuses BOTH update and archive;
/// - an update is refused when it loses more than half the text unless the
///   model declared it a consolidation;
/// - an empty (or absurdly large) update is not a wipe: the text is left alone
///   and the verdict degrades to a plain re-verify (`verified_at` banked), so
///   the same unsafe rewrite is not re-asked every slot.
///
/// Refusals are logged and counted, never fatal.
async fn apply_verify_manifest(
    db: &Db,
    project: &str,
    batch: &[Candidate],
    manifest: &VerifyManifest,
    started_at: i64,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    // A `verified` or `skip` verdict stamps `verified_at` with the run's START,
    // so an unchanged pool is not re-checked on the next slot while anything
    // written during this run stays newer and is picked up then. Memories the
    // manifest omitted keep their old watermark and stay candidates
    // ("omissions keep their old verified_at, the next run picks them up").
    let bank = |ids: &[String]| -> Vec<String> {
        ids.iter()
            .filter(|id| batch.iter().any(|memory| &memory.id == *id))
            // A memory the same manifest also rewrites or archives has a
            // competing verdict; the write wins and banking it first would be
            // noise.
            .filter(|id| {
                !manifest.updates.iter().any(|update| &update.id == *id)
                    && !manifest.archives.iter().any(|archive| &archive.id == *id)
            })
            .cloned()
            .collect()
    };
    let banked = bank(&manifest.verified);
    if !banked.is_empty() {
        report.verified = memory::touch_memories(db, project, &banked, started_at).await?;
    }
    let skip_banked = bank(&manifest.skipped);
    if !skip_banked.is_empty() {
        report.skipped = memory::touch_memories(db, project, &skip_banked, started_at).await?;
    }
    for update in &manifest.updates {
        let Some(original) = batch.iter().find(|memory| memory.id == update.id) else {
            continue;
        };
        if directive_shaped_project_rule(&original.category, &original.content) {
            tracing::warn!(
                memory_id = %update.id,
                verdict = "update",
                reason = "directive-shaped-project-rule",
                "verify safety refusal"
            );
            report.refused += 1;
            continue;
        }
        let content = update.content.trim();
        if content.is_empty() || content.chars().count() > 20_000 {
            // An empty or oversized rewrite is unsafe, but the memory was judged
            // and not contradicted: bank a plain re-verify (text untouched)
            // instead of dropping the verdict, which would re-ask the same
            // unsafe rewrite on every slot.
            tracing::warn!(
                memory_id = %update.id,
                verdict = "update",
                reason = "unsafe-length",
                "verify update refused; banking a re-verify"
            );
            report.verified +=
                memory::touch_memories(db, project, std::slice::from_ref(&update.id), started_at)
                    .await?;
            continue;
        }
        let original_chars = original.content.trim().chars().count();
        if !update.consolidation && content.chars().count() * 2 < original_chars {
            tracing::warn!(
                memory_id = %update.id,
                verdict = "update",
                reason = "content-loss",
                original_chars,
                replacement_chars = content.chars().count(),
                "verify safety refusal"
            );
            report.refused += 1;
            continue;
        }
        // The category is left alone: verify judges truth, not taxonomy.
        if memory::update_memory(db, project, &update.id, content, None).await? {
            report.updated += 1;
        }
    }
    for archive in &manifest.archives {
        let Some(original) = batch.iter().find(|memory| memory.id == archive.id) else {
            continue;
        };
        if directive_shaped_project_rule(&original.category, &original.content) {
            tracing::warn!(
                memory_id = %archive.id,
                verdict = "archive",
                reason = "directive-shaped-project-rule",
                "verify safety refusal"
            );
            report.refused += 1;
            continue;
        }
        let reason = if archive.reason.trim().is_empty() {
            "verify: contradicted by code"
        } else {
            archive.reason.trim()
        };
        if !memory::archive_memories(db, project, std::slice::from_ref(&archive.id), Some(reason))
            .await?
            .is_empty()
        {
            report.archived += 1;
        }
    }
    Ok(report)
}

/// The `verify`/`curate` safety predicate, ported from
/// `dreamer/memory-claim-safety.ts` (all five patterns, one combined regex):
/// only explicit workflow language inside `PROJECT_RULES` counts as a
/// directive. Declarative implementation claims such as "binds use spread
/// args" deliberately stay outside the gate and remain verifiable.
fn directive_shaped_project_rule(category: &str, content: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    if category != CATEGORIES[0] {
        return false;
    }
    let text = content.trim();
    if text.is_empty() {
        return false;
    }
    let re = RE.get_or_init(|| {
        // POLICY_SENTENCE_START alternation 3 (`do not`) and WORKFLOW_IMPERATIVE
        // are both case-sensitive in the reference; the rest are insensitive.
        // ox compiles one case-insensitive regex and spells the two remaining
        // branches in lower case, which is a strict superset of the gate.
        Regex::new(
            r"(?i)(?:^|[.!?]\s+|\n\s*)(?:[-*]\s+|\d+[.)]\s+)?(?:you\s+|we\s+)?(?:must(?:\s+not)?|never|always|do\s+not|don't|shall\s+not)\b|\b(?:you|we|agents?|masons?|workers?|operators?|users?|maintainers?)\s+(?:must(?:\s+not)?|should(?:\s+not)?|need\s+to|shall(?:\s+not)?|cannot|can't|may\s+not)\b|\bwhen\s+(?:you(?:'re|\s+are)?|we(?:'re|\s+are)?|told|asked|requested|working|debugging|reviewing|checking|verifying|investigating|using|running|editing|changing)\b|(?:^|[.!?]\s+|\n\s*)(?:[-*]\s+|\d+[.)]\s+)?(?:please\s+)?(?:run|use|check|ask|avoid|prefer|ensure|keep|follow|brief|report|inspect|search|open|read|review|validate|confirm|delegate|stop|start|remember)\b|\b(?:the\s+)?(?:user|operator|maintainer|owner)\s+(?:decides|chooses|approves|has\s+(?:the\s+)?final\s+say)\b",
        )
        .expect("directive-shaped project rule regex")
    });
    re.is_match(text)
}

// ---------------------------------------------------------------------------
// curate
// ---------------------------------------------------------------------------

/// The category one curate run covers, with the memories in it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurateScope {
    category: &'static str,
    memories: Vec<memory::StoredMemory>,
}

/// Pick the category for this run: scan the five canonical categories from the
/// persisted cursor, wrapping, and take the first one holding at least two
/// memories. Returns `None` when no category has a pair to work with — the
/// cheap gate that keeps an idle project free of model calls.
fn curate_scope(cursor: u32, memories: &[memory::StoredMemory]) -> Option<CurateScope> {
    let len = CATEGORIES.len() as u32;
    let start = cursor % len;
    for offset in 0..len {
        let category = CATEGORIES[((start + offset) % len) as usize];
        let scoped: Vec<memory::StoredMemory> = memories
            .iter()
            .filter(|memory| memory.category == category)
            .cloned()
            .collect();
        if scoped.len() >= 2 {
            return Some(CurateScope {
                category,
                memories: scoped,
            });
        }
    }
    None
}

/// The `curate` user prompt: the scoped category's snapshot. The model mutates
/// nothing directly; it answers with a manifest the host applies.
fn render_curate_prompt(
    project: &str,
    category: &str,
    memories: &[memory::StoredMemory],
) -> String {
    let mut parts = vec![format!(
        "## Project: {project}\n\nThis run covers the `{category}` category only. The other categories run in later windows.\n\nThe memories below are assumed ACCURATE (a separate verify task keeps them true). Remove duplicates, tighten wording, consolidate redundancy, and archive only what another memory in this category already restates. The memories:\n"
    )];
    for memory in memories {
        parts.push(format!("#{}: {}", memory.id, memory.content));
    }
    parts.join("\n")
}

/// The parsed `<curate>` manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CurateManifest {
    merges: Vec<CurateMerge>,
    archives: Vec<CurateArchive>,
}

/// One `<merge>`: the ids to collapse, the consolidated text, and the category
/// the model claims they share.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurateMerge {
    ids: Vec<String>,
    category: Option<String>,
    content: String,
}

/// One `<archive>`: the id and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurateArchive {
    id: String,
    reason: String,
}

/// Parse a `<curate>` manifest with the same fail-closed rules as `verify`.
fn parse_curate_manifest(
    text: &str,
    scope: &[memory::StoredMemory],
) -> Result<CurateManifest, String> {
    let body = complete_manifest_body(text, "curate")?;
    let mut manifest = CurateManifest::default();
    for captures in merge_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let raw_ids = attr_of(attrs, "ids").ok_or("curate manifest <merge> is missing ids")?;
        let mut ids: Vec<String> = Vec::new();
        for id in raw_ids.split(',') {
            let id = id.trim().to_owned();
            if id.is_empty() {
                continue;
            }
            // A duplicated id inside one merge is the reference's
            // `ctx_memory` dedupe: nothing may be merged into itself.
            if ids.contains(&id) {
                return Err(format!("curate manifest <merge> lists id {id} twice"));
            }
            ids.push(id);
        }
        if ids.len() < 2 {
            return Err("curate manifest <merge> needs at least two ids".to_owned());
        }
        manifest.merges.push(CurateMerge {
            ids,
            category: attr_of(attrs, "category"),
            content: captures.get(2).map_or("", |m| m.as_str()).trim().to_owned(),
        });
    }
    for captures in archive_regex().captures_iter(&body) {
        let attrs = captures.get(1).map_or("", |m| m.as_str());
        let id = attr_of(attrs, "id").ok_or("curate manifest <archive> is missing an id")?;
        manifest.archives.push(CurateArchive {
            id,
            reason: attr_of(attrs, "reason").unwrap_or_default(),
        });
    }

    let scope_ids: Vec<&str> = scope.iter().map(|memory| memory.id.as_str()).collect();
    // A merge is all-or-nothing: one id outside the scope drops the whole
    // merge (the reference refuses the whole mutation, and applying half a
    // merge would archive nothing while rewriting the survivor from a group
    // that never existed). Archives are independent single-id verdicts.
    let mut known: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut merges: Vec<CurateMerge> = Vec::new();
    for merge in std::mem::take(&mut manifest.merges) {
        let outside = merge
            .ids
            .iter()
            .filter(|id| !scope_ids.contains(&id.as_str()))
            .count();
        if outside > 0 {
            unknown += outside;
            continue;
        }
        if let Some(duplicate) = merge.ids.iter().find(|id| known.contains(id)) {
            return Err(format!(
                "curate manifest lists id {duplicate} in more than one element"
            ));
        }
        known.extend(merge.ids.iter().cloned());
        merges.push(merge);
    }
    manifest.merges = merges;
    let mut error = None;
    manifest.archives.retain(|archive| {
        keep_batch_id(
            &archive.id,
            &scope_ids,
            &mut known,
            &mut unknown,
            &mut error,
        )
    });
    if let Some(error) = error {
        return Err(error);
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "curate manifest named memories outside the scoped category; dropped"
        );
    }
    Ok(manifest)
}

/// Apply a parsed `<curate>` manifest. Only one thing can be "wrong" enough to
/// refuse here: a merge or archive whose ids (or whose declared category) fall
/// outside the run's category. The reference refuses that in
/// `getCurateCategoryScopeRefusal` before any write; ox refuses it here, per
/// verdict, so one bad entry cannot discard the good ones.
async fn apply_curate_manifest(
    db: &Db,
    project: &str,
    scope: &CurateScope,
    manifest: &CurateManifest,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    for merge in &manifest.merges {
        if merge
            .category
            .as_deref()
            .is_some_and(|c| c != scope.category)
        {
            tracing::warn!(
                scope = scope.category,
                requested = merge.category.as_deref().unwrap_or(""),
                "curate cross-category merge refusal"
            );
            report.refused += 1;
            continue;
        }
        let in_scope = merge
            .ids
            .iter()
            .all(|id| scope.memories.iter().any(|memory| &memory.id == id));
        if !in_scope {
            tracing::warn!(
                scope = scope.category,
                "curate merge outside the scoped category refusal"
            );
            report.refused += 1;
            continue;
        }
        let content = merge.content.trim();
        if content.is_empty() || content.chars().count() > 20_000 {
            tracing::warn!("curate merge with an unsafe content length refusal");
            report.refused += 1;
            continue;
        }
        // A merge rewrites the survivor's text, so it carries the same risk as
        // an update: consolidating a directive-shaped rule into prose would
        // launder a directive into a description.
        if merge.ids.iter().any(|id| {
            scope.memories.iter().any(|memory| {
                &memory.id == id && directive_shaped_project_rule(&memory.category, &memory.content)
            })
        }) {
            tracing::warn!(
                scope = scope.category,
                reason = "directive-shaped-project-rule",
                "curate safety refusal"
            );
            report.refused += 1;
            continue;
        }
        if memory::merge_memories(db, project, &merge.ids, content, None)
            .await?
            .is_some()
        {
            report.merged += 1;
        }
    }
    for archive in &manifest.archives {
        let Some(memory) = scope.memories.iter().find(|memory| memory.id == archive.id) else {
            // An archive outside the scope is the same refusal as a merge
            // outside it; the id was already dropped by the parser when it was
            // unknown, so reaching here means the caller passed a wider scope.
            tracing::warn!(
                scope = scope.category,
                "curate archive outside the scoped category refusal"
            );
            report.refused += 1;
            continue;
        };
        if directive_shaped_project_rule(&memory.category, &memory.content) {
            tracing::warn!(
                memory_id = %archive.id,
                reason = "directive-shaped-project-rule",
                "curate safety refusal"
            );
            report.refused += 1;
            continue;
        }
        let reason = if archive.reason.trim().is_empty() {
            "curate: redundant"
        } else {
            archive.reason.trim()
        };
        if !memory::archive_memories(db, project, std::slice::from_ref(&archive.id), Some(reason))
            .await?
            .is_empty()
        {
            report.archived += 1;
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Shared prompt helpers
// ---------------------------------------------------------------------------

/// The first `max` characters of `text`, cut on a character boundary.
///
/// Borrowed, never copied: a prompt is built once per run from rows the caller
/// already owns. A byte slice would panic on a multi-byte character, which
/// model-written memory text is full of.
fn head_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

// ---------------------------------------------------------------------------
// classify: the host-side gate
// ---------------------------------------------------------------------------

/// One unclassified memory as the classify task reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassifyItem {
    id: String,
    category: String,
    content: String,
}

/// How many active memories of this project have no score yet. The cheap gate:
/// a fully classified pool costs one `count(*)` per tick and no model call.
async fn unclassified_count(db: &Db, project: &str) -> Result<i64> {
    let context = format!("count unclassified memories for project {project}");
    let result: Result<i64> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT count(*) FROM mc_memory
                  WHERE project = ?1 AND status = 'active' AND importance = 0",
            )
            .await?;
        let row = stmt.query_row([project]).await?;
        Ok(row.get(0)?)
    }
    .await;
    result.with_context(|| context)
}

/// The memories one `classify` completion covers: the lowest ids first, so a
/// pool larger than the batch is scored in stable, resumable slices.
async fn unclassified_batch(db: &Db, project: &str) -> Result<Vec<ClassifyItem>> {
    let context = format!("read unclassified memories for project {project}");
    let result: Result<Vec<ClassifyItem>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, category, content FROM mc_memory
                  WHERE project = ?1 AND status = 'active' AND importance = 0
                  ORDER BY CAST(id AS INTEGER)
                  LIMIT ?2",
            )
            .await?;
        let mut rows = stmt.query((project, CLASSIFY_BATCH as i64)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(ClassifyItem {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// The `classify` user prompt: the batch, one handle per memory.
fn render_classify_prompt(project: &str, batch: &[ClassifyItem]) -> String {
    let mut parts = vec![format!(
        "## Project: {project}\n\nThese memories have no importance score yet. Score every one of them.\n"
    )];
    for item in batch {
        parts.push(format!(
            "#{} [{}]\n{}",
            item.id,
            item.category,
            head_chars(&item.content, CLASSIFY_CONTENT_CHARS)
        ));
    }
    parts.join("\n")
}

/// Parse the `{"<id>": <number>}` manifest.
///
/// Strict where it matters and forgiving where it costs nothing: prose around
/// the object is tolerated (the first `{` to the last `}` is the manifest), but
/// every id of the batch must be scored — a partial answer is the shape of a
/// truncated response, and applying it would silently leave the rest at `0`
/// while banking a success. Ids outside the batch are dropped with a warning
/// (the reference drops unknown manifest ids the same way).
fn parse_classify_manifest(
    text: &str,
    batch: &[ClassifyItem],
) -> Result<BTreeMap<String, f64>, String> {
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err("classify manifest carried no JSON object".to_owned());
    };
    if end < start {
        return Err("classify manifest carried no complete JSON object".to_owned());
    }
    let raw: BTreeMap<String, f64> = serde_json::from_str(&text[start..=end]).map_err(|error| {
        format!("classify manifest is not a JSON object of ids to numbers: {error}")
    })?;
    let mut manifest: BTreeMap<String, f64> = BTreeMap::new();
    let mut unknown = 0usize;
    for (id, score) in raw {
        if !batch.iter().any(|item| item.id == id) {
            unknown += 1;
            continue;
        }
        manifest.insert(id, clamp_importance(score));
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "classify manifest named memories outside the batch; dropped"
        );
    }
    for item in batch {
        if !manifest.contains_key(&item.id) {
            return Err(format!(
                "classify manifest is missing memory #{}; rejecting a partial manifest",
                item.id
            ));
        }
    }
    Ok(manifest)
}

/// A model score forced into the stored range. A non-finite value is a broken
/// answer, and `0` (unscored) is the safe reading: it leaves the row for the
/// next run instead of pinning it to the top of the selection.
fn clamp_importance(score: f64) -> f64 {
    if score.is_finite() {
        score.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// Apply a parsed manifest: one transaction, one `importance` write per id,
/// each guarded by `active` so an archived row cannot be scored through a
/// stale batch.
async fn apply_classify_manifest(
    db: &Db,
    project: &str,
    manifest: &BTreeMap<String, f64>,
) -> Result<usize> {
    let context = format!("apply classify manifest for project {project}");
    let result: Result<usize> = async {
        let tx = db.begin().await?;
        let mut applied = 0;
        {
            let mut stmt = tx
                .prepare(
                    "UPDATE mc_memory SET importance = ?2
                      WHERE project = ?3 AND id = ?1 AND status = 'active'",
                )
                .await?;
            for (id, importance) in manifest {
                applied += stmt.execute((id.as_str(), *importance, project)).await? as usize;
            }
        }
        tx.commit().await?;
        Ok(applied)
    }
    .await;
    result.with_context(|| context)
}

// ---------------------------------------------------------------------------
// retrospective
// ---------------------------------------------------------------------------

/// The one durable lesson a retrospective run may record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RetrospectiveLesson {
    /// One of [`CATEGORIES`].
    category: &'static str,
    /// The fact text, newline-collapsed and length-capped.
    content: String,
}

/// The project's user turns newer than `after`, newest first.
///
/// The project key is the session directory (that is what `ctx_memory` stores
/// and what the dreamer enumerates by), so this joins every session of the
/// project. The watermark is the last *successful* run's start, which is what
/// makes a sweep cover exactly the turns that have not been read yet. Rows whose
/// `data` does not decode, or that carry no text, are skipped: a turn with
/// nothing typed into it has nothing to learn from.
async fn recent_user_turns(db: &Db, project: &str, after: i64) -> Result<Vec<String>> {
    let context = format!("read recent user turns for project {project}");
    let result: Result<Vec<String>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT m.data FROM session_message m
                   JOIN session_v2 s ON s.id = m.session_id
                  WHERE m.type = 'user' AND s.directory = ?1 AND m.time_created > ?2
                  ORDER BY m.time_created DESC
                  LIMIT ?3",
            )
            .await?;
        let mut rows = stmt
            .query((project, after, RETROSPECTIVE_TURNS as i64))
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let data: String = row.get(0)?;
            let text = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|value| {
                    value
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            if !text.trim().is_empty() {
                out.push(text);
            }
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// The `retrospective` user prompt: the recent turns, oldest first.
///
/// `turns` arrives newest-first (the query's order) and the prompt reads them in
/// the order they happened. The character budget is spent from the newest end,
/// so an over-long window drops the oldest turns rather than the most recent
/// corrections — which are exactly the ones worth learning from.
fn render_retrospective_prompt(project: &str, turns: &[String]) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut budget = RETROSPECTIVE_TOTAL_CHARS;
    for turn in turns {
        let text = head_chars(turn.trim(), RETROSPECTIVE_TURN_CHARS);
        let cost = text.chars().count();
        if cost == 0 {
            continue;
        }
        if cost > budget {
            break;
        }
        budget -= cost;
        kept.push(text);
    }
    kept.reverse();
    let mut parts = vec![format!(
        "## Project: {project}\n\nThe user's own turns from the recent working session, in the order they happened. Look for a durable lesson.\n\n<turns>"
    )];
    for text in kept {
        parts.push(format!("<turn>{text}</turn>"));
    }
    parts.push("</turns>".to_owned());
    parts.join("\n")
}

/// Parse the one `<memory …>…</memory>` element.
///
/// Three answers are legal: exactly one element carrying a known category, one
/// empty/self-closing element (`nothing durable`), or no element at all (also
/// `nothing durable`). Everything else is a parse failure and writes nothing —
/// an unknown category, an unclosed element (truncation), or two elements (the
/// task asked for at most one, so a list means the model drifted from the
/// contract).
fn parse_retrospective(text: &str) -> Result<Option<RetrospectiveLesson>, String> {
    let mut elements = memory_element_regex().captures_iter(text);
    let Some(captures) = elements.next() else {
        if memory_open_regex().is_match(text) {
            return Err("retrospective answer carries an unclosed <memory> element".to_owned());
        }
        return Ok(None);
    };
    if elements.next().is_some() {
        return Err("retrospective answer carries more than one <memory> element".to_owned());
    }
    let attrs = captures.get(1).map_or("", |group| group.as_str());
    let body = captures.get(2).map_or("", |group| group.as_str());
    // Newlines become single spaces: a memory is one line in every prefix it
    // is rendered into, and a model answer is often indented. An element with
    // no fact in it is the contract's "nothing durable", whatever it says about
    // a category.
    let collapsed = collapse_lines(body);
    let content = head_chars(&collapsed, RETROSPECTIVE_MAX_CHARS).to_owned();
    if content.is_empty() {
        return Ok(None);
    }
    let Some(category) = attr_of(attrs, "category") else {
        return Err("retrospective <memory> element is missing a category".to_owned());
    };
    let category = category.trim();
    let Some(category) = CATEGORIES.iter().find(|known| **known == category) else {
        return Err(format!(
            "retrospective <memory> element carries an unknown category: {category}"
        ));
    };
    Ok(Some(RetrospectiveLesson { category, content }))
}

/// Why [`validate_retrospective_lesson`] rejected a retrospective lesson
/// (upstream dreamer/retrospective-learnings.ts:125-134).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LessonRejection {
    /// A quoted run of >=4 characters — transcription, not distillation.
    RawQuote,
    /// A 19xx/20xx year literal — session-local chronology.
    DateLiteral,
    /// Session-local frustration phrasing.
    FrustrationMarker,
    /// Shares a verbatim word-run of `run_cap` words with a source turn.
    SourceOverlap,
}

/// The longest verbatim word-run a lesson may share with a source turn
/// (upstream `MAX_SOURCE_WORD_RUN`, retrospective-learnings.ts:68).
const MAX_SOURCE_WORD_RUN: usize = 7;

/// Words of one lesson the overlap check reads (upstream
/// `MAX_OVERLAP_LEARNING_WORDS`, :74). A lesson is one line by contract and
/// already capped by [`RETROSPECTIVE_MAX_CHARS`]; the cap is defense.
const MAX_OVERLAP_LEARNING_WORDS: usize = 200;

/// The strongest correction phrases, lowercased. A reduction of the upstream
/// `FRUSTRATION_MARKER_REGEX` (:14-15): that regex also counts "this is
/// wrong", "you missed", "stop claiming/using", repeated no/wrong/again/stop
/// runs, and `???` bursts.
const FRUSTRATION_MARKERS: [&str; 5] = [
    "not what i asked",
    "i already said",
    "you ignored",
    "that's wrong",
    "stop doing",
];

/// Validate one parsed retrospective lesson against the upstream
/// "distill, do not transcribe" gate (retrospective-learnings.ts:65-67 and
/// :125-134): a durable memory must be the third-person lesson, never the
/// user's own words. A lesson that quotes the user, carries a date literal,
/// preserves session-local anger, or shares a verbatim word-run with a source
/// turn is rejected — the caller drops it and logs, never writes.
///
/// Two deliberate reductions from the upstream regexes (documented, not
/// accidental): the frustration check keeps the five strongest correction
/// phrases plus a `!!!` burst; the date check keys on bare 19xx/20xx year
/// literals where upstream requires a full date shape.
pub(crate) fn validate_retrospective_lesson(
    content: &str,
    source_user_texts: &[&str],
) -> Result<(), LessonRejection> {
    if has_raw_quote(content) {
        return Err(LessonRejection::RawQuote);
    }
    if has_year_literal(content) {
        return Err(LessonRejection::DateLiteral);
    }
    if has_frustration_marker(content) {
        return Err(LessonRejection::FrustrationMarker);
    }
    if has_source_overlap(content, source_user_texts) {
        return Err(LessonRejection::SourceOverlap);
    }
    Ok(())
}

/// A run of >=4 characters between any two double-quote-class characters
/// (`" “ ”`, interchangeable as openers/closers in the upstream
/// `RAW_QUOTE_REGEX`, :36), or between two apostrophes. An unpaired
/// apostrophe — a contraction — never matches; two contractions in one
/// sentence can, which the upstream regex also does.
fn has_raw_quote(content: &str) -> bool {
    fn paired_run(text: &str, is_quote: impl Fn(char) -> bool) -> bool {
        let mut inside = false;
        let mut run = 0usize;
        for ch in text.chars() {
            if is_quote(ch) {
                if inside && run >= 4 {
                    return true;
                }
                inside = true;
                run = 0;
            } else if inside {
                run += 1;
            }
        }
        false
    }
    paired_run(content, |ch| matches!(ch, '"' | '“' | '”')) || paired_run(content, |ch| ch == '\'')
}

/// A bare 19xx/20xx year literal: any four-digit ASCII run carrying one of
/// the two century prefixes (upstream `DATE_REGEX`, :37-38, reduced — see
/// [`validate_retrospective_lesson`]).
fn has_year_literal(content: &str) -> bool {
    content
        .split(|ch: char| !ch.is_ascii_digit())
        .any(|run| run.len() == 4 && (run.starts_with("19") || run.starts_with("20")))
}

/// A correction phrase, or a `!!!` punctuation burst.
fn has_frustration_marker(content: &str) -> bool {
    let lowered = content.to_lowercase();
    lowered.contains("!!!")
        || FRUSTRATION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
}

/// Lowercase alphanumeric words — the token convention the mctx search's
/// `is_token_char` uses, and the upstream `toWords` (:76-83) minus the
/// Unicode letter classes ox's tokenizer never carried.
fn overlap_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect()
}

/// The shared-run length that means transcription: an absolute cap of
/// [`MAX_SOURCE_WORD_RUN`], or half the lesson's own length for very short
/// lessons, floored at 3 (upstream :102-105).
fn source_run_cap(lesson_words: usize) -> usize {
    MAX_SOURCE_WORD_RUN.min(3.max(lesson_words.div_ceil(2)))
}

/// True when the lesson shares a contiguous run of `run_cap` words with any
/// source turn: some runCap-gram of the lesson occurs verbatim in the source
/// (upstream `hasHighSourceOverlap`, :99-123). Sources are never truncated —
/// a lifted run at any offset is caught.
fn has_source_overlap(content: &str, source_user_texts: &[&str]) -> bool {
    let mut lesson = overlap_words(content);
    lesson.truncate(MAX_OVERLAP_LEARNING_WORDS);
    let run_cap = source_run_cap(lesson.len());
    if lesson.len() < run_cap {
        return false;
    }
    let grams: HashSet<String> = lesson.windows(run_cap).map(|run| run.join(" ")).collect();
    source_user_texts.iter().any(|source| {
        overlap_words(source)
            .windows(run_cap)
            .any(|run| grams.contains(&run.join(" ")))
    })
}

/// Every non-empty line of `text`, trimmed and joined by single spaces.
fn collapse_lines(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

/// Whether the project already carries this exact fact as an active memory.
///
/// Equality on the stored content is the dedup rule: a lesson identical to an
/// existing memory teaches nothing, and rewriting the row would only bump
/// `updated_at` (moving it up the pinboard's recency tie-break) for no gain.
async fn fact_recorded(db: &Db, project: &str, content: &str) -> Result<bool> {
    let context = format!("check recorded facts for project {project}");
    let result: Result<bool> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT 1 FROM mc_memory
                  WHERE project = ?1 AND status = 'active' AND content = ?2
                  LIMIT 1",
            )
            .await?;
        Ok(ox_db::optional(&mut stmt, (project, content))
            .await?
            .is_some())
    }
    .await;
    result.with_context(|| context)
}

fn memory_element_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<memory\b([^>]*?)(?:/>|>(.*?)</memory\s*>)")
            .expect("retrospective memory element regex")
    })
}

fn memory_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<memory\b").expect("retrospective memory open regex"))
}

// ---------------------------------------------------------------------------
// surface_notes
// ---------------------------------------------------------------------------

/// One parked smart note as the checker reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SurfaceNote {
    /// `#N` handle body.
    id: String,
    title: String,
    body: String,
    /// The condition the note was parked on, verbatim.
    condition: String,
}

/// The cheap gate: how many of this project's notes are parked on a usable
/// condition. A project with none costs one `count(*)` per tick and no model
/// call (`dreamer/task-gates.ts`).
///
/// The project key is the session directory (that is what the dreamer
/// enumerates by), so this joins the notes to their sessions. An empty
/// condition is not a condition: `write_note` drops a blank one, and a row that
/// somehow carries one has nothing to judge.
async fn parked_note_count(db: &Db, project: &str) -> Result<i64> {
    let context = format!("count parked notes for project {project}");
    let result: Result<i64> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT count(*) FROM mc_note n
                   JOIN session_v2 s ON s.id = n.session_id
                  WHERE s.directory = ?1 AND n.status = 'pending'
                    AND n.surface_condition IS NOT NULL AND n.surface_condition <> ''",
            )
            .await?;
        let row = stmt.query_row([project]).await?;
        Ok(row.get(0)?)
    }
    .await;
    result.with_context(|| context)
}

/// The parked notes one `surface_notes` completion covers: the lowest handles
/// first, so a backlog larger than the batch is swept in stable, resumable
/// slices. `session_id` is deliberately not selected — handles are unique across
/// the table and the apply is guarded by id alone.
async fn parked_notes(db: &Db, project: &str) -> Result<Vec<SurfaceNote>> {
    let context = format!("read parked notes for project {project}");
    let result: Result<Vec<SurfaceNote>> = async {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.title, n.body, n.surface_condition FROM mc_note n
                   JOIN session_v2 s ON s.id = n.session_id
                  WHERE s.directory = ?1 AND n.status = 'pending'
                    AND n.surface_condition IS NOT NULL AND n.surface_condition <> ''
                  ORDER BY CAST(n.id AS INTEGER)
                  LIMIT ?2",
            )
            .await?;
        let mut rows = stmt.query((project, SURFACE_BATCH as i64)).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SurfaceNote {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                condition: row.get(3)?,
            });
        }
        Ok(out)
    }
    .await;
    result.with_context(|| context)
}

/// The `surface_notes` user prompt: the parked notes, then the user's turns.
///
/// `turns` arrives newest-first (the query's order) and the prompt reads them in
/// the order they happened, so a condition about "after X" is read against a
/// transcript that runs forwards.
fn render_surface_prompt(project: &str, batch: &[SurfaceNote], turns: &[String]) -> String {
    let mut parts = vec![format!(
        "## Project: {project}\n\nThese notes are parked on a condition. Decide which conditions hold NOW, on the evidence of the user's own turns below.\n\n<notes>"
    )];
    for note in batch {
        parts.push(format!("<note id=\"{}\">", note.id));
        parts.push(format!("<condition>{}</condition>", note.condition));
        parts.push(format!("<title>{}</title>", note.title));
        let body = head_chars(note.body.trim(), SURFACE_BODY_CHARS);
        if !body.is_empty() {
            parts.push(body.to_owned());
        }
        parts.push("</note>".to_owned());
    }
    parts.push("</notes>".to_owned());
    parts.push("<turns>".to_owned());
    for turn in turns.iter().rev() {
        parts.push(format!(
            "<turn>{}</turn>",
            head_chars(turn.trim(), SURFACE_TURN_CHARS)
        ));
    }
    parts.push("</turns>".to_owned());
    parts.join("\n")
}

/// Parse the `<ready id="N"/>` list.
///
/// The tolerant half mirrors the other manifests (prose around the elements is
/// ignored, ids outside the batch are dropped with a warning) and adds one
/// concession the answer invites: `id='7'` is accepted as well as `id="7"`,
/// because a one-attribute list is exactly the shape a model switches quoting
/// on. Duplicates collapse — a note is either surfaced or not.
///
/// The strict half is fail-closed: a `<ready>` element the host cannot address,
/// or an unclosed `<ready` (truncation), rejects the whole answer instead of
/// surfacing a guessed subset. No `<ready` at all is the contract's "no
/// condition holds".
fn parse_ready_ids(text: &str, batch_ids: &[&str]) -> Result<Vec<String>, String> {
    // A `<ready` opener without a complete element is a truncated answer: the
    // ids that did arrive would be a prefix of the verdict, never the verdict.
    if ready_open_regex().find_iter(text).count() > ready_element_regex().find_iter(text).count() {
        return Err("surface answer carries an unclosed <ready> element".to_owned());
    }
    let mut ids: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut unaddressed = false;
    for captures in ready_element_regex().captures_iter(text) {
        let attrs = captures.get(1).map_or("", |group| group.as_str());
        let Some(id) = ready_id_attr(attrs) else {
            unaddressed = true;
            continue;
        };
        if !batch_ids.contains(&id.as_str()) {
            unknown += 1;
            continue;
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if unaddressed {
        return Err("surface answer carries a <ready> element without an id".to_owned());
    }
    if unknown > 0 {
        tracing::warn!(
            unknown,
            "surface answer named notes outside the batch; dropped"
        );
    }
    Ok(ids)
}

/// The `id` attribute of one `<ready>` element, in either quote style. `None`
/// covers a missing attribute and an empty value: neither addresses a note.
fn ready_id_attr(attrs: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?i)\bid\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("ready id attr regex")
    });
    let captures = re.captures(attrs)?;
    let value = captures.get(1).or_else(|| captures.get(2))?.as_str().trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Apply a surface verdict: one transaction, one status flip per id, each
/// guarded by `pending` so a note that was dismissed, edited into `active` or
/// surfaced by an earlier run cannot be flipped through a stale batch.
///
/// `at` is the run's start (see [`crate::mctx::dreamer`]'s watermark rules); it
/// becomes the row's `updated_at`, which is what the `ctx_note read` page ages.
async fn apply_surfaced(db: &Db, ids: &[String], at: i64) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let context = format!("surface {} notes", ids.len());
    let result: Result<usize> = async {
        let tx = db.begin().await?;
        let mut applied = 0;
        {
            let mut stmt = tx
                .prepare(
                    "UPDATE mc_note SET status = 'ready', updated_at = ?2
                      WHERE id = ?1 AND status = 'pending'",
                )
                .await?;
            for id in ids {
                applied += stmt.execute((id.as_str(), at)).await? as usize;
            }
        }
        tx.commit().await?;
        Ok(applied)
    }
    .await;
    result.with_context(|| context)
}

/// The `<ready` opener (any of `/>`, `>`, or a truncated element).
fn ready_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<ready\b").expect("ready open regex"))
}

/// One complete `<ready …>` element's attribute string.
fn ready_element_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<ready\b([^>]*?)/?>").expect("ready element regex"))
}

// ---------------------------------------------------------------------------
// Applied-manifest report
// ---------------------------------------------------------------------------

/// What one applied manifest did, for the report and the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ApplyReport {
    verified: usize,
    skipped: usize,
    updated: usize,
    merged: usize,
    archived: usize,
    refused: usize,
}

impl ApplyReport {
    fn describe(&self) -> String {
        format!(
            "verified={} skipped={} updated={} merged={} archived={} refused={}",
            self.verified, self.skipped, self.updated, self.merged, self.archived, self.refused
        )
    }
}

// ---------------------------------------------------------------------------
// Manifest lexing
// ---------------------------------------------------------------------------

/// The body of `<root>…</root>`, or an error. Ported from
/// `dreamer/manifest-parser.ts::extractCompleteManifestBody`: a missing
/// closing root is truncation and rejects the whole output, so a length-capped
/// response can never apply a prefix of its verdicts. Like the reference, the
/// search is unanchored (prose around a complete manifest is tolerated) and
/// takes the first complete root.
fn complete_manifest_body(text: &str, root: &str) -> Result<String, String> {
    let regex = match root {
        "verify" => verify_root_regex(),
        _ => curate_root_regex(),
    };
    regex
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|body| body.as_str().to_owned())
        .ok_or_else(|| {
            format!(
                "{root} manifest missing a complete root element \
                 (truncated output, or the wrong task's manifest)"
            )
        })
}

fn verify_root_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<verify\b[^>]*>(.*?)</verify\s*>").expect("verify root regex")
    })
}

fn curate_root_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<curate\b[^>]*>(.*?)</curate\s*>").expect("curate root regex")
    })
}

/// The `name="value"` attributes of one element's attribute string.
fn attr_of(attrs: &str, name: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"(\w+)\s*=\s*"([^"]*)""#).expect("xml attr regex"));
    re.captures_iter(attrs)
        .find(|captures| captures.get(1).is_some_and(|key| key.as_str() == name))
        .and_then(|captures| captures.get(2))
        .map(|value| value.as_str().to_owned())
}

fn verified_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<verified\b([^>]*)>").expect("verified element regex"))
}

fn update_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<update\b([^>]*?)(?:/>|>(.*?)</update\s*>)")
            .expect("update element regex")
    })
}

fn archive_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<archive\b([^>]*)>").expect("archive element regex"))
}

fn skip_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<skip\b([^>]*)>").expect("skip element regex"))
}

fn merge_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<merge\b([^>]*?)(?:/>|>(.*?)</merge\s*>)").expect("merge element regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str) -> Candidate {
        Candidate {
            id: id.to_owned(),
            category: "ARCHITECTURE".into(),
            content: format!("memory {id}"),
            updated_at: 0,
            paths: Vec::new(),
        }
    }

    fn batch(ids: &[&str]) -> Vec<Candidate> {
        ids.iter().map(|id| cand(id)).collect()
    }

    fn verify_batch_of(ids: &[String]) -> Vec<Candidate> {
        ids.iter().map(|id| cand(id)).collect()
    }

    #[tokio::test]
    async fn skip_verdict_is_stamped_like_verified() {
        let db = Db::open_in_memory().await.expect("db");
        let mut ids = Vec::new();
        for text in ["one", "two", "three"] {
            ids.push(
                memory::write_memory(&db, "p", text, "ARCHITECTURE")
                    .await
                    .expect("write"),
            );
        }
        let batch = verify_batch_of(&ids);
        let manifest = VerifyManifest {
            verified: vec![ids[0].clone()],
            skipped: vec![ids[1].clone()],
            ..VerifyManifest::default()
        };
        let started_at = now_millis() + 60_000;

        let report = apply_verify_manifest(&db, "p", &batch, &manifest, started_at)
            .await
            .expect("apply");

        assert_eq!((report.verified, report.skipped), (1, 1));
        assert!(
            report.describe().contains("skipped=1"),
            "{}",
            report.describe()
        );
        let stored = memory::active_memories_with_watermark(&db, "p")
            .await
            .expect("read");
        let stamp = |id: &String| {
            stored
                .iter()
                .find(|m| &m.id == id)
                .expect("row")
                .verified_at
        };
        assert_eq!(stamp(&ids[0]), started_at);
        assert_eq!(stamp(&ids[1]), started_at);
        assert_eq!(stamp(&ids[2]), 0, "an omitted id keeps its old watermark");
    }

    #[test]
    fn describe_reports_skipped_between_verified_and_updated() {
        let report = ApplyReport {
            skipped: 3,
            ..ApplyReport::default()
        };
        assert_eq!(
            report.describe(),
            "verified=0 skipped=3 updated=0 merged=0 archived=0 refused=0"
        );
    }

    #[test]
    fn manifest_covering_every_id_with_the_three_old_verdicts_parses() {
        let manifest = parse_verify_manifest(
            "<verify><verified id=\"1\"/><update id=\"2\">new text</update><archive id=\"3\" reason=\"gone\"/></verify>",
            &batch(&["1", "2", "3"]),
        )
        .expect("fully covered manifest parses");
        assert_eq!(manifest.verified, vec!["1".to_owned()]);
        assert_eq!(manifest.updates.len(), 1);
        assert_eq!(manifest.updates[0].id, "2");
        assert_eq!(manifest.updates[0].content, "new text");
        assert_eq!(manifest.archives.len(), 1);
        assert_eq!(manifest.archives[0].id, "3");
        assert!(manifest.skipped.is_empty());
    }

    #[test]
    fn skip_verdicts_count_toward_coverage() {
        let manifest = parse_verify_manifest(
            "<verify><verified id=\"1\"/><skip id=\"2\" reason=\"behavioral\"/></verify>",
            &batch(&["1", "2", "3", "4"]),
        )
        .expect("2/4 is not below half");
        assert_eq!(manifest.verified, vec!["1".to_owned()]);
        assert_eq!(manifest.skipped, vec!["2".to_owned()]);
    }

    #[test]
    fn skip_forms_self_closing_with_and_without_reason_parse() {
        let manifest = parse_verify_manifest(
            "<verify><skip id=\"1\"/><skip id=\"2\" reason=\"x\"/><skip id=\"3\" reason=\"y\"></skip></verify>",
            &batch(&["1", "2", "3"]),
        )
        .expect("skip forms parse");
        assert_eq!(
            manifest.skipped,
            vec!["1".to_owned(), "2".to_owned(), "3".to_owned()]
        );
    }

    #[test]
    fn manifest_below_half_coverage_is_rejected() {
        let ids: Vec<String> = (1..=15).map(|n| n.to_string()).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let err = parse_verify_manifest(
            "<verify><verified id=\"1\"/><verified id=\"2\"/><skip id=\"3\"/></verify>",
            &batch(&refs),
        )
        .unwrap_err();
        assert!(err.contains("covers 3/15"), "{err}");
    }

    #[test]
    fn same_id_in_verified_and_skip_is_a_duplicate() {
        let err = parse_verify_manifest(
            "<verify><verified id=\"1\"/><skip id=\"1\"/></verify>",
            &batch(&["1", "2"]),
        )
        .unwrap_err();
        assert!(err.contains("duplicate id"), "{err}");
    }

    #[test]
    fn skip_outside_the_batch_is_dropped_and_not_counted() {
        let err = parse_verify_manifest(
            "<verify><verified id=\"1\"/><skip id=\"99\"/></verify>",
            &batch(&["1", "2", "3", "4"]),
        )
        .unwrap_err();
        assert!(err.contains("covers 1/4"), "{err}");
    }

    #[test]
    fn skip_without_an_id_is_rejected() {
        let err = parse_verify_manifest(
            "<verify><verified id=\"1\"/><skip reason=\"x\"/></verify>",
            &batch(&["1", "2"]),
        )
        .unwrap_err();
        assert!(err.contains("missing an id"), "{err}");
    }

    #[test]
    fn verify_prompt_asks_for_every_id_and_offers_skip() {
        assert!(!VERIFY_SYSTEM_PROMPT.contains("omit rather than guess"));
        assert!(VERIFY_SYSTEM_PROMPT.contains("<skip id="));
        assert!(VERIFY_SYSTEM_PROMPT.contains("exactly once"));
    }

    fn st(exists: bool, newer: bool) -> PathState {
        PathState {
            path: None,
            written: "x".into(),
            exists,
            mtime: None,
            newer,
        }
    }

    #[test]
    fn an_existing_newer_file_is_checked_whether_or_not_the_memory_was_stamped() {
        assert!(needs_check(&[st(true, true)], 0));
        assert!(needs_check(&[st(true, true)], 500));
    }

    #[test]
    fn an_existing_unchanged_file_is_never_checked() {
        assert!(!needs_check(&[st(true, false)], 0));
        assert!(!needs_check(&[st(true, false)], 500));
    }

    #[test]
    fn a_missing_file_is_checked_only_while_the_memory_was_never_verified() {
        assert!(needs_check(&[st(false, false)], 0));
        assert!(!needs_check(&[st(false, false)], 500));
    }

    #[test]
    fn an_unresolvable_path_behaves_like_a_missing_one() {
        // `st` builds `path: None`, the shape `resolve_path` returns for a
        // `~`-prefixed or out-of-project path.
        let unresolvable = PathState {
            path: None,
            ..st(false, false)
        };
        assert!(needs_check(std::slice::from_ref(&unresolvable), 0));
        assert!(!needs_check(std::slice::from_ref(&unresolvable), 500));
    }

    #[test]
    fn mixed_paths_follow_the_existing_files_once_stamped() {
        assert!(!needs_check(&[st(false, false), st(true, false)], 500));
        assert!(needs_check(&[st(false, false), st(true, true)], 500));
    }

    #[test]
    fn a_memory_with_no_paths_is_never_checked() {
        assert!(!needs_check(&[], 0));
        assert!(!needs_check(&[], 500));
    }

    fn stored(id: &str, content: &str, verified_at: i64) -> memory::StoredMemory {
        memory::StoredMemory {
            id: id.to_owned(),
            category: "ARCHITECTURE".into(),
            content: content.to_owned(),
            updated_at: 0,
            verified_at,
        }
    }

    #[test]
    fn candidate_stops_resending_stamped_memories_whose_file_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir src");
        std::fs::write(dir.path().join("src/exists.rs"), "fn main() {}").expect("write file");
        let project = dir.path().to_str().expect("utf-8 tempdir");
        let pick = |content: &str, verified_at: i64| {
            candidate(project, &stored("1", content, verified_at)).is_some()
        };

        // The file was just written: its mtime is later than 0 and 1000.
        assert!(pick("see src/exists.rs", 0), "never verified, file exists");
        assert!(pick("see src/gone.rs", 0), "never verified, file missing");
        assert!(!pick("see src/gone.rs", 1000), "stamped, file missing");
        assert!(!pick("see ~/x/y.rs", 1000), "stamped, unresolvable path");
        assert!(!pick("no file is cited here", 0), "no path at all");
        assert!(
            pick("see src/exists.rs", 1000),
            "stamped, file changed since"
        );
    }

    const INTERVAL: i64 = 15 * 60_000;
    const HOUR: i64 = 60 * 60_000;
    const NOW: i64 = 1_000_000_000_000;

    fn failed(consecutive_failures: u32, attempted_ago: i64) -> DreamState {
        DreamState {
            last_attempt_at: NOW - attempted_ago,
            last_status: Some("failed".into()),
            consecutive_failures,
            ..DreamState::default()
        }
    }

    #[test]
    fn failure_backoff_doubles_per_failure_past_the_threshold_up_to_the_cap() {
        for failures in 0..MAX_DREAM_FAILURES {
            assert_eq!(failure_backoff_millis(failures, INTERVAL), 0, "{failures}");
        }
        assert_eq!(failure_backoff_millis(3, INTERVAL), 2 * INTERVAL);
        assert_eq!(failure_backoff_millis(4, INTERVAL), 4 * INTERVAL);
        assert_eq!(failure_backoff_millis(5, INTERVAL), 8 * INTERVAL);
        assert_eq!(failure_backoff_millis(6, INTERVAL), 16 * INTERVAL);
        for failures in [9, 100, u32::MAX] {
            assert_eq!(
                failure_backoff_millis(failures, INTERVAL),
                MAX_DREAM_BACKOFF_MILLIS,
                "{failures}"
            );
        }
    }

    #[test]
    fn failure_backoff_survives_degenerate_intervals() {
        for failures in [0, 3, 100, u32::MAX] {
            assert!(failure_backoff_millis(failures, 0) <= MAX_DREAM_BACKOFF_MILLIS);
            assert!(failure_backoff_millis(failures, -5) <= MAX_DREAM_BACKOFF_MILLIS);
        }
        assert_eq!(
            failure_backoff_millis(3, i64::MAX),
            MAX_DREAM_BACKOFF_MILLIS
        );
        assert_eq!(
            failure_backoff_millis(u32::MAX, i64::MAX),
            MAX_DREAM_BACKOFF_MILLIS
        );
    }

    #[test]
    fn is_due_gates_on_the_last_successful_run() {
        assert!(is_due(&DreamState::default(), NOW, INTERVAL), "fresh row");
        let ran = |ago: i64| DreamState {
            last_run_at: Some(NOW - ago),
            ..DreamState::default()
        };
        assert!(!is_due(&ran(INTERVAL - 1), NOW, INTERVAL), "too young");
        assert!(is_due(&ran(INTERVAL), NOW, INTERVAL), "old enough");
    }

    #[test]
    fn is_due_retries_hot_below_the_failure_threshold() {
        assert!(is_due(&failed(2, 0), NOW, INTERVAL));
    }

    #[test]
    fn is_due_backs_off_exponentially_once_failures_reach_the_threshold() {
        assert!(!is_due(&failed(3, INTERVAL), NOW, INTERVAL));
        assert!(!is_due(&failed(3, 2 * INTERVAL - 1), NOW, INTERVAL));
        assert!(is_due(&failed(3, 2 * INTERVAL), NOW, INTERVAL));
        assert!(!is_due(&failed(6, 3 * HOUR), NOW, INTERVAL));
        assert!(is_due(&failed(6, 4 * HOUR), NOW, INTERVAL));
        assert!(!is_due(&failed(50, 23 * HOUR), NOW, INTERVAL));
        assert!(is_due(&failed(50, 24 * HOUR), NOW, INTERVAL));
    }

    #[test]
    fn is_due_treats_a_future_attempt_stamp_as_not_yet_due() {
        // Clock skew: `now - last_attempt_at` is negative, below any wait.
        assert!(!is_due(&failed(3, -HOUR), NOW, INTERVAL));
    }
}
