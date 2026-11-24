//! Pure mapping for stable session updates whose source data is trustworthy.

use agent_client_protocol::v1::schema::{SessionUpdate, UsageUpdate};
use serde_json::Value;

/// Convert usage only when both current context usage and window size are
/// supplied by the event. Token counters alone are not a context-window size.
pub(crate) fn usage(data: &Value) -> Option<SessionUpdate> {
    let used = data.get("used").and_then(Value::as_u64)?;
    let size = data.get("size").and_then(Value::as_u64)?;
    if size == 0 || used > size {
        return None;
    }
    Some(SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)))
}
