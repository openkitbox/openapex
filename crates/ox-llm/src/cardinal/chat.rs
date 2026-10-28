//! Mockable streaming-chat port over [`CardinalClient::stream_chat`].
//!
//! The runner depends on this object-safe trait (`Arc<dyn Chat>`) instead of
//! the concrete client so tests can substitute a scripted transport. The
//! stream itself is boxed: the client's `ChatStream` owns a live
//! `reqwest::Response` and cannot be fabricated offline, so the trait
//! returns a pinned `dyn Stream` with the same
//! `Result<StreamEvent, CardinalError>` item type. The real client boxes
//! its `ChatStream` unchanged.
//!
//! The trait shape is intentionally one method — `stream_chat(&ChatRequest)
//! -> Result<stream, CardinalError>` — mirroring the client; only the stream
//! type is erased.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;

use super::CardinalError;
use super::client::CardinalClient;
use super::stream::StreamEvent;
use super::wire::ChatRequest;

/// The erased stream every [`Chat`] implementation hands back.
pub type BoxChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, CardinalError>> + Send>>;

/// The boxed future [`Chat::stream_chat`] returns (async-fn-in-trait is not
/// dyn-compatible).
pub type ChatFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BoxChatStream, CardinalError>> + Send + 'a>>;

/// The streaming chat-completions seam. See the [module docs](self).
pub trait Chat: Send + Sync {
    /// Start a streaming chat completion for `request`.
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a>;
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
