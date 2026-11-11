//! Mockable streaming-chat port over [`CardinalClient::stream_chat`].
//!
//! The runner depends on this object-safe trait (`Arc<dyn Chat>`) instead of
//! the concrete client so tests can substitute a scripted transport. The
//! stream itself is boxed: the client's `ChatStream` owns a live
//! `reqwest::Response` and cannot be fabricated offline, so the trait
//! returns a pinned `dyn Stream` with the same
//! `Result<StreamEvent, ChatError>` item type. The real client boxes
//! its `ChatStream` unchanged.
//!
//! The trait keeps provider selection outside the wire request. The optional
//! provider-aware method is the routing seam for hidden requests and defaults
//! to the existing request-only behavior for transports that have no routing.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;

use super::ChatError;
use super::client::CardinalClient;
use super::stream::StreamEvent;
use super::wire::ChatRequest;

/// The erased stream every [`Chat`] implementation hands back.
pub type BoxChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ChatError>> + Send>>;

/// The boxed future [`Chat::stream_chat`] returns (async-fn-in-trait is not
/// dyn-compatible).
pub type ChatFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BoxChatStream, ChatError>> + Send + 'a>>;

/// The streaming chat-completions seam. See the [module docs](self).
pub trait Chat: Send + Sync {
    /// Start a streaming chat completion for `request`.
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a>;

    /// Start a request while selecting an internal provider without adding a
    /// provider field to the public wire request shape.
    fn stream_chat_for<'a>(
        &'a self,
        request: &'a ChatRequest,
        provider: Option<&'a str>,
    ) -> ChatFuture<'a> {
        let _ = provider;
        self.stream_chat(request)
    }
}

impl Chat for CardinalClient {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            CardinalClient::stream_chat(self, request)
                .await
                .map(|stream| Box::pin(stream) as BoxChatStream)
        })
    }
}
