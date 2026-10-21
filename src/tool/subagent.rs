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
}

impl SubagentTool {
    pub fn new(handle: std::sync::Arc<SubagentHandle>) -> Self {
        Self { handle }
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
            text: prompt,
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

    match transcript(tool, &child_id).await {
        Ok(summary) => ok_text(summary).with_metadata(json!({
            "sessionId": child_id,
            "description": description,
        })),
        Err(error) => fail(format!(
            "Child session finished but its transcript is unreadable: {error}"
        )),
    }
}

/// `(project_id, directory, workspace_id, subpath)` of a session row.
type ParentTuple = (String, String, Option<String>, Option<String>);

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

/// The child's reply for the parent: every assistant text message joined,
/// or a fallback when the child produced nothing.
async fn transcript(tool: &SubagentTool, child_id: &str) -> Result<String, String> {
    let rows = crate::runner::step::load_history(&tool.handle.db, child_id)
        .await
        .map_err(|error| error.to_string())?;
    let mut parts: Vec<String> = Vec::new();
    for message in rows {
        if let crate::schema::message::Message::Assistant(assistant) = message {
            for content in &assistant.content {
                if let crate::schema::message::AssistantContent::Text { text, .. } = content
                    && !text.trim().is_empty()
                {
                    parts.push(preview(text));
                }
            }
        }
    }
    if parts.is_empty() {
        return Ok(format!(
            "Subagent finished without a final reply. Its transcript is on session {child_id}."
        ));
    }
    Ok(format!("Subagent report:\n\n{}", parts.join("\n\n")))
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
}
