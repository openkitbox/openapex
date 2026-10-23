//! Dreamer: the scheduled memory-maintenance loop (Task 8).
//!
//! The dreamer is the only place in ox where a model is asked to *maintain*
//! the memory store instead of answering a user turn. A tick enumerates the
//! projects that have memories, and for every due `(project, task)` pair runs
//! one task: `verify` (memory vs. code) and `curate` (duplicate/stale
//! consolidation). Results are applied host-side through
//! [`crate::mctx::memory`]'s store functions, never by the model.
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
//!   "consecutive failures past [`MAX_DREAM_FAILURES`] are not re-attempted
//!   until the next interval has elapsed since the failed attempt".
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
//!   nightly, `curate` weekly). ox runs both tasks on the single
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
//!   all-or-nothing at the batch level. A memory the manifest never mentions
//!   keeps its old watermark and stays a candidate — which is also why ox has no
//!   `skip` verdict: "keep it, do not claim verification" is already the
//!   meaning of omission.
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
//! - **Not ported at all:** the other ten reference tasks, doc proposals,
//!   smart notes, primers, the mural, `classify`, token budgets, model
//!   fallback chains, `retrospective` watermarks, and the
//!   `MC-D09`/`MC-S05` status reporting.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use regex::Regex;

use crate::cardinal::chat::Chat;
use crate::cardinal::wire::{ChatMessage, ChatRequest};
use crate::mctx::historian::{collect_completion, historian_model};
use crate::mctx::memory::{self, CATEGORIES};
use ox_core::config::Config;
use ox_db::Db;
use ox_schema::id::now_millis;

/// Consecutive failures of one `(project, task)` after which the task stops
/// being retried on every tick and waits for its next interval slot instead
/// (the reference's `MAX_TASK_RETRIES` in `dreamer/task-scheduler.ts`).
pub const MAX_DREAM_FAILURES: u32 = 3;

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

Decide ONE of three outcomes per memory:
- VERIFIED — still accurate. Keep it as-is.
- UPDATE — a CODE FACT is still true but a file-falsifiable DETAIL drifted (a renamed symbol, a moved file, a changed number or name). Provide the corrected content in terse present tense (\"X uses Y\", not \"X was changed to Y\"). Only update for genuine drift, not style. If a legitimate update intentionally consolidates the old claim into much shorter content, set consolidation=\"true\" explicitly.
- ARCHIVE — a CODE FACT is positively falsified: the code clearly contradicts it, or the API/symbol/path it describes no longer exists.

UPDATE and ARCHIVE are ONLY for claims a repository file can falsify. Behavioral directives (when to act, how to work, who decides, tool-usage discipline) can only be VERIFIED — a named path inside a directive does not make the directive a code fact, and a file's failure to corroborate a behavioral rule is never grounds to update or archive it. A memory whose referenced file is simply gone is NOT automatically falsified: if the claim is behavioral, or the excerpt cannot decide it, mark it VERIFIED and leave it alone.

BE CONSERVATIVE ABOUT ARCHIVING. Wrong archival of a TRUE memory is the worst possible outcome — far worse than leaving a slightly-stale memory. Archive ONLY with positive evidence.

Memories you do not mention are left unchanged; omit rather than guess.

Output ONE XML manifest at the very end and NOTHING else — no narration, no per-memory commentary, no reasoning:
<verify>
<verified id=\"N\"/>
<update id=\"M\">corrected present-tense content</update>
<update id=\"C\" consolidation=\"true\">intentionally consolidated content</update>
<archive id=\"K\" reason=\"specific evidence the code contradicts it\"/>
</verify>

Rules:
- Each id you mention must appear exactly once, in exactly one element, and must come from the list below.
- Default to VERIFIED. UPDATE and ARCHIVE are the exceptions, not the norm.";

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

// ---------------------------------------------------------------------------
// Task, failure and report types
// ---------------------------------------------------------------------------

/// The tasks the MVP schedules, in the order a tick runs them. The order is
/// the reference's: `task-registry.ts::CANONICAL_DREAM_TASKS` lists `verify`
/// before `curate`, and `compareTaskOrder` orders due tasks that way. It is
/// fixed so a tick is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DreamTask {
    /// Re-check memories whose referenced files are missing or newer.
    Verify,
    /// Duplicate/stale consolidation of one memory category.
    Curate,
}

impl DreamTask {
    /// Every task, in tick order.
    pub const ALL: [DreamTask; 2] = [DreamTask::Verify, DreamTask::Curate];

    /// The `task` column spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            DreamTask::Verify => "verify",
            DreamTask::Curate => "curate",
        }
    }
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
    catalog: Arc<ox_core::model_catalog::ModelCatalog>,
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
        catalog: Arc<ox_core::model_catalog::ModelCatalog>,
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
        }
    }

    /// One hidden, tool-free completion through the historian's resolved model
    /// and transport (one route to the model, not two).
    async fn complete(&self, system: &str, user_prompt: &str) -> Result<String, TaskFailure> {
        let model = historian_model(&self.config, &self.catalog).map_err(|error| TaskFailure {
            class: FailureClass::NoModels,
            message: error.root_cause().to_string(),
        })?;
        let request = ChatRequest::new(
            model,
            vec![
                ChatMessage::System {
                    content: system.to_owned(),
                },
                ChatMessage::User {
                    content: user_prompt.to_owned().into(),
                },
            ],
        );
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

    /// `verify`: candidates are memories whose referenced files are missing or
    /// newer than the memory itself; only then is one completion issued.
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
}

/// Whether a task should run at `now`.
///
/// Due means `now - last_run_at >= interval`, with `last_run_at` the start of
/// the last *successful* run (so work landing during a run is not skipped).
/// A row that has failed [`MAX_DREAM_FAILURES`] times in a row is not
/// retried until the interval has elapsed *since the failed attempt*, which is
/// the two-cadence behavior of the reference's `MAX_TASK_RETRIES` reduced to a
/// single interval: the first retries are hot (next tick), then it waits.
fn is_due(state: &DreamState, now: i64, interval_millis: i64) -> bool {
    if let Some(last_run_at) = state.last_run_at
        && now - last_run_at < interval_millis
    {
        return false;
    }
    if state.consecutive_failures >= MAX_DREAM_FAILURES
        && now - state.last_attempt_at < interval_millis
    {
        return false;
    }
    true
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

/// One memory whose referenced file is missing or newer, with every path its
/// text named and what the host found there.
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
    /// when the path is outside the project.
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
/// (the reference's "file-independent" exclusion); a memory whose every
/// referenced file exists and is no newer than the memory is not one either.
fn candidate(project_dir: &str, memory: &memory::StoredMemory) -> Option<Candidate> {
    let paths: Vec<PathState> = referenced_paths(&memory.content)
        .into_iter()
        .map(|written| inspect(project_dir, &written, memory.verified_at))
        .collect();
    if !paths.iter().any(|state| !state.exists || state.newer) {
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
/// id is a rejected manifest, ids outside the batch are dropped, and a
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

    let batch_ids: Vec<&str> = batch.iter().map(|memory| memory.id.as_str()).collect();
    let mut known: Vec<String> = Vec::new();
    let mut unknown = 0usize;
    let mut error: Option<String> = None;
    manifest
        .verified
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
    // other prompt; a conservative run still mentions at least half, because
    // omission is how "leave it alone" is said.
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
    // A `verified` verdict stamps `verified_at` with the run's START, so an
    // unchanged pool is not re-checked on the next slot while anything written
    // during this run stays newer and is picked up then. Memories the manifest
    // omitted keep their old watermark and stay candidates ("omissions keep
    // their old verified_at, the next run picks them up").
    let banked: Vec<String> = manifest
        .verified
        .iter()
        .filter(|id| batch.iter().any(|memory| &memory.id == *id))
        // A memory the same manifest also rewrites or archives has a competing
        // verdict; the write wins and banking it first would be noise.
        .filter(|id| {
            !manifest.updates.iter().any(|update| &update.id == *id)
                && !manifest.archives.iter().any(|archive| &archive.id == *id)
        })
        .cloned()
        .collect();
    if !banked.is_empty() {
        report.verified = memory::touch_memories(db, project, &banked, started_at).await?;
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
// Applied-manifest report
// ---------------------------------------------------------------------------

/// What one applied manifest did, for the report and the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ApplyReport {
    verified: usize,
    updated: usize,
    merged: usize,
    archived: usize,
    refused: usize,
}

impl ApplyReport {
    fn describe(&self) -> String {
        format!(
            "verified={} updated={} merged={} archived={} refused={}",
            self.verified, self.updated, self.merged, self.archived, self.refused
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

fn merge_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<merge\b([^>]*?)(?:/>|>(.*?)</merge\s*>)").expect("merge element regex")
    })
}
