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

use crate::agent::{Agent, Mode, SUBAGENT_AGENT};
use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::jobs::Jobs;
use ox_schema::event::SessionEventData;
use ox_schema::support::{LocationRef, ModelRef, SessionMetadata};
use ox_schema::wire::ToolDef;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{fail, object_param, ok_text, param};

/// Inline budget for the child's final message folded into the parent's
/// result; a longer final message is head-truncated to this many bytes and
/// its full text spilled to disk.
const SUBAGENT_FOLD_INLINE_BYTES: usize = 16 * 1024;

/// Directory under `ox_home()` where oversized subagent output is spilled
/// (one `<child sessionID>.md` per truncated child).
const SUBAGENT_OUTPUT_DIR: &str = "subagent-output";

/// Model-facing text when the parent's turn is interrupted while it waits.
/// Deliberately NOT `question`'s sentinel: no question was cancelled here, and
/// borrowing its text whitewashed the trajectory.
const CANCELLED_SENTINEL: &str = "(subagent cancelled by user interrupt)";

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
            return fail(CANCELLED_SENTINEL);
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
            let fold = transcript_fold(
                &tail,
                &child_id,
                &ox_core::config::ox_home().join(SUBAGENT_OUTPUT_DIR),
            );
            let mut metadata = json!({
                "sessionID": child_id,
                "status": "completed",
                "description": description,
            });
            if let (Some(queued), Some(map)) = (queued_for, metadata.as_object_mut()) {
                map.insert(
                    "queuedMs".to_owned(),
                    json!(u64::try_from(queued.as_millis()).unwrap_or(u64::MAX)),
                );
            }
            ok_text(envelope(&child_id, "completed", None, &fold)).with_metadata(metadata)
        }
        ChildOutcome::Failed(error) => {
            // Keep the sessionID visible: the model can resume the child.
            fail(format!("Subagent failed (sessionID: {child_id}): {error}"))
        }
        ChildOutcome::Interrupted => fail(format!("Subagent cancelled (sessionID: {child_id})")),
    }
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
/// Nothing here is persisted, so a server restart loses the notification
/// (crash recovery is out of scope). The slot is held by this task and
/// freed when it ends, on every return path.
fn spawn_completion_watcher(
    handle: std::sync::Arc<SubagentHandle>,
    parent_id: String,
    child_id: String,
    description: String,
    start: WatcherStart,
) {
    tokio::spawn(async move {
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
            match child_outcome(&tail) {
                ChildOutcome::Completed => (
                    "completed",
                    final_text(&tail).unwrap_or_else(|| {
                        "Subagent completed without a text response.".to_owned()
                    }),
                ),
                ChildOutcome::Failed(error) => ("error", format!("Subagent failed: {error}")),
                ChildOutcome::Interrupted => ("cancelled", "Subagent cancelled".to_owned()),
            }
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
        let wrapper = envelope(&child_id, state, Some(&description), &text);
        let inbox_id = ox_schema::id::id("msg");
        let item = ox_schema::inbox::Item::Synthetic {
            payload: ox_schema::inbox::SyntheticPayload {
                text: wrapper,
                description: Some(description),
                metadata: Some(record(&[
                    ("source", json!("subagent")),
                    ("childID", json!(child_id)),
                    ("state", json!(state)),
                ])),
            },
            delivery: ox_schema::inbox::Delivery::Steer,
        };
        if let Err(error) = handle
            .bus
            .publish(SessionEventData::InboxEnqueued {
                session_id: parent_id.clone(),
                inbox_id: inbox_id.clone(),
                item,
            })
            .await
        {
            tracing::warn!(parent = %parent_id, child = %child_id, error = %error, "unable to enqueue subagent completion");
            return;
        }
        if let Err(error) = spawner.dispatch(&parent_id, &inbox_id).await {
            tracing::warn!(parent = %parent_id, child = %child_id, error = %error, "unable to wake parent for subagent completion");
        }
    });
}

/// A `Record` from literal pairs (the schema's metadata map).
fn record(pairs: &[(&str, serde_json::Value)]) -> ox_schema::support::Record {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
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
/// when it fits [`SUBAGENT_FOLD_INLINE_BYTES`]; otherwise its head plus a
/// pointer to the full output spilled under `output_dir` (one
/// `<child sessionID>.md`). Pure over the child's settled tail apart from the
/// spill, so the caller reads that tail once.
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
    if final_reply.len() <= SUBAGENT_FOLD_INLINE_BYTES {
        return format!("Subagent report:\n\n{final_reply}");
    }
    let mut end = SUBAGENT_FOLD_INLINE_BYTES;
    while !final_reply.is_char_boundary(end) {
        end -= 1;
    }
    let head = &final_reply[..end];
    let omitted = final_reply.len() - end;
    let path = output_dir.join(format!("{child_id}.md"));
    let marker = match std::fs::create_dir_all(output_dir)
        .and_then(|()| std::fs::write(&path, &final_reply))
    {
        Ok(()) => format!(
            "[Output truncated: {omitted} bytes omitted. Full output: {}]",
            path.display()
        ),
        Err(error) => format!(
            "[Output truncated: {omitted} bytes omitted. Writing the full output failed: {error}]"
        ),
    };
    format!("Subagent report:\n\n{head}\n{marker}")
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

    use super::*;
    use crate::contract::ToolRegistry;
    use crate::jobs::JobInfo;

    /// A child that stays active until `settled` flips.
    #[derive(Default)]
    struct StuckChild {
        settled: AtomicBool,
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
            Box::pin(async { Ok(Vec::new()) })
        }
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
}
