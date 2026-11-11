//! Per-endpoint concurrency gate over the [`Chat`] seam.
//!
//! [`LimitedChat`] decorates one endpoint's transport with a FIFO semaphore:
//! requests beyond `max` wait (never fail) until an earlier response has been
//! read to its end. It owns queueing and permit lifetime only — it does not
//! know HTTP, configuration, or request contents.
//!
//! **Permit lifetime** is "reading the stream", not "owning the stream": the
//! permit is released the moment the stream yields its terminal item
//! (`Done`, an error, or end-of-stream), or when the stream is dropped,
//! whichever comes first. The runner keeps a finished stream alive while it
//! joins tool tasks, and a foreground subagent's child session needs a permit
//! of the same endpoint during that window; holding the permit until drop
//! would deadlock a limit-1 endpoint. While a permit is held the code path
//! only reads the stream and never waits for another permit, so there is no
//! hold-and-wait cycle.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::ChatError;
use super::chat::{BoxChatStream, Chat, ChatFuture};
use super::stream::StreamEvent;
use super::wire::ChatRequest;

/// How long a request may wait for a permit before one `warn` is logged.
pub const QUEUE_WARN_AFTER: Duration = Duration::from_secs(30);

/// A [`Chat`] that admits at most `max` concurrent responses to `inner`.
pub struct LimitedChat {
    inner: Arc<dyn Chat>,
    permits: Arc<Semaphore>,
    max: usize,
    provider: Box<str>,
}

impl LimitedChat {
    /// Gate `inner` to `max` concurrent responses. `provider` names the
    /// endpoint in the queue warning.
    pub fn new(
        inner: Arc<dyn Chat>,
        max: std::num::NonZeroUsize,
        provider: impl Into<Box<str>>,
    ) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(max.get())),
            max: max.get(),
            provider: provider.into(),
        }
    }

    /// Permits not currently held (a leak check for tests and logs).
    #[must_use]
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// Wait for a permit in FIFO order. Dropping the returned future leaves
    /// the queue. After [`QUEUE_WARN_AFTER`] it logs once, then keeps waiting
    /// on the same acquire future so the queue position is kept.
    async fn acquire(&self) -> OwnedSemaphorePermit {
        if let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() {
            return permit;
        }
        let mut acquire = std::pin::pin!(Arc::clone(&self.permits).acquire_owned());
        match tokio::time::timeout(QUEUE_WARN_AFTER, &mut acquire).await {
            Ok(permit) => permit.expect("semaphore is never closed"),
            Err(_elapsed) => {
                tracing::warn!(
                    provider = &*self.provider,
                    max = self.max,
                    in_flight = self.max.saturating_sub(self.available()),
                    "request still queued behind the endpoint concurrency limit"
                );
                acquire.await.expect("semaphore is never closed")
            }
        }
    }
}

impl Chat for LimitedChat {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        self.stream_chat_for(request, None)
    }

    fn stream_chat_for<'a>(
        &'a self,
        request: &'a ChatRequest,
        provider: Option<&'a str>,
    ) -> ChatFuture<'a> {
        Box::pin(async move {
            let permit = self.acquire().await;
            // On `Err` the permit drops here, before the error propagates.
            let stream = self.inner.stream_chat_for(request, provider).await?;
            Ok(Box::pin(PermitStream {
                inner: stream,
                permit: Some(permit),
            }) as BoxChatStream)
        })
    }
}

/// Forwards the inner stream and gives the permit back at its terminal item.
/// A stream abandoned before that releases it when the field drops (the
/// connection in `inner` closes first, then the permit).
struct PermitStream {
    inner: BoxChatStream,
    permit: Option<OwnedSemaphorePermit>,
}

impl Stream for PermitStream {
    type Item = Result<StreamEvent, ChatError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.inner.as_mut().poll_next(cx);
        if let Poll::Ready(item) = &polled
            && matches!(item, None | Some(Err(_)) | Some(Ok(StreamEvent::Done)))
        {
            self.permit = None;
        }
        polled
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::cardinal::chat::BoxChatStream;

    struct EmptyStream;

    impl Stream for EmptyStream {
        type Item = Result<StreamEvent, ChatError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    struct ProviderRecorder {
        provider: Arc<Mutex<Option<String>>>,
    }

    impl Chat for ProviderRecorder {
        fn stream_chat<'a>(&'a self, _request: &'a ChatRequest) -> ChatFuture<'a> {
            panic!("the provider-aware seam must be used")
        }

        fn stream_chat_for<'a>(
            &'a self,
            _request: &'a ChatRequest,
            provider: Option<&'a str>,
        ) -> ChatFuture<'a> {
            *self
                .provider
                .lock()
                .expect("provider mutex is not poisoned") = provider.map(str::to_owned);
            Box::pin(async { Ok(Box::pin(EmptyStream) as BoxChatStream) })
        }
    }

    #[test]
    fn forwards_provider_to_inner_chat() {
        let provider = Arc::new(Mutex::new(None));
        let inner = Arc::new(ProviderRecorder {
            provider: Arc::clone(&provider),
        });
        let limited = LimitedChat::new(inner, NonZeroUsize::new(1).expect("non-zero"), "test");
        let request = ChatRequest::new("model", Vec::new());
        let mut future = limited.stream_chat_for(&request, Some("selected"));
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);

        let result = Future::poll(Pin::as_mut(&mut future), &mut context);

        assert!(result.is_ready(), "limited chat should acquire immediately");
        assert_eq!(
            provider
                .lock()
                .expect("provider mutex is not poisoned")
                .as_deref(),
            Some("selected")
        );
    }
}
