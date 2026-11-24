//! Server-side MCP Events integration: subscription lifecycle, untrusted event
//! publication and the HMAC-authenticated webhook receiver.
//!
//! The registry is process-wide, like the MCP hub it subscribes to. It owns
//! only cancellation tokens and task handles; all wire calls stay in `ox-mcp`
//! and every published payload is re-wrapped as untrusted here, so a server
//! that lies about its data cannot enter the local event bus as trusted text.
//!
//! Cursors live with the registry, not with a task: a reconnect or a reload
//! stops every task but keeps the last cursor the publisher acknowledged, so
//! the next delivery resumes at the same wire position.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::mem;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use hmac::{Hmac, KeyInit, Mac};
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::Sha256;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, timeout};
use tokio_util::sync::CancellationToken;

use ox_core::event_bus::EventBus;
use ox_mcp::{
    EventMessage, EventsClient, PollPage, PollRequest, Status as McpStatus, StreamRequest,
    SubscribeRequest, SubscriptionState,
};

use super::AppState;
use super::compat::directory_of;

const EVENT_KIND: &str = "experimental.mcp.event";
const STATE_KIND: &str = "experimental.mcp.subscription.changed";
const SIGNATURE_HEADER: &str = "x-ox-mcp-signature";
const MAX_WEBHOOK_BODY: usize = 1024 * 1024;
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
const UNSUBSCRIBE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_SEEN_EVENTS: usize = 1024;

/// One wire subscription, identified by its server and event name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SubscriptionKey {
    server: String,
    name: String,
}

impl SubscriptionKey {
    pub(crate) fn new(server: &str, name: &str) -> Self {
        Self {
            server: server.to_owned(),
            name: name.to_owned(),
        }
    }

    fn server(&self) -> &str {
        &self.server
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// How the MCP server delivers one event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    Push,
    Poll,
    Webhook,
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Poll => "poll",
            Self::Webhook => "webhook",
        }
    }
}

/// A subscription the registry should own until the next reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    key: SubscriptionKey,
    delivery: Delivery,
    cursor: Option<String>,
}

impl Plan {
    pub(crate) fn new(server: &str, name: &str, delivery: Delivery, cursor: Option<&str>) -> Self {
        Self {
            key: SubscriptionKey::new(server, name),
            delivery,
            cursor: cursor.map(str::to_owned),
        }
    }
}

#[derive(Default)]
struct CursorState {
    cursor: Option<String>,
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
}

struct Entry {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// Lifecycle and replay state of every MCP event subscription.
#[derive(Default)]
pub(crate) struct Registry {
    entries: Mutex<BTreeMap<SubscriptionKey, Entry>>,
    cursors: Mutex<BTreeMap<SubscriptionKey, CursorState>>,
}

impl Registry {
    fn entry_map(&self) -> std::sync::MutexGuard<'_, BTreeMap<SubscriptionKey, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn cursor_map(&self) -> std::sync::MutexGuard<'_, BTreeMap<SubscriptionKey, CursorState>> {
        self.cursors
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn len(&self) -> usize {
        self.entry_map().len()
    }

    pub(crate) fn contains(&self, key: &SubscriptionKey) -> bool {
        self.entry_map().contains_key(key)
    }

    pub(crate) fn cursor(&self, key: &SubscriptionKey) -> Option<String> {
        self.cursor_map()
            .get(key)
            .and_then(|state| state.cursor.clone())
    }

    fn set_cursor(&self, key: &SubscriptionKey, cursor: &str) {
        if cursor.is_empty() {
            return;
        }
        let mut cursors = self.cursor_map();
        cursors.entry(key.clone()).or_default().cursor = Some(cursor.to_owned());
    }

    fn seen(&self, key: &SubscriptionKey, event_id: &str) -> bool {
        self.cursor_map()
            .get(key)
            .is_some_and(|state| state.seen_set.contains(event_id))
    }

    fn remember(&self, key: &SubscriptionKey, event_id: &str, cursor: Option<&str>) {
        let mut cursors = self.cursor_map();
        let state = cursors.entry(key.clone()).or_default();
        if !state.seen_set.contains(event_id) {
            if state.seen.len() == MAX_SEEN_EVENTS
                && let Some(oldest) = state.seen.pop_front()
            {
                state.seen_set.remove(&oldest);
            }
            state.seen.push_back(event_id.to_owned());
            state.seen_set.insert(event_id.to_owned());
        }
        if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
            state.cursor = Some(cursor.to_owned());
        }
    }

    /// Replace the whole set: stop every old subscription, then launch the
    /// plans. Cursors survive because they are a separate map.
    pub(crate) async fn sync(
        &self,
        plans: Vec<Plan>,
        launch: impl Fn(Plan, CancellationToken) -> JoinHandle<()>,
    ) {
        self.stop_all().await;
        for plan in plans {
            let cancel = CancellationToken::new();
            let task = launch(plan.clone(), cancel.clone());
            self.insert(plan.key, cancel, task);
        }
    }

    pub(crate) fn insert(
        &self,
        key: SubscriptionKey,
        cancel: CancellationToken,
        task: JoinHandle<()>,
    ) {
        self.entry_map().insert(key, Entry { cancel, task });
    }

    /// Cancel and join every task. A task that ignores its token is aborted,
    /// so a reload can never leave a zombie behind.
    pub(crate) async fn stop_all(&self) {
        let entries = mem::take(&mut *self.entry_map());
        for (key, entry) in entries {
            stop_entry(&key, entry).await;
        }
    }

    pub(crate) async fn stop_key(&self, key: &SubscriptionKey) {
        let entry = self.entry_map().remove(key);
        if let Some(entry) = entry {
            stop_entry(key, entry).await;
        }
    }

    /// Publish one wire event and only then acknowledge its cursor. Returns
    /// whether the event was new; duplicates are silently dropped.
    pub(crate) fn publish(
        &self,
        bus: &EventBus,
        key: &SubscriptionKey,
        message: EventMessage,
        directory: Option<&str>,
    ) -> Result<bool> {
        let EventMessage::Event {
            event_id,
            name,
            timestamp,
            data,
            cursor,
            ..
        } = message
        else {
            return Ok(false);
        };
        if self.seen(key, &event_id) {
            return Ok(false);
        }
        let mut event_data = data.data;
        escape_line_separators(&mut event_data);
        // The marker is set here, not copied from the wire: no server can
        // promote its own payload into trusted local text. Both maps take
        // ownership of their values, so no payload subtree is re-serialized.
        let mut untrusted_data = Map::with_capacity(2);
        untrusted_data.insert("untrusted".to_owned(), Value::Bool(true));
        untrusted_data.insert("data".to_owned(), event_data);
        let mut payload = Map::with_capacity(5);
        payload.insert("server".to_owned(), Value::String(key.server.clone()));
        payload.insert("name".to_owned(), Value::String(name));
        payload.insert("eventId".to_owned(), Value::String(event_id.clone()));
        payload.insert("timestamp".to_owned(), Value::String(timestamp));
        payload.insert("data".to_owned(), Value::Object(untrusted_data));
        bus.broadcast_ephemeral(EVENT_KIND, Value::Object(payload), directory);
        self.remember(key, &event_id, cursor.as_deref());
        Ok(true)
    }

    /// Apply one poll page. The page cursor moves only after all its events
    /// were published; individual event cursors move inside [`Self::publish`].
    pub(crate) fn apply_poll(
        &self,
        bus: &EventBus,
        key: &SubscriptionKey,
        page: PollPage,
        directory: Option<&str>,
    ) -> Result<()> {
        for message in page.events {
            self.publish(bus, key, message, directory)?;
        }
        if let Some(cursor) = page.next_cursor {
            self.set_cursor(key, &cursor);
        }
        Ok(())
    }
}

async fn stop_entry(key: &SubscriptionKey, entry: Entry) {
    entry.cancel.cancel();
    let mut task = entry.task;
    if timeout(STOP_TIMEOUT, &mut task).await.is_err() {
        tracing::warn!(
            server = key.server,
            name = key.name,
            "MCP event task did not stop; aborting"
        );
        task.abort();
    }
}

static SHARED: OnceLock<Arc<Registry>> = OnceLock::new();

pub(crate) fn shared() -> &'static Arc<Registry> {
    SHARED.get_or_init(|| Arc::new(Registry::default()))
}

/// Replace all subscriptions from the current config and hub. Per-server
/// failures are logged and skipped: one broken MCP server must not undo a
/// reload or stop the other subscriptions.
pub(crate) async fn restart(state: &AppState) -> Result<()> {
    let registry = Arc::clone(shared());
    registry.stop_all().await;
    let services = state.services();
    let config = &services.config.mcp;
    if !config.events.enabled {
        tracing::debug!(
            enabled = config.events.enabled,
            "MCP events have no subscriptions"
        );
        return Ok(());
    }

    let mut plans = Vec::new();
    let mut sources = BTreeMap::new();
    for server in services.mcp.servers() {
        if !matches!(server.status, McpStatus::Connected) {
            continue;
        }
        let Some(client) = services.mcp.events(&server.name) else {
            continue;
        };
        let types = match client.list().await {
            Ok(types) => types,
            Err(error) => {
                tracing::warn!(server = server.name, %error, "MCP events/list failed");
                continue;
            }
        };
        for kind in types {
            let delivery = choose_delivery(
                &kind.delivery,
                &config.events.webhook,
                &server.name,
                &kind.name,
            );
            let Some(delivery) = delivery else {
                continue;
            };
            let key = SubscriptionKey::new(&server.name, &kind.name);
            let plan = Plan::new(
                &server.name,
                &kind.name,
                delivery,
                registry.cursor(&key).as_deref(),
            );
            sources.insert(key, client.clone());
            plans.push(plan);
        }
    }

    let bus = state.bus.clone();
    let directory = state.directory.to_string_lossy().into_owned();
    let period = Duration::from_millis(config.events.poll_interval);
    let webhook_config = config.events.webhook.clone();
    let launch_registry = Arc::clone(&registry);
    registry
        .sync(plans, move |plan, cancel| {
            let key = plan.key.clone();
            let Some(client) = sources.get(&key).cloned() else {
                return tokio::spawn(async {});
            };
            let bus = bus.clone();
            let directory = directory.clone();
            let registry = Arc::clone(&launch_registry);
            let cursor = plan.cursor.clone();
            match plan.delivery {
                Delivery::Push => spawn_push(registry, client, key, cursor, bus, directory, cancel),
                Delivery::Poll => spawn_poll(registry, client, key, bus, directory, period, cancel),
                Delivery::Webhook => match webhook_config.callback_url(key.server(), key.name()) {
                    Ok(url) => spawn_webhook(registry, client, key, url, bus, directory, cancel),
                    Err(error) => {
                        tracing::warn!(%error, server = key.server, "webhook subscription skipped");
                        tokio::spawn(async {})
                    }
                },
            }
        })
        .await;
    tracing::info!(
        subscriptions = registry.len(),
        "MCP event subscriptions rebuilt"
    );
    Ok(())
}

fn choose_delivery(
    offered: &[String],
    webhook_config: &ox_core::config::McpWebhookConfig,
    server: &str,
    name: &str,
) -> Option<Delivery> {
    let offers = |delivery: &str| offered.iter().any(|offered| offered == delivery);
    if offers("push") {
        Some(Delivery::Push)
    } else if offers("poll") {
        Some(Delivery::Poll)
    } else if offers("webhook") && webhook_config.callback_url(server, name).is_ok() {
        Some(Delivery::Webhook)
    } else {
        None
    }
}

fn publish_state(
    registry: &Registry,
    bus: &EventBus,
    key: &SubscriptionKey,
    delivery: Delivery,
    status: &str,
    directory: Option<&str>,
) {
    bus.broadcast_ephemeral(
        STATE_KIND,
        json!({
            "server": key.server,
            "name": key.name,
            "delivery": delivery.as_str(),
            "status": status,
            "cursor": registry.cursor(key),
        }),
        directory,
    );
}

fn spawn_push(
    registry: Arc<Registry>,
    client: EventsClient,
    key: SubscriptionKey,
    cursor: Option<String>,
    bus: EventBus,
    directory: String,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = match client
            .stream(StreamRequest {
                name: key.name(),
                cursor: cursor.as_deref(),
                arguments: None,
            })
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(server = key.server(), name = key.name(), %error, "events/stream refused");
                publish_state(
                    &registry,
                    &bus,
                    &key,
                    Delivery::Push,
                    "failed",
                    Some(&directory),
                );
                return;
            }
        };
        let mut states = stream.state();
        publish_state(
            &registry,
            &bus,
            &key,
            Delivery::Push,
            "active",
            Some(&directory),
        );
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                state = states.recv() => {
                    if let Ok(SubscriptionState::Terminated { .. }) = state {
                        publish_state(&registry, &bus, &key, Delivery::Push, "terminated", Some(&directory));
                        break;
                    }
                }
                message = stream.recv() => {
                    let Some(message) = message else { break; };
                    let cursor = match &message {
                        EventMessage::Event { cursor, .. } => cursor.clone(),
                        _ => None,
                    };
                    match registry.publish(&bus, &key, message, Some(&directory)) {
                        Ok(true) => {
                            if let Some(cursor) = cursor {
                                stream.ack(&cursor);
                            }
                        }
                        Ok(false) => {}
                        Err(error) => tracing::warn!(%error, "MCP event publish failed"),
                    }
                }
            }
        }
        publish_state(
            &registry,
            &bus,
            &key,
            Delivery::Push,
            "stopped",
            Some(&directory),
        );
    })
}

fn spawn_poll(
    registry: Arc<Registry>,
    client: EventsClient,
    key: SubscriptionKey,
    bus: EventBus,
    directory: String,
    period: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        publish_state(
            &registry,
            &bus,
            &key,
            Delivery::Poll,
            "active",
            Some(&directory),
        );
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let cursor = registry.cursor(&key);
                    match client.poll(PollRequest {
                        name: key.name(),
                        cursor: cursor.as_deref(),
                        limit: None,
                    }).await {
                        Ok(page) => {
                            if let Err(error) = registry.apply_poll(&bus, &key, page, Some(&directory)) {
                                tracing::warn!(%error, "MCP poll page publish failed");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(server = key.server(), %error, "events/poll failed");
                            publish_state(&registry, &bus, &key, Delivery::Poll, "failed", Some(&directory));
                        }
                    }
                }
            }
        }
        publish_state(
            &registry,
            &bus,
            &key,
            Delivery::Poll,
            "stopped",
            Some(&directory),
        );
    })
}

fn spawn_webhook(
    registry: Arc<Registry>,
    client: EventsClient,
    key: SubscriptionKey,
    url: String,
    bus: EventBus,
    directory: String,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let cursor = registry.cursor(&key);
        let subscription = match client
            .subscribe_webhook(SubscribeRequest {
                name: key.name(),
                webhook_url: &url,
                cursor: cursor.as_deref(),
            })
            .await
        {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::warn!(server = key.server(), %error, "events/subscribe failed");
                publish_state(
                    &registry,
                    &bus,
                    &key,
                    Delivery::Webhook,
                    "failed",
                    Some(&directory),
                );
                return;
            }
        };
        publish_state(
            &registry,
            &bus,
            &key,
            Delivery::Webhook,
            "active",
            Some(&directory),
        );
        cancel.cancelled().await;
        if timeout(UNSUBSCRIBE_TIMEOUT, client.unsubscribe(&subscription))
            .await
            .is_err()
        {
            tracing::warn!(server = key.server(), "MCP webhook unsubscribe timed out");
        }
        publish_state(
            &registry,
            &bus,
            &key,
            Delivery::Webhook,
            "stopped",
            Some(&directory),
        );
    })
}

/// Create one webhook subscription from an HTTP request. Missing public URL
/// or secret is refused here, before any wire call is sent.
pub(crate) async fn create_webhook(state: &AppState, server: &str, name: &str) -> Result<()> {
    let services = state.services();
    let config = &services.config.mcp;
    ensure!(config.events.enabled, "MCP events are disabled");
    let url = config.events.webhook.callback_url(server, name)?;
    let client = state
        .services()
        .mcp
        .events(server)
        .ok_or_else(|| anyhow::anyhow!("MCP server {server} has no events extension"))?;
    let types = client.list().await?;
    ensure!(
        types
            .iter()
            .any(|kind| kind.name == name
                && kind.delivery.iter().any(|delivery| delivery == "webhook")),
        "event {name} does not support webhook delivery"
    );
    let key = SubscriptionKey::new(server, name);
    let registry = Arc::clone(shared());
    registry.stop_key(&key).await;
    let plan = Plan::new(
        server,
        name,
        Delivery::Webhook,
        registry.cursor(&key).as_deref(),
    );
    let cancel = CancellationToken::new();
    let task = spawn_webhook(
        Arc::clone(&registry),
        client,
        key,
        url,
        state.bus.clone(),
        state.directory.to_string_lossy().into_owned(),
        cancel.clone(),
    );
    registry.insert(plan.key, cancel, task);
    Ok(())
}

fn escape_line_separators(value: &mut Value) {
    match value {
        Value::String(text) => {
            if text.contains('\u{2028}') || text.contains('\u{2029}') {
                let mut escaped = String::with_capacity(text.len());
                for character in text.chars() {
                    match character {
                        '\u{2028}' => escaped.push_str("\\u2028"),
                        '\u{2029}' => escaped.push_str("\\u2029"),
                        other => escaped.push(other),
                    }
                }
                *text = escaped;
            }
        }
        Value::Array(items) => {
            for item in items {
                escape_line_separators(item);
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                escape_line_separators(item);
            }
        }
        _ => {}
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    ensure!(
        !hex.is_empty() && hex.len().is_multiple_of(2),
        "invalid hex signature"
    );
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().as_chunks::<2>().0 {
        let high = hex_digit(pair[0]).context("invalid hex signature")?;
        let low = hex_digit(pair[1]).context("invalid hex signature")?;
        bytes.push(high << 4 | low);
    }
    Ok(bytes)
}

fn verify_signature(secret: &str, header: &str, body: &[u8]) -> bool {
    let Some(hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = decode_hex(hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn required_field<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("webhook payload has no {name}"))
}

fn parse_webhook(mut payload: Value) -> Result<EventMessage> {
    let Some(object) = payload.as_object_mut() else {
        bail!("webhook payload must be an object");
    };
    let event_id = required_field(object, "eventId")?.to_owned();
    let name = required_field(object, "name")?.to_owned();
    let timestamp = required_field(object, "timestamp")?.to_owned();
    let data = object.remove("data").unwrap_or(Value::Null);
    let cursor = object
        .get("cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_owned);
    Ok(EventMessage::Event {
        event_id,
        name,
        timestamp,
        data: ox_mcp::UntrustedData {
            untrusted: true,
            data,
        },
        cursor,
    })
}

/// Verify and publish one webhook body into the event bus.
pub(crate) fn handle_webhook_payload(
    registry: &Registry,
    bus: &EventBus,
    key: &SubscriptionKey,
    secret: &str,
    header: &str,
    body: &[u8],
    directory: Option<&str>,
) -> Result<()> {
    ensure!(
        verify_signature(secret, header, body),
        "invalid webhook signature"
    );
    let payload: Value = serde_json::from_slice(body).context("invalid webhook payload")?;
    registry.publish(bus, key, parse_webhook(payload)?, directory)?;
    Ok(())
}

#[derive(Debug, Deserialize, Default)]
struct SubscribeBody {
    server: Option<String>,
    name: Option<String>,
}

fn client_error(res: &mut Response, status: StatusCode, tag: &str, message: &str) {
    res.status_code(status);
    res.render(Json(json!({ "_tag": tag, "message": message })));
}

/// `POST /api/experimental/mcp/events/subscribe` — only webhook delivery is
/// created explicitly; push and poll follow the server's advertised catalog.
#[handler]
pub(crate) async fn subscribe(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    if !state.services().config.mcp.events.enabled {
        client_error(
            res,
            StatusCode::NOT_FOUND,
            "McpEventsDisabledError",
            "MCP events are disabled",
        );
        return;
    }
    let bytes = match req.payload_with_max_size(MAX_WEBHOOK_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            client_error(
                res,
                StatusCode::BAD_REQUEST,
                "InvalidRequestError",
                "Invalid request body",
            );
            return;
        }
    };
    let Ok(body) = serde_json::from_slice::<SubscribeBody>(bytes) else {
        client_error(
            res,
            StatusCode::BAD_REQUEST,
            "InvalidRequestError",
            "Invalid request body",
        );
        return;
    };
    let (Some(server), Some(name)) = (body.server, body.name) else {
        client_error(
            res,
            StatusCode::BAD_REQUEST,
            "InvalidRequestError",
            "server and name are required",
        );
        return;
    };
    match create_webhook(state, &server, &name).await {
        Ok(()) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(error) => {
            tracing::warn!(%error, %server, %name, "webhook subscription refused");
            client_error(
                res,
                StatusCode::BAD_REQUEST,
                "McpWebhookSubscriptionError",
                &error.to_string(),
            );
        }
    }
}

/// `POST /api/experimental/mcp/events/webhook?server=...&name=...` — HMAC-
/// authenticated callback from an MCP server. The raw bytes are signed; JSON
/// is parsed only after the tag verifies.
#[handler]
pub(crate) async fn webhook(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let directory = directory_of(req);
    let secret = state.services().config.mcp.events.webhook.secret.clone();
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        client_error(
            res,
            StatusCode::BAD_REQUEST,
            "McpWebhookSecretError",
            "webhook secret is not configured",
        );
        return;
    };
    let (Some(server), Some(name)) = (req.query::<String>("server"), req.query::<String>("name"))
    else {
        client_error(
            res,
            StatusCode::BAD_REQUEST,
            "InvalidRequestError",
            "server and name are required",
        );
        return;
    };
    let key = SubscriptionKey::new(&server, &name);
    if !shared().contains(&key) {
        client_error(
            res,
            StatusCode::NOT_FOUND,
            "McpSubscriptionNotFoundError",
            "no such webhook subscription",
        );
        return;
    }
    let signature = req
        .headers()
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = match req.payload_with_max_size(MAX_WEBHOOK_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            client_error(
                res,
                StatusCode::BAD_REQUEST,
                "InvalidRequestError",
                "Invalid request body",
            );
            return;
        }
    };
    match handle_webhook_payload(
        shared(),
        &state.bus,
        &key,
        &secret,
        &signature,
        bytes,
        Some(&directory),
    ) {
        Ok(()) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(error) => {
            tracing::warn!(%error, %server, %name, "MCP webhook rejected");
            client_error(
                res,
                StatusCode::BAD_REQUEST,
                "McpWebhookRejectedError",
                &error.to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hmac::{Hmac, KeyInit, Mac};
    use salvo::http::Body;
    use salvo::http::Response;
    use serde_json::json;
    use sha2::Sha256;

    use ox_core::event_bus::EventBus;
    use ox_mcp::{EventMessage, PollPage, UntrustedData};

    use super::{Plan, Registry, SubscriptionKey, handle_webhook_payload, verify_signature};

    fn event(event_id: &str, cursor: &str, text: &str) -> EventMessage {
        EventMessage::Event {
            event_id: event_id.to_owned(),
            name: "build".to_owned(),
            timestamp: "2026-11-24T00:00:00Z".to_owned(),
            data: UntrustedData {
                untrusted: true,
                data: json!({ "text": text }),
            },
            cursor: Some(cursor.to_owned()),
        }
    }

    fn signature(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            hex.push_str(&format!("{byte:02x}"));
        }
        format!("sha256={hex}")
    }

    #[tokio::test]
    async fn mcp_event_sse_delivery() {
        use crate::api::event::{StreamOptions, stream};

        let db = ox_db::Db::open_in_memory().await.expect("test database");
        let bus = EventBus::new(db);
        let registry = Registry::default();
        let key = SubscriptionKey::new("alpha", "build");
        let runtime = crate::runtime::Runtime::new();
        let mut response = Response::new();
        stream(
            &mut response,
            bus.clone(),
            StreamOptions {
                period: Duration::from_secs(60),
                cursor: None,
                only: None,
                types: Some(vec!["experimental.mcp.event".to_owned()]),
                connected: false,
                follow: true,
                runtime: runtime.clone(),
            },
        )
        .await;

        registry
            .publish(
                &bus,
                &key,
                event("e1", "c1", "hello\u{2028}world"),
                Some("/work/ox"),
            )
            .expect("publish event");

        let mut body = response.take_body();
        let frame = tokio::time::timeout(
            Duration::from_secs(1),
            poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .expect("SSE frame arrives")
        .expect("SSE frame exists")
        .expect("SSE frame is valid")
        .into_data()
        .expect("SSE frame has data");
        let frame = String::from_utf8(frame.to_vec()).expect("UTF-8 frame");
        assert!(frame.contains("experimental.mcp.event"), "{frame}");
        assert!(frame.contains("\"untrusted\":true"), "{frame}");
        assert!(frame.contains("\\u2028"), "{frame}");
        assert_eq!(registry.cursor(&key).as_deref(), Some("c1"));
        drop(body);
        runtime.stop().cancel();
    }

    #[tokio::test]
    async fn mcp_event_marker_is_locally_enforced() {
        let db = ox_db::Db::open_in_memory().await.expect("test database");
        let bus = EventBus::new(db);
        let mut observed = bus.subscribe();
        let registry = Registry::default();
        let key = SubscriptionKey::new("alpha", "build");
        let message = EventMessage::Event {
            event_id: "e1".to_owned(),
            name: "build".to_owned(),
            timestamp: "2026-11-24T00:00:00Z".to_owned(),
            data: UntrustedData {
                untrusted: false,
                data: json!({ "text": "hello\u{2028}world" }),
            },
            cursor: Some("c1".to_owned()),
        };

        registry
            .publish(&bus, &key, message, Some("/work/ox"))
            .expect("publish event");

        let envelope = observed.try_recv().expect("event on the bus");
        assert_eq!(envelope.kind, "experimental.mcp.event");
        assert_eq!(
            envelope.data,
            json!({
                "server": "alpha",
                "name": "build",
                "eventId": "e1",
                "timestamp": "2026-11-24T00:00:00Z",
                "data": {
                    "untrusted": true,
                    "data": { "text": "hello\\u2028world" },
                },
            }),
            "the bus frame must force the untrusted marker and keep its wire shape"
        );
        assert_eq!(registry.cursor(&key).as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn mcp_webhook_hmac() {
        assert!(verify_signature(
            "key",
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
            b"The quick brown fox jumps over the lazy dog",
        ));

        let db = ox_db::Db::open_in_memory().await.expect("test database");
        let bus = EventBus::new(db);
        let mut observed = bus.subscribe();
        let registry = Registry::default();
        let key = SubscriptionKey::new("alpha", "build");
        let body = json!({
            "eventId": "e1",
            "name": "build",
            "timestamp": "2026-11-24T00:00:00Z",
            "data": { "text": "hello\u{2029}world" },
            "cursor": "c1",
        })
        .to_string();

        assert!(
            handle_webhook_payload(
                &registry,
                &bus,
                &key,
                "secret",
                "sha256=deadbeef",
                body.as_bytes(),
                None,
            )
            .is_err(),
            "a bad HMAC must be rejected"
        );
        assert!(
            observed.try_recv().is_err(),
            "a bad HMAC must not enter the event bus"
        );

        handle_webhook_payload(
            &registry,
            &bus,
            &key,
            "secret",
            &signature("secret", body.as_bytes()),
            body.as_bytes(),
            None,
        )
        .expect("valid signature delivers the event");

        let envelope = observed.try_recv().expect("event on the bus");
        assert_eq!(envelope.kind, "experimental.mcp.event");
        assert_eq!(envelope.data["data"]["untrusted"], json!(true));
        assert!(
            envelope.data["data"]["data"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("\\u2029"))
        );
        assert_eq!(registry.cursor(&key).as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn mcp_poll_replay() {
        let db = ox_db::Db::open_in_memory().await.expect("test database");
        let bus = EventBus::new(db);
        let mut observed = bus.subscribe();
        let registry = Registry::default();
        let key = SubscriptionKey::new("alpha", "build");

        registry
            .apply_poll(
                &bus,
                &key,
                PollPage {
                    events: vec![event("e1", "c1", "one"), event("e2", "c2", "two")],
                    next_cursor: None,
                },
                None,
            )
            .expect("first poll");

        // The connection dropped before the client confirmed c2: the server
        // replays from c1, and e1 must not be published twice.
        registry
            .apply_poll(
                &bus,
                &key,
                PollPage {
                    events: vec![
                        event("e1", "c1", "one"),
                        event("e2", "c2", "two"),
                        event("e3", "c3", "three"),
                    ],
                    next_cursor: Some("c3".to_owned()),
                },
                None,
            )
            .expect("replayed poll");

        let mut ids = Vec::new();
        while let Ok(envelope) = observed.try_recv() {
            assert_eq!(envelope.kind, "experimental.mcp.event");
            ids.push(envelope.data["eventId"].as_str().unwrap_or("?").to_owned());
        }
        assert_eq!(ids, ["e1", "e2", "e3"], "replay must not duplicate");
        assert_eq!(registry.cursor(&key).as_deref(), Some("c3"));
    }

    #[tokio::test]
    async fn mcp_subscription_reload() {
        let registry = Registry::default();
        let seen_tokens: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
        let launch = |_plan: Plan, token: tokio_util::sync::CancellationToken| {
            seen_tokens.lock().expect("token list").push(token.clone());
            tokio::spawn(async move { token.cancelled().await })
        };
        let plans = vec![
            Plan::new("alpha", "build", super::Delivery::Push, None),
            Plan::new("beta", "test", super::Delivery::Poll, None),
        ];
        registry.sync(plans, launch).await;
        assert_eq!(registry.len(), 2);

        registry.stop_all().await;
        assert_eq!(registry.len(), 0, "reload stops every old subscription");
        for token in seen_tokens.lock().expect("token list").iter() {
            assert!(token.is_cancelled(), "old subscription must be cancelled");
        }

        registry
            .sync(
                vec![Plan::new("alpha", "deploy", super::Delivery::Webhook, None)],
                launch,
            )
            .await;
        assert_eq!(registry.len(), 1, "new config rebuilds subscriptions");
        assert!(registry.contains(&SubscriptionKey::new("alpha", "deploy")));
        assert!(!registry.contains(&SubscriptionKey::new("alpha", "build")));
    }
}
