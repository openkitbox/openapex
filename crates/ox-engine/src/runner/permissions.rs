//! The tool permission gate: the one check every tool call passes before it
//! runs (built-in, `ctx_*` and MCP alike, since all of them dispatch through
//! the registry in `step/tool.rs`).
//!
//! Owns: the policy lookup over `[permissions]`, the per-session "allow for
//! this session" grants, and the `ask` interaction. The interaction is a form
//! on the same channel the `question` tool uses (`ox_tools::form`), created
//! under the session's *root* so a subagent's call asks on the screen the
//! user is actually watching; a child session has no client of its own and
//! a form there would only time out.
//!
//! Invariants:
//! - A `deny` never reaches the tool, and an `ask` reaches it only after an
//!   explicit allow. No answer within [`ASK_TIMEOUT`], a dismissed form or an
//!   interrupted turn all fail closed.
//! - Grants are process memory keyed by session and tool; they go with the
//!   session's runner entry ([`Grants::forget`]) and never outlive a restart.
//! - A permissive table ([`PermissionsConfig::is_permissive`]) costs one
//!   boolean: no lookup, no JSON rendering, no DB read.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use ox_core::config::{Permission, PermissionsConfig};
use ox_db::Db;
use ox_tools::form::{self, Field, FormValue, Option_, Outcome};

/// How long an `ask` waits for a client. The same bound the `question` tool
/// uses: a human decides in seconds, and a turn with nobody watching must
/// not park forever.
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// Most characters of the call's arguments shown in the prompt.
const PREVIEW_CHARS: usize = 400;

/// The ancestor walk's bound, like `subagent::MAX_DEPTH_WALK`: a `parent_id`
/// chain this long is a cycle, and the ask then falls back to the session
/// itself rather than looping.
const MAX_ROOT_WALK: usize = 32;

/// Field key and option values of the permission form (clients answer with
/// the option's `value`).
const DECISION_KEY: &str = "decision";
const ALLOW_ONCE: &str = "once";
const ALLOW_SESSION: &str = "session";
const DENY: &str = "deny";

/// Per-session grants from "allow for this session" answers.
#[derive(Default)]
pub(crate) struct Grants {
    by_session: Mutex<HashMap<String, HashSet<String>>>,
}

impl Grants {
    fn contains(&self, session_id: &str, tool: &str) -> bool {
        super::lock(&self.by_session)
            .get(session_id)
            .is_some_and(|tools| tools.contains(tool))
    }

    fn insert(&self, session_id: &str, tool: &str) {
        super::lock(&self.by_session)
            .entry(session_id.to_owned())
            .or_default()
            .insert(tool.to_owned());
    }

    /// Drop a session's grants (its runner entry is going away).
    pub(crate) fn forget(&self, session_id: &str) {
        super::lock(&self.by_session).remove(session_id);
    }
}

/// The gate's verdict for one call.
pub(crate) enum Decision {
    /// Run the tool.
    Allow,
    /// Do not run it; the string is the model-facing reason.
    Deny(String),
}

/// Everything one authorization needs, cloned out of the drain context so the
/// check runs inside the spawned tool task.
#[derive(Clone)]
pub(crate) struct Gate {
    pub config: std::sync::Arc<ox_core::config::Config>,
    pub db: Db,
    pub grants: std::sync::Arc<Grants>,
}

impl Gate {
    /// Decide whether `tool` may run with `input` for `session_id`. `ask`
    /// prompts the user (on the session's root) and waits; `cancellation` is
    /// the turn's token, so an interrupt ends the wait as a refusal.
    pub(crate) async fn authorize(
        &self,
        session_id: &str,
        directory: &std::path::Path,
        tool: &str,
        input: &Value,
        cancellation: &CancellationToken,
    ) -> Decision {
        let permissions: &PermissionsConfig = &self.config.permissions;
        if permissions.is_permissive() {
            return Decision::Allow;
        }
        let arguments = input.to_string();
        match permissions.policy_for(tool, &arguments) {
            Permission::Allow => Decision::Allow,
            Permission::Deny => Decision::Deny(format!(
                "Tool `{tool}` is denied by the permissions configuration; it was not run."
            )),
            Permission::Ask => {
                if self.grants.contains(session_id, tool) {
                    return Decision::Allow;
                }
                self.ask(session_id, directory, tool, input, &arguments, cancellation)
                    .await
            }
        }
    }

    async fn ask(
        &self,
        session_id: &str,
        directory: &std::path::Path,
        tool: &str,
        input: &Value,
        arguments: &str,
        cancellation: &CancellationToken,
    ) -> Decision {
        let owner = root_session(&self.db, session_id).await;
        let pending = form::shared().create(
            &owner,
            &directory.to_string_lossy(),
            "Permission",
            vec![decision_field(tool, &preview(tool, input, arguments))],
        );
        let form_id = pending.info().id.clone();
        let outcome = tokio::select! {
            outcome = pending.wait(ASK_TIMEOUT) => outcome,
            () = cancellation.cancelled() => {
                settle_cancel(&owner, &form_id, "interrupted");
                return Decision::Deny(format!("Permission for `{tool}` was not granted: the turn was interrupted."));
            }
        };
        match outcome {
            None => {
                settle_cancel(&owner, &form_id, "nobody answered");
                Decision::Deny(format!(
                    "Permission for `{tool}` was not granted: nobody answered within {} seconds.",
                    ASK_TIMEOUT.as_secs()
                ))
            }
            Some(Outcome::Cancelled(_)) => Decision::Deny(format!(
                "Permission for `{tool}` was not granted: the user dismissed the request."
            )),
            Some(Outcome::Answered(answer)) => match decision_of(answer.get(DECISION_KEY)) {
                Some(ALLOW_ONCE) => Decision::Allow,
                Some(ALLOW_SESSION) => {
                    self.grants.insert(session_id, tool);
                    Decision::Allow
                }
                _ => Decision::Deny(format!(
                    "Permission for `{tool}` was refused by the user; it was not run."
                )),
            },
        }
    }
}

/// The answer's option value, if it is one of ours.
fn decision_of(value: Option<&FormValue>) -> Option<&'static str> {
    let chosen = match value? {
        FormValue::String(text) => text.as_str(),
        FormValue::Multiple(values) => values.first()?.as_str(),
        FormValue::Number(_) | FormValue::Boolean(_) => return None,
    };
    [ALLOW_ONCE, ALLOW_SESSION, DENY]
        .into_iter()
        .find(|option| *option == chosen)
}

/// Cancel a form nobody will answer any more; a settle race (the client
/// answered at the same instant) is not an error worth reporting.
fn settle_cancel(owner: &str, form_id: &str, message: &str) {
    if let Err(error) = form::shared().cancel(owner, form_id, Some(message.to_owned())) {
        tracing::debug!(form = %form_id, ?error, "permission form already settled");
    }
}

/// The one-field permission form: three fixed options, no free text.
fn decision_field(tool: &str, preview: &str) -> Field {
    Field::String {
        key: DECISION_KEY.to_owned(),
        title: Some(format!("Allow `{tool}`?")),
        description: Some(preview.to_owned()),
        required: Some(true),
        options: Some(vec![
            Option_ {
                value: ALLOW_ONCE.to_owned(),
                label: "Allow once".to_owned(),
                description: None,
            },
            Option_ {
                value: ALLOW_SESSION.to_owned(),
                label: "Allow for this session".to_owned(),
                description: Some(format!(
                    "Every later `{tool}` call in this session runs without asking."
                )),
            },
            Option_ {
                value: DENY.to_owned(),
                label: "Deny".to_owned(),
                description: None,
            },
        ]),
        custom: Some(false),
    }
}

/// What the user sees of the call: the command itself for `shell`, the
/// compact arguments otherwise, cut at a character boundary.
fn preview(tool: &str, input: &Value, arguments: &str) -> String {
    let text = match (tool, input.get("command").and_then(Value::as_str)) {
        ("shell", Some(command)) => command,
        _ => arguments,
    };
    let mut out: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// The root of `session_id`'s parent chain (itself when it has no parent, is
/// unknown, or the walk fails). The form goes there because that is the
/// session a client is attached to.
async fn root_session(db: &Db, session_id: &str) -> String {
    let mut current = session_id.to_owned();
    for _ in 0..MAX_ROOT_WALK {
        match parent_of(db, &current).await {
            Ok(Some(parent)) => current = parent,
            Ok(None) => return current,
            Err(error) => {
                tracing::warn!(session = %session_id, %error, "permission ask: parent lookup failed; asking on the session itself");
                return session_id.to_owned();
            }
        }
    }
    tracing::warn!(session = %session_id, "permission ask: parent chain exceeds {MAX_ROOT_WALK} hops; asking on the session itself");
    session_id.to_owned()
}

async fn parent_of(db: &Db, session_id: &str) -> anyhow::Result<Option<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT parent_id FROM session_v2 WHERE id = ?1")
        .await?;
    match ox_db::optional(&mut stmt, [session_id]).await? {
        Some(row) => Ok(row.get::<Option<String>>(0)?),
        None => Ok(None),
    }
}
