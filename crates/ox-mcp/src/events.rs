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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{RequestMetadata, Rpc, ServerNotification};
    use futures_util::future::BoxFuture;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::broadcast;

    struct RecordedRequest {
        method: String,
        params: Option<Value>,
    }

    type Responder = Box<
        dyn Fn(
                String,
                Option<Value>,
                u64,
                CancellationToken,
            ) -> BoxFuture<'static, Result<Value, CallError>>
            + Send
            + Sync,
    >;

    struct MockRpc {
        responder: Responder,
        requests: Mutex<Vec<RecordedRequest>>,
        ids: Mutex<Vec<u64>>,
        next_id: AtomicU64,
        notifications: broadcast::Sender<ServerNotification>,
    }

    impl MockRpc {
        fn new(responder: Responder) -> Self {
            let (notifications, _) = broadcast::channel(128);
            Self {
                responder,
                requests: Mutex::new(Vec::new()),
                ids: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                notifications,
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().expect("request log lock").len()
        }

        fn ids(&self) -> Vec<u64> {
            self.ids.lock().expect("id log lock").clone()
        }

        fn params_for(&self, method: &str) -> Option<Value> {
            self.requests
                .lock()
                .expect("request log lock")
                .iter()
                .find(|request| request.method == method)
                .and_then(|request| request.params.clone())
        }

        fn last_params_for(&self, method: &str) -> Option<Value> {
            self.requests
                .lock()
                .expect("request log lock")
                .iter()
                .rev()
                .find(|request| request.method == method)
                .and_then(|request| request.params.clone())
        }

        fn notify(&self, method: &str, params: Value) {
            let _ = self.notifications.send(ServerNotification {
                method: method.to_owned(),
                params: Some(params),
            });
        }
    }

    impl Rpc for MockRpc {
        fn request<'a>(
            &'a self,
            metadata: RequestMetadata<'a>,
            params: Option<Value>,
            timeout: Option<Duration>,
            cancel: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Value, CallError>> {
            self.request_with_id(metadata, params, timeout, cancel, None)
        }

        fn request_with_id<'a>(
            &'a self,
            metadata: RequestMetadata<'a>,
            params: Option<Value>,
            _timeout: Option<Duration>,
            cancel: &'a CancellationToken,
            observe_id: Option<&'a (dyn Fn(u64) + Send + Sync)>,
        ) -> BoxFuture<'a, Result<Value, CallError>> {
            Box::pin(async move {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                if let Some(observer) = observe_id {
                    observer(id);
                }
                self.ids.lock().expect("id log lock").push(id);
                self.requests
                    .lock()
                    .expect("request log lock")
                    .push(RecordedRequest {
                        method: metadata.method.to_owned(),
                        params: params.clone(),
                    });
                (self.responder)(metadata.method.to_owned(), params, id, cancel.clone()).await
            })
        }

        fn notify<'a>(
            &'a self,
            _method: &'a str,
            _params: Option<Value>,
        ) -> BoxFuture<'a, Result<(), CallError>> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn subscribe_notifications(&self) -> broadcast::Receiver<ServerNotification> {
            self.notifications.subscribe()
        }
    }

    fn client(rpc: Arc<MockRpc>, supported: bool) -> EventsClient {
        let rpc: Arc<dyn Rpc> = rpc;
        EventsClient::new(
            rpc,
            Duration::from_secs(1),
            CancellationToken::new(),
            supported,
        )
    }

    fn event_type(name: &str, delivery: &[&str]) -> Value {
        json!({
            "name": name,
            "delivery": delivery,
            "inputSchema": { "type": "object" },
            "payloadSchema": { "type": "object" },
        })
    }

    fn subscribed(id: u64, mut params: Value) -> Value {
        params
            .as_object_mut()
            .expect("notification params are an object")
            .insert(
                "_meta".to_owned(),
                json!({ SUBSCRIPTION_ID: id.to_string() }),
            );
        params
    }

    async fn until(predicate: impl Fn() -> bool, what: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    async fn next_state(
        states: &mut broadcast::Receiver<SubscriptionState>,
        wanted: fn(&SubscriptionState) -> bool,
        what: &str,
    ) -> SubscriptionState {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match states.recv().await {
                    Ok(state) if wanted(&state) => return state,
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(error) => panic!("state stream closed: {error}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    #[tokio::test]
    async fn events_list_delivery_modes() {
        let rpc = Arc::new(MockRpc::new(Box::new(|method, params, _, _| {
            Box::pin(async move {
                match method.as_str() {
                    "events/list" => {
                        let cursor = params
                            .as_ref()
                            .and_then(|value| value.get("cursor"))
                            .and_then(Value::as_str);
                        if cursor.is_none() {
                            Ok(json!({
                                "events": [
                                    event_type("push.event", &["push"]),
                                    { "name": "broken" },
                                ],
                                "nextCursor": "page-2",
                            }))
                        } else {
                            Ok(json!({
                                "events": [
                                    event_type("poll.event", &["poll"]),
                                    event_type("webhook.event", &["webhook"]),
                                ],
                            }))
                        }
                    }
                    "events/poll" => Ok(json!({
                        "events": [{
                            "eventId": "poll-1",
                            "name": "poll.event",
                            "timestamp": "2026-11-24T00:00:00Z",
                            "data": { "value": "polled" },
                            "cursor": "p1",
                        }],
                        "nextCursor": "p2",
                    })),
                    "events/subscribe" => Ok(json!({ "subscriptionId": "sub-1" })),
                    "events/unsubscribe" => Ok(json!({})),
                    other => Err(CallError::Transport(format!("unexpected method {other}"))),
                }
            })
        })));
        let events = client(Arc::clone(&rpc), true);

        let listed = events.list().await.expect("event types list");
        let delivery: Vec<(&str, &str)> = listed
            .iter()
            .map(|event| {
                (
                    event.name.as_str(),
                    event.delivery.first().map_or("", String::as_str),
                )
            })
            .collect();
        assert_eq!(
            delivery,
            vec![
                ("push.event", "push"),
                ("poll.event", "poll"),
                ("webhook.event", "webhook"),
            ],
            "all three declared delivery modes must survive paging and per-entry validation"
        );
        assert_eq!(
            rpc.params_for("events/list"),
            Some(json!({})),
            "the first page has no cursor"
        );
        assert_eq!(rpc.request_count(), 2, "two pages, no extra requests");

        let page = events
            .poll(PollRequest {
                name: "poll.event",
                cursor: Some("c7"),
                limit: Some(20),
            })
            .await
            .expect("poll page");
        let EventMessage::Event { data, .. } = page.events.first().expect("one polled event")
        else {
            panic!("expected a polled event, got {:?}", page.events);
        };
        assert!(data.untrusted, "polled data must be marked untrusted too");
        assert_eq!(data.data, json!({ "value": "polled" }));
        assert_eq!(page.next_cursor.as_deref(), Some("p2"));
        assert_eq!(
            rpc.params_for("events/poll"),
            Some(json!({"name": "poll.event", "cursor": "c7", "limit": 20}))
        );

        let subscription = events
            .subscribe_webhook(SubscribeRequest {
                name: "webhook.event",
                webhook_url: "https://example.invalid/hook",
                cursor: None,
            })
            .await
            .expect("webhook subscription");
        assert_eq!(subscription.id, "sub-1");
        assert_eq!(
            rpc.params_for("events/subscribe"),
            Some(json!({
                "name": "webhook.event",
                "delivery": "webhook",
                "url": "https://example.invalid/hook",
            }))
        );
        events
            .unsubscribe(&subscription)
            .await
            .expect("unsubscribe");
        assert_eq!(
            rpc.params_for("events/unsubscribe"),
            Some(json!({"subscriptionId": "sub-1"}))
        );
    }

    #[tokio::test]
    async fn events_stream_reconnect_cursor() {
        let rpc = Arc::new(MockRpc::new(Box::new(|method, _, _, cancel| {
            Box::pin(async move {
                if method == "events/stream" {
                    // A live stream has no response until the server ends it;
                    // notifications arrive out of band.
                    cancel.cancelled().await;
                    return Err(CallError::Cancelled);
                }
                Err(CallError::Transport(format!("unexpected method {method}")))
            })
        })));
        let events = client(Arc::clone(&rpc), true);
        let mut stream = events
            .stream(StreamRequest {
                name: "push.event",
                cursor: None,
                arguments: None,
            })
            .await
            .expect("event stream");
        let mut states = stream.state();

        until(|| rpc.ids().len() == 1, "first stream request").await;
        let first_id = rpc.ids()[0];
        rpc.notify(
            "notifications/events/active",
            subscribed(
                first_id,
                json!({ "cursor": null, "heartbeatMs": 25, "truncated": false }),
            ),
        );
        assert_eq!(
            stream.recv().await.expect("active message"),
            EventMessage::Active {
                cursor: None,
                truncated: false,
                heartbeat_ms: 25,
            }
        );
        next_state(
            &mut states,
            |state| {
                matches!(
                    state,
                    SubscriptionState::Active {
                        heartbeat_ms: 25,
                        ..
                    }
                )
            },
            "active state",
        )
        .await;

        let first_event = json!({
            "eventId": "e1",
            "name": "push.event",
            "timestamp": "2026-11-24T00:00:00Z",
            "data": { "value": "one" },
            "cursor": "c1",
        });
        // A notification from another stream must not reach this subscriber.
        rpc.notify(
            "notifications/events/event",
            subscribed(
                first_id + 1000,
                json!({
                    "eventId": "wrong-stream",
                    "name": "push.event",
                    "timestamp": "2026-11-24T00:00:00Z",
                    "data": { "value": "wrong" },
                    "cursor": "wrong",
                }),
            ),
        );
        rpc.notify(
            "notifications/events/event",
            subscribed(first_id, first_event.clone()),
        );
        let delivered = stream.recv().await.expect("first event");
        let EventMessage::Event { data, cursor, .. } = delivered else {
            panic!("expected an event message, got {delivered:?}");
        };
        assert!(data.untrusted, "event data must be marked untrusted");
        assert_eq!(data.data, json!({ "value": "one" }));
        assert_eq!(cursor.as_deref(), Some("c1"));
        stream.ack("c1");

        // At-least-once redelivery of the same eventId is dropped.
        rpc.notify(
            "notifications/events/event",
            subscribed(first_id, first_event),
        );
        let duplicate = tokio::time::timeout(Duration::from_millis(120), stream.recv()).await;
        assert!(
            duplicate.is_err(),
            "same eventId must not be delivered twice: {duplicate:?}"
        );

        // Three missed heartbeat intervals kill the stream; it reopens from
        // the cursor the caller acknowledged, not the last event seen.
        next_state(
            &mut states,
            |state| matches!(state, SubscriptionState::Reconnecting { .. }),
            "reconnect state",
        )
        .await;
        until(|| rpc.ids().len() == 2, "second stream request").await;
        let second_id = rpc.ids()[1];
        assert_eq!(
            rpc.last_params_for("events/stream"),
            Some(json!({"name": "push.event", "cursor": "c1"})),
            "the resumed stream starts at the acknowledged cursor"
        );
        rpc.notify(
            "notifications/events/active",
            subscribed(
                second_id,
                json!({ "cursor": "c1", "heartbeatMs": 25, "truncated": true }),
            ),
        );
        assert!(matches!(
            stream.recv().await.expect("second active"),
            EventMessage::Active {
                truncated: true,
                ..
            }
        ));
        rpc.notify(
            "notifications/events/event",
            subscribed(
                second_id,
                json!({
                    "eventId": "e2",
                    "name": "push.event",
                    "timestamp": "2026-11-24T00:00:01Z",
                    "data": { "value": "two" },
                    "cursor": "c2",
                }),
            ),
        );
        assert!(matches!(
            stream.recv().await.expect("second event"),
            EventMessage::Event { event_id, .. } if event_id == "e2"
        ));

        // "c2" was never acknowledged: another break must still resume at
        // "c1", so an unacknowledged event cannot be lost by advancing the
        // replay position.
        next_state(
            &mut states,
            |state| matches!(state, SubscriptionState::Reconnecting { .. }),
            "second reconnect state",
        )
        .await;
        until(|| rpc.ids().len() == 3, "third stream request").await;
        assert_eq!(
            rpc.last_params_for("events/stream"),
            Some(json!({"name": "push.event", "cursor": "c1"})),
            "an unacknowledged cursor must not advance"
        );
        drop(stream);
    }

    #[tokio::test]
    async fn full_queue_reconnect_replays_the_failed_event() {
        let rpc = Arc::new(MockRpc::new(Box::new(|method, _, _, cancel| {
            Box::pin(async move {
                if method == "events/stream" {
                    cancel.cancelled().await;
                    return Err(CallError::Cancelled);
                }
                Err(CallError::Transport(format!("unexpected method {method}")))
            })
        })));
        let cancel = CancellationToken::new();
        let (messages, mut receiver) = mpsc::channel(1);
        let (states, _) = broadcast::channel(STATE_QUEUE);
        let seen = Arc::new(Mutex::new(SeenEvents::default()));
        let params = json!({ "name": "push.event", "cursor": "c0" });
        let notify_event = |id: u64, event_id: &str, cursor: &str| {
            rpc.notify(
                "notifications/events/event",
                subscribed(
                    id,
                    json!({
                        "eventId": event_id,
                        "name": "push.event",
                        "timestamp": "2026-11-24T00:00:00Z",
                        "data": { "value": event_id },
                        "cursor": cursor,
                    }),
                ),
            );
        };

        let first = tokio::spawn({
            let rpc: Arc<dyn Rpc> = rpc.clone();
            let cancel = cancel.clone();
            let params = params.clone();
            let messages = messages.clone();
            let states = states.clone();
            let seen = Arc::clone(&seen);
            async move { drive_stream(&rpc, &params, &cancel, &messages, &states, &seen).await }
        });
        until(|| rpc.ids().len() == 1, "first stream request").await;
        let first_id = rpc.ids()[0];
        notify_event(first_id, "e1", "c1");
        until(|| messages.capacity() == 0, "first event queued").await;
        notify_event(first_id, "e2", "c2");
        assert!(
            matches!(
                first.await.expect("first drive task"),
                DriveOutcome::Reconnect
            ),
            "a full queue must trigger reconnect"
        );
        assert!(
            matches!(
                receiver.recv().await.expect("queued first event"),
                EventMessage::Event { event_id, .. } if event_id == "e1"
            ),
            "the first event was accepted by the queue"
        );

        // The caller consumed e1 and acknowledged its cursor; e2 never made
        // it out of the full queue and must be replayed from "c1".
        let replay_params = json!({ "name": "push.event", "cursor": "c1" });
        let replay = tokio::spawn({
            let rpc: Arc<dyn Rpc> = rpc.clone();
            let cancel = cancel.clone();
            let params = replay_params.clone();
            let messages = messages.clone();
            let states = states.clone();
            let seen = Arc::clone(&seen);
            async move { drive_stream(&rpc, &params, &cancel, &messages, &states, &seen).await }
        });
        until(|| rpc.ids().len() == 2, "replayed stream request").await;
        let second_id = rpc.ids()[1];
        assert_eq!(
            rpc.last_params_for("events/stream"),
            Some(replay_params),
            "reconnect resumes from the acknowledged cursor"
        );
        notify_event(second_id, "e2", "c2");
        let replayed = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("replayed event must not be deduped away")
            .expect("replay stream still alive");
        assert!(
            matches!(replayed, EventMessage::Event { event_id, .. } if event_id == "e2"),
            "the event that overflowed the queue must be deliverable on replay"
        );
        cancel.cancel();
        let _ = replay.await;
    }

    #[tokio::test]
    async fn events_extension_gate() {
        let rpc = Arc::new(MockRpc::new(Box::new(|method, _, _, _| {
            Box::pin(async move { Err(CallError::Transport(format!("unexpected call {method}"))) })
        })));
        let events = client(Arc::clone(&rpc), false);
        assert!(!events.supported());

        assert!(matches!(
            events.list().await,
            Err(EventsError::ExtensionUnsupported)
        ));
        assert!(matches!(
            events
                .poll(PollRequest {
                    name: "x",
                    cursor: None,
                    limit: None,
                })
                .await,
            Err(EventsError::ExtensionUnsupported)
        ));
        assert!(matches!(
            events
                .subscribe_webhook(SubscribeRequest {
                    name: "x",
                    webhook_url: "https://example.invalid/hook",
                    cursor: None,
                })
                .await,
            Err(EventsError::ExtensionUnsupported)
        ));
        assert!(matches!(
            events
                .stream(StreamRequest {
                    name: "x",
                    cursor: None,
                    arguments: None,
                })
                .await,
            Err(EventsError::ExtensionUnsupported)
        ));
        assert_eq!(rpc.request_count(), 0, "no events/* request may be sent");

        assert!(!supports_events(&json!({})), "no extension capability");
        assert!(supports_events(
            &json!({ "extensions": { EVENTS_EXTENSION: {} } })
        ));
        assert_eq!(
            client_capabilities(true)["extensions"][EVENTS_EXTENSION],
            json!({})
        );
        assert!(
            client_capabilities(false).get("extensions").is_none(),
            "undeclared clients do not advertise the extension"
        );

        // Negotiation itself must fail closed: a modern server that omits the
        // extension in its capabilities must leave every events/* call
        // unissued, even when ox requested it.
        let negotiated = Arc::new(MockRpc::new(Box::new(|method, _, _, _| {
            Box::pin(async move {
                match method.as_str() {
                    "server/discover" => Ok(json!({
                        "protocolVersion": "2026-07-28",
                        "capabilities": {},
                    })),
                    other => Err(CallError::Transport(format!("unexpected call {other}"))),
                }
            })
        })));
        let handshake = crate::client::Client::connect_events(
            Arc::clone(&negotiated) as Arc<dyn Rpc>,
            Duration::from_secs(1),
            &CancellationToken::new(),
            ox_core::config::McpProtocol::Modern,
            true,
        )
        .await
        .expect("modern handshake");
        assert!(!handshake.events, "server did not advertise the extension");
        let gated = EventsClient::new(
            Arc::clone(&negotiated) as Arc<dyn Rpc>,
            Duration::from_secs(1),
            CancellationToken::new(),
            handshake.events,
        );
        assert!(matches!(
            gated.list().await,
            Err(EventsError::ExtensionUnsupported)
        ));
        assert_eq!(
            negotiated.request_count(),
            1,
            "only server/discover was sent; no events/* request"
        );
    }
}
