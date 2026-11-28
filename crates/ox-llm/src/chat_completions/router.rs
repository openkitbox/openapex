//! Multi-endpoint chat routing over the [`Chat`] seam.
//!
//! Every endpoint comes from `[providers.<name>]` or a credential-resolved
//! builtin; `[models.<id>]` gives a logical model an ordered candidate list.
//! Routing is explicit: an unrouted model without a selected provider is
//! [`ChatError::NoRoute`] — there is no implicit endpoint to fall back to.
//!
//! The decisions are pure ([`should_failover`], candidate lookup); the
//! [`Chat`] impl only drives the attempt loop. The wire is untouched:
//! [`ChatRequest`] serializes exactly as before, with `model` rewritten per
//! candidate only when it differs.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::{EndpointLimits, LlmConfig, Protocol, ProviderConfig};

use super::ChatError;
use super::chat::{Chat, ChatFuture};
use super::client::ChatCompletionsClient;
use super::limit::LimitedChat;
use super::wire::ChatRequest;
use crate::messages::client::MessagesClient;
use crate::messages::wire::{MessagesDialect, MessagesPolicy};
use crate::responses::client::ResponsesClient;
use crate::responses::options::{ResponsesOptions, merge_options};

/// One resolved fallback target: a chat transport plus the wire model to
/// request on it.
pub(crate) struct Candidate {
    chat: Arc<dyn Chat>,
    /// Wire model id; `None` passes the logical id through unchanged.
    model: Option<String>,
    /// Which configured endpoint serves this candidate: the `[providers]`
    /// key or the builtin id. Carried for the attempt log.
    provider: String,
}

/// Ordered candidate routing over the configured endpoints. One per process,
/// built at the composition root; stateless per request.
pub struct EndpointRouter {
    resolver: crate::config::Routes,
    /// `[models.<id>]` candidates in configured order.
    routes: BTreeMap<String, Vec<Candidate>>,
    /// One direct candidate per configured endpoint, including credential-
    /// resolved builtins.
    direct: BTreeMap<String, Candidate>,
}

impl EndpointRouter {
    /// Build one client per configured endpoint. Client construction errors
    /// surface here, at the composition root.
    pub fn from_config(config: &LlmConfig) -> Result<Self, ChatError> {
        let mut clients: BTreeMap<String, Arc<dyn Chat>> = BTreeMap::new();
        for (name, provider) in &config.providers {
            let options = config.responses_options_providers.get(name);
            clients.insert(
                name.clone(),
                gated(
                    client_for(provider, config.messages_dialect_for(name), name, options)?,
                    &provider.limits,
                    name,
                ),
            );
        }
        // A builtin is an endpoint like any other once its credential resolved.
        for (name, resolved) in &config.builtins {
            let options = config.responses_options_providers.get(name);
            clients.insert(
                name.clone(),
                gated(
                    client_for(
                        &resolved.config,
                        config.messages_dialect_for(name),
                        name,
                        options,
                    )?,
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
        let resolve = |provider: &str, model: &str, index: usize| -> Result<Candidate, ChatError> {
            let chat = match config
                .responses_options_candidates
                .get(&(model.to_owned(), index))
            {
                Some(candidate_options) => {
                    let endpoint = config_for(config, provider).ok_or_else(|| {
                        ChatError::Upstream(format!(
                            "no configuration for Responses candidate `{provider}`"
                        ))
                    })?;
                    if endpoint.protocol() != Protocol::Responses {
                        return Err(ChatError::Upstream(format!(
                            "Responses options configured for non-Responses provider `{provider}`"
                        )));
                    }
                    let merged = merge_options(
                        config.responses_options_providers.get(provider),
                        Some(candidate_options),
                        None,
                    )
                    .map_err(|error| {
                        ChatError::Upstream(format!("invalid Responses options: {error}"))
                    })?;
                    Arc::new(ResponsesClient::with_options(
                        endpoint.base_url.clone(),
                        endpoint.api_key.clone(),
                        endpoint.limits.stall_timeout(),
                        provider.to_owned(),
                        merged,
                    )?) as Arc<dyn Chat>
                }
                None => Arc::clone(clients.get(provider).ok_or_else(|| {
                    ChatError::Upstream(format!("no configured client for provider `{provider}`"))
                })?),
            };
            Ok(Candidate {
                chat,
                model: Some(model.to_owned()),
                provider: provider.to_owned(),
            })
        };
        let mut routes = BTreeMap::new();
        for (id, route) in &config.models {
            let mut candidates = Vec::with_capacity(route.candidates.len());
            for (index, candidate) in route.candidates.iter().enumerate() {
                candidates.push(resolve(&candidate.provider, &candidate.model, index)?);
            }
            routes.insert(id.clone(), candidates);
        }
        Ok(Self {
            resolver: crate::config::Routes::new(config),
            routes,
            direct,
        })
    }

    /// The candidates serving `model`: the explicit route, else the builtin
    /// that owns its namespace, else the explicitly selected endpoint, else
    /// `NoRoute`.
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
/// A Messages endpoint uses the dialect its configuration declares. Builtins
/// install their compatibility dialect during credential resolution; an
/// operator-named endpoint gets the portable Anthropic form unless it declares
/// `messages_dialect` explicitly. No provider name is matched here.
fn config_for<'a>(config: &'a LlmConfig, name: &str) -> Option<&'a ProviderConfig> {
    if let Some(provider) = config.providers.get(name) {
        return Some(provider);
    }
    config.builtins.get(name).map(|resolved| &resolved.config)
}

fn client_for(
    provider: &ProviderConfig,
    dialect: MessagesDialect,
    provider_id: &str,
    options: Option<&ResponsesOptions>,
) -> Result<Arc<dyn Chat>, ChatError> {
    let stall = provider.limits.stall_timeout();
    Ok(match provider.protocol() {
        Protocol::ChatCompletions => Arc::new(ChatCompletionsClient::with_stall_timeout(
            provider.base_url.clone(),
            provider.api_key.clone(),
            stall,
        )?),
        Protocol::Messages => Arc::new(MessagesClient::new(
            provider.base_url.clone(),
            provider.api_key.clone(),
            MessagesPolicy::for_dialect(dialect),
            stall,
        )?),
        Protocol::Responses => Arc::new(ResponsesClient::with_options(
            provider.base_url.clone(),
            provider.api_key.clone(),
            stall,
            provider_id.to_owned(),
            options.cloned().unwrap_or_default(),
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
    use super::client_for;
    use crate::config::{EndpointLimits, LlmConfig, Protocol, ProviderConfig};
    use crate::messages::wire::MessagesDialect;

    fn provider(protocol: Protocol) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://api.openai.com".to_owned(),
            api_key: Some("test-key".to_owned()),
            protocol: Some(protocol),
            limits: EndpointLimits::default(),
        }
    }

    fn call(protocol: Protocol) -> bool {
        client_for(
            &provider(protocol),
            MessagesDialect::Anthropic,
            "test",
            None,
        )
        .is_ok()
    }

    #[test]
    fn protocol_responses_selects_client() {
        assert!(call(Protocol::Responses));
        assert!(call(Protocol::ChatCompletions));
        assert!(call(Protocol::Messages));

        // Existing config validation rejects an empty Responses base URL.
        let mut config = LlmConfig::default();
        config.providers.insert(
            "responses".to_owned(),
            ProviderConfig {
                base_url: String::new(),
                ..provider(Protocol::Responses)
            },
        );
        assert!(config.validate().is_err());
    }
}
