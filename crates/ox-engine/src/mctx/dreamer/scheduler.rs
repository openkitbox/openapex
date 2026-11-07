use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::mctx::historian::{ModelChain, collect_completion};
use crate::mctx::memory;
use crate::mctx::redact;
use ox_core::config::Config;
use ox_db::Db;
use ox_llm::cardinal::chat::Chat;
use ox_llm::cardinal::wire::{ChatMessage, ChatRequest};
use ox_schema::id::now_millis;

use super::classify::{
    CLASSIFY_SYSTEM_PROMPT, apply_classify_manifest, parse_classify_manifest,
    render_classify_prompt, unclassified_batch, unclassified_count,
};
use super::curate::{
    CURATE_SYSTEM_PROMPT, apply_curate_manifest, curate_scope, parse_curate_manifest,
    render_curate_prompt,
};
use super::expire::run_expire;
use super::retrospective::{
    RETROSPECTIVE_SYSTEM_PROMPT, fact_recorded, parse_retrospective, render_retrospective_prompt,
    validate_retrospective_lesson,
};
use super::schedule::{
    DreamState, is_due, load_state, settle_failure, settle_skip, settle_success,
};
use super::surface_notes::{
    SURFACE_SYSTEM_PROMPT, apply_surfaced, parked_note_count, parked_notes, parse_ready_ids,
    render_surface_prompt,
};
use super::task::{
    DreamTask, FailureClass, TaskEntry, TaskFailure, TaskOutcome, TickReport, TickStatus,
};
use super::turns::recent_user_turns;
use super::verify::{
    Candidate, VERIFY_MEMORY_LIMIT, VERIFY_SYSTEM_PROMPT, apply_verify_manifest, candidate,
    parse_verify_manifest, render_verify_prompt,
};

/// Build a dreamer's hidden completion request. Dreamer has no parent session,
/// so the request deliberately leaves reasoning effort unset.
#[must_use]
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
        // A provider that just throttled gets no background completions for
        // the cool-down: the model-free tasks still run, the rest wait for
        // the next tick (their due slot is unchanged, so nothing is lost).
        let cooling = crate::mctx::quota::cooling_until(now);
        if let Some(until) = cooling {
            tracing::debug!(
                until,
                "dreamer: provider throttled recently; model-backed tasks deferred"
            );
        }
        // Owners, not only projects: the user tier is listed like any other
        // when it holds memories, and each task decides whether it applies.
        let projects = memory::projects_with_active_memories(&self.db).await?;
        for project in projects {
            for task in DreamTask::ALL {
                if !task.applies_to(&project) || (cooling.is_some() && task.needs_model()) {
                    continue;
                }
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
                let status = match self.settle(&project, task, &state, now, outcome).await {
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

    /// Record a settled task and render its report entry. `state` is the row
    /// the run loaded; a recorded fingerprint is merged into its map.
    async fn settle(
        &self,
        project: &str,
        task: DreamTask,
        state: &DreamState,
        now: i64,
        outcome: TaskOutcome,
    ) -> Result<TickStatus> {
        match outcome {
            TaskOutcome::Completed {
                detail,
                cursor,
                fingerprint,
            } => {
                let fingerprint = fingerprint.as_ref().map(|fingerprint| (state, fingerprint));
                settle_success(&self.db, project, task, now, cursor, fingerprint).await?;
                tracing::info!(
                    project = %project,
                    task = task.as_str(),
                    detail = %detail,
                    "dreamer task completed"
                );
                Ok(TickStatus::Completed { detail })
            }
            TaskOutcome::Skipped { detail, cursor } => {
                settle_skip(&self.db, project, task, now, cursor).await?;
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
            DreamTask::Expire => self.run_expire(project, started_at).await,
            DreamTask::Verify => self.run_verify(project, started_at).await,
            DreamTask::Curate => self.run_curate(project, state).await,
            DreamTask::Classify => self.run_classify(project).await,
            DreamTask::Retrospective => self.run_retrospective(project, state).await,
            DreamTask::SurfaceNotes => self.run_surface_notes(project, state, started_at).await,
        }
    }

    /// One hidden, tool-free completion through the historian's resolved model
    /// and transport (one route to the model, not two). A failure before any
    /// output steps once down the [`ModelChain`] (the catalogue default, since
    /// a dream has no session).
    async fn complete(&self, system: &str, user_prompt: &str) -> Result<String, TaskFailure> {
        let chain = ModelChain::resolve(&self.config, &self.catalog, None).map_err(|error| {
            TaskFailure {
                class: FailureClass::NoModels,
                message: error.root_cause().to_string(),
            }
        })?;
        let mut model = chain.primary();
        let text = loop {
            let request = dreamer_request(model, system, user_prompt);
            // Dreamer has no parent session policy; the request carries no
            // reasoning effort deliberately, leaving the provider's default.
            match collect_completion(&self.chat, &request, "dreamer").await {
                Ok(text) => break text,
                Err(error) => {
                    let Some(next) = chain.next_after(model) else {
                        return Err(TaskFailure {
                            class: FailureClass::ProviderError,
                            message: error.root_cause().to_string(),
                        });
                    };
                    tracing::warn!(
                        failed_model = model,
                        fallback_model = next,
                        error = %error,
                        "dreamer completion failed before output; retrying on the fallback model"
                    );
                    model = next;
                }
            }
        };
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

    // -- expire -------------------------------------------------------------

    /// `expire`: archive the owner's historian facts that aged out unconfirmed
    /// or unused. No gate and no model: the policy is one `UPDATE`, and a
    /// pass that archives nothing is a skip.
    async fn run_expire(&self, owner: &str, started_at: i64) -> TaskOutcome {
        match run_expire(&self.db, owner, started_at).await {
            Ok(report) if report.archived == 0 => {
                TaskOutcome::skipped("no memory aged past its expiry".to_owned())
            }
            Ok(report) => TaskOutcome::completed(report.describe()),
            Err(error) => TaskOutcome::Failed(TaskFailure::host("expire memories", &error)),
        }
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
            return TaskOutcome::skipped(
                "no memory cites a file that is missing or newer than it".to_owned(),
            );
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
            Ok(report) => TaskOutcome::completed(report.describe()),
            Err(error) => TaskOutcome::Failed(TaskFailure::host("apply verify manifest", &error)),
        }
    }

    // -- curate -------------------------------------------------------------

    /// `curate`: one category per run, scanning from the persisted cursor for
    /// the first category with at least two memories. A category with a single
    /// memory is passed over (it cannot contain a duplicate pair), so the scan
    /// never wedges, and the cursor advances past a category that actually
    /// ran (the reference's `curateTaskStateAfterSuccess`) — or that the
    /// dirty check found unchanged since its last run, so an idle pool costs
    /// no completion and the rotation still moves on.
    async fn run_curate(&self, project: &str, state: &DreamState) -> TaskOutcome {
        let memories = match memory::active_memories_with_watermark(&self.db, project).await {
            Ok(memories) => memories,
            Err(error) => return TaskOutcome::Failed(TaskFailure::host("read memories", &error)),
        };
        let categories = memory::categories_of(project);
        let Some(scope) = curate_scope(state.rotation_cursor, categories, &memories) else {
            return TaskOutcome::skipped("no category holds two or more memories");
        };
        // The modulo keeps the cursor inside the owner's list however the
        // list grows.
        let next_cursor = {
            let len = categories.len() as u32;
            let used = categories
                .iter()
                .position(|category| *category == scope.category)
                .unwrap_or(0) as u32;
            Some((used + 1) % len)
        };
        let fingerprint = scope.fingerprint();
        if state.is_unchanged(&fingerprint) {
            return TaskOutcome::Skipped {
                detail: format!("{} unchanged since its last curate run", scope.category),
                cursor: next_cursor,
            };
        }
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
            // The fingerprint recorded is the input this run saw: a manifest
            // that changed rows moves their watermarks, so the next rotation
            // re-reads the consolidated text once and then settles.
            Ok(report) => TaskOutcome::Completed {
                detail: report.describe(),
                cursor: next_cursor,
                fingerprint: Some(fingerprint),
            },
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
            return TaskOutcome::skipped("no unclassified memories".to_owned());
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
            return TaskOutcome::skipped("no unclassified memories".to_owned());
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
            Ok(applied) => TaskOutcome::completed(format!("classified {applied} memories")),
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
            return TaskOutcome::skipped("no recent user messages".to_owned());
        }
        let prompt = render_retrospective_prompt(project, &turns);
        let text = match self.complete(RETROSPECTIVE_SYSTEM_PROMPT, &prompt).await {
            Ok(text) => text,
            Err(failure) => return TaskOutcome::Failed(failure),
        };
        let lesson = match parse_retrospective(&text) {
            Ok(Some(lesson)) => lesson,
            Ok(None) => {
                return TaskOutcome::completed("nothing durable".to_owned());
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
            return TaskOutcome::completed(format!("rejected: {reason:?}"));
        }
        // The store keeps the redacted text, so the exact-match dedup must
        // compare the redacted form too; the write re-redacts it as a no-op.
        let content = redact::redact_secrets(&lesson.content);
        match fact_recorded(&self.db, project, &content).await {
            Ok(false) => {}
            Ok(true) => {
                return TaskOutcome::completed("already recorded".to_owned());
            }
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("check recorded facts", &error));
            }
        }
        match memory::write_memory(&self.db, project, &content, lesson.category).await {
            Ok(id) => TaskOutcome::completed(format!("recorded memory #{id}")),
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
            return TaskOutcome::skipped("no parked notes with a condition".to_owned());
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
            return TaskOutcome::skipped("no parked notes with a condition".to_owned());
        }
        let turns = match recent_user_turns(&self.db, project, state.last_run_at.unwrap_or(0)).await
        {
            Ok(turns) => turns,
            Err(error) => {
                return TaskOutcome::Failed(TaskFailure::host("read recent user turns", &error));
            }
        };
        if turns.is_empty() {
            return TaskOutcome::skipped(
                "no recent activity to check conditions against".to_owned(),
            );
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
            Ok(0) => TaskOutcome::completed("no conditions satisfied".to_owned()),
            Ok(applied) => TaskOutcome::completed(format!("surfaced {applied} notes")),
            Err(error) => TaskOutcome::Failed(TaskFailure::host("apply surfaced notes", &error)),
        }
    }
}
