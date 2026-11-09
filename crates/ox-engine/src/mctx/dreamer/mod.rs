//! Dreamer: the scheduled memory-maintenance loop (Task 8).
//!
//! The dreamer is the only place in ox where a model is asked to *maintain*
//! the memory store instead of answering a user turn. A tick derives each
//! task's owners from its own inputs, and for every due `(owner, task)` pair runs
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
//!   `idle-schedule-prune.ts`. ox derives hygiene owners from active memories,
//!   note-check owners from pending conditional notes, and retrospective owners
//!   from eligible user activity. Input-specific gates then avoid idle model calls.
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
//! - **Task-specific owners, not a project/task cross product.** Hygiene uses
//!   [`memory::projects_with_active_memories`]; `surface_notes` independently
//!   enumerates pending conditional-note owners, so archiving the last memory
//!   does not stop note checks. `retrospective` independently enumerates eligible
//!   user-activity owners, so a project can produce its first memory. Notes-only
//!   and activity-only owners do not acquire unrelated hygiene work. Existing
//!   task state remains authoritative for due times and successful watermarks.
//! - **The user tier is an owner like any project.** `mc_memory.project` holds
//!   [`memory::USER_SCOPE`] for the user's own observations, so the tick
//!   enumerates it with the projects; [`DreamTask::applies_to`] limits it to
//!   pool hygiene (`expire`, `curate`, `classify`) because nothing in that
//!   tier cites a file, and no transcript or session belongs to it.
//! - **`expire` is ox's own, model-free task** (`expire.rs`): historian facts
//!   nobody confirmed within 30 days, or nobody touched within 180, are
//!   archived under a named reason. The reference prunes by a citation-fed
//!   usage count; ox feeds the same `last_used_at` from retrievals and
//!   citations, and leaves manual rows alone.
//! - **Not ported at all:** the reference's remaining tasks (doc proposals,
//!   primers, the mural, user-memory review), `scope`/`shareable` on classify
//!   (single project model, no consumer), token budgets, model fallback chains,
//!   and the `MC-D09`/`MC-S05` status reporting. `retrospective` uses the same
//!   `last_run_at` watermark as every other task (one unswept window per run)
//!   rather than the reference's per-task cursor set.

mod classify;
mod curate;
mod expire;
mod manifest;
mod retrospective;
mod schedule;
mod scheduler;
mod surface_notes;
mod task;
mod text;
mod turns;
mod verify;

pub use schedule::MAX_DREAM_FAILURES;
pub use scheduler::{Dreamer, dreamer_request};
pub use task::{DreamTask, FailureClass, TaskEntry, TickReport, TickStatus};
