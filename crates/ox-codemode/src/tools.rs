//! Bridge from scanned tool paths to host capabilities.

/// One-shot reply from an asynchronous tool invocation. `Ok` carries the
/// tool's JSON result text; `Err` carries a host-facing failure reason.
pub type Completion = Box<dyn FnOnce(Result<String, String>) + Send + 'static>;

/// Lifecycle state of one nested tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    /// The invocation is about to start.
    Running,
    /// The invocation returned a result.
    Completed,
    /// The invocation failed.
    Error,
}

/// A borrowed lifecycle event. The host may record it, but the runtime keeps
/// ownership of the path and argument text.
#[derive(Debug, Clone, Copy)]
pub struct CallEvent<'a> {
    /// Scanned tool path, e.g. `ns.name`.
    pub path: &'a str,
    /// State of this invocation.
    pub status: CallStatus,
    /// JSON argument text, when available.
    pub input: Option<&'a str>,
}

/// Terminal record for one nested tool invocation, owned by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// Scanned tool path, e.g. `ns.name`.
    pub path: String,
    /// Final state of this invocation.
    pub status: CallStatus,
    /// JSON argument text, when available.
    pub input: Option<String>,
}

/// Invokes an already authorized tool. `path` is a static dot or string-key
/// bracket path from the scan and `args` is a JSON object; the implementation
/// must fail closed for anything else. The return value is a JSON string
/// passed back to the script.
pub trait ToolInvoker: Send + Sync {
    /// Call one tool and return its JSON result.
    fn invoke(&self, path: &str, args: &str) -> anyhow::Result<String>;

    /// Start one tool call and reply through `done` when it settles. The
    /// default keeps synchronous implementations working by calling `invoke`
    /// and completing immediately. An implementation overriding this method
    /// must call `done` exactly once; if it returns `Err` without calling
    /// `done`, the runtime supplies an error completion itself.
    fn start(&self, path: &str, args: &str, done: Completion) -> anyhow::Result<()> {
        match self.invoke(path, args) {
            Ok(result) => done(Ok(result)),
            Err(error) => done(Err(error.to_string())),
        }
        Ok(())
    }

    /// Observe a call's lifecycle. Hosts that only need `invoke` need not
    /// implement this; events are delivered before and after each invocation.
    fn on_call(&self, event: CallEvent<'_>) {
        let _ = event;
    }
}
