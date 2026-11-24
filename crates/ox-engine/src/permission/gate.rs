//! The V2 permission gate: the single owner of pending requests and their
//! replies. HTTP handlers read the same pending map through [`Self::pending`];
//! no second copy of a request is ever kept.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ox_core::config::{Config, Permission};
use ox_core::event_bus::EventBus;
use ox_db::Db;
use ox_schema::permission::{Asked, Effect, Replied, Reply, Rule, new_request_id};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::engine::evaluate;
use super::extract::extract;
use super::store::SavedStore;

/// How long an `ask` waits for a client when the service is in production.
const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// The tool call that caused an ask, as upstream `Source.Tool`.
#[derive(Debug, Clone)]
pub struct SourceRef {
    pub message_id: String,
    pub call_id: String,
}

/// The gate's verdict for one call.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run the tool.
    Allow,
    /// Do not run it; the string is the model-facing reason.
    Deny(String),
}

/// Why a reply could not be applied (HTTP maps this to 404 / 400).
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplyError {
    /// Unknown, already answered or timed-out request id.
    #[error("permission request not found")]
    NotFound,
    /// The decision is syntactically known but not legal here (e.g. `always`
    /// with no `save`, so the client must not have offered it).
    #[error("invalid permission reply: {0}")]
    Invalid(String),
}

struct Answer {
    reply: Reply,
    message: Option<String>,
}

/// One authoritative pending request. T5 reads the shared `Asked` handle;
/// the service keeps the rest private so replies cannot bypass evaluation.
struct Pending {
    asked: Arc<Asked>,
    project_id: String,
    session_id: String,
    directory: String,
    action: String,
    resources: Vec<String>,
    save: Option<Vec<String>>,
    layers: Layers,
    floor: Option<Permission>,
    tx: oneshot::Sender<Answer>,
}

/// The rule layers a request was created with, shared with its pending entry
/// so an `always` cascade re-evaluates the same rules without copying them.
#[derive(Clone)]
struct Layers {
    agent: &'static [Rule],
    config: Arc<Vec<Rule>>,
    session: Arc<Vec<Rule>>,
}

impl Layers {
    fn as_slices(&self) -> Vec<&[Rule]> {
        vec![self.agent, self.config.as_slice(), self.session.as_slice()]
    }
}

/// Authoritative per-session facts read once per authorization: the project
/// saved rules belong to, the parent id an ask advertises, and the session's
/// own ruleset.
struct SessionFacts {
    project_id: String,
    parent_id: Option<String>,
    rules: Vec<Rule>,
}

/// Named inputs to one authorization, so the call site never relies on a
/// long positional argument list.
pub(crate) struct AuthorizeParams<'a> {
    pub config: &'a Config,
    pub agent: &'static ox_tools::agent::Agent,
    pub session_id: &'a str,
    pub directory: &'a Path,
    pub tool: &'a str,
    pub input: &'a Value,
    pub source: Option<SourceRef>,
    pub unattended: Option<&'a [Rule]>,
    /// Whether `tool` was registered by the MCP hub; MCP calls use the
    /// tool-name resource instead of parsing built-in arguments.
    pub is_mcp: bool,
    pub cancellation: &'a CancellationToken,
}

struct AskParams<'a> {
    session_id: &'a str,
    facts: &'a SessionFacts,
    directory: &'a Path,
    tool: &'a str,
    action: &'a str,
    resources: &'a [String],
    save: &'a [String],
    source: Option<SourceRef>,
    layers: Layers,
    floor: Option<Permission>,
    cancellation: &'a CancellationToken,
}

#[derive(Clone)]
pub struct PermissionService {
    db: Db,
    bus: EventBus,
    saved: SavedStore,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    shutdown: CancellationToken,
    ask_timeout: Duration,
}

impl PermissionService {
    #[must_use]
    pub(crate) fn new(db: Db, bus: EventBus, shutdown: CancellationToken) -> Self {
        Self::with_timeout(db, bus, shutdown, ASK_TIMEOUT)
    }

    fn with_timeout(
        db: Db,
        bus: EventBus,
        shutdown: CancellationToken,
        ask_timeout: Duration,
    ) -> Self {
        Self {
            saved: SavedStore::new(db.clone()),
            db,
            bus,
            pending: Arc::new(Mutex::new(HashMap::new())),
            shutdown,
            ask_timeout,
        }
    }

    /// Live requests, as shared handles: the HTTP list/get paths never copy a
    /// request's mutable state out of the service.
    #[must_use]
    pub fn pending(&self) -> Vec<Arc<Asked>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|pending| Arc::clone(&pending.asked))
            .collect()
    }

    #[must_use]
    pub fn pending_one(&self, id: &str) -> Option<Arc<Asked>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .map(|pending| Arc::clone(&pending.asked))
    }

    #[must_use]
    pub fn saved(&self) -> &SavedStore {
        &self.saved
    }

    /// Settle one request. A successful reply is consumed exactly once: the
    /// entry is removed under the lock before the waiter is woken.
    pub async fn reply(
        &self,
        session_id: &str,
        request_id: &str,
        reply: Reply,
        message: Option<String>,
    ) -> Result<(), ReplyError> {
        let (project_id, action, save) = {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(entry) = pending.get(request_id) else {
                return Err(ReplyError::NotFound);
            };
            if entry.session_id != session_id {
                return Err(ReplyError::NotFound);
            }
            (
                entry.project_id.clone(),
                entry.action.clone(),
                entry.save.clone(),
            )
        };
        if reply == Reply::Always {
            match save.as_ref() {
                Some(resources) if !resources.is_empty() => {
                    self.saved
                        .save(&project_id, &action, resources)
                        .await
                        .map_err(|error| {
                            ReplyError::Invalid(format!("could not save approval: {error}"))
                        })?;
                }
                _ => {
                    return Err(ReplyError::Invalid(
                        "always requires a non-empty save set".to_owned(),
                    ));
                }
            }
        }

        // A cascade must be justified by the rules that now exist, not by a
        // matching action/resource alone. A read failure is fail-closed.
        let cascade_rules = if reply == Reply::Always {
            match self.saved.rules(&project_id).await {
                Ok(rules) => Some(rules),
                Err(error) => {
                    return Err(ReplyError::Invalid(format!(
                        "could not re-check saved approvals: {error}"
                    )));
                }
            }
        } else {
            None
        };

        // One lock pass removes the current request and, for `always`, every
        // same-session pending request the new saved rule re-evaluates to
        // Allow. A request is removed exactly once: a concurrent reply sees
        // NotFound.
        let mut settled = Vec::new();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(entry) = pending.remove(request_id) else {
                return Err(ReplyError::NotFound);
            };
            settled.push((request_id.to_owned(), entry));
            if reply == Reply::Always {
                let rules = cascade_rules.as_deref().unwrap_or_default();
                let cascading: Vec<String> = pending
                    .iter()
                    .filter(|(_, entry)| {
                        entry.session_id == session_id
                            && entry.project_id == project_id
                            && cascade_allows(entry, rules)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in cascading {
                    if let Some(entry) = pending.remove(&id) {
                        settled.push((id, entry));
                    }
                }
            }
        }
        for (id, entry) in settled {
            if entry
                .tx
                .send(Answer {
                    reply,
                    message: message.clone(),
                })
                .is_err()
            {
                tracing::debug!(request_id = %id, "permission waiter is gone");
            }
            self.broadcast_replied(&entry.session_id, &entry.directory, &id, reply);
        }
        Ok(())
    }

    /// Authorize one tool call. `unattended` is `Some` for automation turns:
    /// this path never creates a pending request and never waits.
    pub(crate) async fn authorize(&self, params: AuthorizeParams<'_>) -> Decision {
        let AuthorizeParams {
            config,
            agent,
            session_id,
            directory,
            tool,
            input,
            source,
            unattended,
            is_mcp,
            cancellation,
        } = params;
        let Some(extraction) = extract(tool, input, directory, is_mcp) else {
            return Decision::Allow;
        };
        let Ok(mut facts) = self.session_facts(session_id).await else {
            return Decision::Deny(format!(
                "Permission for `{tool}` was refused: the session could not be read."
            ));
        };
        let Ok(saved) = self.saved.rules(&facts.project_id).await else {
            return Decision::Deny(format!(
                "Permission for `{tool}` was refused: saved approvals could not be read."
            ));
        };
        let floor = config.permissions.tool_floor(tool, &input.to_string());
        if floor == Some(Permission::Deny) {
            return Decision::Deny(format!(
                "Tool `{tool}` is denied by the permissions configuration; it was not run."
            ));
        }

        // The config's fallback is a rule too, so `default = "allow"` and
        // `default = "deny"` are honoured without removing the rules that
        // follow it. `ask` needs no rule: it is the engine's fallback.
        let mut config_rules = config.permissions.rules.clone();
        let fallback = match config.permissions.default {
            Permission::Allow => Some(Effect::Allow),
            Permission::Deny => Some(Effect::Deny),
            Permission::Ask => None,
        };
        if let Some(effect) = fallback {
            // The fallback must precede explicit rules: within a layer the
            // *last* match wins, so an explicit allow can still open a call.
            config_rules.insert(
                0,
                Rule {
                    action: "*".to_owned(),
                    resource: "*".to_owned(),
                    effect,
                },
            );
        }
        let layers = Layers {
            agent: agent.permissions,
            config: Arc::new(config_rules),
            session: Arc::new(std::mem::take(&mut facts.rules)),
        };
        let layer_refs = layers.as_slices();
        let mut effect = evaluate(
            &layer_refs,
            &saved,
            unattended,
            extraction.action,
            &extraction.resources,
        );
        // The ox-specific `[permissions.tools]` floor can only tighten.
        if floor == Some(Permission::Ask) && effect == Effect::Allow {
            effect = Effect::Ask;
        }
        match effect {
            Effect::Allow => Decision::Allow,
            Effect::Deny => Decision::Deny(format!(
                "Permission for `{tool}` was denied by the configured rules; it was not run."
            )),
            Effect::Ask if unattended.is_some() => Decision::Deny(format!(
                "Permission for `{tool}` was denied: this unattended run has no matching allow rule."
            )),
            Effect::Ask => {
                self.ask(AskParams {
                    session_id,
                    facts: &facts,
                    directory,
                    tool,
                    action: extraction.action,
                    resources: &extraction.resources,
                    save: &extraction.save,
                    source,
                    layers,
                    floor,
                    cancellation,
                })
                .await
            }
        }
    }
}

/// Re-evaluate a candidate with the rules it was created under and the saved
/// rules written by the `always` reply. An explicit ask floor can only
/// tighten, exactly as in the original authorization.
fn cascade_allows(entry: &Pending, saved: &[Rule]) -> bool {
    if entry.floor == Some(Permission::Ask) {
        return false;
    }
    let layers = entry.layers.as_slices();
    evaluate(&layers, saved, None, &entry.action, &entry.resources) == Effect::Allow
}

impl PermissionService {
    async fn session_facts(&self, session_id: &str) -> Result<SessionFacts> {
        let conn = self.db.acquire().await?;
        let mut stmt = conn
            .prepare("SELECT project_id, parent_id, permission FROM session_v2 WHERE id = ?1")
            .await?;
        let row = ox_db::optional(&mut stmt, (session_id,))
            .await?
            .ok_or_else(|| anyhow::anyhow!("session {session_id} does not exist"))?;
        let rules: Vec<Rule> = match row.get::<Option<String>>(2)? {
            Some(text) if !text.trim().is_empty() => serde_json::from_str(&text)?,
            _ => Vec::new(),
        };
        Ok(SessionFacts {
            project_id: row.get(0)?,
            parent_id: row.get(1)?,
            rules,
        })
    }

    async fn ask(&self, params: AskParams<'_>) -> Decision {
        let AskParams {
            session_id,
            facts,
            directory,
            tool,
            action,
            resources,
            save,
            source,
            layers,
            floor,
            cancellation,
        } = params;
        let id = new_request_id();
        let mut metadata = Map::new();
        if let Some(parent) = &facts.parent_id {
            metadata.insert("parentID".to_owned(), json!(parent));
        }
        let asked = Asked {
            id: id.clone(),
            session_id: session_id.to_owned(),
            action: action.to_owned(),
            resources: resources.to_vec(),
            save: (!save.is_empty()).then(|| save.to_vec()),
            metadata: (!metadata.is_empty()).then_some(metadata),
            source: source.map(|source| {
                // `Source` is a wire type with private fields; constructing
                // through serde keeps its V2 shape owned by ox-schema.
                serde_json::from_value(json!({
                    "type": "tool",
                    "messageID": source.message_id,
                    "id": source.call_id,
                }))
                .expect("the V2 tool Source shape is valid")
            }),
            message: None,
        };
        let (tx, answer) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending.contains_key(&id) {
                return Decision::Deny(format!("Permission for `{tool}` could not be requested."));
            }
            pending.insert(
                id.clone(),
                Pending {
                    asked: Arc::new(asked.clone()),
                    project_id: facts.project_id.clone(),
                    session_id: session_id.to_owned(),
                    directory: directory.to_string_lossy().into_owned(),
                    action: action.to_owned(),
                    resources: resources.to_vec(),
                    save: asked.save.clone(),
                    layers,
                    floor,
                    tx,
                },
            );
        }
        let data = match serde_json::to_value(&asked) {
            Ok(data) => data,
            Err(error) => {
                self.finish(&id, Reply::Reject, None);
                tracing::warn!(%error, "permission ask could not be serialized");
                return Decision::Deny(format!("Permission for `{tool}` could not be requested."));
            }
        };
        self.bus
            .broadcast_ephemeral("permission.asked", data, directory.to_str());

        let decided = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => {
                self.finish(&id, Reply::Reject, Some("server is shutting down".to_owned()));
                return Decision::Deny(format!("Permission for `{tool}` was not granted: the server is shutting down."));
            }
            () = cancellation.cancelled() => {
                self.finish(&id, Reply::Reject, Some("turn interrupted".to_owned()));
                return Decision::Deny(format!("Permission for `{tool}` was not granted: the turn was interrupted."));
            }
            result = answer => match result {
                Ok(answer) => answer,
                Err(_) => {
                    self.finish(&id, Reply::Reject, None);
                    return Decision::Deny(format!("Permission for `{tool}` was not granted: the request was dropped."));
                }
            },
            () = tokio::time::sleep(self.ask_timeout) => {
                self.finish(&id, Reply::Reject, Some("timed out".to_owned()));
                return Decision::Deny(format!(
                    "Permission for `{tool}` was not granted: nobody answered within {} seconds.",
                    self.ask_timeout.as_secs()
                ));
            }
        };
        match decided.reply {
            Reply::Once => Decision::Allow,
            Reply::Always => Decision::Allow,
            Reply::Reject => Decision::Deny(match decided.message {
                Some(message) if !message.trim().is_empty() => {
                    format!("Permission for `{tool}` was refused by the user: {message}")
                }
                _ => format!("Permission for `{tool}` was refused by the user; it was not run."),
            }),
        }
    }

    /// Settle a request without a client reply (timeout, interruption,
    /// shutdown). The client still gets a `replied` frame so no dialog stays
    /// open, and the pending entry is removed by the same helper as a reply.
    fn finish(&self, id: &str, reply: Reply, message: Option<String>) {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        if let Some(entry) = entry {
            if entry.tx.send(Answer { reply, message }).is_err() {
                tracing::debug!(request_id = %id, "permission waiter is gone");
            }
            self.broadcast_replied(&entry.session_id, &entry.directory, id, reply);
        }
    }

    fn broadcast_replied(&self, session_id: &str, directory: &str, request_id: &str, reply: Reply) {
        let data = serde_json::to_value(Replied {
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            reply,
        });
        match data {
            Ok(data) => self
                .bus
                .broadcast_ephemeral("permission.replied", data, Some(directory)),
            Err(error) => tracing::warn!(%error, "permission replied could not be serialized"),
        }
    }
}
