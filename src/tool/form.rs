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

use crate::event_bus::EventBus;

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
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that broadcasts every transition on `bus`.
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

    /// Broadcast one transition in the upstream `Form.Event.*` wire shape.
    fn publish(&self, kind: &str, data: Value) {
        if let Some(bus) = self.bus.lock().ok().and_then(|slot| slot.clone()) {
            bus.broadcast_ephemeral(kind, data);
        }
    }

    /// Register a form and return the handle the tool awaits.
    pub fn create(&self, session_id: &str, title: &str, fields: Vec<Field>) -> Arc<Pending> {
        let pending = Arc::new(Pending {
            info: FormInfo {
                id: crate::id::id("frm"),
                session_id: session_id.to_owned(),
                title: title.to_owned(),
                fields,
            },
            outcome: Mutex::new(None),
            ready: tokio::sync::Notify::new(),
        });
        if let Ok(mut table) = self.pending.lock() {
            table.insert(pending.info.id.clone(), Arc::clone(&pending));
        }
        // Publish only once the form is listable: a client reacting to the
        // event must find it through the HTTP surface.
        self.publish("form.created", json!({ "form": pending.info() }));
        pending
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
    pub fn reply(&self, session_id: &str, form_id: &str, answer: Answer) -> Result<(), SettleError> {
        let Some(pending) = self.get(session_id, form_id) else {
            return Err(SettleError::NotFound);
        };
        if !pending.settle(Outcome::Answered(answer.clone())) {
            return Err(SettleError::AlreadySettled);
        }
        self.publish(
            "form.replied",
            json!({ "id": form_id, "sessionID": session_id, "answer": answer }),
        );
        Ok(())
    }

    /// Cancel a form. Fails like [`Forms::reply`].
    pub fn cancel(&self, session_id: &str, form_id: &str, message: Option<String>) -> Result<(), SettleError> {
        let Some(pending) = self.get(session_id, form_id) else {
            return Err(SettleError::NotFound);
        };
        if !pending.settle(Outcome::Cancelled(message)) {
            return Err(SettleError::AlreadySettled);
        }
        self.publish(
            "form.cancelled",
            json!({ "id": form_id, "sessionID": session_id }),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn question_fields() -> Vec<Field> {
        vec![Field::String {
            key: "q0".to_owned(),
            title: Some("Scope".to_owned()),
            description: Some("Which part?".to_owned()),
            required: Some(true),
            options: Some(vec![Option_ {
                value: "api".to_owned(),
                label: "API".to_owned(),
                description: None,
            }]),
            custom: Some(true),
        }]
    }

    #[tokio::test]
    async fn a_reply_settles_the_waiter() {
        let forms = Forms::new();
        let pending = forms.create("ses_1", "Questions", question_fields());
        let id = pending.info().id.clone();
        let waiter = {
            let pending = Arc::clone(&pending);
            tokio::spawn(async move { pending.wait(std::time::Duration::from_secs(5)).await })
        };
        let mut answer = Answer::new();
        answer.insert("q0".to_owned(), FormValue::String("api".to_owned()));
        assert!(forms.reply("ses_1", &id, answer.clone()).is_ok());
        let outcome = waiter.await.expect("join").expect("outcome");
        match outcome {
            Outcome::Answered(got) => assert_eq!(got, answer),
            Outcome::Cancelled(_) => panic!("unexpected cancellation"),
        }
        // A second reply loses: the form already settled.
        assert_eq!(
            forms.reply("ses_1", &id, Answer::new()),
            Err(SettleError::AlreadySettled)
        );
    }

    #[tokio::test]
    async fn a_cancel_carries_its_message() {
        let forms = Forms::new();
        let pending = forms.create("ses_1", "Questions", question_fields());
        assert!(forms
            .cancel(
                "ses_1",
                &pending.info().id,
                Some("not interactive".to_owned())
            )
            .is_ok());
        match pending.wait(std::time::Duration::from_millis(10)).await {
            Some(Outcome::Cancelled(Some(message))) => assert_eq!(message, "not interactive"),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_transition_broadcasts_the_upstream_form_events() {
        let bus = crate::db::Db::open_in_memory()
            .await
            .map(EventBus::new)
            .expect("db");
        let mut events = bus.subscribe();
        let forms = Forms::with_bus(bus);

        // created → the Form.Info the TUI renders its dialog from.
        let pending = forms.create("ses_evt", "Questions", question_fields());
        let created = events.try_recv().expect("created event");
        assert_eq!(created.kind, "form.created");
        assert!(created.durable.is_none(), "ephemeral: never persisted");
        assert_eq!(created.data["form"]["id"], pending.info().id);
        assert_eq!(created.data["form"]["sessionID"], "ses_evt");
        assert_eq!(created.data["form"]["title"], "Questions");

        // replied → the answer keyed by form id.
        let mut answer = Answer::new();
        answer.insert("q0".to_owned(), FormValue::String("api".to_owned()));
        assert!(forms.reply("ses_evt", &pending.info().id, answer).is_ok());
        let replied = events.try_recv().expect("replied event");
        assert_eq!(replied.kind, "form.replied");
        assert_eq!(replied.data["id"], pending.info().id);
        assert_eq!(replied.data["sessionID"], "ses_evt");
        assert_eq!(replied.data["answer"]["q0"], "api");

        // cancelled → identifying payload only (upstream carries no message).
        let pending = forms.create("ses_evt", "Questions", question_fields());
        let recreated = events.try_recv().expect("created event");
        assert_eq!(recreated.kind, "form.created");
        assert!(forms.cancel("ses_evt", &pending.info().id, None).is_ok());
        let cancelled = events.try_recv().expect("cancelled event");
        assert_eq!(cancelled.kind, "form.cancelled");
        assert_eq!(cancelled.data["id"], pending.info().id);
        assert_eq!(cancelled.data["sessionID"], "ses_evt");
    }

    #[test]
    fn only_pending_forms_are_listed() {
        let forms = Forms::new();
        let settled = forms.create("ses_list", "Questions", question_fields());
        let live = forms.create("ses_list", "Questions", question_fields());
        assert!(forms.cancel("ses_list", &settled.info().id, None).is_ok());
        let ids: Vec<String> = forms
            .list("ses_list")
            .into_iter()
            .map(|info| info.id)
            .collect();
        assert_eq!(ids, vec![live.info().id]);
        assert!(forms.list_all().iter().all(|info| info.id == live.info().id));
    }

    #[tokio::test]
    async fn a_form_is_only_visible_to_its_session() {
        let forms = Forms::new();
        let a = forms.create("ses_a", "Questions", question_fields());
        let _b = forms.create("ses_b", "Questions", question_fields());
        assert_eq!(forms.list("ses_a").len(), 1);
        assert_eq!(forms.list("ses_a")[0].id, a.info().id);
        assert!(forms.list("global").is_empty());
        assert!(forms.get("ses_b", &a.info().id).is_none());
    }

    #[tokio::test]
    async fn an_unanswered_wait_times_out_and_stays_pending() {
        let forms = Forms::new();
        let pending = forms.create("ses_1", "Questions", question_fields());
        assert!(
            pending
                .wait(std::time::Duration::from_millis(10))
                .await
                .is_none()
        );
        // Still answerable after the caller gave up.
        assert!(forms.reply("ses_1", &pending.info().id, Answer::new()).is_ok());
    }

    #[test]
    fn detail_state_tracks_the_outcome() {
        let forms = Forms::new();
        let pending = forms.create("ses_1", "Questions", question_fields());
        assert!(matches!(pending.state(), FormState::Pending));
        assert!(forms.cancel("ses_1", &pending.info().id, None).is_ok());
        assert!(matches!(pending.state(), FormState::Cancelled { .. }));
    }
}
