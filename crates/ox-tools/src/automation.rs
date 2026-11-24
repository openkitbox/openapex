//! Built-in automation tools. The domain service is a host port: `ox-tools`
//! owns the tool contract and the wire shapes, `ox-server` owns the automation
//! table and the project/session checks behind it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::contract::{Tool, ToolContext, ToolOutput};
use ox_schema::wire::ToolDef;

/// One caller-scoped automation operation. Every method receives the calling
/// session id; the host must derive the project from it and reject a cross-
/// project id rather than letting one session manage another project's jobs.
pub trait AutomationService: Send + Sync {
    fn create<'a>(
        &'a self,
        session_id: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
    fn list<'a>(
        &'a self,
        session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
    fn pause<'a>(
        &'a self,
        session_id: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
    fn resume<'a>(
        &'a self,
        session_id: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
    fn delete<'a>(
        &'a self,
        session_id: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;
}

/// Register the five tools on a registry. Wiring-time only: the service owns a
/// database handle that does not exist when `with_builtins` runs.
pub fn register(registry: &mut crate::contract::ToolRegistry, service: Arc<dyn AutomationService>) {
    let tools: Vec<Arc<dyn Tool>> = Operation::ALL
        .iter()
        .map(|operation| {
            Arc::new(AutomationTool {
                operation: *operation,
                service: Arc::clone(&service),
            }) as Arc<dyn Tool>
        })
        .collect();
    registry.register_all(tools);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Create,
    List,
    Pause,
    Resume,
    Delete,
}

impl Operation {
    const ALL: &'static [Self] = &[
        Self::Create,
        Self::List,
        Self::Pause,
        Self::Resume,
        Self::Delete,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Create => "automation.create",
            Self::List => "automation.list",
            Self::Pause => "automation.pause",
            Self::Resume => "automation.resume",
            Self::Delete => "automation.delete",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Create => {
                "Create an automation that runs a prompt, a Code Mode script, or a shell command once at a set time, on an interval, on a cron, or when an MCP server sends an event."
            }
            Self::List => {
                "List this project's automations with their status and next run, and the current time."
            }
            Self::Pause => {
                "Pause an automation. It keeps its settings and does not run until resumed."
            }
            Self::Resume => {
                "Resume a paused automation. A missed `at` runs once; repeating timers resume at their next slot. Event watches request replay from their saved cursor."
            }
            Self::Delete => "Delete an automation for good.",
        }
    }

    fn schema(self) -> Value {
        match self {
            Self::Create => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "trigger": { "type": "object" },
                    "action": { "type": "object" },
                    "expires": { "type": "string" }
                },
                "required": ["name", "trigger", "action"]
            }),
            Self::List => json!({ "type": "object", "properties": {} }),
            Self::Pause | Self::Resume | Self::Delete => json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        }
    }
}

struct AutomationTool {
    operation: Operation,
    service: Arc<dyn AutomationService>,
}

impl Tool for AutomationTool {
    fn name(&self) -> &str {
        self.operation.name()
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Use automation tools for scheduled, recurring and event-driven work instead of sleeping or polling in a turn.",
        )
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            self.operation.name(),
            self.operation.description(),
            self.operation.schema(),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move {
            let session = ctx.session_id.as_str();
            let result = match self.operation {
                Operation::Create => self.service.create(session, &input).await,
                Operation::List => self.service.list(session).await,
                Operation::Pause | Operation::Resume | Operation::Delete => {
                    let Some(id) = input.get("id").and_then(Value::as_str) else {
                        return crate::fail("automation id is required");
                    };
                    if self.operation == Operation::Pause {
                        self.service.pause(session, id).await
                    } else if self.operation == Operation::Resume {
                        self.service.resume(session, id).await
                    } else {
                        self.service.delete(session, id).await
                    }
                }
            };
            match result {
                Ok(content) => ToolOutput {
                    content,
                    error: None,
                    metadata: None,
                    attachments: None,
                },
                Err(error) => crate::fail(error.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::contract::ToolRegistry;

    struct RecordingService {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl AutomationService for RecordingService {
        fn create<'a>(
            &'a self,
            session_id: &'a str,
            input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            self.calls
                .lock()
                .unwrap()
                .push(("create".into(), session_id.into()));
            let value = input.clone();
            Box::pin(async move { Ok(value) })
        }

        fn list<'a>(
            &'a self,
            session_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            self.calls
                .lock()
                .unwrap()
                .push(("list".into(), session_id.into()));
            let value = json!({"now": "now", "automations": []});
            Box::pin(async move { Ok(value) })
        }

        fn pause<'a>(
            &'a self,
            session_id: &'a str,
            id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            let id = id.to_owned();
            self.calls
                .lock()
                .unwrap()
                .push(("pause".into(), session_id.into()));
            Box::pin(async move { Ok(json!({"id": id})) })
        }

        fn resume<'a>(
            &'a self,
            session_id: &'a str,
            id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            let id = id.to_owned();
            self.calls
                .lock()
                .unwrap()
                .push(("resume".into(), session_id.into()));
            Box::pin(async move { Ok(json!({"id": id})) })
        }

        fn delete<'a>(
            &'a self,
            session_id: &'a str,
            id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
            let id = id.to_owned();
            self.calls
                .lock()
                .unwrap()
                .push(("delete".into(), session_id.into()));
            Box::pin(async move { Ok(json!({"id": id, "removed": true})) })
        }
    }

    #[tokio::test]
    async fn automation_tools_are_named_and_scoped_to_the_caller() {
        let mut registry = ToolRegistry::new();
        let service = Arc::new(RecordingService {
            calls: Mutex::new(Vec::new()),
        });
        register(&mut registry, service.clone());

        for name in [
            "automation.create",
            "automation.list",
            "automation.pause",
            "automation.resume",
            "automation.delete",
        ] {
            assert!(registry.get(name).is_some(), "{name} must be registered");
        }
        let names: Vec<String> = registry
            .definitions()
            .iter()
            .map(|definition| definition.function.name.clone())
            .collect();
        assert_eq!(
            names.len(),
            5,
            "automation tools must not shadow other tools"
        );

        let tool = registry
            .get("automation.list")
            .expect("registered list tool");
        let ctx = ToolContext {
            session_id: "ses_caller".into(),
            directory: std::env::temp_dir(),
            assistant_message_id: "msg_1".into(),
            call_id: "call_1".into(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            registry: Arc::new(ToolRegistry::new()),
        };
        let output = tool.execute(ctx, json!({})).await;
        assert!(
            output.error.is_none(),
            "a scoped list succeeds: {:?}",
            output.error
        );
        assert_eq!(
            service.calls.lock().unwrap().as_slice(),
            &[("list".into(), "ses_caller".into())]
        );
    }
}
