//! `subagent`: spawn a child session, run its turn to completion, and fold
//! its final message into the parent's tool result.
//!
//! ox has no in-process agent runtime to reuse, so the tool builds on the two
//! primitives the runner already exposes: `session.create`-style rows (a child
//! session row with `parent_id` set, projected through the same event) and the
//! inbox/drain pipeline (`InboxEnqueued` + `runner.dispatch`), which is exactly
//! how an HTTP prompt drives a session. The tool waits for the child to go
//! idle and then reads its transcript back through the projector.
//!
//! A foreground wait registers as a job in the registry's
//! [`crate::jobs::Jobs`] (id = child sessionID), so `session.background` can
//! hand it to the background exactly as `background=true` would (upstream
//! `subagent.ts` `jobs.block`).

use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;

use crate::agent::{Agent, Mode, SUBAGENT_AGENT};
use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::jobs::Jobs;
use ox_core::text;
use ox_schema::event::SessionEventData;
use ox_schema::support::{LocationRef, ModelRef, SessionMetadata};
use ox_schema::wire::ToolDef;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::TryAcquireError;
use tokio_util::sync::CancellationToken;

use super::{fail, object_param, ok_text, param};

/// The content budget of one child result as its parent sees it: `cap` bytes
/// kept in total, `head` of them from the front and the rest from the back.
///
/// The shape is the only thing that differs between the two delivery paths; the
/// cut, the spill to disk and the marker are one implementation ([`child_view`])
/// so the two paths cannot point a parent at different places for the same
/// text. The child's stored transcript keeps every byte either way (the same
/// rule as the tool-output budget: only this delivery's bytes are traded).
#[derive(Clone, Copy, Debug)]
struct ResultBudget {
    /// Content bytes the view keeps, head plus tail. The marker sits outside
    /// this budget.
    cap: usize,
    /// Bytes taken from the front; the rest of `cap` is the tail. `head == cap`
    /// keeps no tail at all, for a reader that wants the report from its start.
    head: usize,
}

/// Budget for the child's final message folded into a foreground parent's tool
/// result: head-only, because that parent waited on this one report and reads it
/// from the top.
const FOLD_BUDGET: ResultBudget = ResultBudget {
    cap: 16 * 1024,
    head: 16 * 1024,
};

/// Budget for a background child's final message injected into the parent's
/// inbox. A parent routing a dozen children must not have its context drowned
/// by their reports, so the injected view keeps the head (outcome and evidence)
/// and the tail (caveats and open questions).
const INJECT_BUDGET: ResultBudget = ResultBudget {
    cap: 10 * 1024,
    head: 8 * 1024,
};

/// Directory under the ox home where oversized child output is spilled. Each
/// child has a directory of immutable, uniquely named reports that the parent
/// model opens with `read`.
const SUBAGENT_OUTPUT_DIR: &str = "subagent-output";

/// Longest child description repeated in the in-flight line, in characters.
const OUTSTANDING_DESCRIPTION_CHARS: usize = 40;

/// Most completions one startup replays. A dying process can strand every
/// child that settled while it was going down; waking dozens of parents at once
/// would spend their context on notices arriving after the fact, so the replay
/// is capped and the remainder is reported instead of dropped in silence.
const MAX_BACKFILL: usize = 20;

/// Model-facing text when the parent's turn is interrupted while it waits.
/// Deliberately NOT `question`'s sentinel: no question was cancelled here, and
/// borrowing its text whitewashed the trajectory.
const CANCELLED_SENTINEL: &str = "(subagent cancelled by user interrupt)";

/// `metadata.status` of a foreground subagent result, and the `state` the same
/// outcome carries in the completion envelope. A foreground call is itself the
/// delivery of that outcome to the parent (its tool part holds the report), so
/// the startup replay reads these back: [`PENDING_CHILDREN_SQL`] accepts
/// exactly these three literals. A hand-off (`running`, `queued`) is not one of
/// them: the child is still working and the watcher still owes the notice.
const STATUS_COMPLETED: &str = "completed";
/// A foreground child whose turn failed; the parent holds the failure.
const STATUS_ERROR: &str = "error";
/// A foreground child cancelled by the user or with the parent's turn.
const STATUS_CANCELLED: &str = "cancelled";

/// Safety fuse for the ancestor walk: a `parent_id` chain that does not
/// terminate within this many hops is corrupt (a cycle), so the walk fails
/// with an error instead of hanging or saturating below a higher limit.
const MAX_DEPTH_WALK: usize = 32;

/// Context prepended to a fresh child's first prompt: who it is, the facts
/// that keep a delegation from hanging or losing its result, and the output
/// contract. The `question` line covers models that still see the tool in a
/// parent-shaped transcript; the transcript line is the output channel (only
/// the final assistant text is folded back into the parent). Four behavioral
/// lines: outcome-then-evidence-then-caveats structure (CodeWhale's
/// five-section output contract), report only verification actually
/// performed (oh-my-pi: "workers don't verify"), no polling or sleeping
/// while blocked (dimcode/opencode subagent discipline), and stay inside the
/// assigned task's files (twelve-repo convergence line; surveys under
/// `.ox/reference/agents/surveys/`).
const SPAWN_PREFIX: &str = "You are a subagent spawned by another session.\nDo not call the question tool; the user cannot see forms from a subagent.\nYour final message is the only thing the parent session receives; make it self-contained.\nBegin the final message with the outcome, then the evidence for it, then open questions or caveats.\nReport only verification you actually performed; state explicitly what you did not verify.\nDo not poll or sleep; when blocked on a background task, say so in the final message and end the turn.\nWork only within the assigned task; do not modify files outside it.\n";

/// The engine capabilities a child session needs. Implemented by the
/// runner; the tool only ever sees this trait, which is what breaks the
/// tool → engine dependency without a process-global slot.
pub trait SubagentSpawner: Send + Sync {
    fn try_admit(&self) -> Result<crate::contract::ExecutionAdmission, crate::contract::Closing>;
    fn is_closing(&self) -> bool;
    /// Wake (or start) the session actor for an enqueued inbox row.
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>;
    /// Whether work is scheduled or a busy period is in flight.
    fn is_active(&self, session_id: &str) -> bool;
    /// Resolve once the session has no work scheduled or in flight (at once
    /// for an idle or unknown session). Event-driven on the runner side, so a
    /// waiting parent costs nothing until the child settles.
    fn wait_idle<'a>(&'a self, session_id: &'a str) -> WaitIdle<'a>;
    /// Cancel the session's busy period.
    fn interrupt(&self, session_id: &str) -> bool;
    /// The rows a settled child is judged by: its newest idle row and its
    /// newest [`is_final_reply`] assistant row, each optional. Never the
    /// whole transcript: several children settling together must not hold
    /// several transcripts at once.
    fn settled_tail<'a>(&'a self, session_id: &'a str) -> SettledTail<'a>;
}

/// The boxed future [`SubagentSpawner::wait_idle`] answers with.
pub type WaitIdle<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

/// The boxed future [`SubagentSpawner::settled_tail`] answers with.
pub type SettledTail<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = anyhow::Result<Vec<ox_schema::message::Message>>>
            + Send
            + 'a,
    >,
>;

/// The pieces of the runner the tool drives a child session through. The
/// runner attaches itself at construction ([`SubagentSpawner`] impl), so the
/// tool never holds the runner that holds the tool.
#[derive(Clone)]
pub struct SubagentHandle {
    db: ox_db::Db,
    bus: ox_core::event_bus::EventBus,
    spawner: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<dyn SubagentSpawner>>>,
    /// One permit per concurrently running child, shared by every clone. A
    /// child holds its permit from dispatch until it settles; tokio's
    /// semaphore wakes waiters in FIFO order.
    slots: Arc<Semaphore>,
    /// The registry's job table; a foreground call blocks its parent on it.
    jobs: Arc<Jobs>,
    /// The background children still in flight, shared by every clone.
    in_flight: Arc<InFlightRegistry>,
    stop: tokio_util::sync::CancellationToken,
    watchers: crate::contract::TaskOwner,
}

/// One background child a live completion watcher is still responsible for.
struct InFlight {
    /// The child session this entry belongs to. A resumed child can be in
    /// flight twice, so the id labels the entry rather than keying it.
    child_id: String,
    /// The session that delegated it: only its own children are reported back.
    parent_id: String,
    description: String,
}

/// The children whose completion watchers are alive right now.
///
/// This is a mirror of running watcher tasks, not state of record: it is never
/// persisted and never derived from the database, and an entry disappears when
/// its watcher task ends, so a parent can be told what it is still waiting for
/// without guessing. A child queued for a slot is in here too — its watcher
/// exists from the moment the queue wait starts.
///
/// Entries are keyed by registration, not by child id: a resumed child whose
/// earlier turn is still running has two watchers, and the earlier one ending
/// must not unregister the survivor.
#[derive(Default)]
struct InFlightRegistry {
    entries: Mutex<HashMap<u64, InFlight>>,
    next_registration: AtomicU64,
}

impl InFlightRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, InFlight>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record `child_id` as in flight; the caller must hold the returned guard
    /// for as long as the child's watcher lives.
    fn register(
        self: &Arc<Self>,
        parent_id: &str,
        child_id: &str,
        description: &str,
    ) -> InFlightGuard {
        let registration = self.next_registration.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            registration,
            InFlight {
                child_id: child_id.to_owned(),
                parent_id: parent_id.to_owned(),
                description: description.to_owned(),
            },
        );
        InFlightGuard {
            registry: Arc::clone(self),
            registration,
        }
    }

    /// The `outstanding: 2 (ses_ab12:fix parser, ses_cd34:run tests)` line for
    /// `parent_id`, or `None` when it has nothing left in flight (the caller
    /// then appends no line at all).
    fn outstanding(&self, parent_id: &str, exclude: Option<u64>) -> Option<String> {
        let entries = self.lock();
        let mut labels: Vec<String> = entries
            .iter()
            .filter(|(registration, in_flight)| {
                Some(**registration) != exclude && in_flight.parent_id == parent_id
            })
            .map(|(_, in_flight)| outstanding_label(&in_flight.child_id, &in_flight.description))
            .collect();
        drop(entries);
        if labels.is_empty() {
            return None;
        }
        labels.sort();
        Some(format!(
            "outstanding: {} ({})",
            labels.len(),
            labels.join(", ")
        ))
    }
}

/// Removes its own registration from the in-flight registry when the watcher
/// task ends — on every return path, including the early returns before the
/// child settles.
struct InFlightGuard {
    registry: Arc<InFlightRegistry>,
    registration: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.registration);
    }
}

/// `ses_ab12:fix parser`: a description squeezed onto one line and cut short
/// so the in-flight list stays a single readable line.
fn outstanding_label(child_id: &str, description: &str) -> String {
    let mut label = format!("{child_id}:");
    for c in description.chars().take(OUTSTANDING_DESCRIPTION_CHARS) {
        label.push(if c.is_whitespace() { ' ' } else { c });
    }
    label
}

impl SubagentHandle {
    pub fn close(&self) {
        self.stop.cancel();
        self.slots.close();
    }

    /// Seal after actors end, including foreground watcher hand-backs.
    pub async fn join(&self) -> anyhow::Result<()> {
        self.close();
        self.watchers.seal_and_join().await
    }

    /// Live watchers retain wake responsibility through durable admission.
    #[must_use]
    pub fn owes_wakeup(&self, parent: &str) -> bool {
        self.in_flight
            .lock()
            .values()
            .any(|entry| entry.parent_id == parent)
    }

    /// `max_active` is the config's `[subagent] max_active`, validated `>= 1`
    /// at load; `0` would queue every call forever, so it is refused here too.
    /// `jobs` is the registry's shared job table, so `session.background` can
    /// hand a blocking foreground call to the background.
    #[must_use]
    pub fn new(
        db: ox_db::Db,
        bus: ox_core::event_bus::EventBus,
        max_active: usize,
        jobs: Arc<Jobs>,
    ) -> Self {
        assert!(
            max_active >= 1,
            "subagent max_active must be at least 1, got {max_active}"
        );
        Self {
            db,
            bus,
            spawner: std::sync::Arc::new(std::sync::OnceLock::new()),
            slots: Arc::new(Semaphore::new(max_active)),
            jobs,
            in_flight: Arc::new(InFlightRegistry::default()),
            stop: tokio_util::sync::CancellationToken::new(),
            watchers: crate::contract::TaskOwner::default(),
        }
    }

    /// The spawner connecting child dispatches. Registering twice keeps the
    /// first binding (startup happens once).
    pub fn attach(&self, spawner: std::sync::Arc<dyn SubagentSpawner>) {
        let _ = self.spawner.set(spawner);
    }

    /// `None` until the runner attached itself. The tool reports that as a
    /// tool failure rather than panicking: a registry built without a runner
    /// (tests, mis-wiring) must not take the process down.
    fn spawner(&self) -> Option<&std::sync::Arc<dyn SubagentSpawner>> {
        self.spawner.get()
    }
}

pub struct SubagentTool {
    handle: std::sync::Arc<SubagentHandle>,
    depth_limit: usize,
}

impl SubagentTool {
    #[must_use]
    pub fn new(handle: std::sync::Arc<SubagentHandle>, depth_limit: usize) -> Self {
        Self {
            handle,
            depth_limit,
        }
    }
}

impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Use the subagent tool for isolated exploration that would flood this context; only its final message comes back.",
        )
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "subagent",
            "Spawn a child session, delegate a prompt to it, and wait for its final reply. The child has its own context window and transcript; only its final message returns to this conversation. The output includes a sessionID you can pass back later to continue that specific conversation with the subagent. New child sessions start with fresh context, so include all relevant context and instructions when you don't pass a sessionID.\n\n\
             Foreground (default) runs the subagent to completion and returns its result. Background mode (background=true) launches it asynchronously and returns immediately; ox notifies you in this conversation when it finishes. Use background only for independent work that can run while you continue elsewhere.\n\n\
             Several subagent calls in one turn run in parallel, up to a server-wide limit of active subagents. Root foreground calls wait FIFO for a free slot; nested foreground calls fail with Capacity when no slot is free, to avoid ancestors waiting on descendants that cannot start. Background calls may queue. Passing the sessionID of a subagent that is still running redirects it with the new prompt and returns its final reply. Set agent=\"explore\" for exploration or review that must not change anything; the child then cannot run shell or edit files.\n\n\
             When to use: an isolated research or exploration burst whose intermediate steps would flood this context (read a broad area, then summarize), or an independent subtask that needs the full toolkit but not this conversation's history.\n\
             When NOT to use: a question a single read or grep answers (do it directly), work that needs the parent's context, or several rounds of back-and-forth — the child gets one prompt and answers once.",
            object_param(
                vec![
                    (
                        "prompt",
                        param(
                            "string",
                            "Complete, self-contained instructions for the child.",
                        ),
                    ),
                    (
                        "description",
                        param(
                            "string",
                            "Short label for this delegation, shown in the transcript.",
                        ),
                    ),
                    (
                        "sessionID",
                        param(
                            "string",
                            "Continue a specific previous subagent conversation by passing its sessionID. Calls without a sessionID start a new conversation.",
                        ),
                    ),
                    (
                        "background",
                        param(
                            "boolean",
                            "Run the subagent in the background and return immediately. You will be notified when it completes. DO NOT sleep, poll, or proactively check on its progress.",
                        ),
                    ),
                    (
                        "agent",
                        param(
                            "string",
                            "Subagent profile: `general` (full toolkit) or `explore` (read-only search: no shell, write, edit, patch or nested subagent). Defaults to `general`. Only for a new subagent: a continued sessionID keeps the agent it started with.",
                        ),
                    ),
                ],
                &["prompt", "description"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(self, &ctx, &input).await })
    }
}

fn required(input: &Value, key: &str) -> Result<String, ToolOutput> {
    match input.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        Some(Value::String(_)) | None => Err(super::invalid_arguments(
            "subagent",
            &[(key, format!("Missing string field `{key}`"))],
            input,
        )),
        Some(other) => Err(super::invalid_arguments(
            "subagent",
            &[(key, format!("Expected a string, got {other}"))],
            input,
        )),
    }
}

/// The agent a new child runs as: the `agent` field's id, or `general` when
/// none is given. Only built-in subagent-mode ids are accepted, and an explicit
/// id is refused on a resumed call (the child keeps the agent it started with).
/// The removed `read_only` key fails loudly instead of being ignored.
fn requested_agent(input: &Value, resumed: bool) -> Result<&'static Agent, ToolOutput> {
    let invalid = |key: &str, message: String| {
        Err(super::invalid_arguments(
            "subagent",
            &[(key, message)],
            input,
        ))
    };
    if input.get("read_only").is_some() {
        return invalid(
            "read_only",
            "`read_only` was removed; pass `agent: \"explore\"` for a read-only subagent"
                .to_owned(),
        );
    }
    let given = match input.get("agent") {
        Some(Value::String(id)) if !id.trim().is_empty() => Some(id.trim()),
        Some(Value::String(_) | Value::Null) | None => None,
        Some(other) => return invalid("agent", format!("Expected a string, got {other}")),
    };
    if given.is_some() && resumed {
        return invalid(
            "agent",
            "Cannot be combined with `sessionID`: a continued subagent keeps the agent it started with"
                .to_owned(),
        );
    }
    let id = given.unwrap_or(SUBAGENT_AGENT);
    let available = || {
        let ids: Vec<&str> = crate::agent::all()
            .iter()
            .filter(|agent| agent.mode == Mode::Subagent)
            .map(|agent| agent.id)
            .collect();
        ids.join(", ")
    };
    match crate::agent::get(id) {
        None => invalid(
            "agent",
            format!("Unknown agent `{id}`; available: {}", available()),
        ),
        Some(agent) if agent.mode == Mode::Subagent => Ok(agent),
        Some(_) => invalid(
            "agent",
            format!(
                "Agent `{id}` cannot run as a subagent; available: {}",
                available()
            ),
        ),
    }
}

async fn run(tool: &SubagentTool, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let runner_missing =
        || fail("The subagent runner is not attached; the server is mis-configured".to_owned());
    let prompt = match required(input, "prompt") {
        Ok(prompt) => prompt,
        Err(output) => return output,
    };
    let description = match required(input, "description") {
        Ok(description) => description,
        Err(output) => return output,
    };
    // Optional `sessionID`: continue that child conversation instead of
    // spawning a new one. Parsed before the depth guard; validated before
    // any child is touched.
    let resume_id = match input.get("sessionID") {
        Some(Value::String(id)) if !id.trim().is_empty() => Some(id.clone()),
        Some(Value::String(_)) | None => None,
        Some(other) => {
            return super::invalid_arguments(
                "subagent",
                &[("sessionID", format!("Expected a string, got {other}"))],
                input,
            );
        }
    };
    let resumed = resume_id.is_some();
    let agent = match requested_agent(input, resumed) {
        Ok(agent) => agent,
        Err(output) => return output,
    };
    let background = matches!(input.get("background"), Some(Value::Bool(true)));

    // Nesting guard first, mirroring upstream's order: the parent's ancestor
    // count decides whether another delegation is allowed at all.
    let depth = match nesting_depth(&tool.handle.db, &ctx.session_id).await {
        Ok(depth) => depth,
        Err(error) => return fail(format!("Unable to inspect session nesting: {error}")),
    };
    if depth >= tool.depth_limit {
        return fail(format!(
            "Subagent depth limit reached ({}). Increase the subagent depth limit to allow nested subagents.",
            tool.depth_limit
        ));
    }

    let Some(spawner) = tool.handle.spawner().cloned() else {
        return runner_missing();
    };

    // The target child: a provided `sessionID` continues that conversation
    // (validated to exist AND be a child of the current session before it is
    // touched); otherwise a new child is created through the same event as
    // `POST /api/session {parentID}`, inheriting the parent's directory. A
    // new child's row is only created after its slot is held, so a call
    // cancelled in the queue leaves no session behind.
    let target = if let Some(id) = resume_id {
        match load_child_parent(&tool.handle.db, &id).await {
            Ok(None) => return fail(format!("Subagent session not found: {id}")),
            Ok(Some(parent)) if parent.as_deref() != Some(ctx.session_id.as_str()) => {
                return fail(format!(
                    "Session {id} is not a child of the current session"
                ));
            }
            Ok(Some(_)) => Target::Resume(id),
            Err(error) => return fail(format!("Unable to load subagent session: {error}")),
        }
    } else {
        match load_parent(&tool.handle.db, &ctx.session_id).await {
            Ok(Some(parent)) => Target::New(parent),
            Ok(None) => return fail(format!("Unknown parent session: {}", ctx.session_id)),
            Err(error) => return fail(format!("Unable to load parent session: {error}")),
        }
    };
    let active = match &target {
        Target::Resume(id) => spawner.is_active(id),
        Target::New(_) => false,
    };
    let wants_slot = needs_slot(resumed, active);

    // Root foreground calls queue FIFO. Nested foreground calls must not
    // wait while their ancestor holds a slot; background work may still queue.
    let mut permit: Option<OwnedSemaphorePermit> = None;
    let mut queued_for: Option<Duration> = None;
    let mut queued_in_background = false;
    if wants_slot {
        if background {
            match try_slot(&tool.handle.slots, &ctx.cancellation) {
                Ok(held) => permit = Some(held),
                Err(SlotError::Capacity) => queued_in_background = true,
                Err(error) => return slot_failure(error),
            }
        } else {
            let acquired = if depth > 0 {
                try_slot(&tool.handle.slots, &ctx.cancellation).map(|permit| Acquired {
                    permit,
                    queued: None,
                })
            } else {
                acquire_slot(&tool.handle.slots, &ctx.cancellation).await
            };
            match acquired {
                Ok(acquired) => {
                    permit = Some(acquired.permit);
                    queued_for = acquired.queued;
                }
                Err(error) => return slot_failure(error),
            }
        }
    }

    let _admission = match spawner.try_admit() {
        Ok(admission) if !tool.handle.stop.is_cancelled() => admission,
        _ => return fail("Subagent execution admission is closed"),
    };
    let child_id = match target {
        Target::Resume(id) => id,
        Target::New(parent) => {
            match create_child(
                &tool.handle,
                &ctx.session_id,
                &description,
                parent,
                agent.id,
            )
            .await
            {
                Ok(id) => id,
                Err(message) => return fail(message),
            }
        }
    };

    // Name the child on the running part before it can start (upstream
    // `ctx.metadata({ sessionId })`): the same bus orders this ahead of the
    // child's `execution.started`, so a client can count and open a
    // foreground child while it runs. Ephemeral, so a failure only costs the
    // live view; the result's metadata still names the child.
    let progress = SessionEventData::ToolProgress {
        session_id: ctx.session_id.clone(),
        assistant_message_id: ctx.assistant_message_id.clone(),
        id: ctx.call_id.clone(),
        metadata: [
            ("sessionID".to_owned(), json!(child_id)),
            ("description".to_owned(), json!(description)),
        ]
        .into_iter()
        .collect(),
    };
    if let Err(error) = tool.handle.bus.publish(progress).await {
        tracing::warn!(%error, child = %child_id, "subagent progress failed to publish");
    }

    // Enqueue the prompt, then wake the child through the same dispatch path
    // an HTTP prompt uses.
    let message_id = ox_schema::id::id("msg");
    // A resumed conversation already carries the spawn context; only NEW
    // children get the fresh-context notice plus the output contract.
    let text = if resumed {
        prompt
    } else {
        format!("{SPAWN_PREFIX}{prompt}")
    };
    let payload = ox_schema::inbox::UserPayload {
        prompt: ox_schema::support::Prompt {
            text,
            files: None,
            agents: None,
            skills: None,
        },
        metadata: None,
    };
    let enqueued = SessionEventData::InboxEnqueued {
        session_id: child_id.clone(),
        inbox_id: message_id.clone(),
        item: ox_schema::inbox::Item::User {
            payload,
            delivery: ox_schema::inbox::Delivery::Steer,
        },
    };
    if let Err(error) = tool.handle.bus.publish(enqueued).await {
        return fail(format!("Unable to enqueue child prompt: {error}"));
    }
    // A queued background child is dispatched by its watcher once a slot
    // frees; everything else starts now.
    if !queued_in_background && let Err(error) = spawner.dispatch(&child_id, &message_id).await {
        return fail(format!("Unable to start child session: {error}"));
    }

    if background {
        // The watcher outlives this turn by design: the child keeps working
        // and the parent is woken when it settles.
        let start = if queued_in_background {
            WatcherStart::Queued {
                inbox_id: message_id,
            }
        } else {
            WatcherStart::Running(permit)
        };
        spawn_completion_watcher(
            tool.handle.clone(),
            ctx.session_id.clone(),
            child_id.clone(),
            description.clone(),
            start,
        );
        return background_output(&child_id, &description, queued_in_background);
    }

    drop(_admission);
    // `permit` is held until this function returns on any path, so the slot
    // frees on success, failure, and cancellation alike, unless
    // `session.background` hands it to a completion watcher below.
    //
    // The wait blocks the parent session as a job (upstream `jobs.block`,
    // id = child sessionID), so `session.background` can turn this call
    // into a background one.
    let guard = tool
        .handle
        .jobs
        .block(&child_id, "subagent", &description, &ctx.session_id);

    // Wait for the child to settle (or the parent's turn to be interrupted —
    // the child is cancelled with it, so nothing keeps running). Upstream has
    // no timeout here: a foreground subagent waits indefinitely and only the
    // user can stop it. A wedged child is visible to the user, who can
    // interrupt the turn; killing healthy work on a wall clock is the worse
    // failure mode.
    tokio::select! {
        () = spawner.wait_idle(&child_id) => {}
        () = ctx.cancellation.cancelled() => {
            spawner.interrupt(&child_id);
            // Keep the foreground permit until actual child cleanup completes.
            spawner.wait_idle(&child_id).await;
            // The child is cancelled with the turn: the parent holds that
            // outcome, so the startup replay must not report it again.
            return fail(CANCELLED_SENTINEL)
                .with_metadata(delivered_metadata(&child_id, STATUS_CANCELLED));
        }
        () = guard.backgrounded() => {
            // Same hand-off as `background=true`: the watcher takes the slot
            // and notifies the parent when the child settles. A child that
            // settled at this instant is reported by the watcher, once.
            drop(guard);
            spawn_completion_watcher(
                tool.handle.clone(),
                ctx.session_id.clone(),
                child_id.clone(),
                description.clone(),
                WatcherStart::Running(permit),
            );
            return background_output(&child_id, &description, false);
        }
    }
    drop(guard);

    let tail = match spawner.settled_tail(&child_id).await {
        Ok(tail) => tail,
        Err(error) => {
            return fail(format!(
                "Child session finished but its transcript is unreadable: {error}"
            ));
        }
    };
    match child_outcome(&tail) {
        ChildOutcome::Completed => {
            let fold = transcript_fold(&tail, &child_id, &output_dir());
            let mut metadata = json!({
                "sessionID": child_id,
                "status": STATUS_COMPLETED,
                "description": description,
            });
            if let (Some(queued), Some(map)) = (queued_for, metadata.as_object_mut()) {
                map.insert(
                    "queuedMs".to_owned(),
                    json!(u64::try_from(queued.as_millis()).unwrap_or(u64::MAX)),
                );
            }
            ok_text(envelope(&child_id, STATUS_COMPLETED, None, &fold)).with_metadata(metadata)
        }
        ChildOutcome::Failed(error) => {
            // Keep the sessionID visible: the model can resume the child.
            // Metadata marks the outcome as delivered, like the clean path.
            fail(format!("Subagent failed (sessionID: {child_id}): {error}"))
                .with_metadata(delivered_metadata(&child_id, STATUS_ERROR))
        }
        ChildOutcome::Interrupted => fail(format!("Subagent cancelled (sessionID: {child_id})"))
            .with_metadata(delivered_metadata(&child_id, STATUS_CANCELLED)),
    }
}

/// The `metadata` of a foreground result whose outcome the parent now holds,
/// however it ended: the child's sessionID plus its terminal status. The
/// startup replay reads this back to know the child was delivered, so no
/// replay is manufactured for it.
fn delivered_metadata(child_id: &str, status: &str) -> Value {
    json!({ "sessionID": child_id, "status": status })
}

/// The tool result for a child handed to the background, by `background=true`
/// or by `session.background`: `queued` when it still waits for a slot,
/// otherwise `running`.
fn background_output(child_id: &str, description: &str, queued: bool) -> ToolOutput {
    let (status, lead) = if queued {
        (
            "queued",
            format!(
                "All subagent slots are busy, so the subagent is queued (sessionID: {child_id}) \
                 and starts when a slot frees. "
            ),
        )
    } else {
        (
            "running",
            format!("The subagent is working in the background (sessionID: {child_id}). "),
        )
    };
    ok_text(format!(
        "{lead}You will be notified automatically when it finishes.\n\
         DO NOT sleep, poll for progress, ask the subagent for status, or duplicate \
         this subagent's work; avoid working with the same files or topics it is using.\n\
         Work on non-overlapping tasks, or briefly tell the user what you launched and \
         end your response."
    ))
    .with_metadata(json!({
        "sessionID": child_id,
        "status": status,
        "description": description,
    }))
}

/// What a call runs against once validated.
enum Target {
    /// An existing child of the calling session.
    Resume(String),
    /// A child still to be created under the calling session, from the
    /// parent's inherited row data.
    New(ParentTuple),
}

/// Whether a call must hold a concurrency slot. A resumed child that is
/// already active holds one through the call that started it; every other
/// case (new child, resume of an idle child) adds a running child.
fn needs_slot(resumed: bool, active: bool) -> bool {
    !(resumed && active)
}

/// Admission failures are distinct from the child's eventual outcome.
#[derive(Debug, PartialEq, Eq)]
enum SlotError {
    Cancelled,
    Closed,
    Capacity,
}

fn slot_failure(error: SlotError) -> ToolOutput {
    match error {
        SlotError::Cancelled => fail(CANCELLED_SENTINEL),
        SlotError::Closed => fail("Subagent admission is closed; no child was started"),
        SlotError::Capacity => fail(
            "Subagent Capacity exhausted: nested foreground calls cannot wait for a slot while ancestors hold slots. No child was started; use background=true for independent work or finish existing children first.",
        ),
    }
}

/// Atomically take a slot without joining the queue. No separate slot count
/// can race admission; cancellation is checked before touching the semaphore.
fn try_slot(
    slots: &Arc<Semaphore>,
    cancellation: &CancellationToken,
) -> Result<OwnedSemaphorePermit, SlotError> {
    if cancellation.is_cancelled() {
        return Err(SlotError::Cancelled);
    }
    Arc::clone(slots)
        .try_acquire_owned()
        .map_err(|error| match error {
            TryAcquireError::Closed => SlotError::Closed,
            TryAcquireError::NoPermits => SlotError::Capacity,
        })
}

/// A granted slot. Dropping `permit` frees it.
struct Acquired {
    permit: OwnedSemaphorePermit,
    /// How long the call waited; `None` when a slot was free immediately.
    queued: Option<Duration>,
}

/// Take a slot, queueing FIFO behind earlier callers. Cancellation wins over
/// an available slot (a cancelled turn must not start a child) and ends a
/// queued wait without leaking a permit.
async fn acquire_slot(
    slots: &Arc<Semaphore>,
    cancellation: &CancellationToken,
) -> Result<Acquired, SlotError> {
    match try_slot(slots, cancellation) {
        Ok(permit) => {
            return Ok(Acquired {
                permit,
                queued: None,
            });
        }
        Err(SlotError::Capacity) => {}
        Err(error) => return Err(error),
    }
    let started = Instant::now();
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(SlotError::Cancelled),
        acquired = slots.clone().acquire_owned() => match acquired {
            Ok(permit) => Ok(Acquired {
                permit,
                queued: Some(started.elapsed()),
            }),
            Err(_closed) => Err(SlotError::Closed),
        },
    }
}

/// Create the child session row under `parent_session_id` through the same
/// event as `POST /api/session {parentID}`. Delegation facts ride the title
/// and the parent link; `metadata` stays empty (upstream keeps it
/// host-owned). `agent` is the child's agent id; the engine picks the child's
/// system prompt and toolset from it.
async fn create_child(
    handle: &SubagentHandle,
    parent_session_id: &str,
    description: &str,
    parent: ParentTuple,
    agent: &str,
) -> Result<String, String> {
    let (project_id, directory, workspace_id, subpath, model) = parent;
    let child_id = ox_schema::id::id("ses");
    let created = SessionEventData::SessionCreated {
        session_id: child_id.clone(),
        project_id,
        location: LocationRef {
            directory,
            workspace_id,
        },
        subpath,
        parent_id: Some(parent_session_id.to_owned()),
        slug: ox_schema::slug::create(),
        title: Some(description.to_owned()),
        agent: Some(agent.to_owned()),
        model,
        metadata: Some(SessionMetadata::default()),
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    handle
        .bus
        .publish(created)
        .await
        .map_err(|error| format!("Unable to create child session: {error}"))?;
    Ok(child_id)
}

/// `(project_id, directory, workspace_id, subpath, model)` of a session row.
type ParentTuple = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<ModelRef>,
);

/// How a background watcher begins.
///
/// Background calls never block the parent's turn. When every slot is busy
/// the tool still creates the child and enqueues its prompt at once, then
/// defers the dispatch to this watcher, which waits FIFO for a slot. This
/// keeps the sessionID available to the model immediately and keeps
/// "children started by `subagent` that are active ≤ `max_active`", because
/// a queued child is not dispatched (and so not active) until it holds a
/// permit.
enum WatcherStart {
    /// Already dispatched. Carries the slot it holds, if any (`None` for a
    /// resume of a child that was already active).
    Running(Option<OwnedSemaphorePermit>),
    /// Waiting for a slot; dispatch `inbox_id` once one is granted.
    Queued { inbox_id: String },
}

/// Watch a background child and, when it settles, hand its result to the
/// parent as a synthetic message — which also wakes the parent for a new turn
/// (or steers it if it is already busy; `Runner::dispatch` coalesces).
///
/// Detached on purpose: the parent's turn is over, the child keeps working.
/// Nothing here is persisted, so a restart loses a notification that was still
/// in flight; [`SubagentHandle::backfill_completions`] replays exactly those at
/// the next startup. The slot is held by this task and freed when it ends, on
/// every return path.
fn spawn_completion_watcher(
    handle: std::sync::Arc<SubagentHandle>,
    parent_id: String,
    child_id: String,
    description: String,
    start: WatcherStart,
) {
    // Registered before the task starts, so a sibling call made later in this
    // same turn already sees this child as in flight. The guard rides the task
    // and unregisters on every one of its return paths.
    let in_flight = handle
        .in_flight
        .register(&parent_id, &child_id, &description);
    let watchers = handle.watchers.clone();
    if let Err(error) = watchers.spawn(async move {
        // Held for the whole task: the entry exists exactly as long as this
        // watcher does.
        let _in_flight = in_flight;
        let Some(spawner) = handle.spawner().cloned() else {
            tracing::warn!(child = %child_id, "subagent runner not attached; dropping completion");
            return;
        };
        // A queued child that cannot start still owes the parent a notice:
        // it was told it would be notified.
        let mut start_error: Option<String> = None;
        let _permit = match start {
            WatcherStart::Running(permit) => permit,
            WatcherStart::Queued { inbox_id } => {
                let Ok(permit) = handle.slots.clone().acquire_owned().await else {
                    tracing::warn!(child = %child_id, "subagent slots closed; queued child never started");
                    return;
                };
                let _admission = match spawner.try_admit() {
                    Ok(admission) if !handle.stop.is_cancelled() => admission,
                    _ => return,
                };
                if let Err(error) = spawner.dispatch(&child_id, &inbox_id).await {
                    if handle.stop.is_cancelled() || spawner.is_closing() { return; }
                    tracing::warn!(child = %child_id, error = %error, "unable to start queued subagent");
                    start_error = Some(format!("Subagent failed to start: {error}"));
                }
                Some(permit)
            }
        };
        let (state, text) = if let Some(text) = start_error {
            ("error", text)
        } else {
            // Park on the runner's settle notification — the DB is not
            // touched until the child has settled.
            spawner.wait_idle(&child_id).await;
            let Ok(tail) = spawner.settled_tail(&child_id).await else {
                tracing::warn!(child = %child_id, "subagent finished but its transcript is unreadable; dropping notification");
                return;
            };
            completion_state_text(&tail)
        };
        // The parent may have been deleted while the child ran; the check
        // must precede the publish (a dead parent cannot receive).
        match load_parent(&handle.db, &parent_id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::warn!(parent = %parent_id, child = %child_id, "parent session gone; dropping subagent completion");
                return;
            }
            Err(error) => {
                tracing::warn!(parent = %parent_id, error = %error, "unable to check parent session; dropping subagent completion");
                return;
            }
        }
        // Exclude this watcher from the text, not from wake responsibility.
        let item = completion_item(
            &handle,
            &parent_id,
            &child_id,
            &description,
            state,
            &text,
            NoticeDelivery::Live(_in_flight.registration),
        );
        // The watcher is the live path: nothing counts what it delivers, and
        // `enqueue_completion` already warned if the notice did not land.
        let _ = enqueue_completion(&handle, &spawner, &parent_id, &child_id, item).await;
    }) {
        tracing::error!(%error, "subagent cleanup watcher registration failed");
    }
}

/// The notification's `(state, text)` for a settled child's tail: the envelope
/// attribute and the body the parent reads. Shared by the live watcher and the
/// startup replay so a replayed notice says exactly what it would have said.
fn completion_state_text(tail: &[ox_schema::message::Message]) -> (&'static str, String) {
    match child_outcome(tail) {
        ChildOutcome::Completed => (
            STATUS_COMPLETED,
            final_text(tail)
                .unwrap_or_else(|| "Subagent completed without a text response.".to_owned()),
        ),
        ChildOutcome::Failed(error) => (STATUS_ERROR, format!("Subagent failed: {error}")),
        ChildOutcome::Interrupted => (STATUS_CANCELLED, "Subagent cancelled".to_owned()),
    }
}

/// Whether a notification is the live watcher's or a replay of one a restart
/// lost. Only the metadata key differs; the parent reads the same envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeDelivery {
    Live(u64),
    Backfilled,
}

/// The parent's inbox item for one settled child: the `<subagent>` envelope
/// around a bounded view of the child's report, plus this parent's still
/// outstanding children — which is the parent's own view, so it sits outside
/// the envelope.
fn completion_item(
    handle: &SubagentHandle,
    parent_id: &str,
    child_id: &str,
    description: &str,
    state: &str,
    text: &str,
    delivery: NoticeDelivery,
) -> ox_schema::inbox::Item {
    // The body is child-authored and can be arbitrarily long: keep only a
    // bounded view of it in the parent's context, whose marker names the file
    // holding the full text, then close the envelope.
    let body = child_view(text, child_id, &output_dir(), INJECT_BUDGET);
    let mut wrapper = envelope(child_id, state, Some(description), &body);
    let exclude = match delivery {
        NoticeDelivery::Live(registration) => Some(registration),
        NoticeDelivery::Backfilled => None,
    };
    if let Some(outstanding) = handle.in_flight.outstanding(parent_id, exclude) {
        wrapper.push('\n');
        wrapper.push_str(&outstanding);
    }
    let mut metadata = vec![
        ("source", json!("subagent")),
        ("childID", json!(child_id)),
        ("state", json!(state)),
    ];
    if delivery == NoticeDelivery::Backfilled {
        // Troubleshooting only: whether a notice was replayed is never the
        // delivery key — the child id in the parent's own rows is.
        metadata.push(("backfilled", json!(true)));
    }
    ox_schema::inbox::Item::Synthetic {
        payload: ox_schema::inbox::SyntheticPayload {
            text: wrapper,
            description: Some(description.to_owned()),
            metadata: Some(record(&metadata)),
        },
        delivery: ox_schema::inbox::Delivery::Steer,
    }
}

/// Enqueue one completion into the parent's inbox and wake it. Every failure is
/// warned about and never takes a caller down.
///
/// Answers whether the notice reached the parent's inbox. The wake is
/// best-effort: a queued item already counts as delivered, because the parent
/// reads it on its next turn. A caller that counts replays must count only what
/// this reports, or a swallowed enqueue would be reported as a replay.
async fn enqueue_completion(
    handle: &SubagentHandle,
    spawner: &std::sync::Arc<dyn SubagentSpawner>,
    parent_id: &str,
    child_id: &str,
    item: ox_schema::inbox::Item,
) -> bool {
    let inbox_id = ox_schema::id::id("msg");
    if let Err(error) = handle
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: parent_id.to_owned(),
            inbox_id: inbox_id.clone(),
            item,
        })
        .await
    {
        tracing::warn!(parent = %parent_id, child = %child_id, error = %error, "unable to enqueue subagent completion");
        return false;
    }
    if handle.stop.is_cancelled() || spawner.is_closing() {
        return true;
    }
    if let Err(error) = spawner.dispatch(parent_id, &inbox_id).await {
        tracing::warn!(parent = %parent_id, child = %child_id, error = %error, "unable to wake parent for subagent completion");
    }
    true
}

/// A `Record` from literal pairs (the schema's metadata map).
fn record(pairs: &[(&str, serde_json::Value)]) -> ox_schema::support::Record {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Replaying the completions a restart lost
// ---------------------------------------------------------------------------

/// One child session's replay candidacy, as the database reports it.
#[derive(Debug)]
struct PendingChild {
    /// The session that delegated the child.
    parent_id: String,
    /// The child session the notification would be about.
    child_id: String,
    /// The delegation description (the child's session title), which the
    /// envelope repeats as an attribute.
    description: Option<String>,
    /// The child's transcript holds a terminal (`idle`) row, so its turn ended.
    settled: bool,
    /// The parent already has this child's notification — promoted into its
    /// transcript, or still queued in its inbox. Sending it again would report
    /// the same result twice.
    delivered: bool,
    /// The parent session row still exists; a deleted parent cannot receive.
    parent_exists: bool,
    /// The parent's last activity: the replay's recency key.
    parent_activity: i64,
    /// No message row of the parent was created after the child settled, so the
    /// parent is still stopped where it asked for this result.
    ///
    /// Derived from the two transcripts, never stored: a child that settled
    /// while the process was dying left the parent waiting, while a child whose
    /// outcome already reached the parent — a foreground result written by a
    /// version that predates the delivery metadata is the case that motivated
    /// this — has a parent that produced a row after that settle.
    parent_waiting: bool,
}

/// What one startup replays: the children still owed a notification, how many
/// the cap left out, how many had no parent left to tell, and how many parents
/// had already moved on.
#[derive(Debug, Default)]
struct BackfillPlan<'child> {
    due: Vec<&'child PendingChild>,
    /// Eligible children the cap dropped, reported so nothing vanishes in
    /// silence.
    dropped: usize,
    /// Settled, undelivered children whose parent row is gone: undeliverable,
    /// counted so the replay can report what it skipped and why.
    orphaned: usize,
    /// Settled, undelivered children whose parent's transcript moved after the
    /// settle: the parent is not waiting, so a notice would arrive after the
    /// fact. Counted for the same reason as `orphaned`.
    not_waiting: usize,
}

/// The children a restart owes a completion notification: settled, not yet
/// delivered, with a living parent still waiting on them, newest parent
/// activity first, at most [`MAX_BACKFILL`] of them.
///
/// Pure: it decides over facts the caller already read, so the entire
/// selection — settledness, delivery, a missing parent, a parent that moved on,
/// the order, the cap — is testable without a database. The gate comes before
/// the cap, so the newest [`MAX_BACKFILL`] *waiting* parents are served rather
/// than the newest [`MAX_BACKFILL`] candidates of which most had moved on.
///
/// Its only idempotency key is the child id in the parent's own rows: no replay
/// state is stored anywhere. The `delivered` fact covers every path a notice
/// reaches a parent by — a promoted synthetic message, a queued inbox item, and
/// a foreground result whose tool-part metadata names the child — so one child
/// id yields at most one notification, however often this runs.
///
/// Known false negative of the `parent_waiting` gate: a notice lost after the
/// parent produced a row of its own (a user message typed meanwhile, a step that
/// ran while the child was still working) is never replayed. The alternative is
/// announcing outcomes the parent's transcript already accounts for, which is
/// what the gate exists to stop.
#[must_use]
fn backfill_plan(children: &[PendingChild]) -> BackfillPlan<'_> {
    let mut orphaned = 0usize;
    let mut not_waiting = 0usize;
    let mut due: Vec<&PendingChild> = Vec::new();
    for child in children {
        if !child.settled || child.delivered {
            continue;
        }
        if !child.parent_exists {
            orphaned += 1;
            continue;
        }
        if !child.parent_waiting {
            not_waiting += 1;
            continue;
        }
        due.push(child);
    }
    // Newest parent activity first: the parents most likely to still be waiting
    // are the ones that were active when the process died. The child id breaks
    // ties, so the order is total and two replays over one database agree.
    due.sort_by(|left, right| {
        right
            .parent_activity
            .cmp(&left.parent_activity)
            .then_with(|| left.child_id.cmp(&right.child_id))
    });
    let dropped = due.len().saturating_sub(MAX_BACKFILL);
    due.truncate(MAX_BACKFILL);
    BackfillPlan {
        due,
        dropped,
        orphaned,
        not_waiting,
    }
}

/// Every child session, with the facts the replay decision needs: whether its
/// transcript holds a terminal row (`settled`), whether its parent already
/// carries a notification for it (`delivered`), and whether the parent is still
/// stopped where it asked for the result (`parent_waiting`).
///
/// The parent-waiting fact compares two transcripts: the newest `time_created`
/// of any parent message row against the `time_created` of the child's `idle`
/// row. A parent that produced a row at or after the settle has moved on — it
/// wrote a step, a user message, or the idle row ending its turn — while a
/// parent whose rows all predate the settle is still parked on this child.
///
/// The clock is `time_created`, not `time_updated`, because
/// `message_updater::store_assistant` sets `time_updated` from the message's own
/// creation time when it rewrites an assistant row in place. An in-place rewrite
/// therefore moves no clock at all: a tool result settling into the parent's
/// existing assistant row is invisible here, and the row that proves the parent
/// moved on is the one it produced next. The comparison is strict on purpose —
/// a same-millisecond tie is unreadable, and a parent not provably waiting is a
/// parent the replay does not wake.
///
/// `delivered` is read from the authoritative rows — a synthetic message in the
/// parent's transcript (a delivered notice), a synthetic inbox item still
/// queued (a notice waiting for the parent's next turn), or a foreground tool
/// result whose metadata names the child (the parent already holds that
/// outcome). The first two are cheap indexed lookups in this query; the
/// foreground fact needs a transcript JSON scan and is filled in afterwards,
/// only for children that pass every other gate. A parent row that is gone
/// reports `parent_exists = false` rather
/// than disappearing from the result, so the decision stays in [`backfill_plan`].
///
/// The `WHERE` clause only narrows the rows: a child with no `idle` row can
/// never be due, so it need not be materialized. The settled decision itself
/// stays in [`backfill_plan`].
///
const PENDING_CHILDREN_SQL: &str = "\
    SELECT c.id, c.parent_id, c.title, p.id IS NOT NULL, COALESCE(p.time_updated, 0), \
           COALESCE((SELECT MAX(a.time_created) FROM session_message a \
                      WHERE a.session_id = c.parent_id), 0) \
             < COALESCE((SELECT MAX(s.time_created) FROM session_message s \
                          WHERE s.session_id = c.id AND s.type = 'idle'), 0), \
           EXISTS (SELECT 1 FROM session_message m \
                    WHERE m.session_id = c.id AND m.type = 'idle'), \
           EXISTS (SELECT 1 FROM session_message n \
                    WHERE n.session_id = c.parent_id AND n.type = 'synthetic' \
                      AND json_extract(n.data, '$.metadata.source') = 'subagent' \
                      AND json_extract(n.data, '$.metadata.childID') = c.id) \
        OR EXISTS (SELECT 1 FROM session_inbox i \
                    WHERE i.session_id = c.parent_id AND i.type = 'synthetic' \
                      AND json_extract(i.payload, '$.metadata.source') = 'subagent' \
                      AND json_extract(i.payload, '$.metadata.childID') = c.id) \
      FROM session_v2 c \
      LEFT JOIN session_v2 p ON p.id = c.parent_id \
     WHERE c.parent_id IS NOT NULL \
       AND EXISTS (SELECT 1 FROM session_message s \
                    WHERE s.session_id = c.id AND s.type = 'idle')";

/// The foreground delivery fact for one child. Its statuses are exactly
/// [`STATUS_COMPLETED`], [`STATUS_ERROR`] and [`STATUS_CANCELLED`]: a
/// `running`/`queued` hand-off names the child too, but its watcher still owes
/// the notice. `json_each` materializes the parent's transcript JSON, so this
/// must stay out of the all-children query above.
const FOREGROUND_DELIVERY_SQL: &str = "\
    SELECT 1 FROM session_message t, json_each(t.data, '$.content') AS part \
     WHERE t.session_id = ?1 AND t.type = 'assistant' \
       AND json_extract(part.value, '$.type') = 'tool' \
       AND json_extract(part.value, '$.state.metadata.sessionID') = ?2 \
       AND json_extract(part.value, '$.state.metadata.status') \
           IN ('completed', 'error', 'cancelled') \
     LIMIT 1";

/// Read every child session's replay candidacy. Read-only, and quiet about
/// nothing: a database error is the caller's to warn about.
async fn pending_children(db: &ox_db::Db) -> Result<Vec<PendingChild>, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare(PENDING_CHILDREN_SQL)
        .await
        .map_err(|error| error.to_string())?;
    let mut rows = stmt.query(()).await.map_err(|error| error.to_string())?;
    let mut children: Vec<PendingChild> = Vec::new();
    while let Some(row) = rows.next().await.map_err(|error| error.to_string())? {
        children.push(PendingChild {
            child_id: row.get::<String>(0).map_err(|error| error.to_string())?,
            parent_id: row.get::<String>(1).map_err(|error| error.to_string())?,
            description: row
                .get::<Option<String>>(2)
                .map_err(|error| error.to_string())?,
            parent_exists: row.get::<i64>(3).map_err(|error| error.to_string())? != 0,
            parent_activity: row.get::<i64>(4).map_err(|error| error.to_string())?,
            parent_waiting: row.get::<i64>(5).map_err(|error| error.to_string())? != 0,
            settled: row.get::<i64>(6).map_err(|error| error.to_string())? != 0,
            delivered: row.get::<i64>(7).map_err(|error| error.to_string())? != 0,
        });
    }
    // A foreground result settles into the parent's existing assistant row, so
    // it moves no clock the `parent_waiting` gate can see. Check it only for a
    // child that can still be due — settled, not yet notified, with a living
    // parent still waiting — rather than rescanning every parent's transcript
    // for every child that ever ran. On a database whose parents all moved on,
    // this loop is empty.
    let mut foreground = conn
        .prepare(FOREGROUND_DELIVERY_SQL)
        .await
        .map_err(|error| error.to_string())?;
    for child in &mut children {
        if !child.settled || child.delivered || !child.parent_exists || !child.parent_waiting {
            continue;
        }
        child.delivered = ox_db::optional(
            &mut foreground,
            (child.parent_id.clone(), child.child_id.clone()),
        )
        .await
        .map_err(|error| error.to_string())?
        .is_some();
    }
    Ok(children)
}

impl SubagentHandle {
    /// Replay the child completions a restart lost: children that settled while
    /// the previous process was going down, whose notification never reached
    /// the parent. Spawned once from the server's startup path, after the
    /// runner attached itself, so recovery never delays the listener.
    ///
    /// Each replay publishes the same envelope and inbox item the live watcher
    /// would have, marked `backfilled` for troubleshooting, and wakes the
    /// parent. A child the parent already accounts for is skipped, and so is one
    /// whose parent's transcript moved after the child settled — the parent is
    /// no longer parked on that result — so running this twice notifies once.
    /// Every failure — a missing runner, an unreadable database, an unreadable
    /// child transcript, a parent that vanished — warns and moves on: startup
    /// recovery must never keep the server from serving.
    ///
    /// Deliberate blind spots: a child that died mid-turn with the process is
    /// not replayed (no terminal row, so no honest outcome to report); a notice
    /// the user cancelled while it was still queued comes back (a cancellation
    /// leaves no trace outside the event log, which is replay history, not
    /// state); a child that was resumed, whose *second* notice was lost, is not
    /// replayed either — the judgement is per child id, and the first round
    /// already named it; and a notice lost after the parent wrote a row of its
    /// own (see [`backfill_plan`]) stays lost. Re-notifying either of the last
    /// two would need per-round delivery state, which is exactly what this
    /// design refuses to store.
    pub async fn backfill_completions(&self) {
        let Some(spawner) = self.spawner().cloned() else {
            tracing::warn!("subagent runner not attached; skipping the completion replay");
            return;
        };
        let children = match pending_children(&self.db).await {
            Ok(children) => children,
            Err(error) => {
                tracing::warn!(error = %error, "unable to read child sessions; skipping the completion replay");
                return;
            }
        };
        let plan = backfill_plan(&children);
        if plan.dropped > 0 {
            tracing::warn!(
                dropped = plan.dropped,
                cap = MAX_BACKFILL,
                "more subagent completions are owed than one startup replays"
            );
        }
        if plan.orphaned > 0 {
            tracing::warn!(
                orphaned = plan.orphaned,
                "settled subagents have no parent session left; their notices are unreachable"
            );
        }
        if plan.not_waiting > 0 {
            // Not a fault: these are outcomes the parent's own transcript
            // already moved past, most of them foreground results written
            // before the delivery metadata existed. Logged so an operator
            // wondering about a missing notice has the count.
            tracing::info!(
                moved_on = plan.not_waiting,
                "settled subagents whose parent already moved on; not replaying their notices"
            );
        }
        let mut replayed = 0usize;
        for child in plan.due {
            let tail = match spawner.settled_tail(&child.child_id).await {
                Ok(tail) => tail,
                Err(error) => {
                    tracing::warn!(
                        parent = %child.parent_id,
                        child = %child.child_id,
                        error = %error,
                        "unable to read the settled subagent's tail; skipping its replay"
                    );
                    continue;
                }
            };
            // The database said the child has a terminal row; the read that
            // carries the report must agree, or there is no outcome to report.
            if !tail
                .iter()
                .any(|message| matches!(message, ox_schema::message::Message::Idle { .. }))
            {
                tracing::warn!(child = %child.child_id, "settled subagent has no terminal row; skipping its replay");
                continue;
            }
            let (state, text) = completion_state_text(&tail);
            let description = child
                .description
                .clone()
                .unwrap_or_else(|| child.child_id.clone());
            let item = completion_item(
                self,
                &child.parent_id,
                &child.child_id,
                &description,
                state,
                &text,
                NoticeDelivery::Backfilled,
            );
            if enqueue_completion(self, &spawner, &child.parent_id, &child.child_id, item).await {
                replayed += 1;
            }
        }
        if replayed > 0 {
            tracing::info!(replayed, "replayed subagent completions a restart lost");
        }
    }
}

/// Ancestor hops from `session_id` up the `parent_id` chain. A root session is
/// `0`; an unknown session is also `0` (nothing to nest under). A chain that
/// does not terminate within [`MAX_DEPTH_WALK`] hops is an error (a corrupt
/// cycle), not a saturating count — the fuse must fail closed for any
/// configured depth limit.
async fn nesting_depth(db: &ox_db::Db, session_id: &str) -> Result<usize, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
        .await
        .map_err(|error| error.to_string())?;
    let mut current = session_id.to_owned();
    let mut depth = 0usize;
    while depth < MAX_DEPTH_WALK {
        let Some(row) = ox_db::optional(&mut stmt, (current.clone(),))
            .await
            .map_err(|error| error.to_string())?
        else {
            break;
        };
        let Some(parent) = row
            .get::<Option<String>>(0)
            .map_err(|error| error.to_string())?
        else {
            break;
        };
        depth += 1;
        current = parent;
    }
    if depth >= MAX_DEPTH_WALK {
        return Err(format!(
            "session parent chain does not terminate within {MAX_DEPTH_WALK} hops (corrupt cycle?)"
        ));
    }
    Ok(depth)
}

async fn load_parent(db: &ox_db::Db, session_id: &str) -> Result<Option<ParentTuple>, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT project_id, directory, workspace_id, path, model
             FROM session_v2 WHERE id = ?1",
        )
        .await
        .map_err(|error| error.to_string())?;
    let row = ox_db::optional(&mut stmt, (session_id,))
        .await
        .map_err(|error| error.to_string())?;
    let Some(row) = row else {
        return Ok(None);
    };
    let model = row
        .get::<Option<String>>(4)
        .map_err(|error| error.to_string())?
        .as_deref()
        .map(serde_json::from_str::<ModelRef>)
        .transpose()
        .map_err(|error| error.to_string())?;
    Ok(Some((
        row.get::<String>(0).map_err(|error| error.to_string())?,
        row.get::<String>(1).map_err(|error| error.to_string())?,
        row.get::<Option<String>>(2)
            .map_err(|error| error.to_string())?,
        row.get::<Option<String>>(3)
            .map_err(|error| error.to_string())?,
        model,
    )))
}

/// The `parent_id` of a session row: the outer `None` means the session is
/// unknown, the inner `None` that the row has no parent (a root session).
/// A root must read as "known but parentless" so a resume target that is a
/// root is rejected as not-a-child, not as not-found.
async fn load_child_parent(
    db: &ox_db::Db,
    session_id: &str,
) -> Result<Option<Option<String>>, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
        .await
        .map_err(|error| error.to_string())?;
    let row = ox_db::optional(&mut stmt, (session_id,))
        .await
        .map_err(|error| error.to_string())?;
    match row {
        Some(row) => Ok(Some(
            row.get::<Option<String>>(0)
                .map_err(|error| error.to_string())?,
        )),
        None => Ok(None),
    }
}

/// The child's settle state, decided from persisted traces only — no new
/// state is invented for it.
#[derive(Debug, PartialEq, Eq)]
enum ChildOutcome {
    /// Turn ended clean: fold the final message.
    Completed,
    /// Turn failed: keep the child's sessionID visible so the model can resume.
    Failed(String),
    /// Child was interrupted (e.g. directly through the API).
    Interrupted,
}

/// Read the settle state off the transcript's trailing `idle` row. A
/// transcript with no idle row reads as completed.
fn child_outcome(messages: &[ox_schema::message::Message]) -> ChildOutcome {
    use ox_schema::message::{IdleOutcome, Message};
    match messages.iter().rev().find_map(|message| match message {
        Message::Idle { outcome, error, .. } => Some((*outcome, error)),
        _ => None,
    }) {
        Some((IdleOutcome::Succeeded, _)) | None => ChildOutcome::Completed,
        Some((IdleOutcome::Interrupted, _)) => ChildOutcome::Interrupted,
        Some((IdleOutcome::Failed, error)) => ChildOutcome::Failed(
            error
                .as_ref()
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "unknown error".to_owned()),
        ),
    }
}

/// Whether `message` can carry a child's final reply: an assistant row
/// without an error and with at least one non-blank text part. The engine's
/// settle-tail read keeps the newest such row; [`final_text`] reads it.
#[must_use]
pub fn is_final_reply(message: &ox_schema::message::Message) -> bool {
    reply_text(message).is_some()
}

/// The non-blank text parts of an error-free assistant row, joined.
fn reply_text(message: &ox_schema::message::Message) -> Option<String> {
    use ox_schema::message::{AssistantContent, Message};
    let Message::Assistant(assistant) = message else {
        return None;
    };
    if assistant.error.is_some() {
        return None;
    }
    let parts: Vec<&str> = assistant
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The newest completed assistant text, whole (no preview) — the notification
/// body for background completions, matching upstream's completion text.
fn final_text(messages: &[ox_schema::message::Message]) -> Option<String> {
    messages.iter().rev().find_map(reply_text)
}

/// The child's reply for the parent: only the final assistant message, whole
/// when it fits [`FOLD_BUDGET`]; otherwise its head plus a marker naming the
/// full text spilled under `output_dir`. Pure over the child's settled tail
/// apart from the spill, so the caller reads that tail once.
fn transcript_fold(
    messages: &[ox_schema::message::Message],
    child_id: &str,
    output_dir: &Path,
) -> String {
    let Some(final_reply) = final_text(messages) else {
        return format!(
            "Subagent finished without a final reply. Its transcript is on session {child_id}."
        );
    };
    format!(
        "Subagent report:\n\n{}",
        child_view(&final_reply, child_id, output_dir, FOLD_BUDGET)
    )
}

/// Where this process spills oversized child output. One directory for both
/// delivery paths, so live and replayed notices use the same layout.
fn output_dir() -> std::path::PathBuf {
    ox_core::config::ox_home().join(SUBAGENT_OUTPUT_DIR)
}

/// The parent-facing view of a child's report: byte-identical to `full` when it
/// fits `budget`, otherwise its head and tail around a marker naming where the
/// whole text was spilled.
///
/// Both delivery paths go through here — the foreground fold and the background
/// notification — so a report is cut, spilled and described in exactly one way,
/// and the child's stored transcript keeps every byte regardless.
#[must_use]
fn child_view<'text>(
    full: &'text str,
    child_id: &str,
    output_dir: &Path,
    budget: ResultBudget,
) -> Cow<'text, str> {
    text::bound_head_tail(full, budget.cap, budget.head, |kept| {
        truncated_marker(full, kept, child_id, output_dir)
    })
}

/// The directory holding every spilled report delivered for `child_id`.
#[must_use]
pub fn spill_dir(child_id: &str) -> std::path::PathBuf {
    output_dir().join(child_id)
}

/// Remove all spilled reports for a deleted child session. Repeated cleanup is
/// successful whether or not the child ever produced a report.
pub fn purge_spill(child_id: &str) -> std::io::Result<()> {
    match std::fs::remove_dir_all(spill_dir(child_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Spill `full` to a unique report under `output_dir/<child>/` and describe
/// what the view kept: the content byte counts, the absolute path of the full
/// text (the parent model opens it with `read`), and the child session to
/// resume instead.
///
/// A failed write says so rather than naming a path: a marker that points at a
/// file the parent cannot open spends a turn discovering that. The background
/// path has no other channel to the full text — the parent cannot read a child's
/// transcript through `ctx_search`, which is scoped to the caller's own session.
fn truncated_marker(
    full: &str,
    kept: text::HeadTail<'_>,
    child_id: &str,
    output_dir: &Path,
) -> String {
    let kept_bytes = if kept.tail.is_empty() {
        format!("showing the first {} bytes", kept.head.len())
    } else {
        format!(
            "showing the first {} and last {} bytes",
            kept.head.len(),
            kept.tail.len()
        )
    };
    let full_text = match publish_spill(full, child_id, output_dir) {
        Ok(path) => format!("Full text: {} (open it with the read tool)", path.display()),
        Err(error) => format!("Writing the full text failed: {error}"),
    };
    format!(
        "[subagent result truncated: {} bytes total; {kept_bytes}. {full_text}. \
         Child session {child_id}: continue that conversation with \
         subagent {{sessionID: \"{child_id}\"}}]",
        full.len()
    )
}

/// Publish one immutable report. Writing a sibling temporary file and renaming
/// it keeps concurrent readers from ever observing a partial report.
fn publish_spill(
    full: &str,
    child_id: &str,
    output_dir: &Path,
) -> std::io::Result<std::path::PathBuf> {
    create_private_dir(output_dir)?;
    let child_dir = output_dir.join(child_id);
    create_private_dir(&child_dir)?;
    let delivery_id = ox_schema::id::id("notice");
    let path = child_dir.join(format!("{delivery_id}.md"));
    let temporary = child_dir.join(format!(".{delivery_id}.tmp"));
    let result = write_private_file(&temporary, full)
        .and_then(|()| std::fs::rename(&temporary, &path))
        .map(|()| path);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private_file(path: &Path, full: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(full.as_bytes())
}

/// Wrap child output in the `<subagent>` envelope the parent model reads.
/// The body and description are child-controlled, so both are escaped: a
/// body containing `</subagent>` must not close the envelope early.
fn envelope(child_id: &str, state: &str, description: Option<&str>, body: &str) -> String {
    let body = escape_text(body);
    match description {
        Some(description) => {
            let description = escape_attr(description);
            format!(
                "<subagent sessionID=\"{child_id}\" state=\"{state}\" description=\"{description}\">\n{body}\n</subagent>"
            )
        }
        None => {
            format!("<subagent sessionID=\"{child_id}\" state=\"{state}\">\n{body}\n</subagent>")
        }
    }
}

/// Escape `&`, `<`, `>` for element text; borrows when nothing needs escaping.
fn escape_text(raw: &str) -> Cow<'_, str> {
    escape(raw, |c| matches!(c, '&' | '<' | '>'))
}

/// Escape element-text characters plus `"` for a double-quoted attribute.
fn escape_attr(raw: &str) -> Cow<'_, str> {
    escape(raw, |c| matches!(c, '&' | '<' | '>' | '"'))
}

fn escape(raw: &str, needs_escape: fn(char) -> bool) -> Cow<'_, str> {
    let Some(first) = raw.find(needs_escape) else {
        return Cow::Borrowed(raw);
    };
    let mut out = String::with_capacity(raw.len() + 16);
    out.push_str(&raw[..first]);
    for c in raw[first..].chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if needs_escape('"') => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}
