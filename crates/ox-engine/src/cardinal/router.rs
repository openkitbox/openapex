//! Multi-endpoint chat routing over the [`Chat`] seam.
//!
//! `cardinal_url`/`cardinal_key` stay the implicit endpoint (named
//! [`ox_core::config::DEFAULT_PROVIDER_NAME`]); `[providers.<name>]` adds
//! more, `[models.<id>]` gives a logical model an ordered candidate list.
//! Routing is explicit: once `[providers]` is configured, unrouted models
//! are [`CardinalError::NoRoute`] instead of silently hitting the implicit
//! endpoint.
//!
//! The decisions are pure ([`should_failover`], candidate lookup); the
//! [`Chat`] impl only drives the attempt loop. The wire is untouched:
//! [`ChatRequest`] serializes exactly as before, with `model` rewritten per
//! candidate only when it differs.

use std::collections::BTreeMap;
use std::sync::Arc;

use ox_core::config::{Config, DEFAULT_PROVIDER_NAME};

use super::CardinalError;
use super::chat::{Chat, ChatFuture};
use super::client::CardinalClient;
use super::wire::ChatRequest;

/// One resolved fallback target: a chat transport plus the wire model to
/// request on it.
pub(crate) struct Candidate {
    chat: Arc<dyn Chat>,
    /// Wire model id; `None` passes the logical id through unchanged.
    model: Option<String>,
    /// Which configured endpoint serves this candidate: the `[providers]`
    /// key, or `cardinal` for the implicit one. Carried for the attempt log.
    provider: String,
}

/// Ordered candidate routing over the configured endpoints. One per process,
/// built at the composition root; stateless per request.
pub struct EndpointRouter {
    /// `[models.<id>]` candidates in configured order.
    routes: BTreeMap<String, Vec<Candidate>>,
    /// The passthrough candidate for unrouted models; `None` once
    /// `[providers]` makes routing explicit (then unrouted is `NoRoute`).
    passthrough: Option<Candidate>,
}

impl EndpointRouter {
    /// Build the router from config. `cardinal` is the implicit endpoint
    /// client (from `cardinal_url`/`cardinal_key`), built by the caller so
    /// init failures surface at the composition root with one message.
    pub fn from_config(config: &Config, cardinal: CardinalClient) -> Result<Self, CardinalError> {
        let mut clients: BTreeMap<String, Arc<dyn Chat>> = BTreeMap::new();
        clients.insert(DEFAULT_PROVIDER_NAME.to_owned(), Arc::new(cardinal));
        for (name, provider) in &config.providers {
            let client = CardinalClient::new(provider.base_url.clone(), provider.api_key.clone())?;
            clients.insert(name.clone(), Arc::new(client));
        }
        let resolve = |provider: &str, model: &str| -> Candidate {
            let chat = clients.get(provider).unwrap_or_else(|| {
                panic!("candidate provider `{provider}` missing despite Config::validate_providers")
            });
            Candidate {
                chat: Arc::clone(chat),
                model: Some(model.to_owned()),
                provider: provider.to_owned(),
            }
        };
        let mut routes = BTreeMap::new();
        for (id, route) in &config.models {
            let candidates = route
                .candidates
                .iter()
                .map(|candidate| resolve(&candidate.provider, &candidate.model))
                .collect();
            routes.insert(id.clone(), candidates);
        }
        let passthrough = if config.providers.is_empty() {
            Some(Candidate {
                chat: Arc::clone(&clients[DEFAULT_PROVIDER_NAME]),
                model: None,
                provider: DEFAULT_PROVIDER_NAME.to_owned(),
            })
        } else {
            None
        };
        Ok(Self {
            routes,
            passthrough,
        })
    }

    /// Direct construction for tests. An empty candidate list is treated as
    /// no route for that model.
    // Test-only: nothing in production code builds a router without config,
    // and integration tests (`tests/`) cannot see `pub(crate)` items either.
    #[cfg(test)]
    pub(crate) fn from_parts(
        routes: BTreeMap<String, Vec<Candidate>>,
        passthrough: Option<Candidate>,
    ) -> Self {
        Self {
            routes,
            passthrough,
        }
    }

    /// The candidates serving `model`: the explicit route, else the
    /// passthrough endpoint, else `NoRoute`. Never returns an empty list.
    pub(crate) fn candidates_for(&self, model: &str) -> Result<&[Candidate], CardinalError> {
        if let Some(route) = self.routes.get(model)
            && !route.is_empty()
        {
            return Ok(route);
        }
        match &self.passthrough {
            Some(candidate) => Ok(std::slice::from_ref(candidate)),
            None => Err(CardinalError::NoRoute {
                model: model.to_owned(),
            }),
        }
    }
}

impl Chat for EndpointRouter {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            let candidates = self.candidates_for(&request.model)?;
            let mut last: Option<CardinalError> = None;
            for candidate in candidates {
                // The wire model is rewritten only when it differs: the
                // default route and equal-model candidates serve the borrowed
                // request with zero copies, so single-endpoint deployments
                // stay byte-identical to the direct client.
                let rewritten;
                let wire: &ChatRequest = match &candidate.model {
                    None => request,
                    Some(model) if *model == request.model => request,
                    Some(model) => {
                        rewritten = ChatRequest {
                            model: model.clone(),
                            ..request.clone()
                        };
                        &rewritten
                    }
                };
                match candidate.chat.stream_chat(wire).await {
                    Ok(stream) => return Ok(stream),
                    Err(error) => {
                        let failover = should_failover(&error);
                        tracing::warn!(
                            provider = %candidate.provider,
                            model = %request.model,
                            error = %error,
                            failover,
                            "chat endpoint attempt failed"
                        );
                        last = Some(error);
                        if !failover {
                            break;
                        }
                    }
                }
            }
            Err(last.expect("candidates_for never returns an empty list"))
        })
    }
}

/// Whether `error` justifies trying the next candidate.
///
/// Failover is only sound before any byte of the stream has been consumed:
/// `Upstream`/`EarlyClose`/`Sse` surface mid-consumption, and other 4xx are
/// properties of the request that another endpoint would reject identically.
pub(crate) fn should_failover(error: &CardinalError) -> bool {
    match error {
        // Connect, DNS, TLS, request timeout: the endpoint is unreachable.
        CardinalError::Transport(_) => true,
        CardinalError::Http { status, .. } => {
            // Auth/gateway blocks (401/403), request timeout (408), rate
            // limit (429) and upstream/server failures are endpoint-specific
            // or transient.
            // Deliberate superset of "5xx": `Http` is only built from a live
            // response status, so ≥600 cannot occur, and an unknown 6xx-class
            // is server-class anyway.
            matches!(*status, 401 | 403 | 408 | 429) || *status >= 500
        }
        CardinalError::NoRoute { .. }
        | CardinalError::Upstream(_)
        | CardinalError::EarlyClose
        | CardinalError::Sse(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_classification() {
        let http = |status: u16| CardinalError::Http {
            status,
            body: String::new(),
        };
        for status in [401, 403, 408, 429, 500, 502, 503, 504, 600] {
            assert!(
                should_failover(&http(status)),
                "HTTP {status} must fail over"
            );
        }
        for status in [400, 402, 404, 409, 422, 499] {
            assert!(
                !should_failover(&http(status)),
                "HTTP {status} must not fail over"
            );
        }
        assert!(!should_failover(&CardinalError::Upstream(
            "mid-stream".into()
        )));
        assert!(!should_failover(&CardinalError::EarlyClose));
        assert!(!should_failover(&CardinalError::Sse("bad payload".into())));
        assert!(!should_failover(&CardinalError::NoRoute {
            model: "demo".into()
        }));
        // `Transport` wraps `reqwest::Error`, which has no public
        // constructor; its `true` arm will be exercised end-to-end by
        // Task 4's `tests/provider_router.rs` (connection refused on a
        // closed port).
    }
}

#[cfg(test)]
mod router_tests {
    use super::super::chat::BoxChatStream;
    use super::super::stream::StreamEvent;
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    /// A one-shot stream over scripted events (no futures-util).
    struct OnceStream(std::vec::IntoIter<Result<StreamEvent, CardinalError>>);

    impl futures_core::Stream for OnceStream {
        type Item = Result<StreamEvent, CardinalError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.next())
        }
    }

    fn ok_stream() -> BoxChatStream {
        Box::pin(OnceStream(
            vec![
                Ok(StreamEvent::TextDelta("hi".to_owned())),
                Ok(StreamEvent::Finish {
                    reason: "stop".to_owned(),
                }),
                Ok(StreamEvent::Done),
            ]
            .into_iter(),
        ))
    }

    /// Records the wire model of every attempt and replays scripted outcomes;
    /// `None` means "serve Ok". An exhausted script serves Ok forever.
    struct ScriptedChat {
        results: Mutex<VecDeque<Option<CardinalError>>>,
        seen: Mutex<Vec<String>>,
    }

    impl Chat for ScriptedChat {
        fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
            self.seen.lock().unwrap().push(request.model.clone());
            let next = self.results.lock().unwrap().pop_front();
            let result = match next {
                Some(Some(error)) => Err(error),
                _ => Ok(ok_stream()),
            };
            Box::pin(async move { result })
        }
    }

    fn scripted(results: VecDeque<Option<CardinalError>>) -> (Arc<ScriptedChat>, Arc<dyn Chat>) {
        let chat = Arc::new(ScriptedChat {
            results: Mutex::new(results),
            seen: Mutex::new(Vec::new()),
        });
        let as_chat: Arc<dyn Chat> = chat.clone();
        (chat, as_chat)
    }

    fn seen(chat: &ScriptedChat) -> Vec<String> {
        chat.seen.lock().unwrap().clone()
    }

    fn request(model: &str) -> ChatRequest {
        // Empty messages are fine here: neither the router nor the scripted
        // transport reads them. The real wire is covered by
        // tests/provider_router.rs against live mocks.
        ChatRequest::new(model, Vec::new())
    }

    fn routed(model: &str, candidates: Vec<Candidate>) -> EndpointRouter {
        EndpointRouter::from_parts(BTreeMap::from([(model.to_owned(), candidates)]), None)
    }

    fn http_error(status: u16) -> CardinalError {
        CardinalError::Http {
            status,
            body: String::new(),
        }
    }

    #[tokio::test]
    async fn passthrough_serves_the_logical_model_unchanged() {
        let (chat, as_chat) = scripted(VecDeque::new());
        let router = EndpointRouter::from_parts(
            BTreeMap::new(),
            Some(Candidate {
                chat: as_chat,
                model: None,
                provider: "passthrough".to_owned(),
            }),
        );
        // The stream is intentionally unconsumed: these tests assert routing only.
        let _ = router
            .stream_chat(&request("k3-256k"))
            .await
            .expect("passthrough serves");
        assert_eq!(seen(&chat), ["k3-256k"]);
    }

    #[tokio::test]
    async fn explicit_candidates_rewrite_the_wire_model() {
        let (chat, as_chat) = scripted(VecDeque::new());
        let router = routed(
            "demo",
            vec![Candidate {
                chat: as_chat,
                model: Some("wire-a".to_owned()),
                provider: "test".to_owned(),
            }],
        );
        // The stream is intentionally unconsumed: these tests assert routing only.
        let _ = router
            .stream_chat(&request("demo"))
            .await
            .expect("candidate serves");
        assert_eq!(seen(&chat), ["wire-a"]);
    }

    #[tokio::test]
    async fn same_model_candidate_takes_the_borrowed_path() {
        let (chat, as_chat) = scripted(VecDeque::new());
        let router = routed(
            "demo",
            vec![Candidate {
                chat: as_chat,
                model: Some("demo".to_owned()),
                provider: "test".to_owned(),
            }],
        );
        // The stream is intentionally unconsumed: these tests assert routing only.
        let _ = router.stream_chat(&request("demo")).await.expect("serves");
        assert_eq!(seen(&chat), ["demo"]);
    }

    #[tokio::test]
    async fn failover_tries_the_next_candidate() {
        let (first, first_chat) = scripted(VecDeque::from([Some(http_error(429))]));
        let (second, second_chat) = scripted(VecDeque::new());
        let router = routed(
            "demo",
            vec![
                Candidate {
                    chat: first_chat,
                    model: Some("wire-a".to_owned()),
                    provider: "first".to_owned(),
                },
                Candidate {
                    chat: second_chat,
                    model: Some("wire-b".to_owned()),
                    provider: "second".to_owned(),
                },
            ],
        );
        // The stream is intentionally unconsumed: these tests assert routing only.
        let _ = router
            .stream_chat(&request("demo"))
            .await
            .expect("second candidate serves");
        assert_eq!(seen(&first), ["wire-a"]);
        assert_eq!(seen(&second), ["wire-b"]);
    }

    #[tokio::test]
    async fn non_failover_error_stops_the_chain() {
        let (first, first_chat) = scripted(VecDeque::from([Some(http_error(400))]));
        let (second, second_chat) = scripted(VecDeque::new());
        let router = routed(
            "demo",
            vec![
                Candidate {
                    chat: first_chat,
                    model: Some("wire-a".to_owned()),
                    provider: "first".to_owned(),
                },
                Candidate {
                    chat: second_chat,
                    model: Some("wire-b".to_owned()),
                    provider: "second".to_owned(),
                },
            ],
        );
        let error = match router.stream_chat(&request("demo")).await {
            Ok(_) => panic!("400 surfaces"),
            Err(error) => error,
        };
        assert!(matches!(error, CardinalError::Http { status: 400, .. }));
        assert_eq!(seen(&first), ["wire-a"]);
        assert!(seen(&second).is_empty(), "second must not be tried");
    }

    #[tokio::test]
    async fn exhausted_candidates_return_the_last_error() {
        let (_, first_chat) = scripted(VecDeque::from([Some(http_error(429))]));
        let (_, second_chat) = scripted(VecDeque::from([Some(http_error(503))]));
        let router = routed(
            "demo",
            vec![
                Candidate {
                    chat: first_chat,
                    model: None,
                    provider: "first".to_owned(),
                },
                Candidate {
                    chat: second_chat,
                    model: None,
                    provider: "second".to_owned(),
                },
            ],
        );
        let error = match router.stream_chat(&request("demo")).await {
            Ok(_) => panic!("all candidates failed"),
            Err(error) => error,
        };
        assert!(matches!(error, CardinalError::Http { status: 503, .. }));
    }

    #[tokio::test]
    async fn unrouted_model_errors_when_routing_is_explicit() {
        let router = EndpointRouter::from_parts(BTreeMap::new(), None);
        let error = match router.stream_chat(&request("ghost")).await {
            Ok(_) => panic!("no route configured"),
            Err(error) => error,
        };
        assert!(matches!(error, CardinalError::NoRoute { model } if model == "ghost"));
    }

    #[tokio::test]
    async fn explicit_route_wins_over_passthrough() {
        let (routed, routed_chat) = scripted(VecDeque::new());
        let (fallback, fallback_chat) = scripted(VecDeque::new());
        let router = EndpointRouter::from_parts(
            BTreeMap::from([(
                "demo".to_owned(),
                vec![Candidate {
                    chat: routed_chat,
                    model: Some("wire-a".to_owned()),
                    provider: "moonshot".to_owned(),
                }],
            )]),
            Some(Candidate {
                chat: fallback_chat,
                model: None,
                provider: DEFAULT_PROVIDER_NAME.to_owned(),
            }),
        );
        // The stream is intentionally unconsumed: this test asserts routing only.
        let _ = router
            .stream_chat(&request("demo"))
            .await
            .expect("route serves");
        assert_eq!(seen(&routed), ["wire-a"]);
        assert!(seen(&fallback).is_empty(), "passthrough must not be tried");
    }
}
