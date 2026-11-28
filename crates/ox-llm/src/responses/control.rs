//! Control-plane calls around one Responses endpoint.
//!
//! Everything here is relative to the configured `base_url`: an operator may
//! spell that URL with or without `/v1`, so the API root is derived once and
//! every lifecycle path is appended to it. Non-2xx statuses become
//! [`ChatError::Http`] — a missing/cancelled response is never translated into
//! an empty success. No control call mutates local session state; the caller
//! decides what a retrieved or cancelled response means.

use std::collections::BTreeMap;

use reqwest::{Client, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ChatError;

/// A returned Responses object, keeping protocol fields ox does not interpret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseResource {
    /// Provider response id.
    pub id: String,
    /// Provider state (`queued`, `in_progress`, `completed`, …).
    #[serde(default)]
    pub status: String,
    /// Output items, kept verbatim: typed output is a wire concern.
    #[serde(default)]
    pub output: Vec<Value>,
    /// Complete output text when the endpoint supplied it.
    #[serde(default)]
    pub output_text: Option<String>,
    /// Usage object when reported.
    #[serde(default)]
    pub usage: Option<Value>,
    /// Failure object when the response failed.
    #[serde(default)]
    pub error: Option<Value>,
    /// Fields added by the provider after this struct was written.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A deletion acknowledgement.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Deleted {
    /// Id named by the request.
    pub id: String,
    /// Whether the provider actually removed the object.
    #[serde(default)]
    pub deleted: bool,
    /// Provider object tag, when present.
    #[serde(default)]
    pub object: Option<String>,
}

/// A cursor page of input items, conversation items or conversations.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Page<T> {
    /// Objects in this page.
    #[serde(default)]
    pub data: Vec<T>,
    /// Cursor of the first object.
    #[serde(default)]
    pub first_id: Option<String>,
    /// Cursor of the last object.
    #[serde(default)]
    pub last_id: Option<String>,
    /// Whether another page exists.
    #[serde(default)]
    pub has_more: bool,
}

/// Token accounting from `/responses/count_tokens`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TokenCount {
    /// Tokens the provider would charge for the input.
    pub input_tokens: u64,
    /// Provider-specific breakdown, when reported.
    #[serde(default)]
    pub input_tokens_details: Option<Value>,
    /// Fields added by the provider after this struct was written.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A conversation object.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Conversation {
    /// Conversation id.
    pub id: String,
    /// Creation timestamp, when reported.
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Opaque client metadata.
    #[serde(default)]
    pub metadata: Option<Value>,
    /// Fields added by the provider after this struct was written.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A compacted response. The endpoint may return a response object directly or
/// wrap it in `response`; both shapes expose the same output items.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CompactResult {
    /// Compacted output items.
    #[serde(default)]
    pub output: Vec<Value>,
    /// Full response object when the endpoint wrapped one.
    #[serde(default)]
    pub response: Option<Box<ResponseResource>>,
    /// The response id when the endpoint returned one at the top level.
    #[serde(default)]
    pub id: Option<String>,
}

/// Cursor/order filters shared by the list endpoints. `None` means "omit the
/// query key", so an empty value is never sent as an implicit filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListParams<'a> {
    /// Return items after this cursor.
    pub after: Option<&'a str>,
    /// Return items before this cursor.
    pub before: Option<&'a str>,
    /// Maximum number of items.
    pub limit: Option<u32>,
    /// Sort order (`asc` or `desc`).
    pub order: Option<&'a str>,
}

impl<'a> ListParams<'a> {
    /// Set the page size.
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set the cursor to continue after.
    #[must_use]
    pub fn with_after(mut self, after: &'a str) -> Self {
        self.after = Some(after);
        self
    }

    fn pairs(self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(after) = self.after {
            pairs.push(("after", after.to_owned()));
        }
        if let Some(before) = self.before {
            pairs.push(("before", before.to_owned()));
        }
        if let Some(limit) = self.limit {
            pairs.push(("limit", limit.to_string()));
        }
        if let Some(order) = self.order {
            pairs.push(("order", order.to_owned()));
        }
        pairs
    }
}

/// The control plane for one configured Responses endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ResponsesControl<'a> {
    client: &'a Client,
    base_url: &'a str,
    api_key: Option<&'a str>,
}

impl<'a> ResponsesControl<'a> {
    /// Borrow the process HTTP client and one endpoint's configuration.
    #[must_use]
    pub fn new(client: &'a Client, base_url: &'a str, api_key: Option<&'a str>) -> Self {
        Self {
            client,
            base_url,
            api_key,
        }
    }

    /// The `/v1` prefix, derived from `base_url` without duplicating it.
    fn root(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        match trimmed.strip_suffix("/v1/responses") {
            Some(prefix) => format!("{prefix}/v1"),
            None if trimmed.ends_with("/v1") => trimmed.to_owned(),
            None => format!("{trimmed}/v1"),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.root()));
        if let Some(key) = self.api_key {
            request = request.bearer_auth(key);
        }
        request
    }

    /// Retrieve one response object.
    pub async fn retrieve(&self, id: &str) -> Result<ResponseResource, ChatError> {
        let request = self.request(reqwest::Method::GET, &format!("/responses/{}", segment(id)));
        send_json(request).await
    }

    /// Create a response without streaming, for background-only tiers.
    pub async fn create(&self, body: &Value) -> Result<ResponseResource, ChatError> {
        let request = self.request(reqwest::Method::POST, "/responses");
        send_json(request.json(body)).await
    }

    /// Ask the provider to cancel a background response.
    pub async fn cancel(&self, id: &str) -> Result<ResponseResource, ChatError> {
        let request = self.request(
            reqwest::Method::POST,
            &format!("/responses/{}/cancel", segment(id)),
        );
        send_json(request).await
    }

    /// Delete a stored response.
    pub async fn delete(&self, id: &str) -> Result<Deleted, ChatError> {
        let request = self.request(
            reqwest::Method::DELETE,
            &format!("/responses/{}", segment(id)),
        );
        send_json(request).await
    }

    /// Page through the input items of one response.
    pub async fn list_input_items(
        &self,
        id: &str,
        params: &ListParams<'_>,
    ) -> Result<Page<Value>, ChatError> {
        let path = with_query(
            &format!("/responses/{}/input_items", segment(id)),
            &params.pairs(),
        );
        send_json(self.request(reqwest::Method::GET, &path)).await
    }

    /// Count tokens for a would-be Responses create body.
    pub async fn count_tokens(&self, body: &Value) -> Result<TokenCount, ChatError> {
        let request = self.request(reqwest::Method::POST, "/responses/count_tokens");
        send_json(request.json(body)).await
    }

    /// Compact a context into a new response object.
    pub async fn compact(&self, body: &Value) -> Result<CompactResult, ChatError> {
        let request = self.request(reqwest::Method::POST, "/responses/compact");
        send_json(request.json(body)).await
    }

    /// Open an SSE replay from `starting_after` for a disconnected background
    /// response. The single reconnect is issued here; callers must not loop.
    pub async fn resume_stream(
        &self,
        id: &str,
        starting_after: u64,
    ) -> Result<reqwest::Response, ChatError> {
        let path = with_query(
            &format!("/responses/{}", segment(id)),
            &[
                ("stream", "true".to_owned()),
                ("starting_after", starting_after.to_string()),
            ],
        );
        let request = self.request(reqwest::Method::GET, &path);
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ChatError::from_response(response).await);
        }
        Ok(response)
    }

    /// Create a conversation.
    pub async fn create_conversation(&self, body: &Value) -> Result<Conversation, ChatError> {
        let request = self.request(reqwest::Method::POST, "/conversations");
        send_json(request.json(body)).await
    }

    /// Retrieve one conversation.
    pub async fn get_conversation(&self, id: &str) -> Result<Conversation, ChatError> {
        let request = self.request(
            reqwest::Method::GET,
            &format!("/conversations/{}", segment(id)),
        );
        send_json(request).await
    }

    /// Update a conversation's metadata.
    pub async fn update_conversation(
        &self,
        id: &str,
        body: &Value,
    ) -> Result<Conversation, ChatError> {
        let request = self.request(
            reqwest::Method::POST,
            &format!("/conversations/{}", segment(id)),
        );
        send_json(request.json(body)).await
    }

    /// Delete a conversation.
    pub async fn delete_conversation(&self, id: &str) -> Result<Deleted, ChatError> {
        let request = self.request(
            reqwest::Method::DELETE,
            &format!("/conversations/{}", segment(id)),
        );
        send_json(request).await
    }

    /// Page through conversations.
    pub async fn list_conversations(
        &self,
        params: &ListParams<'_>,
    ) -> Result<Page<Conversation>, ChatError> {
        let path = with_query("/conversations", &params.pairs());
        send_json(self.request(reqwest::Method::GET, &path)).await
    }

    /// Append an item to a conversation.
    pub async fn create_conversation_item(
        &self,
        conversation_id: &str,
        body: &Value,
    ) -> Result<Value, ChatError> {
        let request = self.request(
            reqwest::Method::POST,
            &format!("/conversations/{}/items", segment(conversation_id)),
        );
        send_json(request.json(body)).await
    }

    /// Page through a conversation's items.
    pub async fn list_conversation_items(
        &self,
        conversation_id: &str,
        params: &ListParams<'_>,
    ) -> Result<Page<Value>, ChatError> {
        let path = with_query(
            &format!("/conversations/{}/items", segment(conversation_id)),
            &params.pairs(),
        );
        send_json(self.request(reqwest::Method::GET, &path)).await
    }

    /// Retrieve one conversation item.
    pub async fn get_conversation_item(
        &self,
        conversation_id: &str,
        item_id: &str,
    ) -> Result<Value, ChatError> {
        let request = self.request(
            reqwest::Method::GET,
            &format!(
                "/conversations/{}/items/{}",
                segment(conversation_id),
                segment(item_id)
            ),
        );
        send_json(request).await
    }

    /// Delete one conversation item.
    pub async fn delete_conversation_item(
        &self,
        conversation_id: &str,
        item_id: &str,
    ) -> Result<Deleted, ChatError> {
        let request = self.request(
            reqwest::Method::DELETE,
            &format!(
                "/conversations/{}/items/{}",
                segment(conversation_id),
                segment(item_id)
            ),
        );
        send_json(request).await
    }
}

/// Decode one successful JSON response; a provider that changed its payload
/// shape fails explicitly instead of masquerading as an empty object.
async fn send_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, ChatError> {
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(ChatError::from_response(response).await);
    }
    response
        .json::<T>()
        .await
        .map_err(|error| ChatError::Upstream(format!("invalid Responses control payload: {error}")))
}

/// Percent-encode one path segment so provider ids cannot alter the route.
fn segment(id: &str) -> String {
    encode(id)
}

/// Append query pairs. `reqwest`'s `query` feature is not enabled in this
/// workspace, so the short cursor/flag query is built here instead.
fn with_query(path: &str, pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return path.to_owned();
    }
    let query = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{query}")
}

fn encode(id: &str) -> String {
    let mut encoded = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{ResponsesControl, segment};

    #[test]
    fn api_root_does_not_duplicate_v1() {
        let client = reqwest::Client::new();
        let control = ResponsesControl::new(&client, "https://example.com/v1/", None);
        assert_eq!(control.root(), "https://example.com/v1");
        let control = ResponsesControl::new(&client, "https://example.com/v1/responses", None);
        assert_eq!(control.root(), "https://example.com/v1");
        let control = ResponsesControl::new(&client, "https://example.com", None);
        assert_eq!(control.root(), "https://example.com/v1");
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(segment("resp_1:2"), "resp_1:2");
        assert_eq!(segment("a/b?c"), "a%2Fb%3Fc");
    }
}
