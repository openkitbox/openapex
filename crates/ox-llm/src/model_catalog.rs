//! The model catalogue: every configured endpoint's `GET /v1/models`, not ox's
//! config file.
//!
//! The endpoints already report what a client needs — the id, the provider
//! (`owned_by`) and the context window (`context_length` / `context_window` /
//! `max_input_tokens`) — so keeping a `[[models]]` list in `ox.toml` was a
//! second source of truth that drifted the moment a model was added upstream.
//! The file now names at most a *default* (`default_model`) and, once `[models]`
//! routes exist, the logical ids the client sees; the metadata behind them
//! still comes from here.
//!
//! The cache is deliberately simple: one snapshot, refreshed when older than
//! [`CATALOG_TTL`], served stale if a refresh fails — the previous list is far
//! more useful than an empty one, and the failure is logged once per attempt,
//! not per request.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::config::{
    BuiltinProvider, CandidateConfig, DEFAULT_PROVIDER_NAME, LlmConfig, SeedModel,
};

/// One model as the client sees it, built from the gateway's model list
/// ([`crate::model_catalog`]).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelEntry {
    /// Provider identifier, e.g. `anthropic`.
    pub provider: String,
    /// Model identifier, e.g. `claude-sonnet-4-5`.
    pub id: String,
    /// Human-readable display name.
    pub name: Option<String>,
    /// Wire model type (`chat`, `text`, `image`, or an unknown provider value).
    /// Missing gateway metadata is treated as `chat` by the API projection.
    #[serde(default, rename = "type")]
    pub model_type: Option<String>,
    /// Context window in tokens. Drives the client's context-usage
    /// percentage (it divides the last assistant turn's tokens by this), so a
    /// missing value means the client shows a raw token count with no
    /// percentage rather than a wrong one.
    #[serde(default)]
    pub context: Option<i64>,
    /// Input token limit, when the provider states one.
    #[serde(default)]
    pub input: Option<i64>,
    /// Maximum output tokens, when the provider states one.
    #[serde(default)]
    pub output: Option<i64>,
    /// Input modalities the model accepts (`text`, `image`), as the endpoint's
    /// own listing reports them. Empty means the listing said nothing, and the
    /// API projection then falls back to `text`.
    #[serde(default)]
    pub input_modalities: Vec<String>,
    /// Output modalities the model produces. Empty as above.
    #[serde(default)]
    pub output_modalities: Vec<String>,
    /// Whether the model accepts a sampling temperature parameter.
    #[serde(default)]
    pub temperature: Option<bool>,
    /// Whether the gateway advertises reasoning effort levels for this model.
    #[serde(default)]
    pub reasoning: bool,
    /// Advertised levels, each as `{ "effort": "<name>" }` in catalog order
    /// (cardinal `models.rs` row shape). Empty unless [`ModelEntry::reasoning`].
    #[serde(default)]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
}

/// One entry of cardinal's `supported_reasoning_levels` (`{effort}` only).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReasoningLevel {
    /// The level name as the chat wire accepts it (`low`, `high`, …).
    pub effort: String,
}

/// How long a fetched catalogue is considered fresh. Model lists change when
/// someone adds a provider upstream, so minutes rather than seconds; a client
/// that wants a fresh list after a change restarts or waits one TTL.
const CATALOG_TTL: Duration = Duration::from_secs(300);

/// `GET /v1/models` — the OpenAI list shape each gateway serves.
const MODELS_PATH: &str = "/v1/models";
/// `GET /models` — where a builtin provider publishes its list (origin root,
/// no version segment and no protocol prefix).
const BUILTIN_MODELS_PATH: &str = "/models";

/// One endpoint to list models from.
struct CatalogEndpoint {
    /// Configured provider name; also the `owned_by` fallback label.
    name: String,
    base_url: String,
    api_key: Option<String>,
    /// Where the endpoint lists models (`/v1/models` for OpenAI-compatible
    /// gateways, `/models` for DeepSeek, which publishes it at the origin).
    models_path: &'static str,
    /// Metadata this build ships for the endpoint's models, used to fill gaps
    /// in the endpoint's own list and as the whole list when it is unreachable.
    seeds: &'static [SeedModel],
    /// Set for a credential-resolved builtin provider: the table row, so the
    /// merge can report its label and the provider name it owns.
    builtin: Option<&'static BuiltinProvider>,
}

/// A physical dimension is never inferred from display metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Known { tokens: u64, source: &'static str },
    Unknown { reason: &'static str },
}

impl Limit {
    #[must_use]
    pub fn tokens(self) -> Option<u64> {
        match self {
            Self::Known { tokens, .. } => Some(tokens),
            Self::Unknown { .. } => None,
        }
    }
}

/// Intersection across every endpoint the router can attempt.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    pub shared_context: Limit,
    pub input: Limit,
    pub output: Limit,
    pub candidates: usize,
    pub stale: bool,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        let unknown = Limit::Unknown {
            reason: "missing_snapshot_or_unverified_geometry",
        };
        Self {
            shared_context: unknown,
            input: unknown,
            output: unknown,
            candidates: 0,
            stale: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CandidateLimits {
    shared_context: Limit,
    input: Limit,
    output: Limit,
    advertised: [Option<i64>; 3],
}

/// A fetched model catalogue with a TTL cache.
pub struct ModelCatalog {
    resolver: crate::config::Routes,
    client: reqwest::Client,
    /// The implicit `cardinal_url` endpoint plus every `[providers]` entry.
    endpoints: Vec<CatalogEndpoint>,
    /// `[models]` route table; non-empty switches the served surface from the
    /// endpoints' raw lists to the route keys.
    routes: BTreeMap<String, Vec<CandidateConfig>>,
    preserve_unrouted: bool,
    /// `[context_caps]` by model id. Applied where the window is read
    /// ([`Self::window_of`]) as `min(reported, cap)` — a cap lowers, never
    /// raises. Inert or unsatisfiable caps warn once per refresh.
    context_caps: BTreeMap<String, u32>,
    cache: RwLock<Snapshot>,
}

#[derive(Default)]
struct Snapshot {
    limits: BTreeMap<String, BTreeMap<String, CandidateLimits>>,
    entries: Arc<Vec<ModelEntry>>,
    fetched_at: Option<Instant>,
}

impl ModelCatalog {
    /// A catalogue for `config`'s endpoints. Nothing is fetched yet:
    /// [`Self::warm`] does that, so building state never blocks on the network.
    /// The config must have passed provider validation ([`crate::config::LlmConfig::validate`]
    /// runs it): candidate lists are non-empty and their providers resolve.
    #[must_use]
    pub fn new(config: &LlmConfig) -> Arc<Self> {
        let client = crate::http_client()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let mut endpoints: BTreeMap<String, CatalogEndpoint> = BTreeMap::new();
        endpoints.insert(
            DEFAULT_PROVIDER_NAME.to_owned(),
            CatalogEndpoint {
                name: DEFAULT_PROVIDER_NAME.to_owned(),
                base_url: config.cardinal_url.trim_end_matches('/').to_owned(),
                api_key: config.cardinal_key.clone().filter(|key| !key.is_empty()),
                models_path: MODELS_PATH,
                seeds: &[],
                builtin: None,
            },
        );
        // A [providers.cardinal] section overrides the implicit endpoint.
        for (name, provider) in &config.providers {
            endpoints.insert(
                name.clone(),
                CatalogEndpoint {
                    name: name.clone(),
                    base_url: provider.base_url.trim_end_matches('/').to_owned(),
                    api_key: provider.api_key.clone().filter(|key| !key.is_empty()),
                    models_path: MODELS_PATH,
                    seeds: &[],
                    builtin: None,
                },
            );
        }
        // Builtin providers list from their own origin: DeepSeek publishes
        // `GET /models` at the root, and its Messages base carries an
        // `/anthropic` prefix that the listing must not inherit.
        for (name, resolved) in &config.builtins {
            endpoints.insert(
                name.clone(),
                CatalogEndpoint {
                    name: name.clone(),
                    base_url: resolved
                        .builtin
                        .models_base_url
                        .trim_end_matches('/')
                        .to_owned(),
                    api_key: resolved
                        .config
                        .api_key
                        .clone()
                        .filter(|key| !key.is_empty()),
                    models_path: BUILTIN_MODELS_PATH,
                    seeds: resolved.builtin.models,
                    builtin: Some(resolved.builtin),
                },
            );
        }
        Arc::new(Self {
            resolver: crate::config::Routes::new(config),
            client,
            endpoints: endpoints.into_values().collect(),
            routes: config
                .models
                .iter()
                .map(|(id, route)| (id.clone(), route.candidates.clone()))
                .collect(),
            preserve_unrouted: config.providers.is_empty(),
            context_caps: config.context_caps.clone(),
            cache: RwLock::new(Snapshot::default()),
        })
    }

    /// A catalogue seeded with `entries` and no fetch — the seam tests use to
    /// pin a model list without a gateway.
    #[must_use]
    pub fn from_entries(entries: Vec<ModelEntry>) -> Self {
        Self {
            resolver: crate::config::Routes::new(&LlmConfig::default()),
            client: crate::http_client().build().unwrap_or_default(),
            endpoints: Vec::new(),
            routes: BTreeMap::new(),
            preserve_unrouted: true,
            context_caps: BTreeMap::new(),
            cache: RwLock::new(Snapshot {
                limits: BTreeMap::new(),
                entries: Arc::new(entries),
                fetched_at: Some(Instant::now()),
            }),
        }
    }

    /// Fetch now, logging rather than failing: an unreachable gateway must not
    /// stop the server from starting (a request will surface the same error in
    /// the turn that needs a model).
    pub async fn warm(&self) {
        if let Err(error) = self.refresh().await {
            tracing::warn!(error = %error, "model catalogue unavailable; the client will see no models until a refresh succeeds");
        }
    }

    /// The catalogue, refreshing when the snapshot is stale.
    pub async fn entries(&self) -> Arc<Vec<ModelEntry>> {
        let stale = {
            let cache = self.cache.read().expect("catalog lock");
            cache
                .fetched_at
                .is_none_or(|at| at.elapsed() >= CATALOG_TTL)
        };
        if stale && let Err(error) = self.refresh().await {
            tracing::warn!(error = %error, "model catalogue refresh failed; serving the previous list");
        }
        self.cached()
    }

    /// Refresh a stale snapshot and report refresh failure to admission callers.
    /// Unlike [`Self::entries`], this never serves stale metadata after a failed
    /// refresh: safety-critical request sizing must fail closed.
    pub async fn refresh_if_stale(&self) -> Result<()> {
        let stale = {
            let cache = self.cache.read().expect("catalog lock");
            cache
                .fetched_at
                .is_none_or(|at| at.elapsed() >= CATALOG_TTL)
        };
        if stale {
            self.refresh().await?;
        }
        Ok(())
    }

    /// The current snapshot without any IO — for paths that must not wait on
    /// the network (a step boundary picking its default model).
    pub fn cached(&self) -> Arc<Vec<ModelEntry>> {
        Arc::clone(&self.cache.read().expect("catalog lock").entries)
    }

    /// The model backing sessions without an explicit selection:
    /// `config.default_model` when it names a model the gateway reports, the
    /// configured reference verbatim when the catalogue does not know it (the
    /// gateway decides), else the first model by id. `None` only when the
    /// catalogue is empty and nothing is configured.
    pub fn default_entry(&self, config: &LlmConfig) -> Option<ModelEntry> {
        pick_default(&self.cached(), config)
    }

    fn is_chat_compatible(entry: &ModelEntry) -> bool {
        matches!(
            entry.model_type.as_deref().unwrap_or("chat"),
            "chat" | "text"
        )
    }

    /// The context window the gateway reports for `id` (the bare model id, as
    /// the reporting endpoint states it), lowered by the model's
    /// `[context_caps]` entry when one exists: `min(reported, cap)`. A cap
    /// never raises the window, and a cap on a model without a reported
    /// context does nothing — the unknown-window fallback downstream stands.
    /// The min happens in `i64` space before the `u32` conversion, so a cap
    /// stays effective even when the report exceeds `u32::MAX`. `None` when
    /// the catalogue does not know the model or reports no usable context.
    /// Never refreshes: callers on a step boundary must not wait on the
    /// network.
    pub fn window_of(&self, id: &str) -> Option<u32> {
        self.cached()
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| self.window_of_for(&entry.provider, id))
    }

    /// The context window for the exact provider and model identity.
    pub fn window_of_for(&self, provider: &str, id: &str) -> Option<u32> {
        let context = self
            .cached()
            .iter()
            .find(|entry| entry.provider == provider && entry.id == id)?
            .context?;
        let context = match self.context_caps.get(id) {
            Some(cap) => context.min(i64::from(*cap)),
            None => context,
        };
        u32::try_from(context).ok().filter(|window| *window > 0)
    }

    /// Physical limits for the exact logical wire model; never uses UI projection.
    pub fn runtime_limits(&self, model: &str) -> Result<RuntimeLimits, crate::error::ChatError> {
        self.runtime_limits_for(None, model)
    }

    /// Physical limits for a selected provider and logical model identity.
    pub fn runtime_limits_for(
        &self,
        provider: Option<&str>,
        model: &str,
    ) -> Result<RuntimeLimits, crate::error::ChatError> {
        use crate::config::Targets;
        let cache = self.cache.read().expect("catalog lock");
        let mut result = RuntimeLimits {
            stale: cache
                .fetched_at
                .is_none_or(|at| at.elapsed() >= CATALOG_TTL),
            ..RuntimeLimits::default()
        };
        let mut add = |provider: &str, wire: &str| {
            let candidate = cache
                .limits
                .get(provider)
                .and_then(|models| models.get(wire));
            let unknown = Limit::Unknown {
                reason: "missing_endpoint_model",
            };
            let context = candidate.map_or(unknown, |limits| limits.shared_context);
            // A known shared window is an upper bound on input, but an
            // undisclosed smaller input cap can still cause upstream rejection.
            // Never borrow geometry from a different provider.
            let input = candidate.map_or(unknown, |limits| match limits.input {
                Limit::Known { .. } => limits.input,
                Limit::Unknown { .. } => limits.shared_context,
            });
            let output = candidate.map_or(unknown, |limits| limits.output);
            if let Some(candidate) = candidate {
                tracing::debug!(provider, model = wire, advertised = ?candidate.advertised, "runtime candidate advertised limits");
            }
            result.shared_context = intersect(result.shared_context, context, result.candidates);
            result.input = intersect(result.input, input, result.candidates);
            result.output = intersect(result.output, output, result.candidates);
            result.candidates += 1;
        };
        match self.resolver.resolve(provider, model) {
            Targets::Explicit(candidates) => {
                for candidate in candidates {
                    add(&candidate.provider, &candidate.model);
                }
            }
            Targets::Endpoint { provider, model } => add(provider, model),
            Targets::NoRoute => {
                return Err(crate::error::ChatError::NoRoute {
                    model: model.to_owned(),
                });
            }
        }
        if let Some(cap) = self.context_caps.get(model)
            && let Limit::Known { tokens, .. } = result.shared_context
        {
            result.shared_context = Limit::Known {
                tokens: tokens.min(u64::from(*cap)),
                source: "context_cap",
            };
        }
        Ok(result)
    }

    /// Fetch `GET /v1/models` from every endpoint and replace the snapshot.
    /// One failing endpoint only warns; the others still serve. When every
    /// endpoint fails the refresh errors, so the previous list is kept
    /// (legacy single-endpoint behavior included). A catalogue with no
    /// endpoints (the [`Self::from_entries`] seed) is returned untouched.
    /// Reload calls this to pre-warm a new catalogue before installing it.
    pub async fn refresh(&self) -> Result<()> {
        if self.endpoints.is_empty() {
            // `from_entries` seeds a fetchless catalogue; a TTL refresh must
            // not replace the seed with an empty snapshot.
            return Ok(());
        }
        let mut entries: Vec<ModelEntry> = Vec::new();
        let mut limits = BTreeMap::new();
        let mut failures = 0usize;
        for endpoint in &self.endpoints {
            match self.fetch_endpoint(endpoint, &mut limits).await {
                Ok(mut fetched) => {
                    entries.append(&mut fetched);
                }
                // A builtin ships its model table, so an unreachable list is a
                // degraded start, not an empty catalogue: the seeds are the
                // same entries the endpoint would have reported, minus any it
                // added since this build.
                Err(error) if !endpoint.seeds.is_empty() => {
                    tracing::warn!(
                        provider = %endpoint.name,
                        error = %error,
                        "model list fetch failed; serving the built-in model seeds"
                    );
                    record_seed_limits(&mut limits, endpoint);
                    entries.extend(seeded_entries(endpoint));
                }
                Err(error) => {
                    failures += 1;
                    tracing::warn!(
                        provider = %endpoint.name,
                        error = %error,
                        "model list fetch failed; other endpoints are unaffected"
                    );
                }
            }
        }
        if failures == self.endpoints.len() {
            anyhow::bail!("all {} model-list endpoints failed", failures);
        }
        // A stable order: the default falls back to the first entry, so the
        // order must not depend on how the gateway happened to serialise.
        let builtin_providers: Vec<&BuiltinProvider> = self
            .endpoints
            .iter()
            .filter_map(|endpoint| endpoint.builtin)
            .collect();
        entries = Self::merge_entries(
            &self.routes,
            entries,
            self.preserve_unrouted,
            &builtin_providers,
        );
        // Caps that cannot bite are configuration drift the operator should
        // see: at or over the reported window (inert), or on a model with no
        // reported context at all (inactive — the unknown-window fallback
        // governs, not the cap).
        for (id, cap) in &self.context_caps {
            match entries
                .iter()
                .find(|entry| &entry.id == id)
                .and_then(|entry| entry.context)
            {
                // The cap lowers the window (`window_of` takes the minimum), so
                // it bites exactly when the catalogue reports more than it.
                Some(reported) if reported > i64::from(*cap) => {}
                Some(reported) => tracing::warn!(
                    model = %id, cap = cap, reported = reported,
                    "context cap is inert: the catalogue reports no more than this"
                ),
                None => tracing::warn!(
                    model = %id, cap = cap,
                    "context cap is inactive: the catalogue reports no context length for this model"
                ),
            }
        }
        let count = entries.len();
        {
            let mut cache = self.cache.write().expect("catalog lock");
            cache.entries = Arc::new(entries);
            cache.limits = limits;
            cache.fetched_at = Some(Instant::now());
        }
        tracing::debug!(models = count, "model catalogue refreshed");
        Ok(())
    }

    /// Fetch and parse one endpoint's model list.
    async fn fetch_endpoint(
        &self,
        endpoint: &CatalogEndpoint,
        limits: &mut BTreeMap<String, BTreeMap<String, CandidateLimits>>,
    ) -> Result<Vec<ModelEntry>> {
        let url = format!("{}{}", endpoint.base_url, endpoint.models_path);
        let mut request = self.client.get(&url);
        if let Some(key) = &endpoint.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("call GET {url}"))?;
        let status = response.status();
        let body = response.text().await.context("read the model list")?;
        if !status.is_success() {
            anyhow::bail!(
                "GET {url} returned HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            );
        }
        let rows = parse_list(&body)?;
        record_limits(limits, endpoint, &rows);
        let mut entries: Vec<ModelEntry> = rows
            .into_iter()
            .map(|row| row.into_entry(&endpoint.name))
            .collect();
        for entry in &mut entries {
            merge_seed(entry, endpoint.seeds);
        }
        if entries.is_empty() && !endpoint.seeds.is_empty() {
            record_seed_limits(limits, endpoint);
            entries = seeded_entries(endpoint);
        }
        Ok(entries)
    }

    fn merge_entries(
        routes: &BTreeMap<String, Vec<CandidateConfig>>,
        mut entries: Vec<ModelEntry>,
        preserve_unrouted: bool,
        builtin_providers: &[&BuiltinProvider],
    ) -> Vec<ModelEntry> {
        entries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.provider.cmp(&right.provider))
        });
        entries.dedup_by(|left, right| left.id == right.id && left.provider == right.provider);
        if routes.is_empty() {
            if preserve_unrouted {
                return entries;
            }
            entries.retain(|entry| is_builtin_entry(entry, builtin_providers));
            return entries;
        }
        Self::project_routes(routes, entries, preserve_unrouted, builtin_providers)
    }

    /// The served model face when `[models]` is configured: one entry per
    /// route key, provider = the first candidate's provider (the routing
    /// truth). The first candidate donates `context`/`reasoning`/levels from
    /// the endpoint-reported entry of its wire model, but only when exactly
    /// one entry has that id — an id reported by several endpoints is
    /// ambiguous and donates nothing. Best effort: a missing donor leaves a
    /// usable entry with unknown context.
    fn project_routes(
        routes: &BTreeMap<String, Vec<CandidateConfig>>,
        raw: Vec<ModelEntry>,
        preserve_unrouted: bool,
        builtin_providers: &[&BuiltinProvider],
    ) -> Vec<ModelEntry> {
        let donors = raw;
        let mut entries = if preserve_unrouted {
            donors.clone()
        } else {
            donors
                .iter()
                .filter(|entry| is_builtin_entry(entry, builtin_providers))
                .cloned()
                .collect()
        };
        for (id, route) in routes {
            let first = route.first();
            let donor = first.and_then(|candidate| {
                let mut matches = donors.iter().filter(|entry| entry.id == candidate.model);
                let first_match = matches.next()?;
                matches.next().is_none().then_some(first_match)
            });
            let projection = ModelEntry {
                id: id.clone(),
                provider: first
                    .map(|candidate| candidate.provider.clone())
                    .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_owned()),
                context: donor.and_then(|entry| entry.context),
                input: donor.and_then(|entry| entry.input),
                output: donor.and_then(|entry| entry.output),
                input_modalities: donor
                    .map(|entry| entry.input_modalities.clone())
                    .unwrap_or_default(),
                output_modalities: donor
                    .map(|entry| entry.output_modalities.clone())
                    .unwrap_or_default(),
                name: donor.and_then(|entry| entry.name.clone()),
                model_type: donor.and_then(|entry| entry.model_type.clone()),
                temperature: donor.and_then(|entry| entry.temperature),
                reasoning: donor.map(|entry| entry.reasoning).unwrap_or(false),
                supported_reasoning_levels: donor
                    .map(|entry| entry.supported_reasoning_levels.clone())
                    .unwrap_or_default(),
            };
            entries
                .retain(|entry| entry.id != projection.id || entry.provider != projection.provider);
            entries.push(projection);
        }
        if !preserve_unrouted {
            entries.retain(|entry| {
                is_builtin_entry(entry, builtin_providers)
                    || routes.get(&entry.id).is_some_and(|route| {
                        route
                            .first()
                            .is_some_and(|candidate| candidate.provider == entry.provider)
                    })
            });
        }
        entries.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.provider.cmp(&right.provider))
        });
        entries.dedup_by(|left, right| left.id == right.id && left.provider == right.provider);
        entries
    }
}

fn is_builtin_entry(entry: &ModelEntry, builtins: &[&BuiltinProvider]) -> bool {
    builtins
        .iter()
        .any(|builtin| builtin.id == entry.provider && builtin.owns(&entry.id))
}

fn intersect(previous: Limit, next: Limit, count: usize) -> Limit {
    if count == 0 {
        return next;
    }
    match (previous, next) {
        (Limit::Known { tokens: left, .. }, Limit::Known { tokens: right, .. }) => Limit::Known {
            tokens: left.min(right),
            source: "all_candidates_minimum",
        },
        (Limit::Unknown { reason }, _) | (_, Limit::Unknown { reason }) => {
            Limit::Unknown { reason }
        }
    }
}

fn record_limits(
    limits: &mut BTreeMap<String, BTreeMap<String, CandidateLimits>>,
    endpoint: &CatalogEndpoint,
    entries: &[ModelRow],
) {
    for entry in entries {
        let unknown = Limit::Unknown {
            reason: "unverified_geometry",
        };
        let seed = endpoint
            .builtin
            .and_then(|builtin| builtin.models.iter().find(|seed| seed.id == entry.id));
        let known = |tokens: Option<i64>, source| {
            tokens
                .and_then(|tokens| u64::try_from(tokens).ok())
                .filter(|tokens| *tokens > 0)
                .map_or(unknown, |tokens| Limit::Known { tokens, source })
        };
        let context = entry.context_length.or(entry.context_window);
        let candidate = CandidateLimits {
            shared_context: if context.is_some() {
                known(context, "endpoint_advertised")
            } else {
                known(seed.map(|seed| seed.context), "builtin_seed")
            },
            input: known(entry.max_input_tokens, "endpoint_advertised"),
            output: if entry.max_output_tokens.is_some() {
                known(entry.max_output_tokens, "endpoint_advertised")
            } else {
                known(seed.map(|seed| seed.output), "builtin_seed")
            },
            advertised: [context, entry.max_input_tokens, entry.max_output_tokens],
        };
        limits
            .entry(endpoint.name.clone())
            .or_default()
            .entry(entry.id.clone())
            .and_modify(|existing| {
                existing.shared_context = Limit::Unknown {
                    reason: "ambiguous_endpoint_model",
                };
                existing.input = existing.shared_context;
                existing.output = existing.shared_context;
            })
            .or_insert(candidate);
    }
}

fn record_seed_limits(
    limits: &mut BTreeMap<String, BTreeMap<String, CandidateLimits>>,
    endpoint: &CatalogEndpoint,
) {
    let Some(builtin) = endpoint.builtin else {
        return;
    };
    let known = |tokens: i64| {
        u64::try_from(tokens)
            .ok()
            .filter(|tokens| *tokens > 0)
            .map_or(
                Limit::Unknown {
                    reason: "unverified_geometry",
                },
                |tokens| Limit::Known {
                    tokens,
                    source: "builtin_seed",
                },
            )
    };
    let models = limits.entry(endpoint.name.clone()).or_default();
    for seed in builtin.models {
        models.insert(
            seed.id.to_owned(),
            CandidateLimits {
                shared_context: known(seed.context),
                input: Limit::Unknown {
                    reason: "unverified_geometry",
                },
                output: known(seed.output),
                advertised: [None; 3],
            },
        );
    }
}

/// The default-model decision over one catalogue snapshot.
///
/// Pure — no cache and no IO — so the rule can be exercised without a gateway:
/// `config.default_model` when the snapshot reports it, the configured
/// reference verbatim when it does not (the endpoint decides), a materialized
/// builtin's first model when nothing is pinned, else the first chat-capable
/// entry.
fn pick_default(entries: &[ModelEntry], config: &LlmConfig) -> Option<ModelEntry> {
    if let Some(qualified) = &config.default_model {
        let (provider, id) = qualified
            .split_once('/')
            .unwrap_or((DEFAULT_PROVIDER_NAME, qualified.as_str()));
        if let Some(found) = entries
            .iter()
            .find(|entry| entry.id == id && entry.provider == provider)
        {
            if ModelCatalog::is_chat_compatible(found) {
                return Some(found.clone());
            }
            return entries
                .iter()
                .find(|entry| ModelCatalog::is_chat_compatible(entry))
                .cloned();
        }
        return Some(ModelEntry {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: None,
            model_type: None,
            context: None,
            input: None,
            output: None,
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
            temperature: None,
            reasoning: false,
            supported_reasoning_levels: Vec::new(),
        });
    }
    // A materialized builtin exists only because the operator supplied a
    // credential for it, which is a stronger statement of intent than the
    // implicit gateway: its first model wins when nothing is pinned.
    entries
        .iter()
        .find(|entry| ModelCatalog::is_chat_compatible(entry) && config.is_builtin(&entry.provider))
        .or_else(|| {
            entries
                .iter()
                .find(|entry| ModelCatalog::is_chat_compatible(entry))
        })
        .cloned()
}

/// The seed entries that stand in for an endpoint whose list is unreachable.
///
/// The provider label is the builtin's own id, the same string its listing
/// puts in `owned_by`, so the projection and the routing agree about who owns
/// these models.
fn seeded_entries(endpoint: &CatalogEndpoint) -> Vec<ModelEntry> {
    endpoint
        .seeds
        .iter()
        .map(|seed| seed_entry(endpoint, seed))
        .collect()
}

/// One seed rendered as a catalogue entry.
fn seed_entry(endpoint: &CatalogEndpoint, seed: &SeedModel) -> ModelEntry {
    ModelEntry {
        provider: endpoint
            .builtin
            .map_or_else(|| endpoint.name.clone(), |builtin| builtin.id.to_owned()),
        id: seed.id.to_owned(),
        name: None,
        model_type: None,
        context: Some(seed.context),
        input: None,
        output: Some(seed.output),
        input_modalities: Vec::new(),
        output_modalities: Vec::new(),
        temperature: Some(false),
        reasoning: !seed.reasoning.is_empty(),
        supported_reasoning_levels: seed
            .reasoning
            .iter()
            .map(|effort| ReasoningLevel {
                effort: (*effort).to_owned(),
            })
            .collect(),
    }
}

/// Fill an endpoint-reported entry from the build's seed for the same id.
///
/// The endpoint wins wherever it states something (a newer context window is
/// real); the seed only fills what the listing left out — which, for
/// DeepSeek's short rows, is the output cap and every reasoning level.
fn merge_seed(entry: &mut ModelEntry, seeds: &'static [SeedModel]) {
    let Some(seed) = seeds.iter().find(|seed| seed.id == entry.id) else {
        return;
    };
    entry.context = entry.context.or(Some(seed.context));
    entry.output = entry.output.or(Some(seed.output));
    entry.temperature.get_or_insert(false);
    if !seed.reasoning.is_empty() {
        entry.supported_reasoning_levels =
            merge_levels(&entry.supported_reasoning_levels, seed.reasoning);
        entry.reasoning = !entry.supported_reasoning_levels.is_empty();
    }
}

/// Merge the seed's levels into whatever the endpoint reported.
///
/// The endpoint states which levels it accepts (DeepSeek lists `low`, `high`,
/// `max`); the seed adds the levels only the harness can express — `off` is
/// the absence of thinking, which a listing never mentions. Ordered by a
/// canonical ladder so the picker stays stable when both sources contribute.
fn merge_levels(endpoint: &[ReasoningLevel], seed: &[&'static str]) -> Vec<ReasoningLevel> {
    let mut efforts: Vec<&str> = endpoint.iter().map(|level| level.effort.as_str()).collect();
    for effort in seed {
        if !efforts
            .iter()
            .any(|known| known.eq_ignore_ascii_case(effort))
        {
            efforts.push(effort);
        }
    }
    efforts.sort_by_key(|effort| level_rank(effort));
    efforts
        .into_iter()
        .map(|effort| ReasoningLevel {
            effort: effort.to_owned(),
        })
        .collect()
}

/// Rank a reasoning level on the canonical ladder; unknown levels sort last
/// and keep their relative order.
fn level_rank(effort: &str) -> u8 {
    match effort.to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => u8::MAX,
    }
}

/// Decode one endpoint's model list, dropping rows this build cannot read.
///
/// A provider may report a field of its own, or change a type under a name we
/// decode; one unreadable row must not cost the rest of the list. Dropped rows
/// are counted and warned so a shape change still shows up in the log rather
/// than silently shrinking the catalogue.
fn parse_list(body: &str) -> Result<Vec<ModelRow>> {
    let value: serde_json::Value = serde_json::from_str(body).context("parse the model list")?;
    let rows = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .context("the model list has no `data` array")?;
    let mut decoded = Vec::with_capacity(rows.len());
    let mut unreadable = 0usize;
    for row in rows {
        match serde_json::from_value::<ModelRow>(row.clone()) {
            Ok(row) => decoded.push(row),
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        tracing::warn!(
            unreadable,
            decoded = decoded.len(),
            "skipped model rows this build cannot read"
        );
    }
    Ok(decoded)
}

/// One entry of cardinal's model list. Every field but `id` is optional: a
/// gateway that reports less must not make the list unusable.
#[derive(Deserialize)]
struct ModelRow {
    id: String,
    #[serde(default, rename = "type")]
    model_type: Option<String>,
    #[serde(default)]
    owned_by: Option<String>,
    /// cardinal reports all three; other gateways may report one.
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
    /// Output cap, as DeepSeek's listing reports it.
    #[serde(default)]
    max_output_tokens: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    /// Modalities the listing advertises; DeepSeek reports `["text","image"]`
    /// for its multimodal models and omits the fields entirely for the rest.
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    /// DeepSeek states effort levels in one object; cardinal states them in
    /// `supported_reasoning_levels`. Both are read.
    #[serde(default)]
    effort: Option<ModelEffort>,
    #[serde(default)]
    temperature: Option<bool>,
    /// Reasoning metadata, present only on rows whose upstream members
    /// advertise effort levels (cardinal `models.rs`).
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    supported_reasoning_levels: Vec<ReasoningLevel>,
}

/// DeepSeek's effort block on a model row: the levels the model accepts.
///
/// `default_level` is ignored rather than parsed: upstream `Model.Info` has no
/// field for it, so the catalogue would have nowhere to publish it.
#[derive(Deserialize)]
struct ModelEffort {
    #[serde(default)]
    supported_levels: Vec<String>,
}

impl ModelRow {
    fn into_entry(self, fallback_provider: &str) -> ModelEntry {
        let mut supported_reasoning_levels = self.supported_reasoning_levels;
        if let Some(effort) = self.effort {
            for level in effort.supported_levels {
                let known = supported_reasoning_levels
                    .iter()
                    .any(|existing| existing.effort.eq_ignore_ascii_case(&level));
                if !known {
                    supported_reasoning_levels.push(ReasoningLevel { effort: level });
                }
            }
        }
        ModelEntry {
            provider: self
                .owned_by
                .unwrap_or_else(|| fallback_provider.to_owned()),
            name: self.name,
            model_type: self.model_type,
            context: self
                .context_length
                .or(self.context_window)
                .or(self.max_input_tokens),
            input: self.max_input_tokens,
            output: self.max_output_tokens,
            input_modalities: self.input_modalities,
            output_modalities: self.output_modalities,
            temperature: self.temperature,
            id: self.id,
            reasoning: self.reasoning || !supported_reasoning_levels.is_empty(),
            supported_reasoning_levels,
        }
    }
}
