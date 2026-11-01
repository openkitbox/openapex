//! Pending forms: the interaction channel a `question` tool call waits on.
//!
//! A form is created by a tool (today only `question`), answered or cancelled
//! by a client, and awaited by the tool. Like the shell session table, this is
//! a process-wide registry shared by `Arc` between the tool and the HTTP
//! handlers, so neither side owns the other and neither touches the database.
//!
//! The shape follows upstream `packages/schema/src/form.ts`: a form has an id,
//! an owning `sessionID`, a title, metadata and a non-empty field list; a reply
//! is a `key → value` map; a cancellation may carry a message explaining why
//! nobody answered.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ox_core::event_bus::EventBus;

/// A reply value (upstream `Form.Value`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FormValue {
    String(String),
    Number(f64),
    Boolean(bool),
    Multiple(Vec<String>),
}

impl FormValue {
    /// The answer as display text, for the model-facing result.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            FormValue::String(text) => text.clone(),
            FormValue::Number(number) => number.to_string(),
            FormValue::Boolean(value) => value.to_string(),
            FormValue::Multiple(values) => values.join(", "),
        }
    }
}

/// One answer map (upstream `Form.Answer`).
pub type Answer = HashMap<String, FormValue>;

/// One selectable option (upstream `Form.Option`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Option_ {
    pub value: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A form field (upstream `Form.Field`; only the shapes `question` produces
/// are modelled, plus the generic ones a future producer may send).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Field {
    String {
        key: String,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        required: Option<bool>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        options: Option<Vec<Option_>>,
        /// Allow free-form input beside the options.
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        custom: Option<bool>,
    },
    Multiselect {
        key: String,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        required: Option<bool>,
        options: Vec<Option_>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        custom: Option<bool>,
    },
    Boolean {
        key: String,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        default: Option<bool>,
    },
}

/// How a form ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A client supplied answers.
    Answered(Answer),
    /// A client dismissed the form; `message` explains why when it can.
    Cancelled(Option<String>),
}

/// The `Form.Info` a client lists.
#[derive(Debug, Clone, Serialize)]
pub struct FormInfo {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub fields: Vec<Field>,
}

/// The `Form.Detail` a client reads for one form.
#[derive(Debug, Clone, Serialize)]
pub struct FormDetail {
    #[serde(flatten)]
    pub info: FormInfo,
    pub state: FormState,
}

/// Upstream `Form.State` (tagged union on `status`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum FormState {
    Pending,
    Answered {
        answer: Answer,
    },
    Cancelled {
        #[serde(skip_serializing_if = "std::option::Option::is_none")]
        message: Option<String>,
    },
}

pub struct Pending {
    info: FormInfo,
    /// The owning project directory, stamped as `location` on `form.*` frames.
    directory: String,
    outcome: Mutex<Option<Outcome>>,
    /// Fires once the outcome lands, so the waiting tool wakes immediately.
    ready: tokio::sync::Notify,
}

impl Pending {
    pub fn state(&self) -> FormState {
        match self.outcome.lock().ok().and_then(|guard| guard.clone()) {
            None => FormState::Pending,
            Some(Outcome::Answered(answer)) => FormState::Answered { answer },
            Some(Outcome::Cancelled(message)) => FormState::Cancelled { message },
        }
    }
}

/// Why a reply or a cancel could not settle a form — the model for the HTTP
/// error tags upstream declares (`FormNotFoundError` / `FormAlreadySettledError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleError {
    /// No form with that id belongs to the session.
    NotFound,
    /// The form already has an outcome.
    AlreadySettled,
}

/// Why [`Forms::create_with`] refused a caller-supplied form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// A form with that id already exists (upstream `FormAlreadyExistsError`).
    AlreadyExists,
}

/// The process-wide form registry.
#[derive(Default)]
pub struct Forms {
    pending: Mutex<HashMap<String, Arc<Pending>>>,
    /// The event bus the `form.*` transitions are broadcast on. The TUI
    /// renders its question dialog purely from the SSE `form.created` /
    /// `form.replied` / `form.cancelled` frames (upstream `Form.Event.*`),
    /// so a registry that never publishes strands the dialog — the exact
    /// "Asked 1 question but no picker, then it hung" failure. Set by the
    /// composition root; a registry built directly (unit tests) can run
    /// bus-less.
    bus: Mutex<Option<EventBus>>,
}

impl Forms {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that broadcasts every transition on `bus`.
    #[must_use]
    pub fn with_bus(bus: EventBus) -> Self {
        let forms = Self::new();
        forms.set_bus(bus);
        forms
    }

    /// Point the registry at `bus` (the composition root wires this once).
    pub fn set_bus(&self, bus: EventBus) {
        if let Ok(mut slot) = self.bus.lock() {
            *slot = Some(bus);
        }
    }

    /// Broadcast one transition in the upstream `Form.Event.*` wire shape,
    /// stamped with the owning session's directory (v2.1.x TUI clients drop
    /// `form.*` frames without a `location`).
    fn publish(&self, kind: &str, data: Value, directory: &str) {
        if let Some(bus) = self.bus.lock().ok().and_then(|slot| slot.clone()) {
            bus.broadcast_ephemeral(kind, data, Some(directory));
        }
    }

    /// Register a form and return the handle the tool awaits. `directory` is
    /// the owning project directory, stamped onto the `form.*` events.
    pub fn create(
        &self,
        session_id: &str,
        directory: &str,
        title: &str,
        fields: Vec<Field>,
    ) -> Arc<Pending> {
        self.create_with(session_id, None, directory, title, fields, None)
            .expect("generated form ids never collide")
    }

    /// Register a form with a caller-supplied id and metadata — the
    /// `session.form.create` HTTP surface. Upstream rejects a duplicate id
    /// with `FormAlreadyExistsError`.
    pub fn create_with(
        &self,
        session_id: &str,
        id: Option<String>,
        directory: &str,
        title: &str,
        fields: Vec<Field>,
        metadata: Option<Value>,
    ) -> Result<Arc<Pending>, CreateError> {
        let pending = Arc::new(Pending {
            info: FormInfo {
                id: id.unwrap_or_else(|| ox_schema::id::id("frm")),
                session_id: session_id.to_owned(),
                title: title.to_owned(),
                metadata,
                fields,
            },
            directory: directory.to_owned(),
            outcome: Mutex::new(None),
            ready: tokio::sync::Notify::new(),
        });
        let mut table = self
            .pending
            .lock()
            .map_err(|_| CreateError::AlreadyExists)?;
        if table.contains_key(&pending.info.id) {
            return Err(CreateError::AlreadyExists);
        }
        table.insert(pending.info.id.clone(), Arc::clone(&pending));
        drop(table);
        // Publish only once the form is listable: a client reacting to the
        // event must find it through the HTTP surface.
        self.publish("form.created", json!({ "form": pending.info() }), directory);
        Ok(pending)
    }

    /// Every form owned by `session_id` that has not settled yet. Upstream
    /// filters its in-memory cache by session and pending state, so an
    /// unknown session (including the TUI's `"global"` bootstrap) answers an
    /// empty list rather than an error, and a timed-out form stops being
    /// offered to clients.
    pub fn list(&self, session_id: &str) -> Vec<FormInfo> {
        let table = match self.pending.lock() {
            Ok(table) => table,
            Err(_) => return Vec::new(),
        };
        let mut infos: Vec<FormInfo> = table
            .values()
            .filter(|pending| pending.info.session_id == session_id)
            .filter(|pending| matches!(pending.state(), FormState::Pending))
            .map(|pending| pending.info.clone())
            .collect();
        infos.sort_by(|left, right| left.id.cmp(&right.id));
        infos
    }

    /// Every pending form in the process, for `GET /api/form`.
    pub fn list_all(&self) -> Vec<FormInfo> {
        let table = match self.pending.lock() {
            Ok(table) => table,
            Err(_) => return Vec::new(),
        };
        let mut infos: Vec<FormInfo> = table
            .values()
            .filter(|pending| matches!(pending.state(), FormState::Pending))
            .map(|pending| pending.info.clone())
            .collect();
        infos.sort_by(|left, right| left.id.cmp(&right.id));
        infos
    }

    pub fn get(&self, session_id: &str, form_id: &str) -> Option<Arc<Pending>> {
        self.pending
            .lock()
            .ok()
            .and_then(|table| table.get(form_id).cloned())
            .filter(|pending| pending.info.session_id == session_id)
    }

    /// Answer a form. Fails with [`SettleError::NotFound`] when no form with
    /// that id belongs to the session and [`SettleError::AlreadySettled`] when
    /// it already has an outcome.
    pub fn reply(
        &self,
        session_id: &str,
        form_id: &str,
        answer: Answer,
    ) -> Result<(), SettleError> {
        let Some(pending) = self.get(session_id, form_id) else {
            return Err(SettleError::NotFound);
        };
        if !pending.settle(Outcome::Answered(answer.clone())) {
            return Err(SettleError::AlreadySettled);
        }
        // Forget the settled form: the registry is process-wide and owned
        // sessions are long-lived, so a kept entry would never be dropped.
        // The awaiting tool already holds the outcome via the handle.
        self.remove(session_id, form_id);
        self.publish(
            "form.replied",
            json!({ "id": form_id, "sessionID": session_id, "answer": answer }),
            &pending.directory,
        );
        Ok(())
    }

    /// Cancel a form. Fails like [`Forms::reply`].
    pub fn cancel(
        &self,
        session_id: &str,
        form_id: &str,
        message: Option<String>,
    ) -> Result<(), SettleError> {
        let Some(pending) = self.get(session_id, form_id) else {
            return Err(SettleError::NotFound);
        };
        if !pending.settle(Outcome::Cancelled(message)) {
            return Err(SettleError::AlreadySettled);
        }
        self.remove(session_id, form_id);
        self.publish(
            "form.cancelled",
            json!({ "id": form_id, "sessionID": session_id }),
            &pending.directory,
        );
        Ok(())
    }

    /// Forget a settled form; a pending one stays so it can still be answered.
    pub fn remove(&self, session_id: &str, form_id: &str) -> bool {
        let Ok(mut table) = self.pending.lock() else {
            return false;
        };
        match table.get(form_id) {
            Some(pending) if pending.info.session_id == session_id => {
                table.remove(form_id).is_some()
            }
            _ => false,
        }
    }
}

impl Pending {
    pub fn info(&self) -> FormInfo {
        self.info.clone()
    }

    fn settle(&self, outcome: Outcome) -> bool {
        let Ok(mut guard) = self.outcome.lock() else {
            return false;
        };
        if guard.is_some() {
            return false;
        }
        *guard = Some(outcome);
        drop(guard);
        self.ready.notify_waiters();
        true
    }

    /// Wait for the form to settle. `None` on timeout; the form stays pending
    /// so a later reply can still reach it.
    pub async fn wait(&self, timeout: std::time::Duration) -> Option<Outcome> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(outcome) = self.outcome.lock().ok().and_then(|guard| guard.clone()) {
                return Some(outcome);
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            if tokio::time::timeout(left, self.ready.notified())
                .await
                .is_err()
            {
                return self.outcome.lock().ok().and_then(|guard| guard.clone());
            }
        }
    }
}

/// The process-wide registry, shared by the tool and the handlers.
pub fn shared() -> Arc<Forms> {
    static TABLE: std::sync::OnceLock<Arc<Forms>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| Arc::new(Forms::new())).clone()
}

/// Wire the process-wide registry to the event bus. The composition root
/// calls this once, so every form transition reaches the SSE feed the TUI
/// builds its question dialog from. A registry first materialized by
/// [`shared`] (tests without a bus) simply picks the bus up here.
pub fn init(bus: EventBus) {
    shared().set_bus(bus);
}
