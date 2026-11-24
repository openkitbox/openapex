//! Bridge from scanned tool paths to host capabilities.

/// Invokes an already authorized tool. `path` is a static dot path from the
/// scan and `args` is a JSON object; the implementation must fail closed for
/// anything else. The return value is a JSON string passed back to the script.
pub trait ToolInvoker: Send + Sync {
    /// Call one tool and return its JSON result.
    fn invoke(&self, path: &str, args: &str) -> anyhow::Result<String>;
}
