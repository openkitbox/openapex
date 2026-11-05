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
//!
//! One pressure event may fold several chunks back to back, until the
//! low-water target ([`Budget::target`]). Two details make that loop honest:
//! the trigger measures the **foldable** part of the tail (the whole tail minus
//! the protected fence), not the whole tail — the fence alone used to keep the
//! trigger armed, so every step boundary folded again; and the post-fold
//! estimate is `reported usage − Σ chunk.context_tokens`, because the
//! provider's usage number is stale until the next completion and re-reading it
//! would show the same pressure forever. That estimate is a lower bound: it
//! does not count the compartment summaries added back into `m[0]`.
//! [`MAX_FOLDS_PER_EVENT`] bounds what one event may spend. Each pass
//! deliberately re-renders the remaining tail in memory, up to four times per
//! event; the trade is repeated estimation work for correctness, not a memory
//! leak.

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

/// How many folds one pressure event may run back to back. Four times the
/// historian's chunk budget (32k rendered tokens by default) covers a descent
/// from a 139k-token prompt to the 73k low-water target of a 262,144-token
/// window; the cap is what bounds the hidden-completion cost of a single event
/// when the tail keeps offering foldable history.
const MAX_FOLDS_PER_EVENT: usize = 4;

/// Whether one pressure event folds another chunk or stops, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldStep {
    /// Fold the next chunk: pressure, foldable history, and headroom above the
    /// low-water target all remain.
    Fold,
    /// Stop the event; the reason names which condition ended it.
    Stop(FoldStop),
}

/// Why a pressure event stopped folding. Each variant is a distinct log signal:
/// only some mean "there was nothing to do".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
enum FoldStop {
    /// The event has spent [`MAX_FOLDS_PER_EVENT`] folds. Anything left waits
    /// for the next step boundary rather than making one event unbounded.
    Cap,
    /// The foldable tail is below both pressure bands: nothing is large enough
    /// to be worth another hidden completion.
    NoPressure,
    /// Everything left sits inside the protected tail, which a fold may never
    /// touch — the model keeps what it is working on verbatim.
    NothingFoldable,
    /// The post-fold estimate is at or below the low-water target: the event
    /// has done its job and further folds would only buy prompt-cache misses.
    TargetReached,
    /// No completion has reported usage, so the target cannot be measured
    /// against anything. The first pass still folds once, exactly as before
    /// the loop; a later pass stops here.
    NoUsage,
}

/// The facts [`fold_step`] reads. Named fields keep the trigger, remaining
/// estimate, target, and fold count from being swapped at a call site.
#[derive(Debug, Clone, Copy)]
struct FoldInput {
    /// Current pressure classification.
    trigger: historian::Trigger,
    /// Context tokens outside the protected tail.
    foldable: usize,
    /// Post-fold estimate (`reported usage − Σ folded context tokens`).
    remaining: Option<i64>,
    /// Low-water target; `None` means there is no low-water stop.
    target: Option<u32>,
    /// Folds already completed in this pressure event.
    folds: usize,
}

/// Decide the next step of a pressure event.
///
/// Pure on purpose: the whole stop ladder is testable without a session, and
/// the caller keeps the side effects — the render, the run, the log line. The
/// check order is the contract: the cap and the cheap gates decide before the
/// target arithmetic. Without usage the first pass folds once; only a later
/// pass stops, because the target still cannot be consulted.
#[must_use]
fn fold_step(input: FoldInput) -> FoldStep {
    if input.folds >= MAX_FOLDS_PER_EVENT {
        return FoldStep::Stop(FoldStop::Cap);
    }
    if input.trigger == historian::Trigger::No {
        return FoldStep::Stop(FoldStop::NoPressure);
    }
    if input.foldable == 0 {
        return FoldStep::Stop(FoldStop::NothingFoldable);
    }
    let Some(remaining) = input.remaining else {
        if input.folds == 0 {
            return FoldStep::Fold;
        }
        return FoldStep::Stop(FoldStop::NoUsage);
    };
    if input
        .target
        .is_some_and(|target| remaining <= i64::from(target))
    {
        return FoldStep::Stop(FoldStop::TargetReached);
    }
    FoldStep::Fold
}

/// The accounting of one pressure event.
///
/// A pressure event may fold several chunks back to back. This type owns the
/// numbers that event has to stay honest about — how deep it is, which ordinals
/// it covered, how many compartments it published, and what the prompt is
/// estimated to be after those folds — so the loop body stays orchestration and
/// the invariants sit in one place: the floor only advances, the aggregate range
/// is contiguous, and a fold that published nothing cannot grow the count.
///
/// The estimate is a lower bound: a fold adds its compartment summary back into
/// `m[0]`, which the subtraction does not account for, so an event may stop
/// slightly early and never late — the cap still bounds it.
struct EventFolds {
    /// The floor the next chunk starts above.
    floor: i64,
    /// Folds published so far in this event.
    folds: usize,
    /// The ordinal range the event folded, `first..last`.
    covered: Option<(i64, i64)>,
    /// Compartments the event published.
    compartments: usize,
    /// The low-water target; `None` when the model window is unknown, which
    /// means there is no low-water stop rather than "fold once".
    target: Option<u32>,
    /// Estimated prompt after the folds so far. `None` when no completion has
    /// reported a usage number: the event then folds once, because there is
    /// nothing to subtract the folds from.
    remaining: Option<i64>,
}

impl EventFolds {
    /// Start an event at `floor` with `remaining` as the estimated prompt.
    fn new(floor: i64, target: Option<u32>, remaining: Option<i64>) -> Self {
        Self {
            floor,
            folds: 0,
            covered: None,
            compartments: 0,
            target,
            remaining,
        }
    }

    /// The facts [`fold_step`] reads for the pass about to run.
    fn fold_input(&self, trigger: historian::Trigger, foldable: usize) -> FoldInput {
        FoldInput {
            trigger,
            foldable,
            remaining: self.remaining,
            target: self.target,
            folds: self.folds,
        }
    }

    /// Record one published fold: advance the floor, extend the range, add the
    /// compartments, and subtract what the fold removed from the estimate.
    fn record(&mut self, start: i64, executed: &Executed) {
        // A fold may not move the floor backwards: the chunker cuts above the
        // requested start, so a violation means the coverage bookkeeping broke,
        // and carrying on would re-fold or skip ordinals.
        debug_assert!(
            executed.covered_end >= self.floor,
            "a fold must advance the floor: covered_end {} is below the floor {}",
            executed.covered_end,
            self.floor
        );
        self.floor = executed.covered_end;
        self.folds += 1;
        self.compartments += executed.compartments;
        self.covered = Some(match self.covered {
            None => (start, executed.covered_end),
            Some((first, _)) => (first, executed.covered_end),
        });
        self.remaining = self
            .remaining
            .map(|remaining| remaining.saturating_sub(executed.context_tokens as i64));
    }

    /// The floor the next chunk starts above.
    fn floor(&self) -> i64 {
        self.floor
    }

    /// Folds published so far in this event.
    fn folds(&self) -> usize {
        self.folds
    }

    /// The estimated prompt after the folds so far.
    fn remaining(&self) -> Option<i64> {
        self.remaining
    }

    /// The ordinal range the event folded, `first..last`.
    fn covered(&self) -> Option<(i64, i64)> {
        self.covered
    }

    /// What the event folded, as the transcript notice reports it: one row for
    /// the whole event, not one per chunk.
    #[must_use]
    fn published(self) -> Option<Published> {
        let (first, last) = self.covered?;
        Some(Published {
            covered: format!("{first}..{last}"),
            compartments: self.compartments,
        })
    }
}

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
    /// Measure the tail, classify the pressure, and fold down to the low-water
    /// target: one pressure event may run several folds back to back.
    ///
    /// Each pass re-measures the tail above the current floor, so the trigger
    /// and the fence both speak about the history that is actually left. What
    /// cannot be re-measured is the provider's usage number — stale until the
    /// next completion — so the post-fold estimate subtracts what the folds
    /// covered instead ([`fold_step`]).
    async fn run(&self, event_floor: i64) -> Result<Option<Published>> {
        let db = self.db;
        let budget = self.budget;
        let session_id = self.session_id;
        let ratio = self.ratio;
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
            tail_bytes(db, session_id, event_floor),
            newest_reported_usage(db, session_id, event_floor)
        )?;
        if tail_load_skippable(gate_usage, tail_bytes, &budget, ratio) {
            tracing::debug!(
                session = %session_id,
                tail_bytes,
                "historian skipped: tail far below the trigger budget"
            );
            return Ok(None);
        }
        let rows = rows_after_with_seq(db, session_id, event_floor).await?;
        // The tail's usage as the provider last reported it — `None` before the
        // session's first completion, when the tail size is the only signal.
        let usage = rows.iter().rev().find_map(|(_, row)| prompt_tokens(row));
        let usage_pct = usage.and_then(|tokens| budget.usage_percentage(tokens));
        // The newest row is never covered: it is the turn in flight. Everything
        // older competes for the chunk budget, which stops the run at the fence.
        let Some((last_ordinal, _)) = rows.last() else {
            return Ok(None);
        };
        let eligible_end = *last_ordinal;
        let count = tokens::counter_fn(ratio);
        let target = budget.target;
        // The provider's usage number is stale until the next completion, so
        // each pass's estimate subtracts what this event's folds covered.
        let mut event = EventFolds::new(event_floor, target, usage);
        // Why the event started, kept for the single line that summarises it:
        // the trigger falls to `No` as the event folds, and the entry
        // measurement is the one an operator needs when asking "why did this
        // session fold thirty times".
        let mut first_trigger: Option<historian::Trigger> = None;
        let mut first_foldable: Option<usize> = None;
        loop {
            // Zero-copy: re-measure the tail above the current floor as a
            // subslice of the rows already loaded — no fresh load, no clone.
            let floor = event.floor();
            let start = rows.partition_point(|(ordinal, _)| *ordinal <= floor);
            let above = &rows[start..];
            // The whole tail's estimate, measured by the chunker itself so the
            // pressure and the budget speak the same units. `None` (nothing
            // renderable above the floor) ends the event: on the first pass
            // that is the old empty-tail answer, later it means the fold is
            // done — the aggregate below answers `Ok(None)` either way when
            // nothing folded.
            let Some(whole) =
                historian::chunk(above, floor + 1, i64::MAX, ChunkLimits::unbounded(), &count)
            else {
                break;
            };
            // What a run could actually fold: the tail's real size minus the
            // fence. Both numbers are context tokens (`Chunk::context_tokens`),
            // the unit the provider's usage speaks — a tool result counts by
            // the body it cost, not by the `TC:` line that renders it. The
            // trigger reads this, not the whole tail: the fence is history a
            // fold can never remove, so counting it would keep the trigger
            // armed no matter how much the event already folded.
            let protected = budget.protected_tail(usage_pct.unwrap_or(0.0));
            let foldable = whole.context_tokens.saturating_sub(protected as usize);
            let trigger = historian::trigger(usage_pct, foldable as f64, budget.trigger, &budget);
            first_trigger.get_or_insert(trigger);
            first_foldable.get_or_insert(foldable);
            tracing::debug!(
                session = %session_id,
                tail_tokens = whole.context_tokens,
                foldable,
                protected_tail = protected,
                rendered_tokens = whole.token_estimate,
                // `None` = no completion has reported usage yet; it is not 0%.
                usage_pct = ?usage_pct,
                trigger_budget = budget.trigger,
                trigger = ?trigger,
                target = ?target,
                remaining = ?event.remaining(),
                messages = above.len(),
                bytes = whole.text.len(),
                first_ordinal = above.first().map(|(ordinal, _)| *ordinal),
                last_ordinal = above.last().map(|(ordinal, _)| *ordinal),
                floor,
                folds = event.folds(),
                "historian pressure"
            );
            match fold_step(event.fold_input(trigger, foldable)) {
                FoldStep::Fold => {}
                FoldStep::Stop(reason) => {
                    tracing::debug!(
                        session = %session_id,
                        reason = ?reason,
                        folds = event.folds(),
                        foldable,
                        remaining = ?event.remaining(),
                        target = ?target,
                        "historian event stopped"
                    );
                    break;
                }
            }

            // The chunk is cut against two limits: the historian's read budget
            // (the rendered text it sends) and the fence (what the region may
            // cost the conversation). Nothing fitting means the oldest eligible
            // block alone would leave less than the fence's worth of verbatim
            // tail; a fold that did that would hand the model a summarized old
            // block plus almost no recent history, so the run waits instead.
            let Some(chunk) = historian::chunk(
                above,
                floor + 1,
                eligible_end,
                ChunkLimits {
                    request: budget.historian_chunk as usize,
                    context: foldable,
                },
                &count,
            ) else {
                // Under real pressure this is the one shape mctx cannot resolve
                // by itself, so it is an operator signal, not just a trace: a
                // tail dominated by a single block stays unfolded until it
                // grows past the fence. Below the pressure band the same skip
                // is routine (a short session whose whole tail is protected),
                // hence debug.
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
                break;
            };
            let executed = match self.execute(&chunk, Some(trigger), event_floor).await {
                Ok(Some(executed)) => executed,
                Ok(None) => {
                    // Another run holds the claim; it will publish, and the next
                    // pass picks the compartments up.
                    break;
                }
                Err(error) => {
                    let covered_so_far = event.covered().map_or_else(
                        || "none".to_owned(),
                        |(first, last)| format!("{first}..{last}"),
                    );
                    tracing::warn!(
                        session = %session_id,
                        folds = event.folds(),
                        // The chunk this iteration failed on, not just the
                        // aggregate already folded: the two ranges separate a
                        // first-fold failure from a later one.
                        chunk = %format!("{}..{}", chunk.covered.start(), chunk.covered.end()),
                        covered = %covered_so_far,
                        error = %error,
                        "historian event stopped by a failed fold"
                    );
                    break;
                }
            };
            event.record(*chunk.covered.start(), &executed);
        }
        let folds = event.folds();
        let last_remaining = event.remaining();
        let covered_to = event.floor();
        let Some(published) = event.published() else {
            return Ok(None);
        };
        // One line per event, at `info` because production runs at that level:
        // how many hidden completions the pressure bought, why it started, and
        // where the estimate landed. The transcript notice reports the fact to
        // the human; this line is what answers "did the policy converge"
        // without opening the database.
        tracing::info!(
            session = %session_id,
            folds,
            trigger = ?first_trigger,
            first_foldable = ?first_foldable,
            last_remaining = ?last_remaining,
            target = ?target,
            covered = %published.covered,
            covered_to,
            compartments = published.compartments,
            "historian fold event"
        );
        Ok(Some(published))
    }

    /// One synchronous patch run for the background fold (plan D6): fold the
    /// explicit range past `floor` as far as the chunk budget allows — the
    /// same fence as [`Firing::run`] (never fold into the protected tail) —
    /// but no pressure-band classification, because the caller has already
    /// decided a fold must happen (the checkpoint's recent budget is busted).
    /// One attempt, no retry; the single-flight claim stays `historian::run`'s,
    /// so a detached run already in flight wins and this returns `Ok(None)`.
    ///
    /// Deliberately single-fold, unlike [`Firing::run`]: this path repairs one
    /// checkpoint whose recent budget overflowed, and it has no live usage
    /// number to measure a low-water target against — one drain, one fold.
    async fn patch(&self, floor: i64) -> Result<Option<Published>> {
        let db = self.db;
        let budget = self.budget;
        let session_id = self.session_id;
        let ratio = self.ratio;
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
        let covered = format!("{}..{}", chunk.start_ordinal, chunk.end_ordinal);
        Ok(self
            .execute(&chunk, None, floor)
            .await?
            .map(|executed| Published {
                covered,
                compartments: executed.compartments,
            }))
    }

    /// The prompt assembly and the run itself, shared by the detached
    /// pressure trigger and the background fold's patch path. `trigger` is
    /// logged — `Some` for a pressure-band firing, `None` for a patch run.
    ///
    /// The chunk is borrowed: the pressure loop needs its range and context
    /// cost after the run, and its rendered text is tens of KB — copying it
    /// just to hand it over would be the event's largest allocation. The run's
    /// outcome comes back as [`Executed`] instead.
    ///
    /// `prefix_floor` is the event's original floor: the prefix region must
    /// still be loaded and bounded from where the live request was rendered.
    /// The borrowed chunk already carries the current floor's advanced bounds.
    async fn execute(
        &self,
        chunk: &historian::Chunk,
        trigger: Option<historian::Trigger>,
        prefix_floor: i64,
    ) -> Result<Option<Executed>> {
        let db = self.db;
        let config = self.config;
        let chat = self.chat;
        let catalog = self.catalog;
        let session_id = self.session_id;
        let project = self.project;
        let live = self.prefix.as_ref();
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
            // The policy is borrowed: `Firing` lives for the whole pressure
            // event and lends the same string to every run it executes, so
            // the field never outlives the event it came from.
            reasoning_effort: self.reasoning_effort.as_deref(),
        };
        // The request shape, decided before the region is loaded because only a
        // prefix-shaped run pays for it. `same_model` reads the same resolver the
        // run itself uses, so the two can never disagree about the route.
        let switch_on = config.magic_context.historian_prefix_cache;
        let same_model = live.is_some_and(|live| {
            historian::historian_model(config, catalog).is_ok_and(|model| model == live.model_id)
        });
        let (region, fold_below_live_start) = if switch_on && live.is_some() && same_model {
            // THE REGION MUST START WHERE THE LIVE RENDER STARTS. The loop's
            // current `floor` is not that point: after the first fold it has
            // advanced past rows the live request still contains, so the load
            // and its boundary stay anchored at `prefix_floor`.
            // `build_chat_messages` skips rows it cannot render,
            // but every row still advances its temporal-marker cursor, so a slice
            // that starts later loses the `<!-- +2h -->` marker the live request
            // rendered and stops being a byte prefix of what the provider cached
            // — the whole point of this shape. The render also skips everything
            // up to the newest *completed compaction* row, which can sit above
            // `floor`; [`region_start`] maps that boundary back to an ordinal over
            // the same rows the live request decided it over, so the whole tail is
            // loaded here (exactly what the live step loaded) rather than the
            // chunk's window.
            let tail = load_overlaid_tail_with_ordinals(db, session_id, prefix_floor).await?;
            let start = region_start(&tail, prefix_floor);
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
            chunk,
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
        Ok(Some(Executed {
            covered_end: *chunk.covered.end(),
            context_tokens: chunk.context_tokens,
            compartments: outcome.compartment_ids.len(),
        }))
    }
}

/// What one executed fold covered, handed back so the pressure loop can
/// advance its floor and keep the event's accounting without cloning the chunk.
struct Executed {
    /// Last ordinal the fold covered: the next pass's floor.
    covered_end: i64,
    /// Context tokens the fold removed, subtracted from the usage estimate.
    context_tokens: usize,
    /// Compartments the run published.
    compartments: usize,
}

/// What one pressure event or patch published, for the transcript notice.
struct Published {
    /// Aggregate ordinal range covered (`a..b`), spanning every chunk when a
    /// pressure event folded more than one.
    covered: String,
    /// Compartments the event or patch published.
    compartments: usize,
}
