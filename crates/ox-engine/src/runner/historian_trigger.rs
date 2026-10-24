//! The step-boundary historian trigger: measure the live tail, classify the
//! pressure band, and fold the oldest eligible range into compartments.
//!
//! Two properties are worth knowing when reading the logs. First, the chunker
//! admits an oversize block (a single message bigger than the request budget)
//! only as the first block of a run, so a huge message that follows small ones
//! costs one cheap run to reach it and the next run folds it. Second, the
//! context fence is absolute: when the *only* foldable block is bigger than
//! what the fence leaves room for, no run starts — folding it would leave the
//! model almost no verbatim history, which is worse than waiting. Such a tail
//! usually becomes foldable as it grows (the fence keeps a fixed share of the
//! end, and the fence shrinks as usage rises); a message that dominates the
//! tail can outlast that, and then the overflow path is the fallback. That case
//! logs a warning, because it is the one shape where mctx cannot make progress.
//!
//! Every number here comes from the session's [`Budget`], derived at the step
//! boundary from the model catalogue — never a module constant. A 1M-window
//! model folds in 8.5k-token tail steps and one historian run reads at most a
//! quarter of *its own* window; a 32k model folds much sooner.

use std::sync::Arc;

use anyhow::Result;

use crate::cardinal::chat::Chat;
use crate::mctx::budget::Budget;
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::historian::{self, ChunkLimits};
use crate::mctx::memory;
use crate::runner::drain::DrainCtx;
use crate::runner::step::rows_after_with_seq;
use ox_core::config::Config;
use ox_db::Db;

// ---------------------------------------------------------------------------
// Historian trigger (step boundary)
// ---------------------------------------------------------------------------

/// Fire the historian when the live tail has reached a pressure band.
///
/// Detached on purpose: a run is a full hidden completion (seconds), and the
/// tail still has budget headroom when a band trips — the step that notices the
/// pressure is not the one that must wait for it. The pass in the next step
/// picks up whatever compartments the run published.
///
/// Every failure here is logged and swallowed: context assembly must never fail
/// a turn (upstream's transform has the same contract).
pub(crate) fn mctx_historian(ctx: &DrainCtx, budget: Budget, floor: i64) {
    if !ctx.config.magic_context.enabled {
        return;
    }
    let db = ctx.db.clone();
    let config = ctx.config.clone();
    let chat = Arc::clone(&ctx.chat);
    let bus = ctx.bus.clone();
    let catalog = Arc::clone(&ctx.model_catalog);
    let session_id = ctx.session_id.clone();
    let project = ctx.directory.to_string_lossy().into_owned();
    tokio::spawn(async move {
        let firing = Firing {
            db: &db,
            config: &config,
            chat: &chat,
            catalog: &catalog,
            budget,
            session_id: &session_id,
            project: &project,
        };
        match firing.run(floor).await {
            Ok(Some(published)) => {
                // A transcript notice: a fold is background work with no tool
                // call of its own, so without this the only trace is the head
                // changing under the conversation. `text` is empty — the model
                // already has the compartments — so only the human-facing
                // `description` is set (the TUI renders it as a `◈` row).
                let notice = ox_schema::event::SessionEventData::SessionSynthetic {
                    session_id: session_id.clone(),
                    text: String::new(),
                    description: Some(format!(
                        "historian folded {} · {} compartment{}",
                        published.covered,
                        published.compartments,
                        if published.compartments == 1 { "" } else { "s" }
                    )),
                    metadata: None,
                };
                if let Err(error) = bus.publish(notice).await {
                    tracing::warn!(
                        session = %session_id,
                        error = %error,
                        "publish the fold notice failed"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(session = %session_id, error = %error, "historian run failed");
            }
        }
    });
}

/// The context the assistant asked the provider for, as the provider reported
/// it: `input + cache.read` — the same sum the client's footer divides by the
/// model window. `None` when the tail holds no assistant turn (a fresh session)
/// or the row carries no token counts.
///
/// One measurement, one meaning: the background fold's usage criterion reads
/// the tail through this same function (plan D1), so the two triggers can
/// never disagree about what a turn cost. Its decoded twin is step.rs's
/// `assistant_tokens`; the parity test
/// `token_parity_tests::assistant_tokens_parity_with_raw_prompt_tokens`
/// (bottom of step.rs) locks the two to one meaning per row shape.
pub(crate) fn prompt_tokens(row: &serde_json::Value) -> Option<i64> {
    if row.get("type").and_then(|kind| kind.as_str()) != Some("assistant") {
        return None;
    }
    let tokens = row.get("tokens")?;
    let input = tokens.get("input")?.as_i64()?;
    let cached = tokens
        .get("cache")
        .and_then(|cache| cache.get("read"))
        .and_then(|read| read.as_i64())
        .unwrap_or(0);
    Some(input + cached)
}

/// The background fold's synchronous patch (plan D6): at most one historian
/// run folding the explicit range past `floor`, as far as the chunk budget
/// and the protected-tail fence allow. Returns whether a run published
/// compartments (the floor advanced); `Ok(false)` covers every benign skip —
/// a run already in flight, an empty tail, the fence. Errors are the
/// caller's cue to fall back (the template path), not retried here.
pub(crate) async fn patch_fold(ctx: &DrainCtx, budget: Budget, floor: i64) -> Result<bool> {
    let project = ctx.directory.to_string_lossy();
    let firing = Firing {
        db: &ctx.db,
        config: &ctx.config,
        chat: &ctx.chat,
        catalog: &ctx.model_catalog,
        budget,
        session_id: &ctx.session_id,
        project: &project,
    };
    match firing.patch(floor).await {
        Ok(Some(published)) => {
            tracing::info!(
                session = %ctx.session_id,
                covered = %published.covered,
                compartments = published.compartments,
                "historian patch published"
            );
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(error) => Err(error),
    }
}

/// One firing decision's context: the session, its derived budgets, and the
/// collaborators a run needs. Grouped because they are invariantly the same for
/// the whole decision — only the floor varies — and eight positional arguments
/// invite a swap that still compiles.
struct Firing<'a> {
    db: &'a Db,
    config: &'a Config,
    chat: &'a Arc<dyn Chat>,
    catalog: &'a ox_core::model_catalog::ModelCatalog,
    budget: Budget,
    session_id: &'a str,
    project: &'a str,
}

impl Firing<'_> {
    /// Measure the tail, classify the pressure, pick the chunk and run it.
    async fn run(self, floor: i64) -> Result<Option<Published>> {
        let Self {
            db,
            budget,
            session_id,
            ..
        } = self;
        // One pending run per session: a second step in the same burst must not
        // summarize the same range twice. This pre-check is only a shortcut —
        // it avoids loading the whole tail while a run is in flight; the
        // authoritative claim is `historian::run`'s, which checks and inserts in
        // one write transaction. `settle_running_runs` clears the row a crash
        // leaves behind.
        if historian::has_running_run(db, session_id).await? {
            return Ok(None);
        }
        let rows = rows_after_with_seq(db, session_id, floor).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        // The tail's usage as the provider last reported it — `None` before the
        // session's first completion, when the tail size is the only signal.
        let usage = rows.iter().rev().find_map(|(_, row)| prompt_tokens(row));
        let usage_pct = usage.and_then(|tokens| budget.usage_percentage(tokens));
        // The whole tail's estimate, measured by the chunker itself so the pressure
        // and the budget speak the same units.
        let Some(whole) = historian::chunk(&rows, floor + 1, i64::MAX, ChunkLimits::unbounded())
        else {
            return Ok(None);
        };
        // What a run could actually fold: the tail's real size minus the fence.
        // Both numbers are context tokens (`Chunk::context_tokens`), the unit the
        // provider's usage speaks — a tool result counts by the body it cost, not
        // by the `TC:` line that renders it. Decided before the trigger so a tail
        // that is large but wholly protected (a short session, a huge newest
        // turn) is skipped with a visible reason instead of firing a run that
        // cannot start.
        let protected = budget.protected_tail(usage_pct.unwrap_or(0.0));
        let foldable = whole.context_tokens.saturating_sub(protected as usize);
        if foldable == 0 {
            tracing::debug!(
                session = %session_id,
                tail_tokens = whole.context_tokens,
                protected_tail = protected,
                "historian skipped: nothing outside the protected tail"
            );
            return Ok(None);
        }
        let trigger = historian::trigger(
            usage_pct,
            whole.context_tokens as f64,
            budget.trigger,
            &budget,
        );
        tracing::debug!(
            session = %session_id,
            tail_tokens = whole.context_tokens,
            rendered_tokens = whole.token_estimate,
            // `None` = no completion has reported usage yet; it is not 0%.
            usage_pct = ?usage_pct,
            trigger_budget = budget.trigger,
            trigger = ?trigger,
            messages = rows.len(),
            bytes = whole.text.len(),
            first_ordinal = rows.first().map(|(ordinal, _)| *ordinal),
            last_ordinal = rows.last().map(|(ordinal, _)| *ordinal),
            floor,
            "historian pressure"
        );
        if trigger == historian::Trigger::No {
            return Ok(None);
        }

        // The newest row is never covered: it is the turn in flight. Everything
        // older competes for the chunk budget, which stops the run at the fence.
        let Some((last_ordinal, _)) = rows.last() else {
            return Ok(None);
        };
        let eligible_end = *last_ordinal;
        // The chunk is cut against two limits: the historian's read budget (the
        // rendered text it sends) and the fence (what the region may cost the
        // conversation). Nothing fitting means the oldest eligible block alone
        // would leave less than the fence's worth of verbatim tail; a fold that
        // did that would hand the model a summarized old block plus almost no
        // recent history, so the run waits instead.
        let Some(chunk) = historian::chunk(
            &rows,
            floor + 1,
            eligible_end,
            ChunkLimits {
                request: budget.historian_chunk as usize,
                context: foldable,
            },
        ) else {
            // Under real pressure this is the one shape mctx cannot resolve by
            // itself, so it is an operator signal, not just a trace: a tail
            // dominated by a single block stays unfolded until it grows past the
            // fence. Below the pressure band the same skip is routine (a short
            // session whose whole tail is protected), hence debug.
            let message =
                "historian skipped: the oldest eligible block alone crosses the protected tail";
            if usage_pct.unwrap_or(0.0) >= budget.soft_pct {
                tracing::warn!(
                    session = %session_id,
                    foldable,
                    protected_tail = protected,
                    usage_pct = ?usage_pct,
                    "{message}"
                );
            } else {
                tracing::debug!(
                    session = %session_id,
                    foldable,
                    protected_tail = protected,
                    usage_pct = ?usage_pct,
                    "{message}"
                );
            }
            return Ok(None);
        };
        self.execute(chunk, Some(trigger)).await
    }

    /// One synchronous patch run for the background fold (plan D6): fold the
    /// explicit range past `floor` as far as the chunk budget allows — the
    /// same fence as [`Firing::run`] (never fold into the protected tail) —
    /// but no pressure-band classification, because the caller has already
    /// decided a fold must happen (the checkpoint's recent budget is busted).
    /// One attempt, no retry; the single-flight claim stays `historian::run`'s,
    /// so a detached run already in flight wins and this returns `Ok(None)`.
    async fn patch(self, floor: i64) -> Result<Option<Published>> {
        let Self {
            db,
            budget,
            session_id,
            ..
        } = self;
        // Shortcut, not the authority: the claim below is one statement, so a
        // run started after this check simply loses it and returns `None`.
        if historian::has_running_run(db, session_id).await? {
            return Ok(None);
        }
        let rows = rows_after_with_seq(db, session_id, floor).await?;
        // The newest row stays uncovered, exactly as in [`Firing::run`]: it is
        // the turn in flight, and the fence below protects its neighbourhood.
        let Some((last_ordinal, _)) = rows.last() else {
            return Ok(None);
        };
        let usage = rows.iter().rev().find_map(|(_, row)| prompt_tokens(row));
        let usage_pct = usage.and_then(|tokens| budget.usage_percentage(tokens));
        // The same fence arithmetic as [`Firing::run`]: what a run may fold is
        // the tail's real size minus the protected tail. Here the surviving
        // tail becomes the checkpoint's recent part rather than the model's
        // verbatim budget, but the guard is the same — never fold a session
        // down to nothing verbatim.
        let Some(whole) = historian::chunk(&rows, floor + 1, i64::MAX, ChunkLimits::unbounded())
        else {
            return Ok(None);
        };
        let protected = budget.protected_tail(usage_pct.unwrap_or(0.0));
        let foldable = whole.context_tokens.saturating_sub(protected as usize);
        if foldable == 0 {
            tracing::debug!(
                session = %session_id,
                tail_tokens = whole.context_tokens,
                protected_tail = protected,
                "historian patch skipped: nothing outside the protected tail"
            );
            return Ok(None);
        }
        let Some(chunk) = historian::chunk(
            &rows,
            floor + 1,
            *last_ordinal,
            ChunkLimits {
                request: budget.historian_chunk as usize,
                context: foldable,
            },
        ) else {
            tracing::debug!(
                session = %session_id,
                foldable,
                protected_tail = protected,
                "historian patch skipped: the oldest eligible block alone crosses the protected tail"
            );
            return Ok(None);
        };
        self.execute(chunk, None).await
    }

    /// The prompt assembly and the run itself, shared by the detached
    /// pressure trigger and the background fold's patch path. `trigger` is
    /// logged — `Some` for a pressure-band firing, `None` for a patch run.
    async fn execute(
        self,
        chunk: historian::Chunk,
        trigger: Option<historian::Trigger>,
    ) -> Result<Option<Published>> {
        let Firing {
            db,
            config,
            chat,
            catalog,
            session_id,
            project,
            ..
        } = self;
        let compartments = compartments::load(db, session_id).await?;
        let references: Vec<historian::ReferenceCompartment> = compartments
            .iter()
            .map(Compartment::reference_input)
            .collect();
        let blocks =
            historian::build_reference_blocks(session_id, chunk.start_ordinal, &references);
        let memories = memory::active_memories(db, project).await?;
        let memory_block = historian::render_historian_memory_block(&memories);
        let prompt = historian::CompartmentPromptInputs {
            seed_examples: &blocks.seed_examples,
            session_references: &blocks.session_references,
            project_memory: &memory_block,
            input_source: &chunk.text,
            memory_enabled: true,
            // The dreamer writes `<user_observations>` into the pinboard, so they
            // are only worth asking for when it can run.
            user_observations_requested: config.magic_context.dreamer_enabled,
            extraction_free: false,
        };
        let run_ctx = historian::RunCtx {
            db,
            config,
            chat,
            catalog,
        };
        let plan = historian::RunPlan {
            session_id,
            chunk: &chunk,
            attempt: 1,
            prompt,
        };
        tracing::debug!(
            session = %session_id,
            chunk_start = chunk.start_ordinal,
            chunk_end = chunk.end_ordinal,
            chunk_tokens = chunk.token_estimate,
            present = chunk.present_ordinals.len(),
            "historian chunk"
        );
        let Some(outcome) = historian::run(&run_ctx, &plan).await? else {
            // Another run holds the claim; it will publish, and the next pass
            // picks the compartments up.
            return Ok(None);
        };
        tracing::info!(
            session = %session_id,
            trigger = ?trigger,
            run = %outcome.run_id,
            covered = format!("{}..{}", chunk.start_ordinal, chunk.end_ordinal),
            compartments = outcome.compartment_ids.len(),
            "historian published"
        );
        Ok(Some(Published {
            covered: format!("{}..{}", chunk.start_ordinal, chunk.end_ordinal),
            compartments: outcome.compartment_ids.len(),
        }))
    }
}

/// What one published run folded, for the transcript notice.
struct Published {
    /// Ordinal range the run covered (`a..b`).
    covered: String,
    /// Compartments the run published.
    compartments: usize,
}
