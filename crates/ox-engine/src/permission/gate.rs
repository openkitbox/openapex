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

#[cfg(test)]
mod permission_gate_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ox_core::config::{Config, Permission};
    use ox_core::event_bus::EventBus;
    use ox_db::Db;
    use ox_schema::event::EventEnvelope;
    use ox_schema::permission::{Effect, Reply, Rule};
    use serde_json::{Value, json};
    use tokio::sync::broadcast;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{AuthorizeParams, Decision, PermissionService, ReplyError};

    struct Fixture {
        service: Arc<PermissionService>,
        bus: EventBus,
        config: Arc<Config>,
        shutdown: CancellationToken,
    }

    async fn fixture(timeout_ms: u64) -> anyhow::Result<Fixture> {
        let db = Db::open_in_memory().await?;
        let tx = db.begin().await?;
        tx.execute(
            "INSERT INTO session_v2
             (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES ('ses_child', 'prj_1', 's', '/work', '1', 1, 1),
                    ('ses_sibling', 'prj_1', 's', '/work', '1', 1, 1),
                    ('ses_other', 'prj_2', 's', '/work', '1', 1, 1)",
            (),
        )
        .await?;
        tx.execute(
            "UPDATE session_v2 SET parent_id = 'ses_root' WHERE id = 'ses_child'",
            (),
        )
        .await?;
        tx.commit().await?;
        let bus = EventBus::new(db.clone());
        let shutdown = CancellationToken::new();
        let mut config = Config::default();
        config.permissions.default = Permission::Allow;
        config.permissions.rules = vec![Rule {
            action: "*".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Ask,
        }];
        Ok(Fixture {
            service: Arc::new(PermissionService::with_timeout(
                db.clone(),
                bus.clone(),
                shutdown.clone(),
                Duration::from_millis(timeout_ms),
            )),
            bus,
            config: Arc::new(config),
            shutdown,
        })
    }

    async fn next_event(rx: &mut broadcast::Receiver<EventEnvelope>, kind: &str) -> EventEnvelope {
        timeout(Duration::from_secs(2), async {
            loop {
                let env = rx.recv().await.expect("bus is open");
                if env.kind == kind {
                    return env;
                }
            }
        })
        .await
        .expect("event arrives within the bounded wait")
    }

    async fn start_ask(
        fix: &Fixture,
    ) -> anyhow::Result<(tokio::task::JoinHandle<Decision>, String)> {
        let service = Arc::clone(&fix.service);
        let config = Arc::clone(&fix.config);
        let mut rx = fix.bus.subscribe();
        let task = tokio::spawn(async move {
            service
                .authorize(AuthorizeParams {
                    config: &config,
                    agent: ox_tools::agent::default_agent(),
                    session_id: "ses_child",
                    directory: std::path::Path::new("/work"),
                    tool: "shell",
                    input: &json!({"command": "git status"}),
                    source: None,
                    unattended: None,
                    is_mcp: false,
                    cancellation: &CancellationToken::new(),
                })
                .await
        });
        let env = next_event(&mut rx, "permission.asked").await;
        let id = env
            .data
            .get("id")
            .and_then(Value::as_str)
            .expect("asked has an id")
            .to_owned();
        Ok((task, id))
    }

    async fn ask_from(
        fix: &Fixture,
        session_id: &'static str,
        tool: &'static str,
        input: Value,
        cancellation: CancellationToken,
    ) -> anyhow::Result<(tokio::task::JoinHandle<Decision>, String)> {
        let service = Arc::clone(&fix.service);
        let config = Arc::clone(&fix.config);
        let mut rx = fix.bus.subscribe();
        let task = tokio::spawn(async move {
            service
                .authorize(AuthorizeParams {
                    config: &config,
                    agent: ox_tools::agent::default_agent(),
                    session_id,
                    directory: std::path::Path::new("/work"),
                    tool,
                    input: &input,
                    source: None,
                    unattended: None,
                    is_mcp: false,
                    cancellation: &cancellation,
                })
                .await
        });
        let env = next_event(&mut rx, "permission.asked").await;
        let id = env
            .data
            .get("id")
            .and_then(Value::as_str)
            .expect("asked has an id")
            .to_owned();
        Ok((task, id))
    }

    #[tokio::test]
    async fn default_deny_denies_unmatched_calls_without_asking() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let mut config = Config::default();
        config.permissions.default = Permission::Deny;
        let config = Arc::new(config);
        let decision = fix
            .service
            .authorize(AuthorizeParams {
                config: &config,
                agent: ox_tools::agent::default_agent(),
                session_id: "ses_child",
                directory: std::path::Path::new("/work"),
                tool: "shell",
                input: &json!({"command": "git status"}),
                source: None,
                unattended: None,
                is_mcp: false,
                cancellation: &CancellationToken::new(),
            })
            .await;
        assert!(
            matches!(decision, Decision::Deny(_)),
            "default deny must refuse a call no rule matches, got {decision:?}"
        );
        assert!(
            fix.service.pending().is_empty(),
            "default deny must not create a pending ask"
        );
        Ok(())
    }

    #[tokio::test]
    async fn default_deny_still_honors_explicit_allow_rules() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let mut config = Config::default();
        config.permissions.default = Permission::Deny;
        config.permissions.rules = vec![Rule {
            action: "shell".to_owned(),
            resource: "git status".to_owned(),
            effect: Effect::Allow,
        }];
        let config = Arc::new(config);
        let decision = fix
            .service
            .authorize(AuthorizeParams {
                config: &config,
                agent: ox_tools::agent::default_agent(),
                session_id: "ses_child",
                directory: std::path::Path::new("/work"),
                tool: "shell",
                input: &json!({"command": "git status"}),
                source: None,
                unattended: None,
                is_mcp: false,
                cancellation: &CancellationToken::new(),
            })
            .await;
        assert_eq!(
            decision,
            Decision::Allow,
            "an explicit allow rule must override the default deny"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mcp_tool_asks_with_the_mcp_action_and_a_save_pattern() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let service = Arc::clone(&fix.service);
        let config = Arc::clone(&fix.config);
        let mut rx = fix.bus.subscribe();
        let task = tokio::spawn(async move {
            service
                .authorize(AuthorizeParams {
                    config: &config,
                    agent: ox_tools::agent::default_agent(),
                    session_id: "ses_child",
                    directory: std::path::Path::new("/work"),
                    tool: "github_create_issue",
                    input: &json!({}),
                    source: None,
                    unattended: None,
                    is_mcp: true,
                    cancellation: &CancellationToken::new(),
                })
                .await
        });
        let env = next_event(&mut rx, "permission.asked").await;
        let id = env
            .data
            .get("id")
            .and_then(Value::as_str)
            .expect("asked has an id")
            .to_owned();
        let asked = fix.service.pending_one(&id).expect("MCP ask is pending");
        assert_eq!(asked.action, "mcp");
        assert_eq!(asked.resources, ["github_create_issue"]);
        assert_eq!(asked.save.as_deref(), Some(["*".to_owned()].as_slice()));
        fix.service
            .reply("ses_child", &id, Reply::Once, None)
            .await?;
        assert_eq!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Allow
        );
        Ok(())
    }

    #[tokio::test]
    async fn default_config_leaves_the_agent_rules_in_charge() -> anyhow::Result<()> {
        let mut fix = fixture(1_000).await?;
        fix.config = Arc::new(Config::default());
        let plain = timeout(
            Duration::from_millis(200),
            fix.service.authorize(AuthorizeParams {
                config: &fix.config,
                agent: ox_tools::agent::default_agent(),
                session_id: "ses_child",
                directory: std::path::Path::new("/work"),
                tool: "read",
                input: &json!({"path": "note.txt"}),
                source: None,
                unattended: None,
                is_mcp: false,
                cancellation: &CancellationToken::new(),
            }),
        )
        .await
        .expect("an ordinary read must not wait for a permission reply");
        assert_eq!(
            plain,
            Decision::Allow,
            "the default `ask` is a rule fallback, not a tool floor"
        );
        assert!(
            fix.service.pending().is_empty(),
            "no pending request for an agent-allowed read"
        );

        let (task, id) = ask_from(
            &fix,
            "ses_child",
            "read",
            json!({"path": ".env"}),
            CancellationToken::new(),
        )
        .await?;
        fix.service
            .reply("ses_child", &id, Reply::Once, None)
            .await?;
        assert_eq!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Allow,
            "the agent's `.env` ask still governs"
        );
        Ok(())
    }

    #[tokio::test]
    async fn always_does_not_settle_another_sessions_pending() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let a_token = CancellationToken::new();
        let b_token = CancellationToken::new();
        let (a, a_id) = ask_from(
            &fix,
            "ses_child",
            "shell",
            json!({"command": "git status"}),
            a_token.clone(),
        )
        .await?;
        let (b, b_id) = ask_from(
            &fix,
            "ses_other",
            "shell",
            json!({"command": "git status"}),
            b_token.clone(),
        )
        .await?;
        assert_eq!(fix.service.pending().len(), 2);

        fix.service
            .reply("ses_child", &a_id, Reply::Always, None)
            .await
            .expect("always reply for A");
        assert_eq!(
            timeout(Duration::from_secs(2), a).await??,
            Decision::Allow,
            "A is allowed"
        );
        assert!(
            fix.service.pending_one(&b_id).is_some(),
            "a different project's pending must stay open"
        );
        assert!(fix.service.pending_one(&a_id).is_none());

        b_token.cancel();
        assert!(matches!(
            timeout(Duration::from_secs(2), b).await??,
            Decision::Deny(_)
        ));
        assert!(fix.service.pending().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn always_does_not_settle_a_sibling_session_in_the_same_project() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let token = CancellationToken::new();
        let (a, a_id) = ask_from(
            &fix,
            "ses_child",
            "shell",
            json!({"command": "git status"}),
            CancellationToken::new(),
        )
        .await?;
        let (sibling, sibling_id) = ask_from(
            &fix,
            "ses_sibling",
            "shell",
            json!({"command": "git status"}),
            token.clone(),
        )
        .await?;

        fix.service
            .reply("ses_child", &a_id, Reply::Always, None)
            .await
            .expect("always reply");
        assert_eq!(timeout(Duration::from_secs(2), a).await??, Decision::Allow);
        assert!(
            fix.service.pending_one(&sibling_id).is_some(),
            "the cascade is session-scoped even when the saved rule matches"
        );
        token.cancel();
        assert!(matches!(
            timeout(Duration::from_secs(2), sibling).await??,
            Decision::Deny(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn always_cascade_rechecks_every_resource_of_a_pending() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let token = CancellationToken::new();
        let (a, a_id) = ask_from(
            &fix,
            "ses_child",
            "patch",
            json!({"patchText": "*** Begin Patch\n*** Add File: a.rs\n+one\n*** End Patch"}),
            CancellationToken::new(),
        )
        .await?;
        let (b, b_id) = ask_from(
            &fix,
            "ses_child",
            "patch",
            json!({"patchText": "*** Begin Patch\n*** Add File: a.rs\n+one\n*** Add File: b.rs\n+two\n*** End Patch"}),
            token.clone(),
        )
        .await?;

        fix.service
            .reply("ses_child", &a_id, Reply::Always, None)
            .await
            .expect("always reply");
        assert_eq!(timeout(Duration::from_secs(2), a).await??, Decision::Allow);
        assert!(
            fix.service.pending_one(&b_id).is_some(),
            "one approved file does not settle a two-file request"
        );
        token.cancel();
        assert!(matches!(
            timeout(Duration::from_secs(2), b).await??,
            Decision::Deny(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn once_allows_and_pending_is_removed_once() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let (task, id) = start_ask(&fix).await?;
        fix.service
            .reply("ses_child", &id, Reply::Once, None)
            .await
            .expect("once reply");
        assert_eq!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Allow
        );
        assert!(fix.service.pending().is_empty(), "settled request is gone");
        assert_eq!(
            fix.service.reply("ses_child", &id, Reply::Once, None).await,
            Err(ReplyError::NotFound),
            "a second reply cannot settle the same request"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reply_for_another_session_is_not_found() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let (task, id) = start_ask(&fix).await?;
        assert_eq!(
            fix.service.reply("ses_root", &id, Reply::Once, None).await,
            Err(ReplyError::NotFound),
            "request ids are owned by their session"
        );
        fix.service
            .reply("ses_child", &id, Reply::Once, None)
            .await?;
        assert_eq!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Allow
        );
        Ok(())
    }

    #[tokio::test]
    async fn reject_carries_the_message_as_a_denial_reason() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let (task, id) = start_ask(&fix).await?;
        fix.service
            .reply(
                "ses_child",
                &id,
                Reply::Reject,
                Some("use git log".to_owned()),
            )
            .await
            .expect("reject reply");
        let decision = timeout(Duration::from_secs(2), task).await??;
        match decision {
            Decision::Deny(reason) => assert!(reason.contains("use git log"), "{reason}"),
            Decision::Allow => panic!("reject must not run the tool"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn always_without_save_is_refused() -> anyhow::Result<()> {
        let fix = fixture(1_000).await?;
        let mut rx = fix.bus.subscribe();
        let service = Arc::clone(&fix.service);
        let config = Arc::clone(&fix.config);
        let task = tokio::spawn(async move {
            service
                .authorize(AuthorizeParams {
                    config: &config,
                    agent: ox_tools::agent::default_agent(),
                    session_id: "ses_child",
                    directory: std::path::Path::new("/work"),
                    tool: "shell",
                    input: // A composite command cannot yield a safe save pattern.
                    &json!({"command": "git status; rm -rf build"}),
                    source: None,
                    unattended: None,
                    is_mcp: false,
                    cancellation: &CancellationToken::new(),
                })
                .await
        });
        let env = next_event(&mut rx, "permission.asked").await;
        let id = env
            .data
            .get("id")
            .and_then(Value::as_str)
            .expect("asked id")
            .to_owned();
        assert!(
            env.data.get("save").is_none(),
            "no Always for composite commands"
        );
        assert!(matches!(
            fix.service
                .reply("ses_child", &id, Reply::Always, None)
                .await,
            Err(ReplyError::Invalid(_))
        ));
        fix.service
            .reply("ses_child", &id, Reply::Reject, None)
            .await?;
        assert!(matches!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Deny(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn timeout_cancellation_and_shutdown_fail_closed() -> anyhow::Result<()> {
        let timed = fixture(10).await?;
        let decision = timed
            .service
            .authorize(AuthorizeParams {
                config: &timed.config,
                agent: ox_tools::agent::default_agent(),
                session_id: "ses_child",
                directory: std::path::Path::new("/work"),
                tool: "shell",
                input: &json!({"command": "git status"}),
                source: None,
                unattended: None,
                is_mcp: false,
                cancellation: &CancellationToken::new(),
            })
            .await;
        assert!(matches!(decision, Decision::Deny(_)), "timeout denies");
        assert!(timed.service.pending().is_empty(), "timeout clears pending");

        let cancelled = fixture(1_000).await?;
        let token = CancellationToken::new();
        let service = Arc::clone(&cancelled.service);
        let config = Arc::clone(&cancelled.config);
        let mut rx = cancelled.bus.subscribe();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            service
                .authorize(AuthorizeParams {
                    config: &config,
                    agent: ox_tools::agent::default_agent(),
                    session_id: "ses_child",
                    directory: std::path::Path::new("/work"),
                    tool: "shell",
                    input: &json!({"command": "git status"}),
                    source: None,
                    unattended: None,
                    is_mcp: false,
                    cancellation: &task_token,
                })
                .await
        });
        next_event(&mut rx, "permission.asked").await;
        token.cancel();
        assert!(matches!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Deny(_)
        ));
        assert!(cancelled.service.pending().is_empty());

        let stopped = fixture(1_000).await?;
        stopped.shutdown.cancel();
        assert!(matches!(
            stopped
                .service
                .authorize(AuthorizeParams {
                    config: &stopped.config,
                    agent: ox_tools::agent::default_agent(),
                    session_id: "ses_child",
                    directory: std::path::Path::new("/work"),
                    tool: "shell",
                    input: &json!({"command": "git status"}),
                    source: None,
                    unattended: None,
                    is_mcp: false,
                    cancellation: &CancellationToken::new(),
                })
                .await,
            Decision::Deny(_)
        ));
        Ok(())
    }
}

#[cfg(test)]
mod permission_events_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ox_core::config::{Config, Permission};
    use ox_core::event_bus::EventBus;
    use ox_db::Db;
    use ox_schema::event::EventEnvelope;
    use ox_schema::permission::{Effect, Reply, Rule};
    use serde_json::{Value, json};
    use tokio::sync::broadcast;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{AuthorizeParams, PermissionService};

    async fn next(rx: &mut broadcast::Receiver<EventEnvelope>, kind: &str) -> EventEnvelope {
        timeout(Duration::from_secs(2), async {
            loop {
                let env = rx.recv().await.expect("bus is open");
                if env.kind == kind {
                    return env;
                }
            }
        })
        .await
        .expect("event within bound")
    }

    #[tokio::test]
    async fn asked_carries_location_and_child_parent_and_replied_is_broadcast() -> anyhow::Result<()>
    {
        let db = Db::open_in_memory().await?;
        let tx = db.begin().await?;
        for (id, parent) in [("ses_root", None), ("ses_child", Some("ses_root"))] {
            tx.execute(
                "INSERT INTO session_v2
                 (id, project_id, slug, directory, version, time_created, time_updated, parent_id)
                 VALUES (?1, 'prj_1', 's', '/work', '1', 1, 1, ?2)",
                (id, parent),
            )
            .await?;
        }
        tx.commit().await?;
        let bus = EventBus::new(db.clone());
        let mut config = Config::default();
        config.permissions.default = Permission::Allow;
        config.permissions.rules = vec![Rule {
            action: "*".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Ask,
        }];
        let service = Arc::new(PermissionService::new(
            db,
            bus.clone(),
            CancellationToken::new(),
        ));
        let mut rx = bus.subscribe();
        let task = {
            let service = Arc::clone(&service);
            let config = Arc::new(config);
            tokio::spawn(async move {
                service
                    .authorize(AuthorizeParams {
                        config: &config,
                        agent: ox_tools::agent::default_agent(),
                        session_id: "ses_child",
                        directory: std::path::Path::new("/work"),
                        tool: "shell",
                        input: &json!({"command": "git status"}),
                        source: None,
                        unattended: None,
                        is_mcp: false,
                        cancellation: &CancellationToken::new(),
                    })
                    .await
            })
        };
        let asked = next(&mut rx, "permission.asked").await;
        let location = asked.location.expect("asked carries location");
        assert_eq!(location.directory, "/work");
        assert_eq!(
            asked.data.get("sessionID").and_then(Value::as_str),
            Some("ses_child")
        );
        assert_eq!(
            asked
                .data
                .pointer("/metadata/parentID")
                .and_then(Value::as_str),
            Some("ses_root")
        );
        let id = asked
            .data
            .get("id")
            .and_then(Value::as_str)
            .expect("id")
            .to_owned();
        service.reply("ses_child", &id, Reply::Reject, None).await?;
        let replied = next(&mut rx, "permission.replied").await;
        assert_eq!(
            replied.data.get("requestID").and_then(Value::as_str),
            Some(id.as_str())
        );
        assert_eq!(
            replied.data.get("reply").and_then(Value::as_str),
            Some("reject")
        );
        assert_eq!(replied.location.expect("location").directory, "/work");
        let _ = timeout(Duration::from_secs(2), task).await??;
        Ok(())
    }
}

#[cfg(test)]
mod permission_saved_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ox_core::config::{Config, Permission};
    use ox_core::event_bus::EventBus;
    use ox_db::Db;
    use ox_schema::event::EventEnvelope;
    use ox_schema::permission::{Effect, Reply, Rule};
    use serde_json::{Value, json};
    use tokio::sync::broadcast;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{AuthorizeParams, Decision, PermissionService};

    async fn next(rx: &mut broadcast::Receiver<EventEnvelope>, kind: &str) -> EventEnvelope {
        timeout(Duration::from_secs(2), async {
            loop {
                let env = rx.recv().await.expect("bus is open");
                if env.kind == kind {
                    return env;
                }
            }
        })
        .await
        .expect("event within bound")
    }

    #[tokio::test]
    async fn always_writes_saved_cascades_pending_and_delete_revokes_it() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let tx = db.begin().await?;
        tx.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES ('ses_1', 'prj_1', 's', '/work', '1', 1, 1)",
            (),
        )
        .await?;
        tx.commit().await?;
        let bus = EventBus::new(db.clone());
        let mut config = Config::default();
        config.permissions.default = Permission::Allow;
        config.permissions.rules = vec![Rule {
            action: "*".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Ask,
        }];
        let config = Arc::new(config);
        let service = Arc::new(PermissionService::new(
            db.clone(),
            bus.clone(),
            CancellationToken::new(),
        ));
        let mut rx = bus.subscribe();

        let mut tasks = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let service = Arc::clone(&service);
            let config = Arc::clone(&config);
            tasks.push(tokio::spawn(async move {
                service
                    .authorize(AuthorizeParams {
                        config: &config,
                        agent: ox_tools::agent::default_agent(),
                        session_id: "ses_1",
                        directory: std::path::Path::new("/work"),
                        tool: "shell",
                        input: &json!({"command": "git status"}),
                        source: None,
                        unattended: None,
                        is_mcp: false,
                        cancellation: &CancellationToken::new(),
                    })
                    .await
            }));
            let asked = next(&mut rx, "permission.asked").await;
            ids.push(
                asked
                    .data
                    .get("id")
                    .and_then(Value::as_str)
                    .expect("asked id")
                    .to_owned(),
            );
        }

        service
            .reply("ses_1", &ids[0], Reply::Always, None)
            .await
            .expect("always reply");
        for task in tasks {
            assert_eq!(
                timeout(Duration::from_secs(2), task).await??,
                Decision::Allow
            );
        }
        let saved = service.saved().list("prj_1").await?;
        assert_eq!(saved.len(), 1, "one resource was approved");
        assert_eq!(saved[0].action, "shell");
        assert_eq!(saved[0].resource, "git status");
        assert!(service.pending().is_empty(), "both requests were settled");

        service.saved().delete("prj_1", &saved[0].id).await?;
        let service_for_task = Arc::clone(&service);
        let config_for_task = Arc::clone(&config);
        let task = tokio::spawn(async move {
            service_for_task
                .authorize(AuthorizeParams {
                    config: &config_for_task,
                    agent: ox_tools::agent::default_agent(),
                    session_id: "ses_1",
                    directory: std::path::Path::new("/work"),
                    tool: "shell",
                    input: &json!({"command": "git status"}),
                    source: None,
                    unattended: None,
                    is_mcp: false,
                    cancellation: &CancellationToken::new(),
                })
                .await
        });
        let asked = next(&mut rx, "permission.asked").await;
        let id = asked
            .data
            .get("id")
            .and_then(Value::as_str)
            .expect("id")
            .to_owned();
        service.reply("ses_1", &id, Reply::Reject, None).await?;
        assert!(matches!(
            timeout(Duration::from_secs(2), task).await??,
            Decision::Deny(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn always_with_wildcard_save_cascades_other_read_pending() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let tx = db.begin().await?;
        tx.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES ('ses_1', 'prj_1', 's', '/work', '1', 1, 1)",
            (),
        )
        .await?;
        tx.commit().await?;
        let bus = EventBus::new(db.clone());
        let mut config = Config::default();
        config.permissions.default = Permission::Allow;
        config.permissions.rules = vec![Rule {
            action: "*".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Ask,
        }];
        let config = Arc::new(config);
        let service = Arc::new(PermissionService::new(
            db.clone(),
            bus.clone(),
            CancellationToken::new(),
        ));
        let mut rx = bus.subscribe();

        let mut tasks = Vec::new();
        let mut ids = Vec::new();
        for path in ["first.txt", "second.txt"] {
            let service = Arc::clone(&service);
            let config = Arc::clone(&config);
            tasks.push(tokio::spawn(async move {
                service
                    .authorize(AuthorizeParams {
                        config: &config,
                        agent: ox_tools::agent::default_agent(),
                        session_id: "ses_1",
                        directory: std::path::Path::new("/work"),
                        tool: "read",
                        input: &json!({"path": path}),
                        source: None,
                        unattended: None,
                        is_mcp: false,
                        cancellation: &CancellationToken::new(),
                    })
                    .await
            }));
            let asked = next(&mut rx, "permission.asked").await;
            ids.push(
                asked
                    .data
                    .get("id")
                    .and_then(Value::as_str)
                    .expect("asked id")
                    .to_owned(),
            );
        }

        service
            .reply("ses_1", &ids[0], Reply::Always, None)
            .await
            .expect("always reply");
        for task in tasks {
            assert_eq!(
                timeout(Duration::from_secs(2), task).await??,
                Decision::Allow,
                "both wildcard-save reads must settle, not just the replied one"
            );
        }
        assert!(service.pending().is_empty(), "both requests were settled");
        Ok(())
    }
}

#[cfg(test)]
mod permission_unattended_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ox_core::config::{Config, Permission};
    use ox_core::event_bus::EventBus;
    use ox_db::Db;
    use ox_schema::event::EventEnvelope;
    use ox_schema::inbox::{UNATTENDED_KEY, UNATTENDED_RULES_KEY};
    use ox_schema::permission::{Effect, Rule};
    use serde_json::json;
    use tokio::sync::broadcast;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{AuthorizeParams, Decision, PermissionService};

    #[tokio::test]
    async fn subagent_marker_is_taken_once_and_carried_on_the_child_payload() -> anyhow::Result<()>
    {
        let db = Db::open_in_memory().await?;
        let handle = ox_tools::subagent::SubagentHandle::new(
            db.clone(),
            EventBus::new(db),
            2,
            Arc::new(ox_tools::jobs::Jobs::default()),
        );
        let allow = [Rule {
            action: "shell".to_owned(),
            resource: "git status".to_owned(),
            effect: Effect::Allow,
        }];
        handle.mark_unattended("call_1", &allow);
        let inherited = handle.take_unattended("call_1").expect("marked call");
        assert_eq!(inherited, allow);
        assert!(handle.take_unattended("call_1").is_none(), "consumed once");

        let mut metadata = ox_schema::support::Record::new();
        metadata.insert(UNATTENDED_KEY.to_owned(), json!(true));
        metadata.insert(UNATTENDED_RULES_KEY.to_owned(), json!(inherited));
        let payload = ox_schema::inbox::Item::User {
            payload: ox_schema::inbox::UserPayload {
                prompt: ox_schema::support::Prompt {
                    text: "do work".to_owned(),
                    files: None,
                    agents: None,
                    skills: None,
                },
                metadata: Some(metadata),
            },
            delivery: ox_schema::inbox::Delivery::Steer,
        };
        let roundtrip: ox_schema::inbox::Item = serde_json::from_value(json!({
            "type": "user",
            "payload": payload.payload(),
            "delivery": "steer",
        }))?;
        assert_eq!(roundtrip, payload, "child inbox item keeps the marker");
        Ok(())
    }

    #[tokio::test]
    async fn unattended_rules_settle_without_asking_or_waiting() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let tx = db.begin().await?;
        tx.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated)
             VALUES ('ses_1', 'prj_1', 's', '/work', '1', 1, 1)",
            (),
        )
        .await?;
        tx.commit().await?;
        let bus = EventBus::new(db.clone());
        let mut config = Config::default();
        config.permissions.default = Permission::Allow;
        config.permissions.rules = vec![Rule {
            action: "*".to_owned(),
            resource: "*".to_owned(),
            effect: Effect::Ask,
        }];
        let config = Arc::new(config);
        let service = PermissionService::new(db, bus.clone(), CancellationToken::new());
        let mut rx: broadcast::Receiver<EventEnvelope> = bus.subscribe();
        let allow = [Rule {
            action: "shell".to_owned(),
            resource: "git status".to_owned(),
            effect: Effect::Allow,
        }];

        let decision = timeout(
            Duration::from_millis(200),
            service.authorize(AuthorizeParams {
                config: &config,
                agent: ox_tools::agent::default_agent(),
                session_id: "ses_1",
                directory: std::path::Path::new("/work"),
                tool: "shell",
                input: &json!({"command": "git status"}),
                source: None,
                unattended: Some(&allow),
                is_mcp: false,
                cancellation: &CancellationToken::new(),
            }),
        )
        .await
        .expect("unattended must not wait for a reply");
        assert_eq!(decision, Decision::Allow);
        assert!(
            rx.try_recv().is_err(),
            "unattended emits no permission event"
        );
        assert!(
            service.pending().is_empty(),
            "unattended creates no pending"
        );

        let denied = timeout(
            Duration::from_millis(200),
            service.authorize(AuthorizeParams {
                config: &config,
                agent: ox_tools::agent::default_agent(),
                session_id: "ses_1",
                directory: std::path::Path::new("/work"),
                tool: "shell",
                input: &json!({"command": "git push"}),
                source: None,
                unattended: Some(&allow),
                is_mcp: false,
                cancellation: &CancellationToken::new(),
            }),
        )
        .await
        .expect("denial must be immediate");
        assert!(matches!(denied, Decision::Deny(_)));
        assert!(rx.try_recv().is_err(), "denial emits no permission event");
        Ok(())
    }
}
