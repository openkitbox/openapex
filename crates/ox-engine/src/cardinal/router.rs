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
