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
    /// Wake (or start) the session actor for an enqueued inbox row.
    fn dispatch<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>;
    /// Whether work is scheduled or a busy period is in flight.
    fn is_active(&self, session_id: &str) -> bool;
    /// Cancel the session's busy period.
    fn interrupt(&self, session_id: &str) -> bool;
    /// The rows a settled child is judged by: its newest idle row and its
    /// newest [`is_final_reply`] assistant row, each optional. Never the
    /// whole transcript: several children settling together must not hold
    /// several transcripts at once.
    fn settled_tail<'a>(&'a self, session_id: &'a str) -> SettledTail<'a>;
}

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
    fn outstanding(&self, parent_id: &str) -> Option<String> {
        let entries = self.lock();
        let mut labels: Vec<String> = entries
            .values()
            .filter(|in_flight| in_flight.parent_id == parent_id)
            .map(|in_flight| outstanding_label(&in_flight.child_id, &in_flight.description))
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
             Several subagent calls in one turn run in parallel, up to a server-wide limit of active subagents; calls beyond it wait in order for a free slot rather than failing. Passing the sessionID of a subagent that is still running redirects it with the new prompt and returns its final reply. Set agent=\"explore\" for exploration or review that must not change anything; the child then cannot run shell or edit files.\n\n\
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

    // Foreground waits for its slot (cancellable); background never blocks
    // the turn, so a full house is handled after the child row exists.
    let mut permit: Option<OwnedSemaphorePermit> = None;
    let mut queued_for: Option<Duration> = None;
    let mut queued_in_background = false;
    if wants_slot {
        if background {
            match tool.handle.slots.clone().try_acquire_owned() {
                Ok(held) => permit = Some(held),
                Err(_) => queued_in_background = true,
            }
        } else {
            match acquire_slot(&tool.handle.slots, &ctx.cancellation).await {
                Ok(acquired) => {
                    permit = Some(acquired.permit);
                    queued_for = acquired.queued;
                }
                Err(SlotCancelled) => return fail(CANCELLED_SENTINEL),
            }
        }
    }

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
    let wait = async {
        while spawner.is_active(&child_id) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        () = wait => {}
        () = ctx.cancellation.cancelled() => {
            spawner.interrupt(&child_id);
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

/// The queue wait was cancelled before a slot was granted.
#[derive(Debug, PartialEq, Eq)]
struct SlotCancelled;

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
) -> Result<Acquired, SlotCancelled> {
    if cancellation.is_cancelled() {
        return Err(SlotCancelled);
    }
    if let Ok(permit) = slots.clone().try_acquire_owned() {
        return Ok(Acquired {
            permit,
            queued: None,
        });
    }
    let started = Instant::now();
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(SlotCancelled),
        acquired = slots.clone().acquire_owned() => match acquired {
            Ok(permit) => Ok(Acquired {
                permit,
                queued: Some(started.elapsed()),
            }),
            // The semaphore is never closed; fail closed if that changes.
            Err(_closed) => Err(SlotCancelled),
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
    tokio::spawn(async move {
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
                if let Err(error) = spawner.dispatch(&child_id, &inbox_id).await {
                    tracing::warn!(child = %child_id, error = %error, "unable to start queued subagent");
                    start_error = Some(format!("Subagent failed to start: {error}"));
                }
                Some(permit)
            }
        };
        let (state, text) = if let Some(text) = start_error {
            ("error", text)
        } else {
            // Poll only the process-local active set — the DB is not touched
            // until the child has settled.
            while spawner.is_active(&child_id) {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
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
        // This child is no longer outstanding, so its entry goes before the
        // parent is told what is left.
        drop(_in_flight);
        let item = completion_item(
            &handle,
            &parent_id,
            &child_id,
            &description,
            state,
            &text,
            NoticeDelivery::Live,
        );
        // The watcher is the live path: nothing counts what it delivers, and
        // `enqueue_completion` already warned if the notice did not land.
        let _ = enqueue_completion(&handle, &spawner, &parent_id, &child_id, item).await;
    });
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
    Live,
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
    if let Some(outstanding) = handle.in_flight.outstanding(parent_id) {
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use ox_core::event_bus::EventBus;
    use ox_schema::support::Content;
    use ox_schema::support::Record;
    use ox_schema::support::SessionError;

    use super::*;
    use crate::contract::ToolOutput;
    use crate::contract::ToolRegistry;
    use crate::jobs::JobInfo;

    /// A child that stays active until `settled` flips, whose readable tail is
    /// `tail` (empty by default: a watcher then reports an outcome-less turn).
    #[derive(Default)]
    struct StuckChild {
        settled: AtomicBool,
        tail: Vec<ox_schema::message::Message>,
    }

    impl SubagentSpawner for StuckChild {
        fn dispatch<'a>(
            &'a self,
            _session_id: &'a str,
            _inbox_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }

        fn is_active(&self, _session_id: &str) -> bool {
            !self.settled.load(Ordering::SeqCst)
        }

        fn interrupt(&self, _session_id: &str) -> bool {
            self.settled.store(true, Ordering::SeqCst);
            true
        }

        fn settled_tail<'a>(&'a self, _session_id: &'a str) -> SettledTail<'a> {
            let tail = self.tail.clone();
            Box::pin(async move { Ok(tail) })
        }
    }

    #[test]
    fn the_in_flight_line_lists_only_this_parents_children_and_vanishes_with_them() {
        let registry = Arc::new(InFlightRegistry::default());
        assert_eq!(
            registry.outstanding("ses_parent"),
            None,
            "with nothing in flight the parent gets no line at all"
        );
        let first = registry.register("ses_parent", "ses_ab12", "fix parser");
        let second = registry.register("ses_parent", "ses_cd34", "run tests");
        let _other = registry.register("ses_other", "ses_ef56", "unrelated");
        assert_eq!(
            registry.outstanding("ses_parent").as_deref(),
            Some("outstanding: 2 (ses_ab12:fix parser, ses_cd34:run tests)"),
        );
        drop(first);
        assert_eq!(
            registry.outstanding("ses_parent").as_deref(),
            Some("outstanding: 1 (ses_cd34:run tests)"),
            "a dropped guard must unregister its child",
        );
        drop(second);
        assert_eq!(registry.outstanding("ses_parent"), None);
    }

    /// A resumed child whose earlier turn is still running has two watchers. The
    /// first ending must not unregister the survivor: the parent would be told
    /// nothing is outstanding while a notice is still coming.
    #[test]
    fn a_finished_registration_does_not_unregister_a_resumed_child() {
        let registry = Arc::new(InFlightRegistry::default());
        let first = registry.register("ses_parent", "ses_child", "first turn");
        let second = registry.register("ses_parent", "ses_child", "second turn");
        drop(first);
        assert_eq!(
            registry.outstanding("ses_parent").as_deref(),
            Some("outstanding: 1 (ses_child:second turn)"),
            "the registration that is still running must survive its predecessor"
        );
        drop(second);
        assert_eq!(
            registry.outstanding("ses_parent"),
            None,
            "with both registrations gone the child is no longer in flight"
        );
    }

    #[test]
    fn an_outstanding_description_stays_on_one_line() {
        let registry = Arc::new(InFlightRegistry::default());
        let _held = registry.register("ses_parent", "ses_ab12", "fix\n  parser");
        assert_eq!(
            registry.outstanding("ses_parent").as_deref(),
            Some("outstanding: 1 (ses_ab12:fix   parser)"),
        );
    }

    /// Everything after the closing envelope: the parent-owned `outstanding`
    /// line, if the notice carries one.
    fn outstanding_lines(notice: &str) -> &str {
        notice
            .split_once("</subagent>")
            .expect("the completion notice closes its envelope")
            .1
            .trim()
    }

    /// One live background subagent call; returns the child session id the tool
    /// assigned it.
    async fn spawn_background(tool: &SubagentTool, ctx: &ToolContext) -> String {
        let input = json!({
            "prompt": "research",
            "description": "look around",
            "background": true,
        });
        let output = run(tool, ctx, &input).await;
        let metadata = output
            .metadata
            .expect("a background result carries metadata");
        metadata["sessionID"]
            .as_str()
            .expect("sessionID")
            .to_owned()
    }

    /// Poll the parent's inbox until the live watchers have enqueued `expected`
    /// notices, so the test never depends on a fixed sleep.
    async fn wait_for_notices(db: &ox_db::Db, parent_id: &str, expected: usize) -> Vec<String> {
        let started = Instant::now();
        loop {
            let rows = synthetic_inbox(db, parent_id).await;
            if rows.len() >= expected {
                return rows
                    .iter()
                    .map(|(_, payload)| {
                        let payload: serde_json::Value =
                            serde_json::from_str(payload).expect("inbox payload is JSON");
                        payload["text"].as_str().expect("payload text").to_owned()
                    })
                    .collect();
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the watchers enqueued {} of {expected} notices for {parent_id}; a settled \
                 child whose notice never arrives leaves the parent waiting forever",
                rows.len(),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The watcher drops its own entry before it builds the notice, so a parent
    /// is never told to keep waiting for the child whose report just arrived.
    /// The real watcher is driven here: this fails if that `drop` moves below
    /// `completion_item`, or if the notice is built from a stale registry.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_settled_child_is_not_listed_in_its_own_completion_notice() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus, 4, jobs));
        // Settles only when the test flips it, so both calls below run the
        // production watcher path: poll, read the tail, unregister, notify.
        let child = Arc::new(StuckChild::default());
        handle.attach(Arc::clone(&child) as Arc<dyn SubagentSpawner>);
        let tool = SubagentTool::new(Arc::clone(&handle), 3);
        let ctx = ToolContext {
            session_id: "ses_parent".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_test".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };

        // A sibling watcher still in flight, so the first notice must carry an
        // outstanding line and the test is about who is on it.
        let sibling = handle
            .in_flight
            .register("ses_parent", "ses_sibling", "still running");

        let first = spawn_background(&tool, &ctx).await;
        child.settled.store(true, Ordering::SeqCst);
        let notices = wait_for_notices(&db, "ses_parent", 1).await;
        assert!(
            notices[0].starts_with(&format!("<subagent sessionID=\"{first}\"")),
            "the notice must be about the child that settled, got: {}",
            notices[0],
        );
        let outstanding = outstanding_lines(&notices[0]);
        assert!(
            !outstanding.contains(&first),
            "a parent must not be told to wait for the child whose report just arrived; \
             outstanding line was: {outstanding:?}"
        );
        assert!(
            outstanding.contains("ses_sibling:still running"),
            "the sibling that is still in flight must remain listed; \
             outstanding line was: {outstanding:?}"
        );

        // With nothing else in flight the notice carries no line at all.
        drop(sibling);
        let second = spawn_background(&tool, &ctx).await;
        let notices = wait_for_notices(&db, "ses_parent", 2).await;
        assert!(
            notices[1].starts_with(&format!("<subagent sessionID=\"{second}\"")),
            "the second notice must be about the second child, got: {}",
            notices[1],
        );
        assert!(
            !notices[1].contains("outstanding:"),
            "a parent with nothing else in flight gets no outstanding line, got: {}",
            notices[1],
        );
    }

    /// A spill directory belonging to one test alone: never the real ox home,
    /// never a sibling test's file.
    fn spill_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ox-subagent-{name}-{}", ox_schema::id::id("t")))
    }

    /// One error-free assistant row carrying `text`, the shape [`final_text`]
    /// reads as a child's report.
    fn assistant_reply(text: &str) -> ox_schema::message::Message {
        serde_json::from_value(json!({
            "type": "assistant",
            "id": "msg_reply",
            "time": { "created": 1 },
            "agent": "apex",
            "model": { "id": "m", "providerID": "p" },
            "content": [{ "type": "text", "text": text }]
        }))
        .expect("canned assistant row")
    }

    #[test]
    fn a_short_child_result_is_injected_verbatim() {
        let dir = spill_dir("short");
        let body = "Outcome: done.";
        let bounded = child_view(body, "ses_child", &dir, INJECT_BUDGET);
        assert!(matches!(bounded, Cow::Borrowed(_)));
        assert_eq!(bounded, body);
        assert!(
            !dir.exists(),
            "a report inside the budget needs no spill, so nothing is written"
        );
    }

    /// The bounded view is head, marker and tail, each on its own line: split
    /// them so a test can check the content budget and the marker separately
    /// instead of allowing the marker some guessed slack.
    fn split_bounded_view(view: &str) -> (&str, &str, &str) {
        let mut parts = view.splitn(3, '\n');
        let head = parts.next().expect("the view starts with the head");
        let marker = parts
            .next()
            .expect("the marker is a single line between head and tail");
        let tail = parts.next().expect("the tail closes the view");
        (head, marker, tail)
    }

    fn spilled_path(marker: &str) -> std::path::PathBuf {
        let path = marker
            .split("Full text: ")
            .nth(1)
            .and_then(|rest| rest.split(" (open it with the read tool)").next())
            .expect("the marker names its spilled full text");
        std::path::PathBuf::from(path)
    }

    #[test]
    fn an_over_budget_child_result_is_bounded_and_names_the_spilled_full_text() {
        let dir = spill_dir("bounded");
        let body = "b".repeat(40 * 1024);
        let bounded = child_view(&body, "ses_child", &dir, INJECT_BUDGET);
        let (head, marker, tail) = split_bounded_view(&bounded);
        let tail_budget = INJECT_BUDGET.cap - INJECT_BUDGET.head;
        assert!(
            head.bytes().all(|byte| byte == b'b') && !head.is_empty(),
            "the head of the report must survive as content, got {} bytes",
            head.len(),
        );
        assert!(
            tail.bytes().all(|byte| byte == b'b') && !tail.is_empty(),
            "the caveats at the end of the report must survive as content, got {} bytes",
            tail.len(),
        );
        // ASCII input, so character snapping costs nothing: both halves get
        // exactly the budget the shape promises and together they spend the
        // content cap without spilling past it.
        assert_eq!(
            head.len(),
            INJECT_BUDGET.head,
            "the head budget is spent in full, got {} bytes",
            head.len(),
        );
        assert_eq!(
            tail.len(),
            tail_budget,
            "the tail is whatever the head leaves, spent in full, got {} bytes",
            tail.len(),
        );
        assert_eq!(
            head.len() + tail.len(),
            INJECT_BUDGET.cap,
            "head plus tail is exactly the content cap, so no budget byte is lost or overspent",
        );
        let sizes = format!(
            "[subagent result truncated: {} bytes total; showing the first {} and last {} bytes.",
            body.len(),
            INJECT_BUDGET.head,
            tail_budget,
        );
        assert!(
            marker.starts_with(&sizes),
            "the marker must report the real sizes so the parent knows what it lost, got {marker:?}",
        );
        // The one channel that actually exists: the parent cannot read a child's
        // transcript through `ctx_search`, which is scoped to its own session.
        let first_path = spilled_path(marker);
        assert!(
            first_path.starts_with(&dir),
            "the spill must stay under the requested output directory, got {first_path:?}",
        );
        assert!(
            marker.contains("sessionID") && marker.contains("ses_child"),
            "the marker must name the child it can resume instead, got {marker:?}",
        );
        let spilled =
            std::fs::read_to_string(&first_path).expect("the file the marker points at must exist");
        assert_eq!(
            spilled, body,
            "the spill must hold every byte of the report, or the marker lies"
        );
        assert_eq!(
            bounded.len(),
            head.len() + marker.len() + tail.len() + 2,
            "the view is exactly head, marker and tail around two newlines, got {} bytes \
             from {}+{}+{} ",
            bounded.len(),
            head.len(),
            marker.len(),
            tail.len(),
        );
        assert!(
            bounded.len() <= INJECT_BUDGET.cap + marker.len() + 2,
            "past the content cap the view may only add its own marker and two newlines, \
             got {} bytes with a {}-byte marker",
            bounded.len(),
            marker.len(),
        );
        // The ceiling above is only meaningful if the marker itself is constant:
        // a longer report may change the total it prints, never its own shape.
        let longer = "b".repeat(80 * 1024);
        let longer_bounded = child_view(&longer, "ses_child", &dir, INJECT_BUDGET);
        let (_, longer_marker, _) = split_bounded_view(&longer_bounded);
        let longer_path = spilled_path(longer_marker);
        let shape = |marker: &str, total: usize, path: &Path| {
            marker
                .replace(&format!("{total} bytes total"), "<total>")
                .replace(&path.display().to_string(), "<path>")
        };
        assert_eq!(
            shape(marker, body.len(), &first_path),
            shape(longer_marker, longer.len(), &longer_path),
            "the marker's shape must not depend on the report size, or the ceiling grows with \
             the child's answer; the 80 KiB marker was {longer_marker:?}",
        );
        std::fs::remove_dir_all(&dir).expect("the test's spill directory is removable");
    }

    #[test]
    fn repeated_spills_keep_every_marker_pointing_at_its_own_full_text() {
        let dir = spill_dir("unique");
        let first = "first report ".repeat(2000);
        let second = "second report ".repeat(2000);
        let first_view = child_view(&first, "ses_child", &dir, INJECT_BUDGET);
        let second_view = child_view(&second, "ses_child", &dir, INJECT_BUDGET);
        let (_, first_marker, _) = split_bounded_view(&first_view);
        let (_, second_marker, _) = split_bounded_view(&second_view);
        let first_path = spilled_path(first_marker);
        let second_path = spilled_path(second_marker);

        assert_ne!(
            first_path, second_path,
            "a resumed child must not overwrite the report named by an earlier marker",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                std::fs::metadata(&dir)
                    .expect("spill root metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "the spill root is private",
            );
            assert_eq!(
                std::fs::metadata(first_path.parent().expect("child spill directory"))
                    .expect("child spill directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "each child spill directory is private",
            );
            assert_eq!(
                std::fs::metadata(&first_path)
                    .expect("spill file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "spilled reports are readable only by their owner",
            );
        }
        assert_eq!(
            std::fs::read_to_string(&first_path).expect("first spill remains readable"),
            first,
            "the first marker must still resolve to the first complete report",
        );
        assert_eq!(
            std::fs::read_to_string(&second_path).expect("second spill is readable"),
            second,
            "the second marker must resolve to the second complete report",
        );
        std::fs::remove_dir_all(&dir).expect("the test's spill directory is removable");
    }

    #[test]
    fn purging_a_missing_spill_directory_is_idempotent() {
        let child_id = ox_schema::id::id("ses_missing");
        assert!(!spill_dir(&child_id).exists());
        purge_spill(&child_id).expect("the first missing-directory purge is a no-op");
        purge_spill(&child_id).expect("repeated missing-directory purges are no-ops");
    }

    /// A marker that names a file the parent cannot open costs it a turn. When
    /// the spill fails the marker says so, and still names the child to resume.
    #[test]
    fn a_failed_spill_says_so_instead_of_naming_a_path() {
        let dir = spill_dir("failed");
        // A regular file where the spill directory should be: `create_dir_all`
        // then fails for every child this process would spill.
        std::fs::write(&dir, "not a directory").expect("blocking file");
        let body = "b".repeat(40 * 1024);
        let bounded = child_view(&body, "ses_child", &dir, INJECT_BUDGET);
        let (_, marker, _) = split_bounded_view(&bounded);
        assert!(
            marker.contains("Writing the full text failed"),
            "the marker must report the failed write, got {marker:?}",
        );
        assert!(
            !marker.contains("Full text:"),
            "an unwritable path must not be presented as the place to look, got {marker:?}",
        );
        assert!(
            marker.contains("ses_child"),
            "the fallback the parent does have — resume the child — must survive, got {marker:?}",
        );
        std::fs::remove_file(&dir).expect("the blocking file is removable");
    }

    /// The foreground fold and the background notification go through the same
    /// spill implementation, so either marker names a complete readable report.
    #[test]
    fn the_foreground_fold_spills_the_report_where_the_parent_can_read_it() {
        let dir = spill_dir("fold");
        let reply = "evidence. ".repeat(2000);
        let messages = vec![assistant_reply(&reply)];
        let fold = transcript_fold(&messages, "ses_child", &dir);
        assert!(
            fold.starts_with("Subagent report:\n\n"),
            "the fold keeps its report framing, got {fold:?}",
        );
        assert!(
            !fold.contains(&reply),
            "the whole report must not go inline when it is over the fold budget",
        );
        let path = fold
            .split("Full text: ")
            .nth(1)
            .and_then(|rest| rest.split(" (open it with the read tool)").next())
            .map(std::path::PathBuf::from)
            .expect("the fold names its spilled full text");
        assert!(
            fold.contains(&path.display().to_string()),
            "the fold must name the spilled file, got {fold:?}",
        );
        let spilled =
            std::fs::read_to_string(&path).expect("the file the fold points at must exist");
        assert_eq!(
            spilled, reply,
            "the spill must hold the whole report, not the head the fold shows"
        );
        std::fs::remove_dir_all(&dir).expect("the test's spill directory is removable");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_background_hands_a_blocking_child_back() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        bus.publish(SessionEventData::SessionCreated {
            session_id: "ses_parent".to_owned(),
            project_id: "global".to_owned(),
            location: LocationRef {
                directory: std::env::temp_dir().display().to_string(),
                workspace_id: None,
            },
            subpath: None,
            parent_id: None,
            slug: ox_schema::slug::create(),
            title: None,
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            version: "test".to_owned(),
        })
        .await
        .expect("parent session row");
        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db, bus, 4, Arc::clone(&jobs)));
        let child = Arc::new(StuckChild::default());
        handle.attach(Arc::clone(&child) as Arc<dyn SubagentSpawner>);
        let tool = SubagentTool::new(handle, 3);
        let ctx = ToolContext {
            session_id: "ses_parent".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_test".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        let input = json!({ "prompt": "research", "description": "look around" });

        let background = async {
            let started = Instant::now();
            loop {
                let moved = jobs.background_all("ses_parent");
                if !moved.is_empty() {
                    return moved;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "the foreground subagent never registered as a job",
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        // The child never settles on its own, so only the backgrounding can
        // return the call within the bound.
        let call = tokio::time::timeout(Duration::from_secs(5), run(&tool, &ctx, &input));
        let (output, moved) = tokio::join!(call, background);
        let output = output.expect("a backgrounded subagent must return while its child runs");

        let metadata = output
            .metadata
            .expect("a background result carries metadata");
        assert_eq!(metadata["status"], "running", "metadata: {metadata}");
        let child_id = metadata["sessionID"]
            .as_str()
            .expect("sessionID")
            .to_owned();
        assert_eq!(
            moved,
            vec![JobInfo {
                id: child_id,
                kind: "subagent",
                title: "look around".to_owned(),
            }],
        );
        // Let the detached watcher finish.
        child.settled.store(true, Ordering::SeqCst);
    }

    /// A child that settled before this process started: the tail is canned and
    /// `dispatch` only records that the parent was woken.
    struct SettledChild {
        tail: Vec<ox_schema::message::Message>,
        dispatched: std::sync::Mutex<Vec<String>>,
    }

    impl SubagentSpawner for SettledChild {
        fn dispatch<'a>(
            &'a self,
            session_id: &'a str,
            _inbox_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async move {
                self.dispatched
                    .lock()
                    .expect("dispatch log")
                    .push(session_id.to_owned());
                Ok(())
            })
        }

        fn is_active(&self, _session_id: &str) -> bool {
            false
        }

        fn interrupt(&self, _session_id: &str) -> bool {
            false
        }

        fn settled_tail<'a>(&'a self, _session_id: &'a str) -> SettledTail<'a> {
            let tail = self.tail.clone();
            Box::pin(async move { Ok(tail) })
        }
    }

    /// The tail the engine's settle read returns: the final reply plus the
    /// terminal row.
    fn settled_tail_fixture() -> Vec<ox_schema::message::Message> {
        serde_json::from_value(json!([
            {
                "type": "assistant",
                "id": "msg_reply",
                "time": { "created": 1 },
                "agent": "apex",
                "model": { "id": "m", "providerID": "p" },
                "content": [{ "type": "text", "text": "Outcome: backfilled." }]
            },
            { "type": "idle", "id": "msg_idle", "time": { "created": 2 }, "outcome": "succeeded" }
        ]))
        .expect("canned settled tail")
    }

    fn candidate(
        child_id: &str,
        settled: bool,
        delivered: bool,
        parent_exists: bool,
        parent_activity: i64,
        parent_waiting: bool,
    ) -> PendingChild {
        PendingChild {
            parent_id: "ses_parent".to_owned(),
            child_id: child_id.to_owned(),
            description: None,
            settled,
            delivered,
            parent_exists,
            parent_activity,
            parent_waiting,
        }
    }

    fn due_ids<'plan>(plan: &'plan BackfillPlan<'_>) -> Vec<&'plan str> {
        plan.due
            .iter()
            .map(|child| child.child_id.as_str())
            .collect()
    }

    #[test]
    fn the_replay_picks_only_settled_undelivered_children_of_living_parents() {
        let children = vec![
            candidate("ses_delivered", true, true, true, 5, true),
            candidate("ses_running", false, false, true, 5, true),
            candidate("ses_orphan", true, false, false, 5, true),
            candidate("ses_due", true, false, true, 5, true),
        ];
        let plan = backfill_plan(&children);
        assert_eq!(
            due_ids(&plan),
            vec!["ses_due"],
            "a delivered, unfinished or parentless child must not be replayed"
        );
        assert_eq!(plan.dropped, 0, "nothing was due beyond the cap");
        assert_eq!(
            plan.orphaned, 1,
            "the child whose parent is gone is counted, not silently dropped"
        );
    }

    /// The stale notice this replay used to send: the child's outcome is in the
    /// parent's transcript but was written before the delivery metadata existed,
    /// so only the parent's later rows say it is not waiting any more.
    #[test]
    fn the_replay_leaves_a_parent_that_already_moved_on() {
        let children = vec![
            candidate("ses_moved_on", true, false, true, 5, false),
            candidate("ses_waiting", true, false, true, 5, true),
        ];
        let plan = backfill_plan(&children);
        assert_eq!(
            due_ids(&plan),
            vec!["ses_waiting"],
            "only a parent still parked on the wait is owed a notice"
        );
        assert_eq!(
            plan.not_waiting, 1,
            "the skipped parent is counted rather than dropped in silence"
        );
        assert_eq!(plan.dropped, 0, "the gate ran before the cap");
    }

    /// The gate is part of the selection, so it runs before the cap: the newest
    /// [`MAX_BACKFILL`] *waiting* parents are served even when the newest
    /// candidate overall belongs to a parent that already moved on.
    #[test]
    fn the_replay_gate_runs_before_the_cap() {
        let mut children: Vec<PendingChild> = (0..MAX_BACKFILL)
            .map(|n| {
                candidate(
                    &format!("ses_wait_{n:02}"),
                    true,
                    false,
                    true,
                    n as i64,
                    true,
                )
            })
            .collect();
        children.push(candidate("ses_moved_on", true, false, true, 1_000, false));
        let plan = backfill_plan(&children);
        assert_eq!(
            plan.due.len(),
            MAX_BACKFILL,
            "every waiting parent is served, got {:?}",
            due_ids(&plan)
        );
        assert_eq!(
            plan.dropped, 0,
            "a parent that moved on must not spend a cap slot, leaving a waiting one unserved"
        );
        assert_eq!(plan.not_waiting, 1, "the skipped parent is counted");
    }

    #[test]
    fn the_replay_order_is_newest_parent_activity_then_child_id() {
        let children = vec![
            candidate("ses_b", true, false, true, 1, true),
            candidate("ses_c", true, false, true, 9, true),
            candidate("ses_a", true, false, true, 1, true),
        ];
        let plan = backfill_plan(&children);
        assert_eq!(
            due_ids(&plan),
            vec!["ses_c", "ses_a", "ses_b"],
            "the parents that were active last are served first, ties by child id"
        );
    }

    #[test]
    fn the_replay_is_capped_and_reports_what_the_cap_left_out() {
        let children: Vec<PendingChild> = (0..MAX_BACKFILL + 3)
            .map(|n| candidate(&format!("ses_{n:02}"), true, false, true, n as i64, true))
            .collect();
        let plan = backfill_plan(&children);
        assert_eq!(
            plan.due.len(),
            MAX_BACKFILL,
            "one startup replays at most {MAX_BACKFILL} notices"
        );
        assert_eq!(
            plan.dropped, 3,
            "the ones over the cap are counted, not hidden"
        );
        assert_eq!(
            plan.due[0].child_id,
            format!("ses_{:02}", MAX_BACKFILL + 2),
            "the cap keeps the newest parent activity"
        );
    }

    /// Every synthetic inbox item of a session, as `(id, payload)`.
    async fn synthetic_inbox(db: &ox_db::Db, session_id: &str) -> Vec<(String, String)> {
        let conn = db.acquire().await.expect("connection");
        let mut stmt = conn
            .prepare(
                "SELECT id, payload FROM session_inbox
                 WHERE session_id = ?1 AND type = 'synthetic'
                 ORDER BY enqueued_seq",
            )
            .await
            .expect("inbox query");
        let mut rows = stmt.query([session_id]).await.expect("inbox rows");
        let mut items = Vec::new();
        while let Some(row) = rows.next().await.expect("inbox row") {
            items.push((
                row.get::<String>(0).expect("inbox id"),
                row.get::<String>(1).expect("inbox payload"),
            ));
        }
        items
    }

    /// The synthetic rows of a session's transcript (promoted, i.e. delivered).
    async fn synthetic_messages(db: &ox_db::Db, session_id: &str) -> Vec<String> {
        let conn = db.acquire().await.expect("connection");
        let mut stmt = conn
            .prepare(
                "SELECT data FROM session_message
                 WHERE session_id = ?1 AND type = 'synthetic'
                 ORDER BY seq",
            )
            .await
            .expect("message query");
        let mut rows = stmt.query([session_id]).await.expect("message rows");
        let mut messages = Vec::new();
        while let Some(row) = rows.next().await.expect("message row") {
            messages.push(row.get::<String>(0).expect("message data"));
        }
        messages
    }

    async fn create_session(
        bus: &EventBus,
        session_id: &str,
        parent_id: Option<&str>,
        title: Option<&str>,
    ) {
        bus.publish(SessionEventData::SessionCreated {
            session_id: session_id.to_owned(),
            project_id: "global".to_owned(),
            location: LocationRef {
                directory: std::env::temp_dir().display().to_string(),
                workspace_id: None,
            },
            subpath: None,
            parent_id: parent_id.map(str::to_owned),
            slug: ox_schema::slug::create(),
            title: title.map(str::to_owned),
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            version: "test".to_owned(),
        })
        .await
        .expect("session row");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_startup_replays_a_lost_completion_exactly_once() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        create_session(&bus, "ses_child", Some("ses_parent"), Some("look around")).await;
        // The child settled while the previous process was dying: the real
        // event path writes the terminal row a replay reads.
        bus.publish(SessionEventData::ExecutionSucceeded {
            session_id: "ses_child".to_owned(),
        })
        .await
        .expect("child idle row");

        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus.clone(), 4, jobs));
        let spawner = Arc::new(SettledChild {
            tail: settled_tail_fixture(),
            dispatched: std::sync::Mutex::new(Vec::new()),
        });
        handle.attach(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>);

        handle.backfill_completions().await;

        let items = synthetic_inbox(&db, "ses_parent").await;
        assert_eq!(items.len(), 1, "the parent is owed exactly one notice");
        let payload: serde_json::Value =
            serde_json::from_str(&items[0].1).expect("inbox payload is JSON");
        assert_eq!(payload["metadata"]["source"], "subagent");
        assert_eq!(payload["metadata"]["childID"], "ses_child");
        assert_eq!(payload["metadata"]["state"], "completed");
        assert_eq!(
            payload["metadata"]["backfilled"], true,
            "a replayed notice says so, for troubleshooting"
        );
        let text = payload["text"].as_str().expect("payload text");
        assert!(
            text.starts_with(
                "<subagent sessionID=\"ses_child\" state=\"completed\" description=\"look around\">"
            ),
            "the envelope must match the live watcher's, got: {text}"
        );
        assert!(
            text.contains("Outcome: backfilled.") && text.ends_with("</subagent>"),
            "the child's report must sit inside the envelope, got: {text}"
        );
        assert_eq!(
            spawner.dispatched.lock().expect("dispatch log").as_slice(),
            ["ses_parent"],
            "the parent is woken by its session id"
        );

        // Nothing consumed the notice yet: it is still queued, which already
        // accounts for the child.
        handle.backfill_completions().await;
        assert_eq!(
            synthetic_inbox(&db, "ses_parent").await.len(),
            1,
            "a queued notice must not be queued twice"
        );

        // Deliver it the way the drain does: the projector promotes the item
        // into the parent's transcript.
        let inbox_id = items[0].0.clone();
        bus.publish(SessionEventData::InboxDelivered {
            session_id: "ses_parent".to_owned(),
            inbox_id,
        })
        .await
        .expect("delivery");
        assert_eq!(
            synthetic_messages(&db, "ses_parent").await.len(),
            1,
            "delivery promotes the item to a message"
        );

        handle.backfill_completions().await;
        assert_eq!(
            synthetic_inbox(&db, "ses_parent").await.len(),
            0,
            "a delivered child is never replayed"
        );
        assert_eq!(
            synthetic_messages(&db, "ses_parent").await.len(),
            1,
            "and the transcript keeps exactly the one notice"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_child_that_never_settled_is_not_replayed() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        create_session(&bus, "ses_child", Some("ses_parent"), Some("still running")).await;

        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus.clone(), 4, jobs));
        let spawner = Arc::new(SettledChild {
            tail: settled_tail_fixture(),
            dispatched: std::sync::Mutex::new(Vec::new()),
        });
        handle.attach(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>);

        handle.backfill_completions().await;

        assert!(
            synthetic_inbox(&db, "ses_parent").await.is_empty(),
            "a child with no terminal row died mid-turn; its parent gets no false completion"
        );
        assert!(
            spawner.dispatched.lock().expect("dispatch log").is_empty(),
            "and the parent is not woken"
        );
    }

    /// A child the user cancelled: its terminal row says so.
    fn cancelled_tail_fixture() -> Vec<ox_schema::message::Message> {
        serde_json::from_value(json!([
            { "type": "idle", "id": "msg_idle", "time": { "created": 2 }, "outcome": "interrupted" }
        ]))
        .expect("canned cancelled tail")
    }

    /// One step's assistant row, as `runner/step` opens it. The clock matters:
    /// the projector dates the row by the message's own creation time, so a fixed
    /// `started` would place every parent row at the epoch and make the replay's
    /// "did the parent produce anything after the child settled?" unreadable.
    async fn start_step(bus: &EventBus, session_id: &str, assistant_message_id: &str) {
        let step = SessionEventData::StepStarted {
            session_id: session_id.to_owned(),
            assistant_message_id: assistant_message_id.to_owned(),
            agent: "apex".to_owned(),
            model: ModelRef {
                id: "m".to_owned(),
                provider_id: "p".to_owned(),
                variant: None,
            },
            snapshot: None,
            started: ox_schema::id::now_millis(),
        };
        bus.publish(step).await.expect("assistant row");
    }

    /// The row one tool call leaves in its session: the event sequence
    /// `runner/step/tool.rs` publishes — step, streaming input, call, then the
    /// settlement carrying the tool's own metadata. The replay's delivered
    /// judgement reads that row, so the tests below must produce the shape
    /// production produces, not a hand-written fixture.
    async fn record_tool_result(
        bus: &EventBus,
        session_id: &str,
        assistant_message_id: &str,
        call_id: &str,
        output: &ToolOutput,
    ) {
        start_step(bus, session_id, assistant_message_id).await;
        settle_tool_result(bus, session_id, assistant_message_id, call_id, output).await;
    }

    /// The settlement written into a row that already exists. Production opens
    /// the assistant row before the call runs, so a foreground result rewrites
    /// that row in place and moves no clock — the shape [`pending_children`]
    /// must read as delivered while the parent still looks waiting.
    async fn settle_tool_result(
        bus: &EventBus,
        session_id: &str,
        assistant_message_id: &str,
        call_id: &str,
        output: &ToolOutput,
    ) {
        let started = SessionEventData::ToolInputStarted {
            session_id: session_id.to_owned(),
            assistant_message_id: assistant_message_id.to_owned(),
            id: call_id.to_owned(),
            name: "subagent".to_owned(),
        };
        bus.publish(started).await.expect("streaming tool part");
        let called = SessionEventData::ToolCalled {
            session_id: session_id.to_owned(),
            assistant_message_id: assistant_message_id.to_owned(),
            id: call_id.to_owned(),
            input: Record::new(),
            executed: false,
            state: None,
        };
        bus.publish(called).await.expect("tool call");
        // `tool.rs::tool_metadata`: an object becomes the part's metadata.
        let metadata: Option<Record> = match output.metadata.as_ref() {
            Some(Value::Object(map)) => Some(map.clone().into_iter().collect()),
            _ => None,
        };
        let settlement = match &output.error {
            None => SessionEventData::ToolSuccess {
                session_id: session_id.to_owned(),
                assistant_message_id: assistant_message_id.to_owned(),
                id: call_id.to_owned(),
                content: vec![Content::Text {
                    text: "reported".to_owned(),
                }],
                metadata,
                executed: false,
                result_state: None,
            },
            Some(message) => SessionEventData::ToolFailed {
                session_id: session_id.to_owned(),
                assistant_message_id: assistant_message_id.to_owned(),
                id: call_id.to_owned(),
                error: SessionError {
                    kind: "tool.execution".to_owned(),
                    message: message.clone(),
                    status: None,
                    response: None,
                },
                content: None,
                metadata,
                executed: false,
                result_state: None,
            },
        };
        bus.publish(settlement).await.expect("tool settlement");
    }

    /// The delivered fact for one child, as the query reports it.
    async fn delivered_fact(db: &ox_db::Db, child_id: &str) -> PendingChild {
        pending_children(db)
            .await
            .expect("candidate rows")
            .into_iter()
            .find(|child| child.child_id == child_id)
            .unwrap_or_else(|| panic!("child {child_id} is a replay candidate"))
    }

    /// The parent's tool part for a foreground call is the delivery: it holds
    /// the report and names the child. Replaying that child would announce a
    /// result the parent already has.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_foreground_delivery_is_never_replayed() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus.clone(), 4, jobs));
        // Already settled, so the real foreground path returns the report inline.
        let spawner = Arc::new(SettledChild {
            tail: settled_tail_fixture(),
            dispatched: std::sync::Mutex::new(Vec::new()),
        });
        handle.attach(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>);
        // The parent's assistant row is opened before the call, exactly as the
        // runner does it, so the settlement below rewrites it in place and
        // leaves the parent parked on this child.
        start_step(&bus, "ses_parent", "msg_assistant").await;
        let tool = SubagentTool::new(Arc::clone(&handle), 3);
        let ctx = ToolContext {
            session_id: "ses_parent".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_foreground".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        let output = run(
            &tool,
            &ctx,
            &json!({ "prompt": "research", "description": "look around" }),
        )
        .await;
        let metadata = output
            .metadata
            .clone()
            .expect("a foreground result carries metadata");
        assert_eq!(metadata["status"], STATUS_COMPLETED, "metadata: {metadata}");
        let child_id = metadata["sessionID"]
            .as_str()
            .expect("sessionID")
            .to_owned();
        // The child's turn ended, then the tool result landed: exactly what a
        // restart finds after a foreground call completed.
        bus.publish(SessionEventData::ExecutionSucceeded {
            session_id: child_id.clone(),
        })
        .await
        .expect("child idle row");
        settle_tool_result(
            &bus,
            "ses_parent",
            "msg_assistant",
            "call_foreground",
            &output,
        )
        .await;

        handle.backfill_completions().await;

        assert!(
            synthetic_inbox(&db, "ses_parent").await.is_empty(),
            "the parent already holds this child's report; a replay is noise"
        );
        let child = delivered_fact(&db, &child_id).await;
        assert!(child.settled, "the child's turn ended: {child:?}");
        assert!(
            child.delivered,
            "a foreground tool part naming the child counts as delivered: {child:?}"
        );
        assert!(child.parent_exists, "its parent is alive: {child:?}");
    }

    /// A foreground child can end badly, and the parent still holds that
    /// outcome: a cancelled child must not be announced again either.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_foreground_delivery_is_never_replayed() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus.clone(), 4, jobs));
        let spawner = Arc::new(SettledChild {
            tail: cancelled_tail_fixture(),
            dispatched: std::sync::Mutex::new(Vec::new()),
        });
        handle.attach(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>);
        // The parent's assistant row is opened before the call, exactly as the
        // runner does it, so the settlement below rewrites it in place and
        // leaves the parent parked on this child.
        start_step(&bus, "ses_parent", "msg_assistant").await;
        let tool = SubagentTool::new(Arc::clone(&handle), 3);
        let ctx = ToolContext {
            session_id: "ses_parent".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_cancelled".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        let output = run(
            &tool,
            &ctx,
            &json!({ "prompt": "research", "description": "look around" }),
        )
        .await;
        let metadata = output
            .metadata
            .clone()
            .expect("a cancelled foreground result carries metadata");
        assert_eq!(metadata["status"], STATUS_CANCELLED, "metadata: {metadata}");
        let child_id = metadata["sessionID"]
            .as_str()
            .expect("sessionID")
            .to_owned();
        // The child's terminal row: interrupted, as the user's interrupt leaves it.
        let idle = SessionEventData::ExecutionInterrupted {
            session_id: child_id.clone(),
            reason: ox_schema::event::InterruptReason::User,
        };
        bus.publish(idle).await.expect("child interrupted row");
        settle_tool_result(
            &bus,
            "ses_parent",
            "msg_assistant",
            "call_cancelled",
            &output,
        )
        .await;

        handle.backfill_completions().await;

        assert!(
            synthetic_inbox(&db, "ses_parent").await.is_empty(),
            "the parent already holds this child's cancellation; a replay is noise"
        );
        let child = delivered_fact(&db, &child_id).await;
        assert!(child.settled, "the child's turn ended: {child:?}");
        assert!(
            child.delivered,
            "a cancelled foreground result counts as delivered: {child:?}"
        );
    }

    /// A hand-off is not a delivery: the `running` result names the child, but
    /// its watcher still owes the notice — which a restart must replay.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_background_handoff_still_owes_its_notice() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus.clone(), 4, jobs));
        // The child never settles for the spawner: only the database knows the
        // turn ended, which is all a restart has to work with. Its tail is
        // readable, as a restart's read would be.
        let spawner = Arc::new(StuckChild {
            settled: AtomicBool::new(false),
            tail: settled_tail_fixture(),
        });
        handle.attach(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>);
        let tool = SubagentTool::new(Arc::clone(&handle), 3);
        let ctx = ToolContext {
            session_id: "ses_parent".to_owned(),
            directory: std::env::temp_dir(),
            call_id: "call_handoff".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        let output = run(
            &tool,
            &ctx,
            &json!({ "prompt": "research", "description": "look around", "background": true }),
        )
        .await;
        let metadata = output
            .metadata
            .clone()
            .expect("a hand-off carries metadata");
        assert_eq!(metadata["status"], "running", "metadata: {metadata}");
        let child_id = metadata["sessionID"]
            .as_str()
            .expect("sessionID")
            .to_owned();
        record_tool_result(&bus, "ses_parent", "msg_assistant", "call_handoff", &output).await;
        // The child settles after the hand-off row, while the parent is parked
        // waiting for the notice: the ordering a restart must tell apart from a
        // stale notice.
        let handed_off_at = ox_schema::id::now_millis();
        wait_past(handed_off_at).await;
        bus.publish(SessionEventData::ExecutionSucceeded {
            session_id: child_id.clone(),
        })
        .await
        .expect("child idle row");

        let child = delivered_fact(&db, &child_id).await;
        assert!(
            !child.delivered,
            "a hand-off only promises a notice; the child is still owed one: {child:?}"
        );
        assert!(
            child.parent_waiting,
            "after handing off, the parent produced no row of its own: it is parked on the \
             notice, so a restart owes it: {child:?}"
        );

        handle.backfill_completions().await;

        let items = synthetic_inbox(&db, "ses_parent").await;
        assert_eq!(
            items.len(),
            1,
            "the lost notice of a handed-off child is replayed exactly once"
        );
        let payload: serde_json::Value =
            serde_json::from_str(&items[0].1).expect("inbox payload is JSON");
        assert_eq!(payload["metadata"]["childID"], child_id.as_str());
        assert_eq!(payload["metadata"]["backfilled"], true);
    }

    /// Wait until the millisecond clock has moved past `at`.
    ///
    /// The replay compares row timestamps of two sessions at millisecond
    /// resolution, so a test that needs a real ordering must not write both rows
    /// inside one millisecond: a tie is deliberately read as "not provably
    /// waiting", which is exactly what a test about ordering must not produce by
    /// accident.
    async fn wait_past(at: i64) {
        let started = Instant::now();
        while ox_schema::id::now_millis() <= at {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the millisecond clock did not advance past {at}"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// The stale notice an upgrade used to send. A foreground result recorded by
    /// a version that predates the delivery metadata leaves the child reading as
    /// undelivered, and the only evidence the parent is not waiting is its own
    /// row written after the settle.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_parent_that_moved_on_after_the_settle_is_not_replayed() {
        let db = ox_db::Db::open_in_memory().await.expect("temp database");
        let bus = EventBus::new(db.clone());
        create_session(&bus, "ses_parent", None, None).await;
        create_session(&bus, "ses_child", Some("ses_parent"), Some("look around")).await;
        // The parent's step and its subagent call come first, as a foreground
        // call's do. The result carries no status metadata: the shape a version
        // that predates the delivery facts wrote, which the replay can only read
        // as "not delivered".
        let output = fail("Subagent failed (sessionID: ses_child): the child died");
        record_tool_result(&bus, "ses_parent", "msg_assistant", "call_stale", &output).await;
        let recorded_at = ox_schema::id::now_millis();
        wait_past(recorded_at).await;
        bus.publish(SessionEventData::ExecutionSucceeded {
            session_id: "ses_child".to_owned(),
        })
        .await
        .expect("child idle row");
        // Then the parent produces its next row, which is what the gate reads:
        // it is not parked on this child any more.
        let settled_at = ox_schema::id::now_millis();
        wait_past(settled_at).await;
        start_step(&bus, "ses_parent", "msg_next_step").await;

        let jobs = Arc::new(Jobs::default());
        let handle = Arc::new(SubagentHandle::new(db.clone(), bus.clone(), 4, jobs));
        let spawner = Arc::new(SettledChild {
            tail: settled_tail_fixture(),
            dispatched: std::sync::Mutex::new(Vec::new()),
        });
        handle.attach(Arc::clone(&spawner) as Arc<dyn SubagentSpawner>);

        handle.backfill_completions().await;

        let child = delivered_fact(&db, "ses_child").await;
        assert!(
            !child.delivered,
            "a row without the delivery metadata is not a delivery fact: this is the stale \
             shape the gate exists for, {child:?}"
        );
        assert!(
            !child.parent_waiting,
            "the parent's transcript moved after the settle, so it is not parked on this \
             child: {child:?}"
        );
        assert!(
            synthetic_inbox(&db, "ses_parent").await.is_empty(),
            "announcing an outcome the parent's own transcript already moved past would wake \
             it for something it finished long ago"
        );
        assert!(
            spawner.dispatched.lock().expect("dispatch log").is_empty(),
            "a parent that moved on is not woken"
        );
    }
}
