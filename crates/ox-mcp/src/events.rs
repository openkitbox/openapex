//! MCP Events wire layer (SEP-3415 beta extension).
//!
//! This module owns the `events/*` request shapes, notification parsing,
//! heartbeat watchdog, cursor acknowledgement and subscription state. It knows
//! nothing about automations, sessions or the event bus: the caller consumes
//! [`EventMessage`] values and decides what to do with them.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::transport::{CallError, RequestMetadata, Rpc};

/// The beta MCP Events extension only exists on protocol 2026-07-28.
pub(crate) const EVENTS_EXTENSION: &str = "io.modelcontextprotocol/events";
pub(crate) const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";

const MISSED_HEARTBEATS: u64 = 3;
const UNGRANTED_SILENCE: Duration = Duration::from_secs(30);
const MAX_EVENT_PAGES: usize = 100;
const MESSAGE_QUEUE: usize = 256;
const STATE_QUEUE: usize = 16;
const MAX_SEEN_EVENTS: usize = 1024;
const RECONNECT_DELAY: Duration = Duration::from_millis(50);

/// JSON-RPC codes that mean an `events/stream` can never open as sent.
const STREAM_REJECTED: [i64; 5] = [-32601, -32602, -32023, -32024, -32026];

/// One entry of `events/list`. Delivery is kept as the server's strings so a
/// future delivery mode passes through without a protocol change here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventType {
    pub name: String,
    pub description: Option<String>,
    pub delivery: Vec<String>,
    pub input_schema: Value,
    pub payload_schema: Value,
}

/// Event payloads come from an external MCP server. The marker travels with
/// the data so no downstream consumer can mistake it for trusted local input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UntrustedData {
    pub untrusted: bool,
    pub data: Value,
}

impl UntrustedData {
    fn from_wire(data: Value) -> Self {
        Self {
            untrusted: true,
            data,
        }
    }
}

/// One message delivered on an event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMessage {
    Active {
        cursor: Option<String>,
        truncated: bool,
        heartbeat_ms: u64,
    },
    Event {
        event_id: String,
        name: String,
        timestamp: String,
        data: UntrustedData,
        cursor: Option<String>,
    },
    Heartbeat {
        cursor: Option<String>,
    },
    Error {
        code: i64,
        message: String,
    },
    Terminated {
        code: i64,
        message: String,
    },
}

/// Observable lifecycle of one push subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubscriptionState {
    Connecting {
        cursor: Option<String>,
    },
    Active {
        cursor: Option<String>,
        heartbeat_ms: u64,
    },
    Reconnecting {
        cursor: Option<String>,
        attempts: u32,
    },
    Terminated {
        code: i64,
        message: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct PollRequest<'a> {
    pub name: &'a str,
    pub cursor: Option<&'a str>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct SubscribeRequest<'a> {
    pub name: &'a str,
    pub webhook_url: &'a str,
    pub cursor: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct StreamRequest<'a> {
    pub name: &'a str,
    pub cursor: Option<&'a str>,
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollPage {
    pub events: Vec<EventMessage>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub id: String,
}

#[derive(Debug)]
pub enum EventsError {
    /// The server never advertised the extension: no wire call is allowed.
    ExtensionUnsupported,
    Rpc {
        code: i64,
        message: String,
    },
    Transport(String),
    Closed,
    Timeout,
    Cancelled,
}

impl fmt::Display for EventsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExtensionUnsupported => {
                write!(formatter, "MCP events extension is not supported")
            }
            Self::Rpc { code, message } => write!(formatter, "{message} (code {code})"),
            Self::Transport(message) => write!(formatter, "{message}"),
            Self::Closed => write!(formatter, "MCP server is not available"),
            Self::Timeout => write!(formatter, "MCP events request timed out"),
            Self::Cancelled => write!(formatter, "MCP events request cancelled"),
        }
    }
}

impl std::error::Error for EventsError {}

impl From<CallError> for EventsError {
    fn from(error: CallError) -> Self {
        match error {
            CallError::Closed => Self::Closed,
            CallError::Timeout => Self::Timeout,
            CallError::Cancelled => Self::Cancelled,
            CallError::Rpc(error) => Self::Rpc {
                code: error.code,
                message: error.message,
            },
            CallError::Transport(message) | CallError::Unsupported(message) => {
                Self::Transport(message)
            }
        }
    }
}

/// The events surface of one connected MCP server. Constructed only after the
/// handshake confirmed that the server advertises the extension.
#[derive(Clone)]
pub struct EventsClient {
    rpc: Arc<dyn Rpc>,
    catalog: Duration,
    cancel: CancellationToken,
    supported: bool,
}

impl EventsClient {
    pub(crate) fn new(
        rpc: Arc<dyn Rpc>,
        catalog: Duration,
        cancel: CancellationToken,
        supported: bool,
    ) -> Self {
        Self {
            rpc,
            catalog,
            cancel,
            supported,
        }
    }

    #[must_use]
    pub fn supported(&self) -> bool {
        self.supported
    }

    fn gate(&self) -> Result<(), EventsError> {
        if !self.supported {
            Err(EventsError::ExtensionUnsupported)
        } else if self.cancel.is_cancelled() {
            Err(EventsError::Cancelled)
        } else {
            Ok(())
        }
    }

    async fn call(
        &self,
        method: &'static str,
        params: Option<Value>,
    ) -> Result<Value, EventsError> {
        self.gate()?;
        self.rpc
            .request(
                RequestMetadata::new(method),
                params,
                Some(self.catalog),
                &self.cancel,
            )
            .await
            .map_err(Into::into)
    }

    /// Every advertised event type, across at most 100 pages. One malformed
    /// entry is skipped rather than hiding the rest of the catalog.
    pub async fn list(&self) -> Result<Vec<EventType>, EventsError> {
        let mut events = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_EVENT_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let page = self.call("events/list", Some(params)).await?;
            let Some(entries) = page.get("events").and_then(Value::as_array) else {
                return Err(EventsError::Transport(
                    "events/list response has no events array".to_owned(),
                ));
            };
            events.extend(entries.iter().filter_map(parse_event_type));
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(events)
    }

    /// Poll delivery (`events/poll`), used when a push stream is unavailable.
    pub async fn poll(&self, request: PollRequest<'_>) -> Result<PollPage, EventsError> {
        let mut params = Map::new();
        params.insert("name".to_owned(), Value::String(request.name.to_owned()));
        if let Some(cursor) = request.cursor {
            params.insert("cursor".to_owned(), Value::String(cursor.to_owned()));
        }
        if let Some(limit) = request.limit {
            params.insert("limit".to_owned(), json!(limit));
        }
        let page = self
            .call("events/poll", Some(Value::Object(params)))
            .await?;
        let events = page
            .get("events")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_wire_event)
            .collect();
        let next_cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(PollPage {
            events,
            next_cursor,
        })
    }

    /// Webhook delivery (`events/subscribe`). Returns the server's subscription
    /// id; the caller owns its lifetime and ends it with [`Self::unsubscribe`].
    pub async fn subscribe_webhook(
        &self,
        request: SubscribeRequest<'_>,
    ) -> Result<Subscription, EventsError> {
        let mut params = json!({
            "name": request.name,
            "delivery": "webhook",
            "url": request.webhook_url,
        });
        if let Some(cursor) = request.cursor {
            params
                .as_object_mut()
                .expect("json! object literal")
                .insert("cursor".to_owned(), Value::String(cursor.to_owned()));
        }
        let result = self.call("events/subscribe", Some(params)).await?;
        let id = result
            .get("subscriptionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                EventsError::Transport("events/subscribe response has no subscriptionId".to_owned())
            })?;
        Ok(Subscription { id: id.to_owned() })
    }

    pub async fn unsubscribe(&self, subscription: &Subscription) -> Result<(), EventsError> {
        self.call(
            "events/unsubscribe",
            Some(json!({ "subscriptionId": subscription.id })),
        )
        .await?;
        Ok(())
    }

    /// Push delivery (`events/stream`). Notifications are routed by the
    /// subscription id of their stream: the JSON-RPC id of this request.
    pub async fn stream(&self, request: StreamRequest<'_>) -> Result<EventStream, EventsError> {
        self.gate()?;
        let mut params = json!({ "name": request.name });
        let object = params.as_object_mut().expect("json! object literal");
        if let Some(cursor) = request.cursor {
            object.insert("cursor".to_owned(), Value::String(cursor.to_owned()));
        }
        if let Some(arguments) = request.arguments {
            object.insert("arguments".to_owned(), arguments);
        }

        let (sender, messages) = mpsc::channel(MESSAGE_QUEUE);
        let (states, _) = broadcast::channel(STATE_QUEUE);
        let cursor = Arc::new(Mutex::new(request.cursor.map(str::to_owned)));
        let seen = Arc::new(Mutex::new(SeenEvents::default()));
        // tokio-util's CancellationToken has no child(): link an independent
        // scope to the hub's lifetime with a short relay task instead.
        let cancel = CancellationToken::new();
        let parent = self.cancel.clone();
        let relay = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = parent.cancelled() => relay.cancel(),
                () = relay.cancelled() => {}
            }
        });
        let rpc = Arc::clone(&self.rpc);
        let states_task = states.clone();
        let cursor_task = Arc::clone(&cursor);
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            let mut attempts = 0u32;
            loop {
                let resume = cursor_task.lock().expect("cursor lock").clone();
                let mut stream_params = params.clone();
                if let Some(resume) = resume.as_deref() {
                    stream_params
                        .as_object_mut()
                        .expect("stream params object")
                        .insert("cursor".to_owned(), Value::String(resume.to_owned()));
                }
                let _ = states_task.send(SubscriptionState::Connecting {
                    cursor: resume.clone(),
                });
                match drive_stream(
                    &rpc,
                    &stream_params,
                    &cancel_task,
                    &sender,
                    &states_task,
                    &seen,
                )
                .await
                {
                    DriveOutcome::Done => break,
                    DriveOutcome::Rejected { code, message } => {
                        let _ = states_task.send(SubscriptionState::Terminated { code, message });
                        break;
                    }
                    DriveOutcome::Reconnect => {
                        if cancel_task.is_cancelled() {
                            break;
                        }
                        attempts = attempts.saturating_add(1);
                        let _ = states_task.send(SubscriptionState::Reconnecting {
                            cursor: cursor_task.lock().expect("cursor lock").clone(),
                            attempts,
                        });
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }
        });
        Ok(EventStream {
            messages,
            states,
            cursor,
            cancel,
        })
    }
}

/// A live push subscription. Dropping it cancels the stream request and its
/// heartbeat watchdog. Cursors advance only through [`Self::ack`].
pub struct EventStream {
    messages: mpsc::Receiver<EventMessage>,
    states: broadcast::Sender<SubscriptionState>,
    cursor: Arc<Mutex<Option<String>>>,
    cancel: CancellationToken,
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<EventMessage> {
        self.messages.recv().await
    }

    #[must_use]
    pub fn state(&self) -> broadcast::Receiver<SubscriptionState> {
        self.states.subscribe()
    }

    /// Confirm that the caller has consumed the event carrying this cursor.
    /// The wire layer resumes from the last confirmed cursor, never from the
    /// last one it happened to see.
    pub fn ack(&self, cursor: &str) {
        if !cursor.is_empty() {
            *self.cursor.lock().expect("cursor lock") = Some(cursor.to_owned());
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

enum DriveOutcome {
    Done,
    Reconnect,
    Rejected { code: i64, message: String },
}

#[derive(Default)]
struct SeenEvents {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl SeenEvents {
    fn contains(&self, event_id: &str) -> bool {
        self.set.contains(event_id)
    }

    fn insert(&mut self, event_id: &str) {
        if self.set.contains(event_id) {
            return;
        }
        if self.order.len() == MAX_SEEN_EVENTS
            && let Some(oldest) = self.order.pop_front()
        {
            self.set.remove(&oldest);
        }
        self.order.push_back(event_id.to_owned());
        self.set.insert(event_id.to_owned());
    }

    fn remove(&mut self, event_id: &str) {
        if self.set.remove(event_id) {
            self.order.retain(|seen| seen != event_id);
        }
    }
}

async fn drive_stream(
    rpc: &Arc<dyn Rpc>,
    params: &Value,
    cancel: &CancellationToken,
    messages: &mpsc::Sender<EventMessage>,
    states: &broadcast::Sender<SubscriptionState>,
    seen: &Mutex<SeenEvents>,
) -> DriveOutcome {
    // Subscribe before requesting: a server may emit `active` before the
    // `events/stream` request itself settles.
    let mut notifications = rpc.subscribe_notifications();
    let (id_sender, id_receiver) = oneshot::channel::<u64>();
    let id_sender = Mutex::new(Some(id_sender));
    let observe_id = |id: u64| {
        if let Some(sender) = id_sender.lock().expect("id observer lock").take() {
            let _ = sender.send(id);
        }
    };
    let request = rpc.request_with_id(
        RequestMetadata::new("events/stream"),
        Some(params.clone()),
        None,
        cancel,
        Some(&observe_id),
    );
    tokio::pin!(request);
    // The id observer runs when the request is first polled, so the request
    // and the id handoff must be driven together: awaiting the id alone would
    // leave the request unpolled forever.
    let id = tokio::select! {
        id = id_receiver => match id {
            Ok(id) => id,
            Err(_) => return DriveOutcome::Reconnect,
        },
        outcome = &mut request => return request_outcome(outcome, cancel),
    };
    let id = id.to_string();
    let mut silence = UNGRANTED_SILENCE;
    let mut deadline = Instant::now() + silence;
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return DriveOutcome::Done,
            notification = notifications.recv() => match notification {
                Ok(notification) => {
                    let Some(notification_params) = notification.params.as_ref() else {
                        continue;
                    };
                    if subscription_id(notification_params).as_deref() != Some(id.as_str()) {
                        continue;
                    }
                    let Some(message) = parse_notification(&notification.method, notification_params, &id)
                    else {
                        continue;
                    };
                    if let EventMessage::Active { cursor, heartbeat_ms, .. } = &message {
                        silence = Duration::from_millis(heartbeat_ms.saturating_mul(MISSED_HEARTBEATS));
                        let _ = states.send(SubscriptionState::Active {
                            cursor: cursor.clone(),
                            heartbeat_ms: *heartbeat_ms,
                        });
                    }
                    let mut seen = seen.lock().expect("seen events lock");
                    let event_id = match &message {
                        EventMessage::Event { event_id, .. } => Some(event_id.as_str()),
                        _ => None,
                    };
                    if event_id.is_some_and(|event_id| seen.contains(event_id)) {
                        continue;
                    }
                    if let Some(event_id) = event_id {
                        seen.insert(event_id);
                    }
                    let terminated = matches!(message, EventMessage::Terminated { .. });
                    if let Err(error) = messages.try_send(message) {
                        // The rejected event must stay eligible for replay
                        // from the last acknowledged cursor.
                        if let EventMessage::Event { event_id, .. } = error.into_inner() {
                            seen.remove(&event_id);
                        }
                        return DriveOutcome::Reconnect;
                    }
                    if terminated {
                        return DriveOutcome::Done;
                    }
                    deadline = Instant::now() + silence;
                }
                Err(_) => return DriveOutcome::Reconnect,
            },
            outcome = &mut request => return request_outcome(outcome, cancel),
            () = tokio::time::sleep_until(deadline) => return DriveOutcome::Reconnect,
        }
    }
}

fn request_outcome(outcome: Result<Value, CallError>, cancel: &CancellationToken) -> DriveOutcome {
    match outcome {
        Err(CallError::Rpc(error)) if STREAM_REJECTED.contains(&error.code) => {
            DriveOutcome::Rejected {
                code: error.code,
                message: error.message,
            }
        }
        Err(CallError::Cancelled) | Ok(_) if cancel.is_cancelled() => DriveOutcome::Done,
        Err(CallError::Cancelled) => DriveOutcome::Reconnect,
        Ok(_) => DriveOutcome::Reconnect,
        Err(error) => {
            tracing::debug!(%error, "MCP event stream ended");
            DriveOutcome::Reconnect
        }
    }
}

/// Client capabilities sent during handshake. The extension is opt-in: an ox
/// with events disabled must not even advertise it.
pub(crate) fn client_capabilities(enabled: bool) -> Value {
    if enabled {
        json!({ "extensions": { EVENTS_EXTENSION: {} } })
    } else {
        json!({})
    }
}

pub(crate) fn supports_events(capabilities: &Value) -> bool {
    capabilities
        .get("extensions")
        .and_then(|extensions| extensions.get(EVENTS_EXTENSION))
        .is_some_and(Value::is_object)
}

fn subscription_id(params: &Value) -> Option<String> {
    let id = params.get("_meta")?.get(SUBSCRIPTION_ID)?;
    id.as_str()
        .map(str::to_owned)
        .or_else(|| id.as_u64().map(|id| id.to_string()))
}

fn parse_cursor(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(cursor)) => Some(Some(cursor.clone())),
        Some(_) => None,
    }
}

fn parse_event_error(value: Option<&Value>) -> Option<(i64, String)> {
    let error = value?;
    let code = error.get("code")?.as_i64()?;
    let message = error.get("message")?.as_str()?;
    if message.is_empty() {
        return None;
    }
    Some((code, message.to_owned()))
}

fn parse_wire_event(value: &Value) -> Option<EventMessage> {
    let event_id = value.get("eventId")?.as_str()?;
    let name = value.get("name")?.as_str()?;
    let timestamp = value.get("timestamp")?.as_str()?;
    if event_id.is_empty() || name.is_empty() || timestamp.is_empty() {
        return None;
    }
    let data = value.get("data")?.as_object()?;
    Some(EventMessage::Event {
        event_id: event_id.to_owned(),
        name: name.to_owned(),
        timestamp: timestamp.to_owned(),
        data: UntrustedData::from_wire(Value::Object(data.clone())),
        cursor: parse_cursor(value.get("cursor"))?,
    })
}

fn parse_notification(method: &str, params: &Value, id: &str) -> Option<EventMessage> {
    if subscription_id(params)?.as_str() != id {
        return None;
    }
    match method {
        "notifications/events/active" => {
            let heartbeat_ms = params.get("heartbeatMs")?.as_u64()?;
            if heartbeat_ms == 0 {
                return None;
            }
            Some(EventMessage::Active {
                cursor: parse_cursor(params.get("cursor"))?,
                truncated: params
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                heartbeat_ms,
            })
        }
        "notifications/events/event" => parse_wire_event(params),
        "notifications/events/heartbeat" => Some(EventMessage::Heartbeat {
            cursor: parse_cursor(params.get("cursor"))?,
        }),
        "notifications/events/error" => {
            let (code, message) = parse_event_error(params.get("error"))?;
            Some(EventMessage::Error { code, message })
        }
        "notifications/events/terminated" => {
            let (code, message) = parse_event_error(params.get("error"))?;
            Some(EventMessage::Terminated { code, message })
        }
        _ => None,
    }
}

fn parse_event_type(value: &Value) -> Option<EventType> {
    let name = value.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let delivery = value
        .get("delivery")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if delivery.is_empty() {
        return None;
    }
    let input_schema = value.get("inputSchema")?.as_object()?;
    let payload_schema = value.get("payloadSchema")?.as_object()?;
    Some(EventType {
        name: name.to_owned(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        delivery,
        input_schema: Value::Object(input_schema.clone()),
        payload_schema: Value::Object(payload_schema.clone()),
    })
}
