//! Code Mode: static scan plus a resource-limited JavaScript runtime.
//!
//! The scanner is the authority for which tools exist in a run. The runtime
//! builds a fresh QuickJS world per execution and injects only scanned paths,
//! so an unscanned tool is not merely denied — it is absent.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod limits;
pub mod runtime;
pub mod scan;
pub mod tools;

pub use limits::Limits;
pub use runtime::{CodeOutput, ExecutionError, execute, execute_scoped};
pub use scan::{PermissionAction, Scan};
pub use tools::ToolInvoker;

/// One optional hand-off from a code program. Mirrors the automation domain
/// shape without making this crate depend on the automation crate (that
/// dependency would be a cycle once the code action executor is added).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSend {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<CodeSendTarget>,
    pub prompt: String,
}

/// A code program may only address a named subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSendTarget {
    pub subagent: String,
}

/// The strict return shape of a code program.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send: Option<CodeSend>,
}

impl CodeResult {
    /// Decode a JS return value, rejecting unknown keys and empty sends.
    /// `null` and `undefined` mean "do nothing".
    pub fn decode(value: &Value) -> anyhow::Result<Option<Self>> {
        if value.is_null() {
            return Ok(None);
        }
        let result: Self = serde_json::from_value(value.clone())?;
        if let Some(send) = &result.send {
            anyhow::ensure!(
                !send.prompt.trim().is_empty(),
                "send.prompt must not be empty"
            );
            if let Some(to) = &send.to {
                anyhow::ensure!(
                    !to.subagent.trim().is_empty(),
                    "send.to.subagent must not be empty"
                );
            }
        }
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::CodeResult;

    #[test]
    fn code_result_decodes_strictly() {
        assert!(
            CodeResult::decode(&serde_json::json!({"done": true, "surprise": 1})).is_err(),
            "unknown keys must not be dropped from a code result"
        );
        assert!(
            CodeResult::decode(&serde_json::json!({"send": {"prompt": ""}})).is_err(),
            "an empty send prompt must be rejected"
        );
        let result = CodeResult::decode(&serde_json::json!({"done": true}))
            .expect("decode")
            .expect("non-null value");
        assert_eq!(result.done, Some(true));
    }
}
