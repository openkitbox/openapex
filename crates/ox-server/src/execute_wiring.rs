//! Composition of the nested-tool host used by `execute`.
//!
//! `ox-engine` owns permission checks and tool execution; `ox-server` owns the
//! live event bus and the in-memory overlay for running tool parts. This module
//! is the only place those halves meet: the engine host is never given a bus,
//! and the tool never sees the gate.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ox_core::event_bus::EventBus;
use ox_schema::event::SessionEventData;
use ox_schema::support::Record;
use ox_tools::contract::{ExecuteHost, NestedCallContext, ToolRegistry};
use ox_tools::live_tool_metadata::LiveToolMetadata;

/// The registry's host: permission decisions delegate to `nested`, progress
/// goes to the process-wide bus and live-metadata overlay.
pub(crate) struct ExecuteWiring {
    nested: Arc<dyn ExecuteHost>,
    bus: EventBus,
    live: Arc<LiveToolMetadata>,
    handle: tokio::runtime::Handle,
}

impl ExecuteWiring {
    fn new(
        nested: Arc<dyn ExecuteHost>,
        bus: EventBus,
        live: Arc<LiveToolMetadata>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            nested,
            bus,
            live,
            handle,
        }
    }
}

/// Replace the engine-only host (installed by `Runner::new`) with the composite
/// one, after the runner exists. The shared slot means derived registries used
/// by later service generations inherit this host too.
pub(crate) fn install(tools: &ToolRegistry, nested: Arc<dyn ExecuteHost>, bus: EventBus) {
    let live = Arc::clone(&tools.live_tool_metadata);
    let host = Arc::new(ExecuteWiring::new(
        nested,
        bus,
        live,
        tokio::runtime::Handle::current(),
    ));
    tools.set_execute_host(host);
}

impl ExecuteHost for ExecuteWiring {
    fn run_nested<'a>(
        &'a self,
        ctx: NestedCallContext<'a>,
        path: &'a str,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        // One authority: never re-implement or loosen the runner's gate here.
        self.nested.run_nested(ctx, path, input)
    }

    fn publish_progress(
        &self,
        session_id: &str,
        assistant_message_id: &str,
        call_id: &str,
        metadata: serde_json::Value,
    ) {
        let record = match serde_json::from_value::<Record>(metadata) {
            Ok(record) => {
                self.live
                    .record(assistant_message_id, call_id, record.clone());
                Some(record)
            }
            Err(error) => {
                tracing::warn!(%error, call_id, "execute progress metadata is not an object; skipping live overlay");
                None
            }
        };
        let progress = SessionEventData::ToolProgress {
            session_id: session_id.to_owned(),
            assistant_message_id: assistant_message_id.to_owned(),
            id: call_id.to_owned(),
            metadata: record.unwrap_or_default(),
        };
        let bus = self.bus.clone();
        // Called from the QuickJS spawn_blocking thread: block_on enters the
        // runtime context and drives this future on that thread, so no Tokio
        // worker is blocked and the QuickJS VM is not re-entered.
        if let Err(error) = self.handle.block_on(bus.publish(progress)) {
            tracing::warn!(%error, call_id, "execute progress failed to publish");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use ox_db::Db;
    use ox_schema::event::SessionEventData;
    use ox_schema::message::Message;

    use super::*;

    struct StubHost {
        seen: Mutex<Vec<(String, String)>>,
    }

    impl StubHost {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }
    }

    impl ExecuteHost for StubHost {
        fn run_nested<'a>(
            &'a self,
            ctx: NestedCallContext<'a>,
            path: &'a str,
            input: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            self.seen
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((path.to_owned(), input.to_owned()));
            let parent = ctx.parent_call_id.to_owned();
            Box::pin(async move { Ok(format!("ran {path} under {parent}")) })
        }

        fn publish_progress(
            &self,
            _session_id: &str,
            _assistant_message_id: &str,
            _call_id: &str,
            _metadata: Value,
        ) {
        }
    }

    async fn wiring() -> (ExecuteWiring, Db, Arc<LiveToolMetadata>, Arc<StubHost>) {
        let db = Db::open_in_memory().await.expect("test database opens");
        let live = Arc::new(LiveToolMetadata::new());
        let nested = Arc::new(StubHost::new());
        let engine: Arc<dyn ExecuteHost> = nested.clone();
        let host = ExecuteWiring::new(
            engine,
            EventBus::new(db.clone()),
            Arc::clone(&live),
            tokio::runtime::Handle::current(),
        );
        (host, db, live, nested)
    }

    #[tokio::test]
    async fn run_nested_delegates_to_the_engine_host() {
        let (host, _db, _live, nested) = wiring().await;
        let directory = PathBuf::from("/tmp");
        let ctx = NestedCallContext {
            session_id: "ses_test",
            directory: &directory,
            assistant_message_id: "msg_test",
            parent_call_id: "call_parent",
            cancellation: CancellationToken::new(),
        };
        let output = host
            .run_nested(ctx, "probe", "{\"n\":1}")
            .await
            .expect("delegated call succeeds");
        assert_eq!(output, "ran probe under call_parent");
        assert_eq!(
            nested.seen(),
            [("probe".to_owned(), "{\"n\":1}".to_owned())],
            "the host forwards the original path and input"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_replaces_the_host_for_derived_registries() {
        let db = Db::open_in_memory().await.expect("test database opens");
        let bus = EventBus::new(db.clone());
        let mut events = bus.subscribe();
        let registry = ToolRegistry::new();
        let old: Arc<dyn ExecuteHost> = Arc::new(StubHost::new());
        registry.set_execute_host(old);
        let engine: Arc<dyn ExecuteHost> = Arc::new(StubHost::new());
        install(&registry, engine, bus);

        let derived = registry.without(&[]);
        let host = derived.execute_host().expect("derived registry has a host");
        tokio::task::spawn_blocking(move || {
            host.publish_progress("ses_test", "msg_test", "call_test", json!({}));
        })
        .await
        .expect("publisher thread completes");
        let envelope = events
            .recv()
            .await
            .expect("installed host publishes progress");
        assert_eq!(envelope.kind, "session.tool.progress");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_is_ephemeral_and_overlays_running_metadata() {
        let (host, db, live, _nested) = wiring().await;
        let mut events = host.bus.subscribe();
        let metadata = json!({ "toolCalls": [{ "tool": "probe", "status": "running" }] });
        let publisher = Arc::new(host);
        let thread_publisher = Arc::clone(&publisher);
        tokio::task::spawn_blocking(move || {
            thread_publisher.publish_progress("ses_test", "msg_test", "call_test", metadata);
        })
        .await
        .expect("publisher thread completes");

        let envelope = events.recv().await.expect("progress event is broadcast");
        assert_eq!(envelope.kind, "session.tool.progress");
        assert!(envelope.durable.is_none(), "progress is not sequenced");
        let typed = envelope.parse_typed();
        let SessionEventData::ToolProgress {
            session_id,
            assistant_message_id,
            id,
            metadata: record,
        } = typed.data
        else {
            panic!("unexpected event data: {:?}", typed.data);
        };
        assert_eq!(session_id, "ses_test");
        assert_eq!(assistant_message_id, "msg_test");
        assert_eq!(id, "call_test");
        assert_eq!(record["toolCalls"][0]["status"], "running");

        let mut messages: Vec<Message> = vec![
            serde_json::from_value(json!({
                "type": "assistant",
                "id": "msg_test",
                "time": { "created": 0 },
                "agent": "apex",
                "model": { "id": "model", "providerID": "provider" },
                "content": [{
                    "type": "tool",
                    "id": "call_test",
                    "name": "execute",
                    "state": { "status": "running", "input": {}, "metadata": {} },
                    "time": { "created": 0 }
                }]
            }))
            .expect("running tool message decodes"),
        ];
        live.overlay(&mut messages);
        let Message::Assistant(assistant) = &messages[0] else {
            panic!("assistant message stays assistant");
        };
        let ox_schema::message::AssistantContent::Tool { state, .. } = &assistant.content[0] else {
            panic!("tool content stays a tool call");
        };
        let ox_schema::message::ToolState::Running {
            metadata: shown, ..
        } = &**state
        else {
            panic!("tool call is still running");
        };
        assert_eq!(
            shown["toolCalls"][0]["tool"], "probe",
            "the live overlay carries the progress row"
        );

        let conn = db.acquire().await.expect("database connection");
        let mut statement = conn
            .prepare("SELECT count(*) FROM event")
            .await
            .expect("count statement");
        let mut rows = statement.query(()).await.expect("count query");
        let count = rows
            .next()
            .await
            .expect("count row")
            .expect("count value")
            .get::<i64>(0)
            .expect("count number");
        assert_eq!(count, 0, "no durable event row was written");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_object_metadata_publishes_without_overlay() {
        let (host, _db, live, _nested) = wiring().await;
        let mut events = host.bus.subscribe();
        let publisher = Arc::new(host);
        let thread_publisher = Arc::clone(&publisher);
        tokio::task::spawn_blocking(move || {
            thread_publisher.publish_progress("ses_test", "msg_test", "call_test", json!(42));
        })
        .await
        .expect("publisher thread completes");

        let envelope = events.recv().await.expect("progress event is broadcast");
        assert_eq!(envelope.kind, "session.tool.progress");
        let mut messages: Vec<Message> = vec![
            serde_json::from_value(json!({
                "type": "assistant",
                "id": "msg_test",
                "time": { "created": 0 },
                "agent": "apex",
                "model": { "id": "model", "providerID": "provider" },
                "content": [{
                    "type": "tool",
                    "id": "call_test",
                    "name": "execute",
                    "state": { "status": "running", "input": {}, "metadata": {} },
                    "time": { "created": 0 }
                }]
            }))
            .expect("running tool message decodes"),
        ];
        live.overlay(&mut messages);
        let Message::Assistant(assistant) = &messages[0] else {
            panic!("assistant message stays assistant");
        };
        let ox_schema::message::AssistantContent::Tool { state, .. } = &assistant.content[0] else {
            panic!("tool content stays a tool call");
        };
        let ox_schema::message::ToolState::Running {
            metadata: shown, ..
        } = &**state
        else {
            panic!("tool call is still running");
        };
        assert!(shown.is_empty(), "bad metadata never reaches the overlay");
    }
}
