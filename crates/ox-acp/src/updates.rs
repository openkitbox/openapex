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

#[cfg(test)]
mod tests {
    use super::usage;
    use serde_json::json;

    #[test]
    fn usage_requires_real_window_and_rejects_impossible_values() {
        assert!(usage(&json!({"input": 12, "output": 3})).is_none());
        assert!(usage(&json!({"contextUsed": 8, "contextSize": 16})).is_none());
        assert!(usage(&json!({"used": 8, "size": 0})).is_none());
        assert!(usage(&json!({"used": 9, "size": 8})).is_none());
        assert!(usage(&json!({"used": 8, "size": 16})).is_some());
    }
}
