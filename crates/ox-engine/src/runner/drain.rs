//! The drain loop, ported from `packages/core/src/session/runner/llm.ts`.
//!
//! Upstream shape (kept here so later tasks slot in without re-deriving):
//!
//! - **settleStaleCompactions** and **settleStaleToolCalls** run before
//!   anything else: compaction rows / tool parts left `running` by a process
//!   death are published as failed, newest-first.
//! - Boundary scan (`advanceToStep`): pick the next promotable item — steer
//!   items first (a compaction control at the head of the steers is unshifted
//!   ahead of earlier steers), else the oldest queue item at an *input*
//!   boundary. A non-steer pending head re-opens the entry boundary
//!   (llm.ts:109-112), so queued items chain across turns inside one busy
//!   period. A queued item on a steer-scoped drain ends the drain.
//! - Controls: a compaction publishes `inbox.delivered` +
//!   `compaction.started`, then the summarization request runs and the row
//!   settles via `compaction.ended`/`compaction.failed`; a move publishes
//!   `inbox.delivered` and would re-root the session (ox fails the
//!   execution).
//! - Input: `SessionInbox.promote` delivers every steer item in order, then
//!   exactly one queue item at input boundaries; a turn runs consecutive
//!   steps until a step finishes without tool calls (`needsContinuation`).
//!   Between steps, steer items that arrived mid-turn are delivered (a
//!   compaction control at the steer head owns that boundary and runs
//!   first). A 400 context-overflow before any output compacts (reason
//!   `auto`) and retries the step once; a mid-stream interrupted stream
//!   settles its partial row and continues via a synthetic prompt
//!   (upstream `Outcome.Continue`).
//! - Quiet inbox → the drain completes and the actor publishes the terminal.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use ox_core::config::Config;
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_llm::cardinal::chat::Chat;
use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::inbox::{Delivery, Item};
use ox_schema::message::{CompactionReason, Message};
use ox_schema::support::{FinishReason, SessionError};

use super::ToolRegistry;
use super::compaction;
use super::resolve_selection;
use super::unknown_error;

/// Which delivery classes a drain pass may promote. Upstream `Promotable`:
/// `"input"` (steer + queue) or `"steer"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Promotable {
    /// Any delivery (turn entry).
    Input,
    /// Steer-delivered items only (mid-turn boundaries, interrupt resume).
    Steer,
}

/// Terminal states of one drain pass, consumed by the actor.
#[derive(Debug)]
pub(crate) enum DrainOutcome {
    /// No promotable input remains; the actor may publish the terminal.
    Complete,
    /// The cancellation token fired; the actor publishes `interrupted`.
    Interrupted,
    /// The drain failed terminally; the actor publishes `failed`.
    Failed(SessionError),
}

/// A pending inbox row (decoded `session_inbox` entry).
#[derive(Debug, Clone)]
pub(crate) struct InboxRow {
    pub id: String,
    pub item: Item,
}

impl InboxRow {
    fn is_control(&self) -> bool {
        matches!(self.item, Item::Compaction { .. } | Item::Move { .. })
    }

    /// Decode a `session_inbox` row into its [`Item`].
    fn from_columns(id: String, kind: &str, payload: Value, delivery: &str) -> Result<Self> {
        let adapter = json!({ "type": kind, "payload": payload, "delivery": delivery });
        Ok(InboxRow {
            id,
            item: serde_json::from_value(adapter)?,
        })
    }
}

/// Everything one busy period's drains and steps share.
pub(crate) struct DrainCtx {
    pub session_id: String,
    /// Whether this session was spawned by another session. Resolved once
    /// when the actor starts, so prompt assembly does not query the DB per
    /// step.
    pub is_subagent: bool,
    pub directory: std::path::PathBuf,
    /// The ox home directory (`~/.ox`) the global AGENTS.md lives in.
    pub home: std::path::PathBuf,
    pub config: Arc<Config>,
    pub db: Db,
    pub bus: EventBus,
    pub chat: Arc<dyn Chat>,
    pub tools: Arc<ToolRegistry>,
    pub model_catalog: Arc<ox_llm::model_catalog::ModelCatalog>,
    /// MCP servers whose instructions join the system prompt.
    pub mcp: Arc<ox_mcp::Hub>,
    /// Per-session keyed-instruction baselines, shared by every actor of the
    /// process (the instruction layer's frozen initial render rides system[1]).
    pub instruction_baselines: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, super::instructions::Baseline>>,
    >,
    /// Per-session provider ÷ local token ratio (see
    /// [`crate::mctx::calibration`]): the unit every budget walk is measured in.
    pub calibrations: crate::mctx::calibration::Store,
    /// The session's cached git-toplevel anchor, cloned from the session
    /// handle so step and generate share the same per-directory slot.
    pub instruction_root: super::InstructionRootCache,
    /// The session's background-fold slot (plan D3): shared with the handle so the
    /// drain entry knows a running compaction row is alive, and so a manual
    /// compaction never runs beside a fold.
    pub fold_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

/// Run drains until the inbox is quiet, a control interrupts, or a turn
/// fails. One call covers every queued turn (upstream's `drain` only returns
/// on `Complete`/`Moved`/`Reloaded`).
pub(crate) async fn drain(
    ctx: &DrainCtx,
    token: &CancellationToken,
    scope: Promotable,
) -> DrainOutcome {
    // A process death skips compaction/tool finalizers; settle orphans
    // first (upstream settleStaleCompactions + settleStaleToolCalls).
    // Failures only warn: a broken settle must not wedge the turn.
    // Background folds publish no running row, so a running row here belongs
    // to a blocking compaction; the flag check stays as belt-and-suspenders
    // in case a fold ever regains a durable in-flight marker.
    if ctx
        .fold_in_flight
        .load(std::sync::atomic::Ordering::Acquire)
    {
        tracing::debug!(session = %ctx.session_id, "compaction row running for a live fold; not settling");
    } else if let Err(error) = settle_stale_compactions(&ctx.bus, &ctx.db, &ctx.session_id).await {
        tracing::warn!(session = %ctx.session_id, error = %error, "settleStaleCompactions failed");
    }
    if let Err(error) = settle_stale_tool_calls(&ctx.bus, &ctx.db, &ctx.session_id).await {
        tracing::warn!(session = %ctx.session_id, error = %error, "settleStaleToolCalls failed");
    }

    let mut entering = true;
    let mut continuing = false;

    loop {
        if token.is_cancelled() {
            return DrainOutcome::Interrupted;
        }

        // Pending scope: input at turn boundaries, steer-only mid-turn.
        let input_scope = entering || !continuing;
        let next = match next_promotable(&ctx.db, &ctx.session_id, input_scope).await {
            Ok(next) => next,
            Err(error) => return DrainOutcome::Failed(unknown_error(error.to_string())),
        };

        let Some(next) = next else {
            tracing::debug!(session = %ctx.session_id, input_scope, "inbox scan: nothing promotable");
            return DrainOutcome::Complete;
        };
        tracing::debug!(session = %ctx.session_id, inbox = %next.id, input_scope, continuing, "inbox scan: head");

        // llm.ts:109-112: a non-steer pending head re-opens the entry
        // boundary, so queued items chain across turns within one busy
        // period — no mailbox doorbell is needed to pick up the next turn.
        if !continuing && next.item.delivery() != Delivery::Steer {
            entering = true;
        }

        // Upstream guard: a quiet inbox ends the drain; so does a queued item
        // when this drain itself is steer-scoped (it belongs to the next
        // turn entry, not this pass).
        if !continuing && scope == Promotable::Steer && next.item.delivery() == Delivery::Queue {
            return DrainOutcome::Complete;
        }

        match &next.item {
            Item::Compaction { .. } => {
                // Upstream: deliver the control, run the summarization
                // compaction (reason "manual", the inbox row as input), then
                // loop back with force=false. A failed compaction settles
                // its row and does NOT fail the execution.
                if let Err(error) = deliver(ctx, &next.id).await {
                    return DrainOutcome::Failed(unknown_error(error.to_string()));
                }
                // Single flight (plan D3): an in-flight fold owns the
                // session's one compaction slot. Skipping (not queueing)
                // keeps two running rows from ever crossing — the fold will
                // land the checkpoint in seconds anyway.
                if ctx
                    .fold_in_flight
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    tracing::info!(
                        session = %ctx.session_id,
                        "manual compaction skipped: a background fold is running"
                    );
                } else if let Err(error) =
                    compaction::compact(ctx, CompactionReason::Manual, Some(next.id.clone())).await
                {
                    tracing::warn!(session = %ctx.session_id, error = %error.message, "compaction failed");
                }
                entering = true;
                continuing = false;
            }
            Item::Move { .. } => {
                if let Err(error) = deliver(ctx, &next.id).await {
                    return DrainOutcome::Failed(unknown_error(error.to_string()));
                }
                // Upstream returns Moved and re-roots the session (task 9+).
                return DrainOutcome::Failed(SessionError {
                    kind: "unknown".to_owned(),
                    message: "session.move is not supported by ox".to_owned(),
                    status: None,
                    response: None,
                });
            }
            Item::User { .. } | Item::Synthetic { .. } => {
                let promotable = if entering && !continuing {
                    scope
                } else {
                    Promotable::Steer
                };
                let promoted = match promote(ctx, promotable).await {
                    Ok(count) => count,
                    Err(error) => return DrainOutcome::Failed(unknown_error(error.to_string())),
                };
                tracing::debug!(session = %ctx.session_id, promoted, "promote returned");
                if promoted == 0 {
                    // Nothing delivered: at a re-opened entry boundary the
                    // scanned row raced away between scan and promote
                    // (cancelled). Upstream would run a forced step; ox has
                    // no force paths, so the drain ends.
                    return DrainOutcome::Complete;
                }
                // Upstream llm.ts:177: the first promoted steer of an untitled
                // root session spawns one title generation. `onlyIfMissing`
                // keeps it to a single attempt per session under the same
                // FiberMap key; ox spawns per promotion, and the guard below
                // (untitled + generated titles are never untitled) keeps it to
                // one completion in practice.
                maybe_spawn_title_generation(ctx).await;
                match run_turn(ctx, token).await {
                    Ok(TurnOutcome::Ended) => {
                        // The next scan re-opens the entry boundary when the
                        // head is a queued item (llm.ts:109-112), chaining
                        // turns inside this busy period.
                        entering = false;
                        continuing = false;
                    }
                    Ok(TurnOutcome::Interrupted) => return DrainOutcome::Interrupted,
                    Ok(TurnOutcome::Failed(error)) => return DrainOutcome::Failed(error),
                    Err(error) => return DrainOutcome::Failed(unknown_error(error.to_string())),
                }
            }
        }
    }
}

/// How one turn ended (consumed by the drain loop).
enum TurnOutcome {
    /// Every step settled and no tool calls remain open.
    Ended,
    Interrupted,
    Failed(SessionError),
}

/// One turn: consecutive steps sharing the session's model selection
/// (upstream `drain` step loop). A step that finished with tool calls
/// continues into the next step after its tools settled; a step rejected as
/// context overflow compacts once (reason `auto`) and retries; a
/// length-truncated step continues via the tool-call boundary when the
/// truncated response carried tool calls (`needsContinuation`), else via a
/// synthetic user prompt, giving up after three consecutive truncations
/// (upstream `Outcome.OutputLimit`).
async fn run_turn(ctx: &DrainCtx, token: &CancellationToken) -> Result<TurnOutcome> {
    let (agent, model) =
        match resolve_selection(&ctx.db, &ctx.session_id, &ctx.config, &ctx.model_catalog).await {
            Ok(selection) => selection,
            Err(error) => return Ok(TurnOutcome::Failed(unknown_error(error.to_string()))),
        };
    let mut compacted = false;
    loop {
        if token.is_cancelled() {
            return Ok(TurnOutcome::Interrupted);
        }
        let mut params = super::step::StepParams {
            assistant_message_id: id::id("msg"),
            agent: &agent,
            model: &model,
        };
        match super::step::run(ctx, token, &mut params).await? {
            super::step::StepOutcome::Completed {
                finish,
                needs_continuation,
            } => {
                if finish == FinishReason::ToolCalls {
                    // The step settled its tool parts (success/failed rows);
                    // steer items that arrived mid-turn join before the
                    // continuation step (upstream step boundary: steer-only).
                    deliver_pending_steers(ctx).await?;
                    continue;
                }
                if finish != FinishReason::Length {
                    // A natural stop with open todo work gets one reminder; the
                    // item is enqueued as a steer, so the drain loop picks it up
                    // on its next scan (upstream `checkCompletion` →
                    // `scheduleAgentContinue`).
                    super::todo_reminder::on_turn_ended(&ctx.db, &ctx.bus, &ctx.session_id).await;
                    return Ok(TurnOutcome::Ended);
                }
                // Upstream Outcome.OutputLimit (llm.ts): a tool-call length
                // truncation continues without a prompt (`needsContinuation`).
                if needs_continuation {
                    deliver_pending_steers(ctx).await?;
                    continue;
                }
                // A text truncation continues via a synthetic user prompt;
                // after two continuations that still end `length`, the turn
                // gives up.
                if output_limit_exhausted(&ctx.db, &ctx.session_id).await? {
                    // Upstream raises StepFailedError with only
                    // execution.failed as the terminal: the last assistant
                    // keeps `finish: "length"`, no step.failed event.
                    return Ok(TurnOutcome::Failed(SessionError {
                        kind: "output-limit".to_owned(),
                        message:
                            "Response still truncated after two output token limit continuations"
                                .to_owned(),
                        status: None,
                        response: None,
                    }));
                }
                ctx.bus
                    .publish(SessionEventData::SessionSynthetic {
                        session_id: ctx.session_id.clone(),
                        text: CONTINUE_AFTER_OUTPUT_LIMIT.to_owned(),
                        description: None,
                        metadata: None,
                    })
                    .await?;
            }
            super::step::StepOutcome::Overflow { error } => {
                if compacted {
                    // Upstream llm.ts: `recoverOverflow` is one-shot, so a
                    // retry that still overflows raises StepFailedError — a
                    // fresh failed assistant row forwarding the provider
                    // error, then execution.failed as the terminal.
                    super::step::fail_step(ctx, &params, error.clone(), None, false).await?;
                    return Ok(TurnOutcome::Failed(error));
                }
                compacted = true;
                // Upstream Outcome.Compacted: compact, then retry the step
                // against the post-compaction context. A failed compaction
                // settles its row and fails the turn with the original
                // overflow error (llm.ts:221 — compaction failures carry
                // only compaction.failed + execution.failed, no step.failed).
                if let Err(compact_error) =
                    compaction::compact(ctx, CompactionReason::Auto, None).await
                {
                    tracing::warn!(
                        session = %ctx.session_id,
                        error = %compact_error.message,
                        "overflow compaction failed"
                    );
                    return Ok(TurnOutcome::Failed(error));
                }
            }
            super::step::StepOutcome::Interrupted => return Ok(TurnOutcome::Interrupted),
            super::step::StepOutcome::Failed(error) => return Ok(TurnOutcome::Failed(error)),
        }
    }
}

/// Upstream `CONTINUE_AFTER_OUTPUT_LIMIT` (llm.ts).
const CONTINUE_AFTER_OUTPUT_LIMIT: &str = "Your last response hit the output token limit (stop reason: length). Do not apologize, recap, or repeat yourself. Break the remaining work into smaller pieces.";
/// Upstream exhausts the length-continuation streak on the third truncated
/// response (two continuations).
const OUTPUT_LIMIT_STRIKES: usize = 3;

/// Whether the last `OUTPUT_LIMIT_STRIKES` user/assistant rows are all
/// length-truncated assistants (upstream llm.ts OutputLimit exhaustion
/// check; compaction/other rows break the streak).
async fn output_limit_exhausted(db: &Db, session_id: &str) -> Result<bool> {
    let rows = super::step::load_history(db, session_id).await?;
    let recent: Vec<&Message> = rows
        .iter()
        .filter(|message| matches!(message, Message::User { .. } | Message::Assistant(_)))
        .rev()
        .take(OUTPUT_LIMIT_STRIKES)
        .collect();
    Ok(recent.len() == OUTPUT_LIMIT_STRIKES
        && recent.iter().all(|message| {
            matches!(
                message,
                Message::Assistant(assistant)
                    if assistant.finish == Some(FinishReason::Length)
            )
        }))
}

/// Spawn the automatic title generation for a root session that still has no
/// title (upstream `llm.ts:177`: the first promoted steer of an untitled root
/// session, `SessionTitle.isUntitled`). Non-root sessions are skipped: a child
/// session inherits its parent's context, so a generated title there is noise.
///
/// Fire-and-forget: the completion runs off the drain, and any failure is
/// logged inside [`crate::runner::title::generate`].
async fn maybe_spawn_title_generation(ctx: &DrainCtx) {
    let untitled = match crate::runner::title::is_untitled(&ctx.db, &ctx.session_id).await {
        Ok(untitled) => untitled,
        Err(error) => {
            tracing::debug!(
                session = %ctx.session_id,
                error = %error,
                "title generation: untitled check failed"
            );
            return;
        }
    };
    if !untitled {
        return;
    }
    match crate::runner::title::is_root_session(&ctx.db, &ctx.session_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::debug!(
                session = %ctx.session_id,
                error = %error,
                "title generation: root check failed"
            );
            return;
        }
    }
    let db = ctx.db.clone();
    let bus = ctx.bus.clone();
    let chat = Arc::clone(&ctx.chat);
    let config = ctx.config.clone();
    let catalog = Arc::clone(&ctx.model_catalog);
    let session_id = ctx.session_id.clone();
    tokio::spawn(async move {
        crate::runner::title::generate(db, bus, chat, config, catalog, session_id).await;
    });
}

/// Deliver the steers pending at a step boundary, steer-scope only: queued
/// items wait for the next turn entry. A compaction control at the steer
/// head owns the boundary — it runs before the continuation step; a move
/// still bounds it (handled at drain level after the turn).
async fn deliver_pending_steers(ctx: &DrainCtx) -> Result<()> {
    let steers: Vec<InboxRow> = list_rows(&ctx.db, &ctx.session_id)
        .await?
        .into_iter()
        .filter(|row| row.item.delivery() == Delivery::Steer)
        .collect();
    let steers = reorder_steers(steers);
    let Some(head) = steers.first() else {
        return Ok(());
    };
    if matches!(head.item, Item::Compaction { .. }) {
        let inbox_id = head.id.clone();
        deliver(ctx, &inbox_id).await?;
        if let Err(error) = compaction::compact(ctx, CompactionReason::Manual, Some(inbox_id)).await
        {
            tracing::warn!(session = %ctx.session_id, error = %error.message, "mid-turn compaction failed");
        }
        return Ok(());
    }
    deliver_up_to_control(ctx, steers).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Inbox scanning + promotion (packages/core/src/session/inbox.ts)
// ---------------------------------------------------------------------------

/// `SessionInbox.list`: every pending row in arrival order.
async fn list_rows(db: &Db, session_id: &str) -> Result<Vec<InboxRow>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, type, payload, delivery
             FROM session_inbox WHERE session_id = ?1
             ORDER BY enqueued_seq ASC",
        )
        .await?;
    let mut rows = stmt.query([session_id]).await?;
    let mut raw = Vec::new();
    while let Some(row) = rows.next().await? {
        raw.push((
            row.get::<String>(0)?,
            row.get::<String>(1)?,
            row.get::<String>(2)?,
            row.get::<String>(3)?,
            row.get::<String>(4)?,
        ));
    }
    raw.into_iter()
        .map(|(id, _session_id, kind, payload, delivery)| {
            InboxRow::from_columns(id, &kind, serde_json::from_str(&payload)?, &delivery)
        })
        .collect()
}

/// `pendingSteers`: steer rows in arrival order with the first compaction
/// control unshifted ahead (a move never moves — it bounds the boundary).
fn reorder_steers(rows: Vec<InboxRow>) -> Vec<InboxRow> {
    let control = rows
        .iter()
        .position(|row| matches!(row.item, Item::Compaction { .. } | Item::Move { .. }));
    let mut rows = rows;
    if let Some(at) = control
        && at > 0
        && matches!(rows[at].item, Item::Compaction { .. })
    {
        let control_row = rows.remove(at);
        rows.insert(0, control_row);
    }
    rows
}

/// `nextPromotable`: the head steer (compaction-unshifted), else the oldest
/// queue item when the scope allows input.
async fn next_promotable(db: &Db, session_id: &str, input_scope: bool) -> Result<Option<InboxRow>> {
    let steers: Vec<InboxRow> = list_rows(db, session_id)
        .await?
        .into_iter()
        .filter(|row| row.item.delivery() == Delivery::Steer)
        .collect();
    let steers = reorder_steers(steers);
    if let Some(head) = steers.into_iter().next() {
        return Ok(Some(head));
    }
    if !input_scope {
        return Ok(None);
    }
    Ok(list_rows(db, session_id)
        .await?
        .into_iter()
        .find(|row| row.item.delivery() == Delivery::Queue))
}

/// Deliver items at one boundary (task contract + upstream inbox.ts):
/// - steer scope: the pending steers in arrival order, up to the first
///   control (a compaction unshifts ahead of earlier steers; a move bounds).
/// - input scope: steers first, then exactly one queue item, then steers
///   that arrived while delivering. A control at the steer head owns the
///   boundary — nothing is delivered and the drain rescans.
///
/// Returns the delivered count; `Ok(0)` when nothing was deliverable.
async fn promote(ctx: &DrainCtx, scope: Promotable) -> Result<usize> {
    let steers: Vec<InboxRow> = list_rows(&ctx.db, &ctx.session_id)
        .await?
        .into_iter()
        .filter(|row| row.item.delivery() == Delivery::Steer)
        .collect();
    let steers = reorder_steers(steers);
    if steers.first().is_some_and(InboxRow::is_control) {
        return Ok(0);
    }
    if scope == Promotable::Steer {
        return deliver_up_to_control(ctx, steers).await;
    }
    // Input boundary: the steer batch, then one queued item, then new steers.
    let mut count = deliver_up_to_control(ctx, steers).await?;
    let queued = list_rows(&ctx.db, &ctx.session_id)
        .await?
        .into_iter()
        .find(|row| row.item.delivery() == Delivery::Queue);
    if let Some(queued) = queued.filter(|row| !row.is_control()) {
        deliver(ctx, &queued.id).await?;
        count += 1;
        let arrived: Vec<InboxRow> = list_rows(&ctx.db, &ctx.session_id)
            .await?
            .into_iter()
            .filter(|row| row.item.delivery() == Delivery::Steer)
            .collect();
        count += deliver_up_to_control(ctx, reorder_steers(arrived)).await?;
    }
    Ok(count)
}

/// Publish `inbox.delivered` for each row up to the first control.
async fn deliver_up_to_control(ctx: &DrainCtx, rows: Vec<InboxRow>) -> Result<usize> {
    let mut count = 0;
    for row in rows {
        if row.is_control() {
            break;
        }
        deliver(ctx, &row.id).await?;
        count += 1;
    }
    Ok(count)
}

/// Deliver one item: `session.inbox.delivered`; the projector deletes the
/// row (and promotes user/synthetic items to messages).
async fn deliver(ctx: &DrainCtx, inbox_id: &str) -> Result<()> {
    ctx.bus
        .publish(SessionEventData::InboxDelivered {
            session_id: ctx.session_id.clone(),
            inbox_id: inbox_id.to_owned(),
        })
        .await?;
    Ok(())
}

/// `settleStaleToolCalls` (upstream runner/llm.ts): assistant tool parts
/// left `running`/`streaming` by a process death are published as failed
/// (`session.tool.failed`, upstream error "aborted") newest-message-first,
/// so no tool part stays unsettled across restarts.
async fn settle_stale_tool_calls(bus: &EventBus, db: &Db, session_id: &str) -> Result<()> {
    // Assistant rows with an unsettled tool part, newest first. The JSON
    // match mirrors the projector's stored shapes: `running` keeps
    // `{status, input, metadata}`, `streaming` keeps `{status, input}`.
    let rows: Vec<(String, String)> = {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, data FROM session_message
                 WHERE session_id = ?1 AND type = 'assistant'
                   AND (json_extract(data, '$.error') IS NULL)
                   AND EXISTS (
                     SELECT 1 FROM json_each(session_message.data, '$.content')
                     WHERE json_extract(value, '$.type') = 'tool'
                       AND json_extract(value, '$.state.status') IN ('running', 'streaming')
                   )
                 ORDER BY seq DESC",
            )
            .await?;
        let mut rows = stmt.query([session_id]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }
        out
    };
    for (message_id, data) in rows {
        let message: Value = serde_json::from_str(&data)?;
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            let is_tool = part.get("type").and_then(Value::as_str) == Some("tool");
            let status = part
                .get("state")
                .and_then(|state| state.get("status"))
                .and_then(Value::as_str);
            if !is_tool || !matches!(status, Some("running") | Some("streaming")) {
                continue;
            }
            let Some(id) = part.get("id").and_then(Value::as_str) else {
                continue;
            };
            bus.publish(SessionEventData::ToolFailed {
                session_id: session_id.to_owned(),
                assistant_message_id: message_id.clone(),
                id: id.to_owned(),
                error: SessionError {
                    kind: "aborted".to_owned(),
                    message: "Tool execution interrupted".to_owned(),
                    status: None,
                    response: None,
                },
                content: None,
                metadata: None,
                executed: false,
                result_state: None,
            })
            .await?;
        }
    }
    Ok(())
}

/// `settleStaleCompactions`: publish `compaction.failed` for compaction rows
/// left running by a process death, newest first.
async fn settle_stale_compactions(bus: &EventBus, db: &Db, session_id: &str) -> Result<()> {
    let rows: Vec<(String, String)> = {
        let conn = db.acquire().await?;
        let mut stmt = conn
            .prepare(
                "SELECT id, data FROM session_message
                 WHERE session_id = ?1 AND type = 'compaction'
                   AND json_extract(data, '$.status') = 'running'
                 ORDER BY seq DESC",
            )
            .await?;
        let mut rows = stmt.query([session_id]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get::<String>(0)?, row.get::<String>(1)?));
        }
        out
    };
    for (message_id, data) in rows {
        let reason = serde_json::from_str::<Value>(&data)?
            .get("reason")
            .and_then(Value::as_str)
            .map(|raw| match raw {
                "auto" => CompactionReason::Auto,
                _ => CompactionReason::Manual,
            })
            .unwrap_or(CompactionReason::Manual);
        bus.publish(SessionEventData::CompactionFailed {
            session_id: session_id.to_owned(),
            reason,
            error: SessionError {
                kind: "compaction.interrupted".to_owned(),
                message: "Compaction was interrupted".to_owned(),
                status: None,
                response: None,
            },
            input_id: Some(message_id),
            cost: None,
            tokens: None,
        })
        .await?;
    }
    Ok(())
}
