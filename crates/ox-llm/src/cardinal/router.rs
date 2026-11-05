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

use crate::config::{
    BuiltinProvider, DEFAULT_PROVIDER_NAME, EndpointLimits, LlmConfig, Protocol, ProviderConfig,
};

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
    /// `[models.<id>]` candidates in configured order.
    routes: BTreeMap<String, Vec<Candidate>>,
    /// One candidate per credential-resolved builtin, with the table row that
    /// owns its model namespace (so `deepseek-flash` needs no `[models]` route).
    builtins: Vec<(Candidate, &'static BuiltinProvider)>,
    /// The passthrough candidate for unrouted models; `None` once
    /// `[providers]` makes routing explicit (then unrouted is `NoRoute`).
    passthrough: Option<Candidate>,
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
        let builtins: Vec<(Candidate, &'static BuiltinProvider)> = config
            .builtins
            .iter()
            .map(|(name, resolved)| {
                let chat = clients
                    .get(name)
                    .unwrap_or_else(|| panic!("builtin client `{name}` missing after insert"));
                (
                    Candidate {
                        chat: Arc::clone(chat),
                        // The logical id is already the builtin's wire id.
                        model: None,
                        provider: name.clone(),
                    },
                    resolved.builtin,
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
            builtins,
            passthrough,
        })
    }

    /// The candidates serving `model`: the explicit route, else the builtin
    /// that owns its namespace, else the passthrough endpoint, else `NoRoute`.
    /// Never returns an empty list.
    pub(crate) fn candidates_for(&self, model: &str) -> Result<&[Candidate], ChatError> {
        if let Some(route) = self.routes.get(model)
            && !route.is_empty()
        {
            return Ok(route);
        }
        // Ownership comes after an explicit route (the operator's mapping wins)
        // and before the passthrough endpoint (a materialized builtin exists
        // only because its own key was supplied).
        if let Some((candidate, _)) = self
            .builtins
            .iter()
            .find(|(_, builtin)| builtin.owns(model))
        {
            return Ok(std::slice::from_ref(candidate));
        }
        match &self.passthrough {
            Some(candidate) => Ok(std::slice::from_ref(candidate)),
            None => Err(ChatError::NoRoute {
                model: model.to_owned(),
            }),
        }
    }
}

impl Chat for EndpointRouter {
    fn stream_chat<'a>(&'a self, request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            let candidates = self.candidates_for(&request.model)?;
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
