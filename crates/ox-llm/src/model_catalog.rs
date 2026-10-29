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

use crate::config::{CandidateConfig, DEFAULT_PROVIDER_NAME, LlmConfig};

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

/// `GET /v1/models` — the OpenAI list shape each endpoint serves.
const MODELS_PATH: &str = "/v1/models";

/// One endpoint to list models from.
struct CatalogEndpoint {
    /// Configured provider name; also the `owned_by` fallback label.
    name: String,
    base_url: String,
    api_key: Option<String>,
}

/// A fetched model catalogue with a TTL cache.
pub struct ModelCatalog {
    client: reqwest::Client,
    /// The implicit `cardinal_url` endpoint plus every `[providers]` entry.
    endpoints: Vec<CatalogEndpoint>,
    /// `[models]` route table; non-empty switches the served surface from the
    /// endpoints' raw lists to the route keys.
    routes: BTreeMap<String, Vec<CandidateConfig>>,
    /// `[context_caps]` by model id. Applied where the window is read
    /// ([`Self::window_of`]) as `min(reported, cap)` — a cap lowers, never
    /// raises. Inert or unsatisfiable caps warn once per refresh.
    context_caps: BTreeMap<String, u32>,
    cache: RwLock<Snapshot>,
}

#[derive(Default)]
struct Snapshot {
    entries: Arc<Vec<ModelEntry>>,
    fetched_at: Option<Instant>,
}

impl ModelCatalog {
    /// A catalogue for `config`'s endpoints. Nothing is fetched yet:
    /// [`Self::warm`] does that, so building state never blocks on the network.
    /// The config must have passed provider validation ([`crate::config::LlmConfig::validate`]
    /// runs it): candidate lists are non-empty and their providers resolve.
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
                },
            );
        }
        Arc::new(Self {
            client,
            endpoints: endpoints.into_values().collect(),
            routes: config
                .models
                .iter()
                .map(|(id, route)| (id.clone(), route.candidates.clone()))
                .collect(),
            context_caps: config.context_caps.clone(),
            cache: RwLock::new(Snapshot::default()),
        })
    }

    /// A catalogue seeded with `entries` and no fetch — the seam tests use to
    /// pin a model list without a gateway.
    pub fn from_entries(entries: Vec<ModelEntry>) -> Self {
        Self {
            client: crate::http_client().build().unwrap_or_default(),
            endpoints: Vec::new(),
            routes: BTreeMap::new(),
            context_caps: BTreeMap::new(),
            cache: RwLock::new(Snapshot {
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
        let entries = self.cached();
        if let Some(qualified) = &config.default_model {
            let (provider, id) = qualified
                .split_once('/')
                .unwrap_or((DEFAULT_PROVIDER_NAME, qualified.as_str()));
            if let Some(found) = entries
                .iter()
                .find(|entry| entry.id == id && entry.provider == provider)
            {
                if Self::is_chat_compatible(found) {
                    return Some(found.clone());
                }
                return entries
                    .iter()
                    .find(|entry| Self::is_chat_compatible(entry))
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
                temperature: None,
                reasoning: false,
                supported_reasoning_levels: Vec::new(),
            });
        }
        entries
            .iter()
            .find(|entry| {
                matches!(
                    entry.model_type.as_deref().unwrap_or("chat"),
                    "chat" | "text"
                )
            })
            .cloned()
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
        let context = self.cached().iter().find(|entry| entry.id == id)?.context?;
        let context = match self.context_caps.get(id) {
            Some(cap) => context.min(i64::from(*cap)),
            None => context,
        };
        u32::try_from(context).ok().filter(|window| *window > 0)
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
        let mut failures = 0usize;
        for endpoint in &self.endpoints {
            match self.fetch_endpoint(endpoint).await {
                Ok(mut fetched) => entries.append(&mut fetched),
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
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        entries.dedup_by(|left, right| left.id == right.id && left.provider == right.provider);
        if !self.routes.is_empty() {
            entries = Self::project_routes(&self.routes, &entries);
        }
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
            cache.fetched_at = Some(Instant::now());
        }
        tracing::debug!(models = count, "model catalogue refreshed");
        Ok(())
    }

    /// Fetch and parse one endpoint's model list.
    async fn fetch_endpoint(&self, endpoint: &CatalogEndpoint) -> Result<Vec<ModelEntry>> {
        let mut request = self
            .client
            .get(format!("{}{MODELS_PATH}", endpoint.base_url));
        if let Some(key) = &endpoint.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.context("call GET /v1/models")?;
        let status = response.status();
        let body = response.text().await.context("read the model list")?;
        if !status.is_success() {
            anyhow::bail!(
                "GET /v1/models returned HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            );
        }
        let list: ModelList = serde_json::from_str(&body).context("parse the model list")?;
        Ok(list
            .data
            .into_iter()
            .map(|row| row.into_entry(&endpoint.name))
            .collect())
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
        raw: &[ModelEntry],
    ) -> Vec<ModelEntry> {
        routes
            .iter()
            .map(|(id, route)| {
                let first = route.first();
                let donor = first.and_then(|candidate| {
                    let mut matches = raw.iter().filter(|entry| entry.id == candidate.model);
                    let first_match = matches.next()?;
                    matches.next().is_none().then_some(first_match)
                });
                ModelEntry {
                    id: id.clone(),
                    provider: first
                        .map(|candidate| candidate.provider.clone())
                        .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_owned()),
                    context: donor.and_then(|entry| entry.context),
                    input: donor.and_then(|entry| entry.input),
                    output: donor.and_then(|entry| entry.output),
                    name: donor.and_then(|entry| entry.name.clone()),
                    model_type: donor.and_then(|entry| entry.model_type.clone()),
                    temperature: donor.and_then(|entry| entry.temperature),
                    reasoning: donor.map(|entry| entry.reasoning).unwrap_or(false),
                    supported_reasoning_levels: donor
                        .map(|entry| entry.supported_reasoning_levels.clone())
                        .unwrap_or_default(),
                }
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelRow>,
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
    #[serde(default)]
    temperature: Option<bool>,
    /// Reasoning metadata, present only on rows whose upstream members
    /// advertise effort levels (cardinal `models.rs`).
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    supported_reasoning_levels: Vec<ReasoningLevel>,
}

impl ModelRow {
    fn into_entry(self, fallback_provider: &str) -> ModelEntry {
        ModelEntry {
            provider: self
                .owned_by
                .unwrap_or_else(|| fallback_provider.to_owned()),
            name: None,
            model_type: self.model_type,
            context: self
                .context_length
                .or(self.context_window)
                .or(self.max_input_tokens),
            input: self.max_input_tokens,
            output: None,
            temperature: self.temperature,
            id: self.id,
            reasoning: self.reasoning,
            supported_reasoning_levels: self.supported_reasoning_levels,
        }
    }
}
