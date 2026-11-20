use serde_json::Value;

use crate::runner::ToolOutput;
use ox_tools::ok_text;

/// Parse an `ids` / `note_ids` argument into canonical decimal-id strings.
/// Accepts an integer, a digit string, or an array of either. Errors are
/// the model-facing text.
pub(super) fn parse_int_ids(
    value: Option<&Value>,
    field: &str,
    max: usize,
) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Err(format!("Error: '{field}' must be provided."));
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    if items.is_empty() {
        return Err(format!("Error: '{field}' must not be empty."));
    }
    if items.len() > max {
        return Err(format!(
            "Error: '{field}' takes at most {max} ids (got {}).",
            items.len()
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let parsed = match item {
            Value::Number(number) => number.as_i64(),
            Value::String(text) => text.trim().parse::<i64>().ok(),
            _ => None,
        };
        match parsed {
            Some(id) if id > 0 => out.push(id.to_string()),
            _ => {
                return Err(format!(
                    "Error: '{field}' must contain positive integer ids (got {item})."
                ));
            }
        }
    }
    Ok(out)
}

/// A non-empty required string argument, already trimmed.
pub(super) fn required_text(input: &Value, field: &str) -> Result<String, String> {
    match input.get(field) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
        _ => Err(format!("Error: '{field}' must be provided.")),
    }
}

/// An optional non-empty string argument, already trimmed.
pub(super) fn optional_text(input: &Value, field: &str) -> Result<Option<String>, String> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim().to_owned())),
        Some(other) => Err(format!("Error: '{field}' must be a string (got {other}).")),
    }
}

/// Log an internal failure and render it for the model (never a panic, never
/// a silent no-op). Shared with [`crate::mctx::search`], which reports the
/// same way.
pub(crate) fn internal_error(what: &str, error: &anyhow::Error) -> ToolOutput {
    // The operator gets the whole chain (and its context) from the log; the
    // model gets the root cause, not ox's internal call path.
    tracing::error!(error = %error, "mctx {what} failed");
    ok_text(format!("Error: {what} failed: {}", error.root_cause()))
}
