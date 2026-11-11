//! Multi-endpoint chat routing over the [`Chat`] seam.
//!
//! `cardinal_url`/`cardinal_key` stay the implicit endpoint (named
//! [`crate::config::DEFAULT_PROVIDER_NAME`]); `[providers.<name>]` adds
//! more, `[models.<id>]` gives a logical model an ordered candidate list.
//! Routing is explicit: once `[providers]` is configured, unrouted models
//! are [`ChatError::NoRoute`] instead of silently hitting the implicit
//! endpoint.
//!
//! The decisions are pure ([`should_failover`], candidate lookup); the
//! [`Chat`] impl only drives the attempt loop. The wire is untouched:
//! [`ChatRequest`] serializes exactly as before, with `model` rewritten per
//! candidate only when it differs.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::{DEFAULT_PROVIDER_NAME, EndpointLimits, LlmConfig, Protocol, ProviderConfig};

use super::ChatError;
use super::chat::{Chat, ChatFuture};
use super::client::CardinalClient;
use super::limit::LimitedChat;
use super::wire::ChatRequest;
use crate::messages::client::MessagesClient;
use crate::messages::wire::MessagesPolicy;

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
    resolver: crate::config::Routes,
    /// `[models.<id>]` candidates in configured order.
    routes: BTreeMap<String, Vec<Candidate>>,
    /// One direct candidate per configured endpoint, including the implicit
    /// cardinal endpoint and credential-resolved builtins.
    direct: BTreeMap<String, Candidate>,
}

impl EndpointRouter {
    /// Build the router from config. `cardinal` is the implicit endpoint
    /// client (from `cardinal_url`/`cardinal_key`), built by the caller so
    /// init failures surface at the composition root with one message.
    pub fn from_config(config: &LlmConfig, cardinal: CardinalClient) -> Result<Self, ChatError> {
        let mut clients: BTreeMap<String, Arc<dyn Chat>> = BTreeMap::new();
        clients.insert(
            DEFAULT_PROVIDER_NAME.to_owned(),
            gated(
                Arc::new(cardinal),
                &config.cardinal_limits,
                DEFAULT_PROVIDER_NAME,
            ),
        );
        for (name, provider) in &config.providers {
            clients.insert(
                name.clone(),
                gated(client_for(name, provider)?, &provider.limits, name),
            );
        }
        // A builtin is an endpoint like any other once its credential resolved.
        for (name, resolved) in &config.builtins {
            clients.insert(
                name.clone(),
                gated(
                    client_for(name, &resolved.config)?,
                    &resolved.config.limits,
                    name,
                ),
            );
        }
        let direct: BTreeMap<String, Candidate> = clients
            .iter()
            .map(|(name, chat)| {
                (
                    name.clone(),
                    Candidate {
                        chat: Arc::clone(chat),
                        model: None,
                        provider: name.clone(),
                    },
                )
            })
            .collect();
        let resolve = |provider: &str, model: &str| -> Candidate {
            let chat = clients.get(provider).unwrap_or_else(|| {
                panic!("candidate provider `{provider}` missing despite LlmConfig::validate")
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
        Ok(Self {
            resolver: crate::config::Routes::new(config),
            routes,
            direct,
        })
    }

    /// The candidates serving `model`: the explicit route, else the builtin
    /// that owns its namespace, else the passthrough endpoint, else `NoRoute`.
    /// Never returns an empty list.
    pub(crate) fn candidates_for(
        &self,
        provider: Option<&str>,
        model: &str,
    ) -> Result<&[Candidate], ChatError> {
        match self.resolver.resolve(provider, model) {
            crate::config::Targets::Explicit(_) => self.routes.get(model).map(Vec::as_slice),
            crate::config::Targets::Endpoint { provider, .. } => {
                self.direct.get(provider).map(std::slice::from_ref)
            }
            crate::config::Targets::NoRoute => None,
        }
        .ok_or_else(|| ChatError::NoRoute {
            model: model.to_owned(),
        })
    }
}

impl Chat for EndpointRouter {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        self.stream_chat_for(request, None)
    }

    fn stream_chat_for<'a>(
        &'a self,
        request: &'a ChatRequest,
        provider: Option<&'a str>,
    ) -> ChatFuture<'a> {
        Box::pin(async move {
            let candidates = self.candidates_for(provider, &request.model)?;
            let mut last: Option<ChatError> = None;
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
                match candidate
                    .chat
                    .stream_chat_for(wire, Some(&candidate.provider))
                    .await
                {
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

/// The transport for one configured endpoint.
///
/// The protocol picks the [`Chat`] implementation, so an endpoint's wire shape
/// cannot disagree with the path ox appends to its base URL: chat completions
/// and Messages each own their request builder, decoder and URL.
///
/// A Messages endpoint also picks the dialect its `provider` name implies
/// ([`MessagesPolicy::for_provider`]): `deepseek` states its thinking switch and
/// replays unsigned reasoning, anything else takes the portable Anthropic form.
fn client_for(name: &str, provider: &ProviderConfig) -> Result<Arc<dyn Chat>, ChatError> {
    let stall = provider.limits.stall_timeout();
    Ok(match provider.protocol() {
        Protocol::ChatCompletions => Arc::new(CardinalClient::with_stall_timeout(
            provider.base_url.clone(),
            provider.api_key.clone(),
            stall,
        )?),
        Protocol::Messages => Arc::new(MessagesClient::new(
            provider.base_url.clone(),
            provider.api_key.clone(),
            MessagesPolicy::for_provider(name),
            stall,
        )?),
    })
}

/// `chat` behind its endpoint's concurrency gate when `limits` set one;
/// otherwise `chat` itself, so an unlimited endpoint gains no wrapper object.
/// Logs the effective limits once per limited endpoint (a misspelled key is
/// silently "no limit", and this line is how an operator notices).
fn gated(chat: Arc<dyn Chat>, limits: &EndpointLimits, provider: &str) -> Arc<dyn Chat> {
    let stall = limits.stall_timeout();
    if limits.max_concurrent.is_some() || stall.is_some() {
        tracing::info!(
            provider,
            max_concurrent = limits.max_concurrent.map(|max| max.get()),
            stall_timeout_secs = stall.map(|stall| stall.as_secs()),
            "endpoint limits active"
        );
    }
    match limits.max_concurrent {
        Some(max) => Arc::new(LimitedChat::new(chat, max, provider)),
        None => chat,
    }
}

/// Whether `error` justifies trying the next candidate.
///
/// Failover is only sound before any byte of the stream has been consumed:
/// `Upstream`/`EarlyClose`/`Sse` surface mid-consumption, and other 4xx are
/// properties of the request that another endpoint would reject identically.
pub(crate) fn should_failover(error: &ChatError) -> bool {
    match error {
        // Connect, DNS, TLS, request timeout: the endpoint is unreachable.
        ChatError::Transport(_) => true,
        ChatError::Http { status, .. } => {
            // Auth/gateway blocks (401/403), request timeout (408), rate
            // limit (429) and upstream/server failures are endpoint-specific
            // or transient.
            // Deliberate superset of "5xx": `Http` is only built from a live
            // response status, so ≥600 cannot occur, and an unknown 6xx-class
            // is server-class anyway.
            matches!(*status, 401 | 403 | 408 | 429) || *status >= 500
        }
        ChatError::NoRoute { .. }
        | ChatError::Upstream(_)
        | ChatError::EarlyClose
        | ChatError::Sse(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cardinal::chat::BoxChatStream;
    use crate::cardinal::stream::StreamEvent;
    use crate::config::ProviderConfig;

    #[test]
    fn direct_selection_uses_the_selected_provider_endpoint() {
        let mut config = LlmConfig::default();
        config.providers.insert(
            "other".to_owned(),
            ProviderConfig {
                base_url: "https://other.example".to_owned(),
                api_key: None,
                protocol: None,
                limits: EndpointLimits::default(),
            },
        );
        let cardinal = CardinalClient::new("https://cardinal.example", None)
            .expect("cardinal client should construct");
        let router = EndpointRouter::from_config(&config, cardinal)
            .expect("configured endpoints should construct");

        let candidates = router
            .candidates_for(Some("other"), "shared")
            .expect("selected provider should route directly");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, "other");
        assert!(candidates[0].model.is_none());
        assert!(router.candidates_for(Some("missing"), "shared").is_err());
    }

    struct EmptyStream;

    impl futures_core::Stream for EmptyStream {
        type Item = Result<StreamEvent, ChatError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(None)
        }
    }

    /// One attempt a recorder saw: the provider it was routed to and the
    /// serialized body, so a test asserts the wire bytes, not a reconstruction.
    struct Call {
        provider: Option<String>,
        body: serde_json::Value,
    }

    struct ProviderRecorder {
        calls: std::sync::Arc<std::sync::Mutex<Vec<Call>>>,
    }

    impl Chat for ProviderRecorder {
        fn stream_chat<'a>(&'a self, _request: &'a ChatRequest) -> ChatFuture<'a> {
            panic!("the router must use the provider-aware seam");
        }

        fn stream_chat_for<'a>(
            &'a self,
            request: &'a ChatRequest,
            provider: Option<&'a str>,
        ) -> ChatFuture<'a> {
            self.calls
                .lock()
                .expect("recorder mutex is not poisoned")
                .push(Call {
                    provider: provider.map(str::to_owned),
                    body: serde_json::to_value(request).expect("request serializes"),
                });
            Box::pin(async { Ok(Box::pin(EmptyStream) as BoxChatStream) })
        }
    }

    #[test]
    fn stream_chat_for_uses_matching_explicit_route_and_wire_model() {
        let mut config = LlmConfig::default();
        config.providers.insert(
            "route-provider".to_owned(),
            ProviderConfig {
                base_url: "https://route.example".to_owned(),
                api_key: None,
                protocol: None,
                limits: EndpointLimits::default(),
            },
        );
        config.models.insert(
            "logical-alias".to_owned(),
            crate::config::ModelRouteConfig {
                candidates: vec![crate::config::CandidateConfig {
                    provider: "route-provider".to_owned(),
                    model: "wire-model".to_owned(),
                }],
            },
        );
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let candidate = Candidate {
            chat: std::sync::Arc::new(ProviderRecorder {
                calls: std::sync::Arc::clone(&calls),
            }),
            model: Some("wire-model".to_owned()),
            provider: "route-provider".to_owned(),
        };
        let router = EndpointRouter {
            resolver: crate::config::Routes::new(&config),
            routes: BTreeMap::from([("logical-alias".to_owned(), vec![candidate])]),
            direct: BTreeMap::new(),
        };
        let request = ChatRequest::new("logical-alias", Vec::new());
        let mut future = router.stream_chat_for(&request, Some("route-provider"));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let result = std::future::Future::poll(std::pin::Pin::as_mut(&mut future), &mut context);

        assert!(
            result.is_ready(),
            "route candidate should open an empty stream"
        );
        drop(future);
        let calls = calls.lock().expect("recorder mutex is not poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].provider.as_deref(), Some("route-provider"));
        assert_eq!(calls[0].body["model"], "wire-model");
        assert!(
            calls[0].body.get("provider").is_none(),
            "provider metadata must stay off the wire"
        );
    }

    #[test]
    fn stream_chat_for_passes_selected_provider_to_candidate() {
        let mut config = LlmConfig::default();
        config.providers.insert(
            "selected".to_owned(),
            ProviderConfig {
                base_url: "https://selected.example".to_owned(),
                api_key: None,
                protocol: None,
                limits: EndpointLimits::default(),
            },
        );
        config.providers.insert(
            "route-provider".to_owned(),
            ProviderConfig {
                base_url: "https://route.example".to_owned(),
                api_key: None,
                protocol: None,
                limits: EndpointLimits::default(),
            },
        );
        config.models.insert(
            "shared".to_owned(),
            crate::config::ModelRouteConfig {
                candidates: vec![crate::config::CandidateConfig {
                    provider: "route-provider".to_owned(),
                    model: "route-model".to_owned(),
                }],
            },
        );
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut direct = BTreeMap::new();
        direct.insert(
            "selected".to_owned(),
            Candidate {
                chat: std::sync::Arc::new(ProviderRecorder {
                    calls: std::sync::Arc::clone(&calls),
                }),
                model: None,
                provider: "selected".to_owned(),
            },
        );
        let router = EndpointRouter {
            resolver: crate::config::Routes::new(&config),
            routes: BTreeMap::new(),
            direct,
        };
        let request = ChatRequest::new("shared", Vec::new());
        let mut future = router.stream_chat_for(&request, Some("selected"));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let result = std::future::Future::poll(std::pin::Pin::as_mut(&mut future), &mut context);

        assert!(
            result.is_ready(),
            "selected endpoint should open an empty stream"
        );
        drop(future);
        let calls = calls.lock().expect("recorder mutex is not poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].provider.as_deref(), Some("selected"));
        assert_eq!(calls[0].body["model"], "shared");
        assert!(
            calls[0].body.get("provider").is_none(),
            "provider metadata must stay off the wire"
        );

        let mut unknown = router.stream_chat_for(&request, Some("missing"));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let result = std::future::Future::poll(std::pin::Pin::as_mut(&mut unknown), &mut context);
        assert!(
            matches!(
                result,
                std::task::Poll::Ready(Err(ChatError::NoRoute { .. }))
            ),
            "unknown provider must fail closed"
        );
    }
}
