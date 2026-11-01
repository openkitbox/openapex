use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Upstream `DefaultSessionsLimit` / `DefaultMessagesLimit`.
const DEFAULT_PAGE_LIMIT: i64 = 50;

// ---------------------------------------------------------------------------
// Pagination cursors
// ---------------------------------------------------------------------------

/// Upstream `Session.ListAnchor` direction.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Direction {
    Previous,
    Next,
}

/// The request-shaped subset stored inside an opaque cursor. Field names match
/// the query parameters so a cursor round-trips verbatim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct CursorQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) search: Option<String>,
    #[serde(default, rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub(super) parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) archived: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Anchor {
    pub(super) id: String,
    pub(super) time: i64,
    pub(super) direction: Direction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SessionsCursor {
    pub(super) query: CursorQuery,
    pub(super) anchor: Anchor,
}

/// The message-list cursor (`{ id, order, direction }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MessagesCursor {
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) order: Option<String>,
    pub(super) direction: Direction,
}

pub(super) fn encode_cursor<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_vec(value)
        .ok()
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn decode_cursor<T: DeserializeOwned>(raw: &str) -> Option<T> {
    URL_SAFE_NO_PAD
        .decode(raw)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Effective ascending order: "previous" walks the opposite way.
pub(super) fn ascending_of(order: Option<&str>, direction: Direction) -> bool {
    let requested_asc = order == Some("asc");
    match direction {
        Direction::Previous => !requested_asc,
        Direction::Next => requested_asc,
    }
}

/// `PositiveInt` limit: a positive integer, defaulting to the page size.
pub(super) fn resolve_limit(raw: Option<String>) -> Result<i64, String> {
    match raw {
        None => Ok(DEFAULT_PAGE_LIMIT),
        Some(value) => match value.parse::<i64>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("Invalid limit: {value}")),
        },
    }
}

/// `archived` list filter: `true` returns only archived sessions, `false` only
/// live ones, absent returns both (upstream `store.ts:116` adds the condition
/// only when the parameter is defined). Anything else is rejected rather than
/// silently treated as absent.
pub(super) fn resolve_archived(raw: Option<String>) -> Result<Option<bool>, String> {
    match raw.as_deref() {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(value) => Err(format!("Invalid archived: {value}")),
    }
}

pub(super) fn resolve_order(raw: Option<String>) -> Result<Option<String>, String> {
    match raw.as_deref() {
        None => Ok(None),
        Some("asc") => Ok(Some("asc".to_owned())),
        Some("desc") => Ok(Some("desc".to_owned())),
        Some(other) => Err(format!("Invalid order: {other}")),
    }
}
