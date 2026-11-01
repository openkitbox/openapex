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

use crate::mctx::budget::Budget;
use crate::mctx::calibration;
use crate::mctx::compartments::{self, Compartment};
use crate::mctx::historian::{self, ChunkLimits};
use crate::mctx::memory;
use crate::mctx::tokens;
use crate::runner::drain::DrainCtx;
use crate::runner::step::{
    compaction_boundary, load_overlaid_tail_with_ordinals, rows_after_with_seq,
};
use ox_core::config::Config;
use ox_db::Db;
use ox_llm::cardinal::chat::Chat;
use ox_llm::cardinal::wire::ToolDef;
use ox_schema::message::Message;

// ---------------------------------------------------------------------------
// Historian trigger (step boundary)
// ---------------------------------------------------------------------------

/// The live step request a fold can continue (plan A3): the bytes the provider
/// already holds for this conversation, so the historian's hidden completion
/// re-reads them at the prompt-cache price instead of paying a full miss.
///
/// Owned, not borrowed: the fold is detached and outlives the step that fired
/// it. `system_static` and `instructions` are `Arc<str>` so handing them to that
/// task is a refcount bump, not a copy of the static layer on every step
/// boundary — the step made this allocation anyway, and the common case is a
/// fold that never starts.
pub(crate) struct PrefixContext {
    /// The request's first message: the static system layer.
    pub(crate) system_static: Arc<str>,
    /// The request's second message: the session's initial instruction render.
    pub(crate) instructions: Arc<str>,
    /// The frozen head halves (`m[0]`, `m[1]`), as the live request sent them.
    pub(crate) head_m0: Option<String>,
    pub(crate) head_m1: Option<String>,
    /// The session's model id: a hidden completion that routes anywhere else
    /// cannot reuse these bytes, because the prefix is model-scoped.
    pub(crate) model_id: String,
    /// The tool definitions the step request advertises
    /// (`ctx.tools.definitions_shared()`, the same `Arc` the request itself
    /// carries). They are part of the prefix: providers render the tools array
    /// ahead of the system prompt and the messages, so a fold that offered none
    /// would share no cached prefix at all.
    pub(crate) tools: Arc<Vec<ToolDef>>,
}

/// Fire the historian when the live tail has reached a pressure band.
///
/// Detached on purpose: a run is a full hidden completion (seconds), and the
/// tail still has budget headroom when a band trips — the step that notices the
/// pressure is not the one that must wait for it. The pass in the next step
/// picks up whatever compartments the run published.
///
/// `live` is the step request's own prefix, handed over so a run can continue
/// it rather than re-send the conversation as a flat chunk. The fail-open rule
/// lives in [`request_shape`]: anything that cannot reuse the prefix byte for
/// byte sends the flat shape instead.
///
/// Every failure here is logged and swallowed: context assembly must never fail
/// a turn (upstream's transform has the same contract).
pub(crate) fn mctx_historian(
    ctx: &DrainCtx,
    budget: Budget,
    floor: i64,
    reasoning_effort: Option<String>,
    live: PrefixContext,
) {
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
    // Resolved here, not inside the detached task: the measurement is the
    // session's current value at the step boundary that decided to fire.
    let ratio = calibration::ratio_for(&ctx.calibrations, &ctx.session_id);
    tokio::spawn(async move {
        let firing = Firing {
            db: &db,
            config: &config,
            chat: &chat,
            catalog: &catalog,
            budget,
            session_id: &session_id,
            project: &project,
            ratio,
            reasoning_effort,
            prefix: Some(live),
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
/// `step::tests::assistant_tokens_parity_with_raw_prompt_tokens`
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

/// The tail's total `session_message.data` size in bytes, above `floor`.
/// One aggregate row: the rows stream through the storage engine without
/// materializing here.
async fn tail_bytes(db: &Db, session_id: &str, floor: i64) -> Result<i64> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(SUM(LENGTH(CAST(data AS BLOB))), 0) FROM session_message \
             WHERE session_id = ?1 AND seq > ?2",
        )
        .await?;
    let mut rows = stmt.query((session_id, floor)).await?;
    match rows.next().await? {
        Some(row) => Ok(row.get::<i64>(0)?),
        None => Ok(0),
    }
}

/// The usage number the gate will compare against the soft band — the same
/// number the full evaluation's back-scan derives (`rows.rev().find_map`),
/// read without loading the tail. The third state is the honesty valve: a
/// back-scan that exhausts its bounded window without finding tokens has NOT
/// proven "no usage" — an older row could still report, and only the full
/// load can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewestUsage {
    /// The newest assistant row above the floor that reports tokens carries
    /// this `input + cache.read`.
    Reported(i64),
    /// No assistant row above the floor at all: the full path's back-scan
    /// finds nothing either, so the byte bound alone decides.
    NoneReported,
    /// Assistant rows exist but none of the newest
    /// [`GATE_BACKSCAN_ROWS`] carries tokens. Never a skip.
    Ambiguous,
}

/// How many newest assistant rows the gate's usage back-scan may decode
/// before it gives up and lets the full evaluation look further. One row
/// answers in the common case; a run of this many tokenless assistant rows
/// means failed steps in a row, where paying the full load is the old
/// behavior anyway.
const GATE_BACKSCAN_ROWS: i64 = 8;

/// The usage the newest assistant rows reported, above `floor` — the same
/// number the full evaluation derives from the loaded rows (back-scan to the
/// newest row carrying tokens), read here without loading them. The `type`
/// column filters (the row JSON does not carry it reliably), so it is
/// injected before the shared extractor runs.
async fn newest_reported_usage(db: &Db, session_id: &str, floor: i64) -> Result<NewestUsage> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT data FROM session_message \
             WHERE session_id = ?1 AND type = 'assistant' AND seq > ?2 \
             ORDER BY seq DESC LIMIT {GATE_BACKSCAN_ROWS}"
        ))
        .await?;
    let mut rows = stmt.query((session_id, floor)).await?;
    let mut saw_rows = false;
    while let Some(row) = rows.next().await? {
        saw_rows = true;
        let mut data: serde_json::Value = serde_json::from_str(&row.get::<String>(0)?)?;
        data["type"] = serde_json::json!("assistant");
        if let Some(tokens) = prompt_tokens(&data) {
            return Ok(NewestUsage::Reported(tokens));
        }
    }
    Ok(if saw_rows {
        NewestUsage::Ambiguous
    } else {
        NewestUsage::NoneReported
    })
}

/// Whether the trigger's full evaluation could only answer [`historian::Trigger::No`],
/// which makes the whole-tail load unnecessary.
///
/// An ambiguous usage scan never skips: uncertainty is the full load's cue.
/// A usage at or above the soft band forces an evaluation (the band alone
/// decides Speculative/Forced, and the chunk selection and fence math need
/// the rows). Otherwise the tail's raw JSON byte length decides: it embeds
/// every byte the rendered text carries plus keys and escaping, so the
/// chunker's estimate — bytes of rendered text over four, scaled by the
/// session's provider ratio — is bounded above by `ratio × tail_bytes / 4`.
/// Strictly below the trigger budget, the estimate cannot exceed the trigger
/// and the decision is No; the bound is deliberately strict so a tail at the
/// budget still loads.
fn tail_load_skippable(usage: NewestUsage, tail_bytes: i64, budget: &Budget, ratio: f64) -> bool {
    match usage {
        NewestUsage::Ambiguous => return false,
        NewestUsage::Reported(tokens) => {
            if budget
                .usage_percentage(tokens)
                .is_some_and(|pct| pct >= budget.soft_pct)
            {
                return false;
            }
        }
        NewestUsage::NoneReported => {}
    }
    (tail_bytes as f64) / 4.0 * ratio < f64::from(budget.trigger)
}

/// The background fold's synchronous patch (plan D6): at most one historian
/// run folding the explicit range past `floor`, as far as the chunk budget
/// and the protected-tail fence allow. Returns whether a run published
/// compartments (the floor advanced); `Ok(false)` covers every benign skip —
/// a run already in flight, an empty tail, the fence. Errors are the
/// caller's cue to fall back (the template path), not retried here.
///
/// This path sends the flat shape: it runs from the compaction drain, which has
/// no step request in hand (see [`FlatReason::NoLiveContext`]). Reassembling one
/// here — a second `prompt::system_static`, a second baseline read — would be a
/// second renderer of the bytes the next step sends, so the patch keeps the
/// fail-open request instead.
pub(crate) async fn patch_fold(
    ctx: &DrainCtx,
    budget: Budget,
    floor: i64,
    reasoning_effort: Option<String>,
) -> Result<bool> {
    let project = ctx.directory.to_string_lossy();
    let firing = Firing {
        db: &ctx.db,
        config: &ctx.config,
        chat: &ctx.chat,
        catalog: &ctx.model_catalog,
        budget,
        session_id: &ctx.session_id,
        project: &project,
        ratio: calibration::ratio_for(&ctx.calibrations, &ctx.session_id),
        reasoning_effort,
        prefix: None,
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

/// Which request shape a historian run sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestShape {
    /// The live request's own bytes, then the historian's task tail: the shape
    /// an endpoint with automatic prefix caching bills at the cache price.
    Prefixed,
    /// The independent two-message request: the chunk is rendered into the
    /// prompt and no prefix is reused. This is the fail-open shape, and the
    /// reason says why a run fell back to it.
    FlatChunk(FlatReason),
}

/// Why a run sends the flat shape. Each variant is a distinct operator signal:
/// two are routine, three mean the feature could not do its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlatReason {
    /// `magic_context.historian_prefix_cache = false`: expected, quiet.
    ConfiguredOff,
    /// The calling path never held a step request (the background fold's patch).
    /// Structural, not a degradation.
    NoLiveContext,
    /// The hidden completion is not routed to the session's model, so the
    /// prefix it would reuse belongs to a different model's byte history.
    ModelDiffers,
    /// The live render starts after the fold's last ordinal — a completed
    /// compaction row sits above it — so the region has nothing the model was
    /// shown, let alone a shared prefix.
    FoldBelowLiveStart,
    /// The fold's region renders no transcript at all: there is no conversation
    /// above the instruction to fold.
    EmptyRegion,
}

impl FlatReason {
    /// The reason as a log field value.
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            FlatReason::ConfiguredOff => "historian_prefix_cache is off",
            FlatReason::NoLiveContext => "no live step request on this path",
            FlatReason::ModelDiffers => {
                "the hidden completion is not routed to the session's model"
            }
            FlatReason::FoldBelowLiveStart => {
                "the live render starts after the fold's last ordinal"
            }
            FlatReason::EmptyRegion => "the fold's region renders no transcript",
        }
    }

    /// Whether falling back is a degradation worth a warning. A switch the
    /// operator turned off and a path that never had a prefix are how the system
    /// is supposed to work; a lost prefix on a path that had one is not.
    #[must_use]
    fn is_degradation(self) -> bool {
        match self {
            FlatReason::ConfiguredOff | FlatReason::NoLiveContext => false,
            FlatReason::ModelDiffers | FlatReason::FoldBelowLiveStart | FlatReason::EmptyRegion => {
                true
            }
        }
    }
}

/// The facts [`request_shape`] reads. Named fields, because five positional
/// flags at a call site are a swap waiting to happen.
#[derive(Debug, Clone, Copy)]
struct ShapeFacts {
    /// `magic_context.historian_prefix_cache`.
    switch_on: bool,
    /// Whether the caller held the step request's prefix.
    live_context: bool,
    /// Whether the hidden completion resolves to the session's own model.
    same_model: bool,
    /// Whether the live render starts past the fold's last ordinal
    /// ([`region_start`]). Read before `region_len` because a boundary above the
    /// fold also empties the region, and the two are different signals: one is a
    /// checkpoint the fold has not reached, the other a fold with nothing to
    /// fold. Only the caller that loaded the region can answer it.
    fold_below_live_start: bool,
    /// The region's row count. Only read once the checks above hold: a caller
    /// that skipped the load passes 0, and an earlier check decides.
    region_len: usize,
}

/// Which shape a run sends, decided by the first failing precondition — in the
/// order the checks cost (a switch and a caller's context are free, the model is
/// a config/catalog lookup, the region is a tail load).
///
/// Pure on purpose: the whole fallback ladder is testable without a session, and
/// the caller keeps the only side effects — the load and the log line.
#[must_use]
fn request_shape(facts: ShapeFacts) -> RequestShape {
    if !facts.switch_on {
        return RequestShape::FlatChunk(FlatReason::ConfiguredOff);
    }
    if !facts.live_context {
        return RequestShape::FlatChunk(FlatReason::NoLiveContext);
    }
    if !facts.same_model {
        return RequestShape::FlatChunk(FlatReason::ModelDiffers);
    }
    if facts.fold_below_live_start {
        return RequestShape::FlatChunk(FlatReason::FoldBelowLiveStart);
    }
    if facts.region_len == 0 {
        return RequestShape::FlatChunk(FlatReason::EmptyRegion);
    }
    RequestShape::Prefixed
}

/// The ordinal the fold's region starts at: where the live render starts, never
/// below `floor + 1`.
///
/// The live render does not begin at the history floor. `build_chat_messages`
/// starts at the newest *completed compaction* row (`compaction_boundary`), so
/// a checkpoint above the fold makes a region starting at `floor + 1` summarize
/// rows the model was never shown — and share no prefix with the request it is
/// supposed to continue. The boundary is reported as an index into the rows it
/// was given, so the index is mapped back through those very rows; index 0 means
/// "no completed checkpoint in the tail", which the render reads as "start at
/// the first row", and an empty tail keeps the floor.
///
/// `rows` are the live tail's `(ordinal, message)` pairs, in ordinal order.
#[must_use]
fn region_start(rows: &[(i64, Message)], floor: i64) -> i64 {
    let boundary = compaction_boundary(rows.iter().map(|(_, message)| message));
    let start = rows
        .get(boundary)
        .map_or(floor + 1, |(ordinal, _)| *ordinal);
    start.max(floor + 1)
}

/// The fold's region: the rows inside `start..=end`, moved out of the loaded
/// tail rather than copied.
///
/// Empty when `start > end` — the live render starts past the fold's last
/// ordinal — which is the case [`FlatReason::FoldBelowLiveStart`] names.
fn region_rows(rows: Vec<(i64, Message)>, start: i64, end: i64) -> Vec<Message> {
    rows.into_iter()
        .filter(|(ordinal, _)| (start..=end).contains(ordinal))
        .map(|(_, message)| message)
        .collect()
}

/// One firing decision's context: the session, its derived budgets, and the
/// collaborators a run needs. Grouped because they are invariantly the same for
/// the whole decision — only the floor varies — and eight positional arguments
/// invite a swap that still compiles.
struct Firing<'a> {
    db: &'a Db,
    config: &'a Config,
    chat: &'a Arc<dyn Chat>,
    catalog: &'a ox_llm::model_catalog::ModelCatalog,
    budget: Budget,
    session_id: &'a str,
    project: &'a str,
    /// The session's provider ÷ local token ratio
    /// ([`crate::mctx::calibration`]): every count this decision takes is scaled
    /// by it, because both `ChunkLimits` ceilings are in provider tokens.
    ratio: f64,
    /// Snapshot of the owning session's reasoning policy for this firing.
    reasoning_effort: Option<String>,
    /// The step request this firing may continue; `None` on the background
    /// fold's patch path, which has no step request in hand.
    prefix: Option<PrefixContext>,
}

impl Firing<'_> {
    /// Measure the tail, classify the pressure, pick the chunk and run it.
    async fn run(self, floor: i64) -> Result<Option<Published>> {
        let Self {
            db,
            budget,
            session_id,
            ratio,
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
        // Cheap pre-gate: size the tail without materializing it. Loading the
        // whole tail (raw rows plus the rendered text) is the trigger's
        // largest transient allocation, and it is paid on every step boundary
        // even when the decision is No. The aggregate and the newest
        // assistant row answer "could this possibly fire?" in O(1) memory;
        // only a tail that might fire pays the full load below.
        let (tail_bytes, gate_usage) = tokio::try_join!(
            tail_bytes(db, session_id, floor),
            newest_reported_usage(db, session_id, floor)
        )?;
        if tail_load_skippable(gate_usage, tail_bytes, &budget, ratio) {
            tracing::debug!(
                session = %session_id,
                tail_bytes,
                "historian skipped: tail far below the trigger budget"
            );
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
        let count = tokens::counter_fn(ratio);
        let Some(whole) =
            historian::chunk(&rows, floor + 1, i64::MAX, ChunkLimits::unbounded(), &count)
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
            &count,
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
        self.execute(chunk, Some(trigger), floor).await
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
            ratio,
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
        let count = tokens::counter_fn(ratio);
        let Some(whole) =
            historian::chunk(&rows, floor + 1, i64::MAX, ChunkLimits::unbounded(), &count)
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
            &count,
        ) else {
            tracing::debug!(
                session = %session_id,
                foldable,
                protected_tail = protected,
                "historian patch skipped: the oldest eligible block alone crosses the protected tail"
            );
            return Ok(None);
        };
        self.execute(chunk, None, floor).await
    }

    /// The prompt assembly and the run itself, shared by the detached
    /// pressure trigger and the background fold's patch path. `trigger` is
    /// logged — `Some` for a pressure-band firing, `None` for a patch run.
    async fn execute(
        self,
        chunk: historian::Chunk,
        trigger: Option<historian::Trigger>,
        floor: i64,
    ) -> Result<Option<Published>> {
        let Firing {
            db,
            config,
            chat,
            catalog,
            session_id,
            project,
            reasoning_effort,
            prefix: live,
            ..
        } = self;
        let compartments = compartments::load(db, session_id).await?;
        let references: Vec<historian::ReferenceCompartment<'_>> = compartments
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
            reasoning_effort,
        };
        // The request shape, decided before the region is loaded because only a
        // prefix-shaped run pays for it. `same_model` reads the same resolver the
        // run itself uses, so the two can never disagree about the route.
        let switch_on = config.magic_context.historian_prefix_cache;
        let live = live.as_ref();
        let same_model = live.is_some_and(|live| {
            historian::historian_model(config, catalog).is_ok_and(|model| model == live.model_id)
        });
        let (region, fold_below_live_start) = if switch_on && live.is_some() && same_model {
            // THE REGION MUST START WHERE THE LIVE RENDER STARTS, and `floor` is
            // not that point. `build_chat_messages` skips rows it cannot render,
            // but every row still advances its temporal-marker cursor, so a slice
            // that starts later loses the `<!-- +2h -->` marker the live request
            // rendered and stops being a byte prefix of what the provider cached
            // — the whole point of this shape. The render also skips everything
            // up to the newest *completed compaction* row, which can sit above
            // `floor`; [`region_start`] maps that boundary back to an ordinal over
            // the same rows the live request decided it over, so the whole tail is
            // loaded here (exactly what the live step loaded) rather than the
            // chunk's window.
            let tail = load_overlaid_tail_with_ordinals(db, session_id, floor).await?;
            let start = region_start(&tail, floor);
            // The other side of the window: the historian may not read ordinals
            // the fold does not cover.
            let end = *chunk.covered.end();
            (region_rows(tail, start, end), start > end)
        } else {
            (Vec::new(), false)
        };
        let shape = request_shape(ShapeFacts {
            switch_on,
            live_context: live.is_some(),
            same_model,
            fold_below_live_start,
            region_len: region.len(),
        });
        let prefix = match shape {
            RequestShape::Prefixed => live.map(|live| historian::Prefix {
                system_static: live.system_static.as_ref(),
                instructions: live.instructions.as_ref(),
                head: [live.head_m0.as_deref(), live.head_m1.as_deref()],
                region: &region,
                tools: &live.tools,
            }),
            RequestShape::FlatChunk(reason) => {
                if reason.is_degradation() {
                    tracing::warn!(
                        session = %session_id,
                        reason = reason.as_str(),
                        "historian sends the flat chunk instead of the cached prefix"
                    );
                } else {
                    tracing::debug!(
                        session = %session_id,
                        reason = reason.as_str(),
                        "historian sends the flat chunk"
                    );
                }
                None
            }
        };
        let plan = historian::RunPlan {
            session_id,
            project,
            chunk: &chunk,
            attempt: 1,
            prompt,
            prefix,
        };
        tracing::debug!(
            session = %session_id,
            chunk_start = chunk.start_ordinal,
            chunk_end = chunk.end_ordinal,
            chunk_tokens = chunk.token_estimate,
            present = chunk.present_ordinals.len(),
            shape = ?shape,
            region = region.len(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(
        switch_on: bool,
        live_context: bool,
        same_model: bool,
        region_len: usize,
    ) -> RequestShape {
        request_shape(ShapeFacts {
            switch_on,
            live_context,
            same_model,
            fold_below_live_start: false,
            region_len,
        })
    }

    /// The default is a behavior, not an implementation detail: the independent
    /// chunk shape already measures 95–97% cache reuse on DeepSeek-V4.1-Flash,
    /// while the prefixed shape sends ~1.7× the prompt tokens there. Flipping
    /// this default must be a deliberate decision with new measurements.
    #[test]
    fn the_prefixed_shape_is_opt_in() {
        assert!(
            !ox_core::config::MctxConfig::default().historian_prefix_cache,
            "the prefixed historian shape must stay opt-in"
        );
    }

    #[test]
    fn every_precondition_must_hold_for_the_prefix_shape() {
        assert_eq!(
            shape(true, true, true, 1),
            RequestShape::Prefixed,
            "one rendered row is enough to be a prefix of the live request"
        );
        assert_eq!(
            shape(false, true, true, 1),
            RequestShape::FlatChunk(FlatReason::ConfiguredOff),
            "the switch is absolute: a disabled prefix cache never sends one"
        );
        assert_eq!(
            shape(true, false, true, 1),
            RequestShape::FlatChunk(FlatReason::NoLiveContext),
            "a path without the step request cannot invent a prefix"
        );
        assert_eq!(
            shape(true, true, false, 1),
            RequestShape::FlatChunk(FlatReason::ModelDiffers),
            "the cached prefix is model-scoped, so another model cannot reuse it"
        );
        assert_eq!(
            shape(true, true, true, 0),
            RequestShape::FlatChunk(FlatReason::EmptyRegion),
            "an empty region leaves nothing above the instruction to fold"
        );
    }

    #[test]
    fn the_first_failing_precondition_names_the_reason() {
        // Every combination of failing preconditions reports the earliest one,
        // so a log line always names the check that actually decided.
        assert_eq!(
            shape(false, false, false, 0),
            RequestShape::FlatChunk(FlatReason::ConfiguredOff),
            "the switch is checked before the caller's context"
        );
        assert_eq!(
            shape(true, false, false, 0),
            RequestShape::FlatChunk(FlatReason::NoLiveContext),
            "the caller's context is checked before the model route"
        );
        assert_eq!(
            shape(true, true, false, 0),
            RequestShape::FlatChunk(FlatReason::ModelDiffers),
            "the model route is checked before the region is measured"
        );
    }

    #[test]
    fn only_a_lost_prefix_on_a_path_that_had_one_is_a_degradation() {
        // The two routine fallbacks stay at debug: an operator's switch and a
        // path that never held a request are how the system is meant to work.
        assert!(!FlatReason::ConfiguredOff.is_degradation());
        assert!(!FlatReason::NoLiveContext.is_degradation());
        // These three are the feature failing to do its job, so they warn.
        assert!(FlatReason::ModelDiffers.is_degradation());
        assert!(FlatReason::FoldBelowLiveStart.is_degradation());
        assert!(FlatReason::EmptyRegion.is_degradation());
    }

    #[test]
    fn a_checkpoint_above_the_fold_falls_open_with_its_own_reason() {
        let facts = ShapeFacts {
            switch_on: true,
            live_context: true,
            same_model: true,
            fold_below_live_start: true,
            region_len: 0,
        };
        assert_eq!(
            request_shape(facts),
            RequestShape::FlatChunk(FlatReason::FoldBelowLiveStart),
            "a checkpoint past the fold is a different signal from a fold with nothing above it"
        );
        assert_eq!(
            request_shape(ShapeFacts {
                fold_below_live_start: false,
                ..facts
            }),
            RequestShape::FlatChunk(FlatReason::EmptyRegion),
            "without a checkpoint above it, an empty region is what the log should say"
        );
        assert_eq!(
            request_shape(ShapeFacts {
                same_model: false,
                ..facts
            }),
            RequestShape::FlatChunk(FlatReason::ModelDiffers),
            "the model route is still decided before the region"
        );
    }

    // -----------------------------------------------------------------------
    // Region window
    // -----------------------------------------------------------------------

    /// A user row with a frozen timestamp: two seconds apart, so no temporal
    /// marker is rendered and the assertions stay about the window.
    fn user_row(ordinal: i64) -> (i64, Message) {
        let value = serde_json::json!({
            "id": format!("msg_{ordinal}"),
            "type": "user",
            "time": { "created": ordinal * 1_000 },
            "text": format!("prompt {ordinal}"),
        });
        (
            ordinal,
            crate::runner::step::decode_row(&value).expect("the user fixture decodes"),
        )
    }

    /// A *completed* compaction row: the row `build_chat_messages` starts the
    /// live render at. Built through the production decoder so it has exactly
    /// the shape a stored row has.
    fn completed_compaction(ordinal: i64) -> (i64, Message) {
        let value = serde_json::json!({
            "id": format!("msg_{ordinal}"),
            "type": "compaction",
            "time": { "created": ordinal * 1_000 },
            "status": "completed",
            "reason": "manual",
            "summary": "the checkpoint",
            "recent": "what survived it",
        });
        (
            ordinal,
            crate::runner::step::decode_row(&value).expect("the compaction fixture decodes"),
        )
    }

    #[test]
    fn the_region_starts_at_the_floor_without_a_checkpoint() {
        let tail = vec![user_row(4), user_row(5), user_row(6)];
        assert_eq!(
            region_start(&tail, 3),
            4,
            "with no completed checkpoint the live render starts at the first row above the floor"
        );
    }

    #[test]
    fn a_checkpoint_above_the_fold_moves_the_region_start() {
        // The fold ended at 4, but the live render begins at the completed
        // checkpoint at 5: summarizing from the floor would cover a row the model
        // was never shown, and share no prefix with the request it continues.
        let tail = vec![user_row(4), completed_compaction(5), user_row(6)];
        let start = region_start(&tail, 3);
        assert_eq!(
            start, 5,
            "the region must start at the checkpoint, not at the floor"
        );
        let region = region_rows(tail, start, 4);
        assert!(
            region.is_empty(),
            "a checkpoint past the fold's last ordinal leaves the region empty, got {} rows",
            region.len()
        );
    }

    #[test]
    fn the_region_is_the_window_the_fold_covers() {
        let tail = vec![user_row(1), user_row(2), user_row(3), user_row(4)];
        let region = region_rows(tail, 2, 3);
        let rendered =
            crate::runner::step::build_chat_messages("static layer", "", &[None, None], &region);
        let users: Vec<&str> = rendered
            .iter()
            .filter_map(|message| match message {
                ox_llm::cardinal::wire::ChatMessage::User { content } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            users,
            ["prompt 2", "prompt 3"],
            "the region must hold exactly the rows inside the fold's window"
        );
    }
}
