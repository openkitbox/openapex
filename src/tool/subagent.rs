//! `subagent`: spawn a child session, run its turn to completion, and fold
//! its transcript into the parent's tool result.
//!
//! ox has no in-process agent runtime to reuse, so the tool builds on the two
//! primitives the runner already exposes: `session.create`-style rows (a child
//! session row with `parent_id` set, projected through the same event) and the
//! inbox/drain pipeline (`InboxEnqueued` + `runner.dispatch`), which is exactly
//! how an HTTP prompt drives a session. The tool waits for the child to go
//! idle and then reads its transcript back through the projector.

use serde_json::{Value, json};

use crate::cardinal::wire::ToolDef;
use crate::runner::{Tool, ToolContext, ToolOutput};
use crate::schema::event::SessionEventData;
use crate::schema::support::{LocationRef, SessionMetadata};

use super::{fail, object_param, ok_text, param};

/// The child's transcript is folded into the parent with this much text per
/// message, matching the compaction preview budget.
const MESSAGE_PREVIEW_CHARS: usize = 400;

/// Model-facing text when the parent's turn is interrupted while it waits.
/// Deliberately NOT `question`'s sentinel: no question was cancelled here, and
/// borrowing its text whitewashed the trajectory.
const CANCELLED_SENTINEL: &str = "(subagent cancelled by user interrupt)";

/// Safety fuse for the ancestor walk: a `parent_id` chain that does not
/// terminate within this many hops is corrupt (a cycle), so the walk fails
/// with an error instead of hanging or saturating below a higher limit.
const MAX_DEPTH_WALK: usize = 32;

/// The pieces of the runner the tool drives a child session through. The
/// runner installs itself here at startup ([`Runner::register_subagent`]), so
/// the tool never holds the runner that holds the tool.
#[derive(Clone)]
pub struct SubagentHandle {
    db: crate::db::Db,
    bus: crate::event_bus::EventBus,
    runner: std::sync::Arc<std::sync::OnceLock<crate::runner::Runner>>,
}

impl SubagentHandle {
    pub fn new(db: crate::db::Db, bus: crate::event_bus::EventBus) -> Self {
        Self {
            db,
            bus,
            runner: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// The runner connecting child dispatches. Registering twice keeps the
    /// first binding (startup happens once).
    pub fn attach(&self, runner: crate::runner::Runner) {
        let _ = self.runner.set(runner);
    }

    fn runner(&self) -> &crate::runner::Runner {
        self.runner
            .get()
            .expect("subagent handle attached at startup")
    }
}

pub struct SubagentTool {
    handle: std::sync::Arc<SubagentHandle>,
    depth_limit: usize,
}

impl SubagentTool {
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

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "subagent",
            "Spawn a child session, delegate a prompt to it, and wait for its final reply. The \
             child has its own context window and transcript; only its final message returns to \
             this conversation.\n\n\
             When to use: an isolated research or exploration burst whose intermediate steps \
             would flood this context (read a broad area, then summarize), or an independent \
             subtask that needs the full toolkit but not this conversation's history.\n\
             When NOT to use: a question a single read or grep answers (do it directly), work \
             that needs the parent's context, or several rounds of back-and-forth — the child \
             gets one prompt and answers once.",
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

async fn run(tool: &SubagentTool, ctx: &ToolContext, input: &Value) -> ToolOutput {
    let runner = tool.handle.runner();
    let prompt = match required(input, "prompt") {
        Ok(prompt) => prompt,
        Err(output) => return output,
    };
    let description = match required(input, "description") {
        Ok(description) => description,
        Err(output) => return output,
    };

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

    // The child session inherits the parent's directory through the create
    // event, exactly like `POST /api/session {parentID}`.
    let (child_id, project_id, directory, workspace_id, subpath) =
        match load_parent(&tool.handle.db, &ctx.session_id).await {
            Ok(Some(parent)) => (crate::id::id("ses"), parent.0, parent.1, parent.2, parent.3),
            Ok(None) => {
                return fail(format!("Unknown parent session: {}", ctx.session_id));
            }
            Err(error) => return fail(format!("Unable to load parent session: {error}")),
        };

    // Delegation facts ride the title and the parent link; `metadata` stays
    // empty (upstream keeps it host-owned).
    let created = SessionEventData::SessionCreated {
        session_id: child_id.clone(),
        project_id,
        location: LocationRef {
            directory,
            workspace_id,
        },
        subpath,
        parent_id: Some(ctx.session_id.clone()),
        slug: crate::slug::create(),
        title: Some(description.clone()),
        agent: None,
        model: None,
        metadata: Some(SessionMetadata::default()),
        permissions: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if let Err(error) = tool.handle.bus.publish(created).await {
        return fail(format!("Unable to create child session: {error}"));
    }

    // Enqueue the prompt, then wake the child through the same dispatch path
    // an HTTP prompt uses.
    let message_id = crate::id::id("msg");
    let payload = crate::schema::inbox::UserPayload {
        prompt: crate::schema::support::Prompt {
            // Fresh-context notice: the child knows nothing about the parent.
            text: format!("You are a subagent spawned by another session.\n{prompt}"),
            files: None,
            agents: None,
            skills: None,
        },
        metadata: None,
    };
    let enqueued = SessionEventData::InboxEnqueued {
        session_id: child_id.clone(),
        inbox_id: message_id.clone(),
        item: crate::schema::inbox::Item::User {
            payload,
            delivery: crate::schema::inbox::Delivery::Steer,
        },
    };
    if let Err(error) = tool.handle.bus.publish(enqueued).await {
        return fail(format!("Unable to enqueue child prompt: {error}"));
    }
    if let Err(error) = runner.dispatch(&child_id, &message_id).await {
        return fail(format!("Unable to start child session: {error}"));
    }

    // Wait for the child to settle (or the parent's turn to be interrupted —
    // the child is cancelled with it, so nothing keeps running). Upstream has
    // no timeout here: a foreground subagent waits indefinitely and only the
    // user can stop it. A wedged child is visible to the user, who can
    // interrupt the turn; killing healthy work on a wall clock is the worse
    // failure mode.
    let wait = async {
        while runner.is_active(&child_id) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        () = wait => {}
        () = ctx.cancellation.cancelled() => {
            runner.interrupt(&child_id);
            return fail(CANCELLED_SENTINEL);
        }
    }

    let history = match crate::runner::step::load_history(&tool.handle.db, &child_id).await {
        Ok(history) => history,
        Err(error) => {
            return fail(format!(
                "Child session finished but its transcript is unreadable: {error}"
            ));
        }
    };
    match child_outcome(&history) {
        ChildOutcome::Completed => {
            let fold = transcript_fold(&history, &child_id);
            ok_text(format!(
                "<subagent sessionID=\"{child_id}\" state=\"completed\">\n{fold}\n</subagent>"
            ))
            .with_metadata(json!({
                "sessionID": child_id,
                "status": "completed",
                "description": description,
            }))
        }
        ChildOutcome::Failed(error) => {
            // Keep the sessionID visible: the model can resume the child.
            fail(format!("Subagent failed (sessionID: {child_id}): {error}"))
        }
        ChildOutcome::Interrupted => {
            fail(format!("Subagent cancelled (sessionID: {child_id})"))
        }
    }
}

/// `(project_id, directory, workspace_id, subpath)` of a session row.
type ParentTuple = (String, String, Option<String>, Option<String>);

/// Ancestor hops from `session_id` up the `parent_id` chain. A root session is
/// `0`; an unknown session is also `0` (nothing to nest under). A chain that
/// does not terminate within [`MAX_DEPTH_WALK`] hops is an error (a corrupt
/// cycle), not a saturating count — the fuse must fail closed for any
/// configured depth limit.
async fn nesting_depth(db: &crate::db::Db, session_id: &str) -> Result<usize, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
        .await
        .map_err(|error| error.to_string())?;
    let mut current = session_id.to_owned();
    let mut depth = 0usize;
    while depth < MAX_DEPTH_WALK {
        let Some(row) = crate::db::optional(&mut stmt, (current.clone(),))
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

async fn load_parent(db: &crate::db::Db, session_id: &str) -> Result<Option<ParentTuple>, String> {
    let conn = db.acquire().await.map_err(|error| error.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT project_id, directory, workspace_id, path
             FROM session_v2 WHERE id = ?1",
        )
        .await
        .map_err(|error| error.to_string())?;
    let row = crate::db::optional(&mut stmt, (session_id,))
        .await
        .map_err(|error| error.to_string())?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some((
        row.get::<String>(0).map_err(|error| error.to_string())?,
        row.get::<String>(1).map_err(|error| error.to_string())?,
        row.get::<Option<String>>(2)
            .map_err(|error| error.to_string())?,
        row.get::<Option<String>>(3)
            .map_err(|error| error.to_string())?,
    )))
}

/// The child's settle state, decided from persisted traces only — no new
/// state is invented for it.
#[derive(Debug, PartialEq, Eq)]
enum ChildOutcome {
    /// Turn ended clean: fold the transcript.
    Completed,
    /// Turn failed: keep the child's sessionID visible so the model can resume.
    Failed(String),
    /// Child was interrupted (e.g. directly through the API).
    Interrupted,
}

/// Read the settle state off the transcript's trailing `idle` row. A
/// transcript with no idle row reads as completed.
fn child_outcome(messages: &[crate::schema::message::Message]) -> ChildOutcome {
    use crate::schema::message::{IdleOutcome, Message};
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

/// The child's reply for the parent: every assistant text message joined
/// (previewed), or a fallback when the child produced nothing. Pure over an
/// already-loaded transcript so the caller loads history once.
fn transcript_fold(messages: &[crate::schema::message::Message], child_id: &str) -> String {
    use crate::schema::message::{AssistantContent, Message};
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        if let Message::Assistant(assistant) = message {
            for content in &assistant.content {
                if let AssistantContent::Text { text, .. } = content
                    && !text.trim().is_empty()
                {
                    parts.push(preview(text));
                }
            }
        }
    }
    if parts.is_empty() {
        return format!(
            "Subagent finished without a final reply. Its transcript is on session {child_id}."
        );
    }
    format!("Subagent report:\n\n{}", parts.join("\n\n"))
}

/// Keep the parent's context small: the head of each message only.
fn preview(text: &str) -> String {
    let mut end = MESSAGE_PREVIEW_CHARS;
    if text.len() > end {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}…", &text[..end]);
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use crate::schema::event::SessionEventData;
    use crate::schema::support::LocationRef;

    fn assistant_text(text: &str) -> crate::schema::message::Message {
        crate::schema::message::Message::Assistant(Box::new(
            crate::schema::message::AssistantMessage {
                id: "msg_a".to_owned(),
                metadata: None,
                time: crate::schema::message::AssistantTime {
                    created: 0,
                    streamed: None,
                    completed: Some(1),
                },
                agent: "build".to_owned(),
                model: crate::schema::support::ModelRef {
                    id: "k3".to_owned(),
                    provider_id: "cardinal".to_owned(),
                    variant: None,
                },
                content: vec![crate::schema::message::AssistantContent::Text {
                    text: text.to_owned(),
                    state: None,
                }],
                snapshot: None,
                finish: None,
                raw_finish: None,
                provider_state: None,
                cost: None,
                tokens: None,
                error: None,
                retry: None,
            },
        ))
    }

    fn idle(outcome: crate::schema::message::IdleOutcome) -> crate::schema::message::Message {
        crate::schema::message::Message::Idle {
            id: "msg_i".to_owned(),
            metadata: None,
            time: crate::schema::message::TimeCreated { created: 2 },
            outcome,
            error: None,
        }
    }

    #[test]
    fn child_outcome_reads_the_trailing_idle_row() {
        use crate::schema::message::IdleOutcome;
        let completed = vec![assistant_text("done"), idle(IdleOutcome::Succeeded)];
        assert!(matches!(
            super::child_outcome(&completed),
            super::ChildOutcome::Completed
        ));

        let interrupted = vec![assistant_text("partial"), idle(IdleOutcome::Interrupted)];
        assert!(matches!(
            super::child_outcome(&interrupted),
            super::ChildOutcome::Interrupted
        ));

        let bare = vec![assistant_text("done")];
        assert!(matches!(
            super::child_outcome(&bare),
            super::ChildOutcome::Completed
        ));
    }

    #[test]
    fn child_outcome_failed_carries_the_session_error_message() {
        use crate::schema::message::IdleOutcome;
        let failed = vec![
            assistant_text("trying"),
            crate::schema::message::Message::Idle {
                id: "msg_i".to_owned(),
                metadata: None,
                time: crate::schema::message::TimeCreated { created: 2 },
                outcome: IdleOutcome::Failed,
                error: Some(crate::schema::support::SessionError {
                    kind: "provider".to_owned(),
                    message: "boom".to_owned(),
                    status: None,
                    response: None,
                }),
            },
        ];
        assert_eq!(
            super::child_outcome(&failed),
            super::ChildOutcome::Failed("boom".to_owned())
        );
    }

    #[test]
    fn cancelled_sentinel_is_not_the_question_one() {
        assert_ne!(
            super::CANCELLED_SENTINEL,
            crate::tool::question::CANCELLED_SENTINEL
        );
        assert_eq!(
            super::CANCELLED_SENTINEL,
            "(subagent cancelled by user interrupt)"
        );
    }

    async fn seeded_db() -> (crate::db::Db, crate::event_bus::EventBus) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("ox.db"))
            .await
            .unwrap();
        let bus = crate::event_bus::EventBus::new(db.clone());
        for (id, parent) in [
            ("ses_root", None),
            ("ses_child", Some("ses_root")),
            ("ses_grand", Some("ses_child")),
        ] {
            bus.publish(SessionEventData::SessionCreated {
                session_id: id.to_owned(),
                project_id: "proj_test".to_owned(),
                location: LocationRef {
                    directory: "/tmp".to_owned(),
                    workspace_id: None,
                },
                subpath: None,
                parent_id: parent.map(str::to_owned),
                slug: crate::slug::create(),
                title: None,
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            })
            .await
            .unwrap();
        }
        (db, bus)
    }

    #[tokio::test]
    async fn nesting_depth_counts_ancestors_and_survives_cycles() {
        let (db, _bus) = seeded_db().await;
        assert_eq!(super::nesting_depth(&db, "ses_root").await.unwrap(), 0);
        assert_eq!(super::nesting_depth(&db, "ses_child").await.unwrap(), 1);
        assert_eq!(super::nesting_depth(&db, "ses_grand").await.unwrap(), 2);
        assert_eq!(super::nesting_depth(&db, "ses_missing").await.unwrap(), 0);

        // A corrupt self-cycle is an error, not a saturating count: the fuse
        // must fail closed for any configured depth limit.
        let conn = db.acquire().await.unwrap();
        conn.execute(
            "UPDATE session_v2 SET parent_id = id WHERE id = 'ses_root'",
            (),
        )
        .await
        .unwrap();
        drop(conn);
        assert!(super::nesting_depth(&db, "ses_root").await.is_err());
    }
}
