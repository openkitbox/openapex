//! LLM endpoint configuration: the implicit `cardinal_url` endpoint, extra
//! `[providers]`, `[models]` routes, `[context_caps]` and the default model.
//!
//! [`LlmConfig`] is flattened into `ox-core`'s `Config`, so the TOML keys stay
//! at the top level of `ox.toml`.

use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::credentials::Credentials;

const DEFAULT_CARDINAL_URL: &str = "http://127.0.0.1:10202";

/// Environment variable overrides.
pub const ENV_CARDINAL_URL: &str = "OX_CARDINAL_URL";
pub const ENV_CARDINAL_KEY: &str = "OX_CARDINAL_KEY";
pub const ENV_DEFAULT_MODEL: &str = "OX_DEFAULT_MODEL";

fn default_cardinal_url() -> String {
    DEFAULT_CARDINAL_URL.to_owned()
}

/// The name of the implicit endpoint built from `cardinal_url`/`cardinal_key`.
/// Route candidates may reference it like any `[providers]` name; a
/// `[providers.cardinal]` section overrides the implicit endpoint.
pub const DEFAULT_PROVIDER_NAME: &str = "cardinal";

/// The stall timeout applied to an endpoint that sets `max_concurrent` but no
/// `stall_timeout_secs`: a concurrency limit without a liveness bound lets a
/// hung stream hold a permit forever. Generous because reasoning models can
/// stay silent for minutes before their first byte.
pub const DEFAULT_LIMITED_STALL: Duration = Duration::from_secs(600);

/// Per-endpoint traffic limits, shared by `[providers.<name>]` (flattened
/// into [`ProviderConfig`]) and `[cardinal_limits]` (the implicit
/// `cardinal_url` endpoint, which `[providers.cardinal]` cannot configure
/// without replacing it).
///
/// Both keys are optional and, when present, must be integers `>= 1`
/// (`0`, negatives and non-integers fail at load). With neither set the
/// endpoint behaves exactly as before: no queue, no timeout.
///
/// Operational notes:
/// - Queueing is FIFO and per endpoint; every model route that names the same
///   endpoint shares its limit.
/// - Values are read once at startup; changing them needs a restart.
/// - Misspelled keys are silently ignored (no `deny_unknown_fields`), which
///   means "no limit". The startup log lists each endpoint's effective limits.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct EndpointLimits {
    /// Maximum requests in flight to this endpoint; further requests wait in
    /// FIFO order instead of failing. Unset = unlimited.
    ///
    /// Recommend `>= 2`: with `1`, a long background fold (historian,
    /// dreamer) can block the foreground turn. An endpoint that is down holds
    /// each permit for up to the 10s connect timeout, so queued requests wait
    /// up to `ceil(queued / max_concurrent) * 10s` before failing over to a
    /// backup candidate.
    #[serde(default)]
    pub max_concurrent: Option<NonZeroUsize>,
    /// Maximum silence, in seconds, between bytes from this endpoint (while
    /// waiting for response headers and between body reads). It is **not** a
    /// total deadline: a stream that keeps producing bytes lives forever. A
    /// stall surfaces as a transport error (retried / failed over like any
    /// other). Unset: [`DEFAULT_LIMITED_STALL`] (600s) when `max_concurrent`
    /// is set, otherwise no timeout.
    #[serde(default)]
    pub stall_timeout_secs: Option<NonZeroU64>,
}

impl EndpointLimits {
    /// The silence bound to install on the endpoint's HTTP client: the
    /// explicit value if given, else [`DEFAULT_LIMITED_STALL`] when a
    /// concurrency limit is set, else `None`.
    #[must_use]
    pub fn stall_timeout(&self) -> Option<Duration> {
        match (self.stall_timeout_secs, self.max_concurrent) {
            (Some(secs), _) => Some(Duration::from_secs(secs.get())),
            (None, Some(_)) => Some(DEFAULT_LIMITED_STALL),
            (None, None) => None,
        }
    }
}

/// The wire protocol an endpoint speaks.
///
/// The protocol decides both the request shape and the path ox appends to
/// `base_url`, so the two can never disagree:
///
/// | protocol | path appended to `base_url` |
/// |---|---|
/// | [`Protocol::ChatCompletions`] | `/v1/chat/completions` (and `/v1/models`) |
/// | [`Protocol::Messages`] | `/v1/messages` (and `/v1/models`) |
///
/// `base_url` is still "the part before the version suffix": for a Messages
/// endpoint that already carries its own path prefix (DeepSeek publishes
/// `https://api.deepseek.com/anthropic`), configure that whole prefix — ox
/// only appends `/v1/messages`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Protocol {
    /// OpenAI Chat Completions — the historical ox default.
    #[default]
    #[serde(rename = "chat_completions", alias = "completions")]
    ChatCompletions,
    /// Anthropic Messages (also served by DeepSeek under `/anthropic`).
    #[serde(rename = "messages")]
    Messages,
}

impl Protocol {
    /// The path ox appends to a provider's `base_url` for a streaming chat
    /// request.
    #[must_use]
    pub fn chat_path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Messages => "/v1/messages",
        }
    }

    /// The path ox appends to a provider's `base_url` to list models.
    #[must_use]
    pub fn models_path(self) -> &'static str {
        "/v1/models"
    }
}

/// One configured outbound endpoint (`[providers.<name>]`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    /// Base URL **without** the version suffix: ox appends the protocol's
    /// path (see [`Protocol`]). For vendors publishing `…/v1` or `…/v4`
    /// bases, configure the part before the suffix (`https://api.moonshot.ai`,
    /// `https://open.bigmodel.cn/api/paas`); a vendor that publishes a
    /// protocol prefix (`https://api.deepseek.com/anthropic`) keeps it.
    ///
    /// Omitting it is only valid for a builtin id, which then takes the
    /// builtin's own endpoint (see [`LlmConfig::apply_credentials`]).
    #[serde(default)]
    pub base_url: String,
    /// Bearer key for `Authorization`; omitted for gateways that need none.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Wire protocol this endpoint speaks. Unstated resolves to chat
    /// completions, except for a `[providers.<id>]` section that omits
    /// `base_url` for a builtin id: that section inherits the builtin's
    /// endpoint *and* protocol (see [`LlmConfig::apply_credentials`]).
    #[serde(default)]
    pub protocol: Option<Protocol>,
    /// `max_concurrent` / `stall_timeout_secs` for this endpoint; see
    /// [`EndpointLimits`].
    #[serde(default, flatten)]
    pub limits: EndpointLimits,
}

impl ProviderConfig {
    /// The protocol this endpoint speaks: the stated one, else the historical
    /// chat-completions default.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.protocol.unwrap_or_default()
    }
}

/// One model a builtin provider ships metadata for.
///
/// The catalogue still prefers what the endpoint reports (`GET /v1/models`);
/// these seeds carry what a list often omits (output limits, reasoning levels)
/// and stand in when the list is unreachable on a fresh start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedModel {
    /// Wire model id.
    pub id: &'static str,
    /// Context window in tokens.
    pub context: i64,
    /// Maximum output tokens.
    pub output: i64,
    /// Reasoning variant names the model accepts, in display order; empty for
    /// a model that does not think.
    pub reasoning: &'static [&'static str],
}

/// A provider ox ships defaults for: base URLs, protocol, credential
/// environment variable and model seeds.
///
/// Unlike `[providers.<name>]` (the operator's own endpoints), a builtin is
/// only materialized when it has a credential — from `ox.toml`, the
/// credential file, or its environment variable. A builtin without one does
/// not exist at all, so it cannot shadow the implicit `cardinal_url` endpoint
/// or advertise models that cannot answer.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinProvider {
    /// Provider id (`deepseek`); also the `[providers.<id>]` key that
    /// overrides it and the `provider` the catalogue reports.
    pub id: &'static str,
    /// Human name for the `/connect` dialog and provider listing.
    pub label: &'static str,
    /// Base URL for [`Protocol::ChatCompletions`] — no version suffix.
    pub completions_base_url: &'static str,
    /// Base URL for [`Protocol::Messages`] — may carry a path prefix
    /// (`https://api.deepseek.com/anthropic`), still no version suffix.
    pub messages_base_url: &'static str,
    /// Base URL answering `GET /v1/models`.
    pub models_base_url: &'static str,
    /// The protocol used when the operator has not chosen one.
    pub protocol: Protocol,
    /// Environment variable that can supply the key
    /// (`DEEPSEEK_API_KEY`).
    pub env_key: &'static str,
    /// Seed models, first one first — the default model when nothing is
    /// selected.
    pub models: &'static [SeedModel],
}

impl BuiltinProvider {
    /// The base URL for `protocol`.
    #[must_use]
    pub fn base_url(&self, protocol: Protocol) -> &'static str {
        match protocol {
            Protocol::ChatCompletions => self.completions_base_url,
            Protocol::Messages => self.messages_base_url,
        }
    }

    /// Whether `model` belongs to this provider's namespace: one of the seeds,
    /// or an id carrying the provider's own prefix (`deepseek-…`).
    ///
    /// The prefix rule is what routes a model the endpoint added after this
    /// build shipped, without a `[models]` route for it: provider ids are
    /// chosen to prefix their model ids for exactly this reason.
    #[must_use]
    pub fn owns(&self, model: &str) -> bool {
        self.models.iter().any(|seed| seed.id == model)
            || model
                .strip_prefix(self.id)
                .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('/'))
    }
}

/// Every provider ox ships defaults for.
pub static BUILTINS: &[BuiltinProvider] = &[BuiltinProvider {
    id: DEEPSEEK,
    label: "DeepSeek",
    completions_base_url: "https://api.deepseek.com",
    messages_base_url: "https://api.deepseek.com/anthropic",
    models_base_url: "https://api.deepseek.com",
    protocol: Protocol::Messages,
    env_key: ENV_DEEPSEEK_KEY,
    models: &[
        SeedModel {
            id: "deepseek-flash",
            context: 1_048_576,
            output: 256_000,
            reasoning: &["off", "low", "high", "max"],
        },
        SeedModel {
            id: "deepseek-v4-pro",
            context: 1_048_576,
            output: 256_000,
            reasoning: &["off", "high", "max"],
        },
    ],
}];

/// The key for `builtin`: the credential file first, then its environment
/// variable. An empty value counts as absent.
fn stored_key(credentials: &Credentials, builtin: &BuiltinProvider) -> Option<String> {
    if let Some(key) = credentials.key(builtin.id) {
        return Some(key.to_owned());
    }
    std::env::var(builtin.env_key)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// The DeepSeek provider id.
pub const DEEPSEEK: &str = "deepseek";

/// The environment variable that can supply the DeepSeek key.
pub const ENV_DEEPSEEK_KEY: &str = "DEEPSEEK_API_KEY";

/// The builtin provider with `id`, if one exists.
#[must_use]
pub fn builtin(id: &str) -> Option<&'static BuiltinProvider> {
    BUILTINS.iter().find(|builtin| builtin.id == id)
}

/// A builtin provider whose credentials resolved at load time.
#[derive(Debug, Clone)]
pub struct ResolvedBuiltin {
    /// The table row this was materialized from.
    pub builtin: &'static BuiltinProvider,
    /// The endpoint it contributes: base URL, key, protocol.
    pub config: ProviderConfig,
}

/// One ordered fallback target inside a `[models.<id>]` route.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CandidateConfig {
    /// A `[providers]` key, or [`DEFAULT_PROVIDER_NAME`] for the implicit
    /// `cardinal_url` endpoint.
    pub provider: String,
    /// The wire model id to request on that endpoint.
    pub model: String,
}

/// One logical model's ordered candidate routes (`[models.<id>]`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelRouteConfig {
    /// Tried in order; the first endpoint whose stream opens serves the turn.
    #[serde(default)]
    pub candidates: Vec<CandidateConfig>,
}

/// Routing metadata shared by transport and runtime geometry; contains no credentials.
#[derive(Default)]
pub struct Routes {
    explicit: BTreeMap<String, ModelRouteConfig>,
    providers: Vec<String>,
    builtins: Vec<(&'static str, &'static BuiltinProvider)>,
    passthrough: bool,
}

/// Borrowed routing decision in actual transport order.
pub enum Targets<'a> {
    Explicit(&'a [CandidateConfig]),
    Endpoint { provider: &'a str, model: &'a str },
    NoRoute,
}

impl Routes {
    #[must_use]
    pub fn new(config: &LlmConfig) -> Self {
        Self {
            explicit: config.models.clone(),
            providers: config.providers.keys().cloned().collect(),
            builtins: config
                .builtins
                .values()
                .map(|resolved| (resolved.builtin.id, resolved.builtin))
                .collect(),
            passthrough: config.providers.is_empty(),
        }
    }

    #[must_use]
    pub fn resolve<'a>(&'a self, provider: Option<&'a str>, model: &'a str) -> Targets<'a> {
        if let Some(route) = self.explicit.get(model)
            && provider.is_none_or(|selected| {
                route
                    .candidates
                    .first()
                    .is_some_and(|candidate| candidate.provider == selected)
            })
        {
            return if route.candidates.is_empty() {
                Targets::NoRoute
            } else {
                Targets::Explicit(&route.candidates)
            };
        }
        if let Some(selected) = provider {
            return if selected == DEFAULT_PROVIDER_NAME
                || self.providers.iter().any(|name| name == selected)
                || self.builtins.iter().any(|(name, _)| *name == selected)
            {
                Targets::Endpoint {
                    provider: selected,
                    model,
                }
            } else {
                Targets::NoRoute
            };
        }
        if let Some((provider, _)) = self
            .builtins
            .iter()
            .find(|(_, builtin)| builtin.owns(model))
        {
            return Targets::Endpoint { provider, model };
        }
        if self.passthrough {
            Targets::Endpoint {
                provider: DEFAULT_PROVIDER_NAME,
                model,
            }
        } else {
            Targets::NoRoute
        }
    }
}

/// The LLM-endpoint slice of `ox.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    /// Base URL of the cardinal LLM gateway (cardinal's `listen`, e.g.
    /// `http://127.0.0.1:10202`).
    #[serde(default = "default_cardinal_url")]
    pub cardinal_url: String,
    /// API key for the cardinal gateway (its `gateway_key`). Required unless
    /// cardinal runs with auth disabled.
    #[serde(default)]
    pub cardinal_key: Option<String>,
    /// `max_concurrent` / `stall_timeout_secs` for the implicit
    /// `cardinal_url` endpoint (`[cardinal_limits]` table); see
    /// [`EndpointLimits`].
    #[serde(default)]
    pub cardinal_limits: EndpointLimits,
    /// Additional OpenAI-compatible endpoints (`[providers.<name>]`). Empty:
    /// the implicit `cardinal_url` endpoint serves everything (legacy mode).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Builtin providers whose credentials resolved at load time (see
    /// [`BuiltinProvider`]). Never read from `ox.toml`: a section there is a
    /// normal `[providers]` entry and wins outright.
    #[serde(skip)]
    pub builtins: BTreeMap<String, ResolvedBuiltin>,
    /// ox's logical model surface with ordered candidate routes
    /// (`[models.<id>]`). Explicit candidates win over the implicit
    /// endpoint; once `[providers]` is non-empty, unrouted models are
    /// refused (`NoRoute`) instead of silently hitting it.
    #[serde(default)]
    pub models: BTreeMap<String, ModelRouteConfig>,
    /// Per-model context-window caps (`[context_caps]`, `model id = tokens`).
    /// A cap never *raises* a window: the compaction geometry runs on
    /// `min(catalogue-reported, cap)`, so an over-typed cap can only fold
    /// earlier, never overflow the model. A cap for a model the catalogue
    /// reports no context for does nothing (the unknown-window fallback
    /// stands); inert or unknown caps warn at catalogue refresh.
    #[serde(default)]
    pub context_caps: BTreeMap<String, u32>,
    /// Qualified model reference (`provider/id`, e.g. `cardinal/k3-256k`)
    /// backing sessions without an explicit selection. The catalogue itself is
    /// fetched from the gateway (`GET /v1/models`); this pins only which entry
    /// to use by default, and a name the gateway does not report is sent
    /// verbatim rather than rejected. When unset the first catalogue entry is
    /// the default.
    #[serde(default)]
    pub default_model: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            cardinal_url: default_cardinal_url(),
            cardinal_key: None,
            cardinal_limits: EndpointLimits::default(),
            providers: BTreeMap::new(),
            builtins: BTreeMap::new(),
            models: BTreeMap::new(),
            context_caps: BTreeMap::new(),
            default_model: None,
        }
    }
}

impl LlmConfig {
    /// Resolve every builtin provider's credentials and materialize the ones
    /// that have a key.
    ///
    /// Precedence for a builtin: a `[providers.<id>] api_key` in `ox.toml`,
    /// then the credential file (`/connect`'s store), then the provider's
    /// environment variable. A builtin with no key is *not* materialized at
    /// all — it cannot shadow the implicit `cardinal_url` endpoint, cannot
    /// advertise models that would 401, and never turns a missing key into a
    /// silent fallback. A `[providers.<id>]` section that exists but has no key
    /// gets the same resolution, so `[providers.deepseek] base_url = "…"`
    /// alone still works with `DEEPSEEK_API_KEY` set.
    pub fn apply_credentials(&mut self, credentials: &Credentials) {
        for builtin in BUILTINS {
            if let Some(provider) = self.providers.get_mut(builtin.id) {
                // `[providers.deepseek]` without a `base_url` is the one-line
                // override form: it means "the builtin's own endpoint", so the
                // base URL and the protocol both come from the builtin. Naming
                // a base URL makes the section an ordinary endpoint, where an
                // unstated protocol is chat completions.
                if provider.base_url.trim().is_empty() {
                    let protocol = provider.protocol.unwrap_or(builtin.protocol);
                    provider.protocol = Some(protocol);
                    provider.base_url = builtin.base_url(protocol).to_owned();
                }
                if provider.api_key.as_deref().is_none_or(str::is_empty) {
                    provider.api_key = stored_key(credentials, builtin);
                }
                continue;
            }
            let Some(api_key) = stored_key(credentials, builtin) else {
                continue;
            };
            let protocol = builtin.protocol;
            self.builtins.insert(
                builtin.id.to_owned(),
                ResolvedBuiltin {
                    builtin,
                    config: ProviderConfig {
                        base_url: builtin.base_url(protocol).to_owned(),
                        api_key: Some(api_key),
                        protocol: Some(protocol),
                        limits: EndpointLimits::default(),
                    },
                },
            );
        }
    }

    /// The endpoint configuration for `name`, whether the operator wrote it
    /// under `[providers]` or it was materialized from a builtin.
    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers
            .get(name)
            .or_else(|| self.builtins.get(name).map(|builtin| &builtin.config))
    }

    /// Whether `name` came from a builtin table rather than `[providers]`.
    #[must_use]
    pub fn is_builtin(&self, name: &str) -> bool {
        self.builtins.contains_key(name)
    }

    /// Every usable endpoint: the operator's `[providers]` first, then the
    /// credential-resolved builtins, both in name order.
    #[must_use]
    pub fn endpoints(&self) -> Vec<(&str, &ProviderConfig)> {
        let mut endpoints: Vec<(&str, &ProviderConfig)> = self
            .providers
            .iter()
            .map(|(name, provider)| (name.as_str(), provider))
            .collect();
        endpoints.extend(
            self.builtins
                .iter()
                .map(|(name, builtin)| (name.as_str(), &builtin.config)),
        );
        endpoints
    }

    /// Apply the `OX_CARDINAL_URL`, `OX_CARDINAL_KEY` and `OX_DEFAULT_MODEL`
    /// environment overrides.
    pub fn apply_env(&mut self) {
        if let Ok(url) = std::env::var(ENV_CARDINAL_URL) {
            self.cardinal_url = url;
        }
        if let Ok(key) = std::env::var(ENV_CARDINAL_KEY) {
            self.cardinal_key = Some(key);
        }
        if let Ok(model) = std::env::var(ENV_DEFAULT_MODEL) {
            self.default_model = Some(model);
        }
    }

    /// Reject inconsistent endpoint configuration: `[providers]`/`[models]`
    /// consistency and positive `[context_caps]`.
    pub fn validate(&self) -> Result<()> {
        self.validate_providers()?;
        self.validate_context_caps()
    }

    /// `[providers]`/`[models]` consistency: usable base URLs, non-empty
    /// candidate lists, every candidate provider resolvable.
    fn validate_providers(&self) -> Result<()> {
        for (name, provider) in &self.providers {
            if provider.base_url.is_empty() {
                anyhow::bail!(
                    "providers.{name}.base_url is empty: set it, or drop the section and put only \
                     `api_key` to use the builtin provider of that id"
                );
            }
            if provider.protocol() == Protocol::ChatCompletions
                && provider.base_url.ends_with("/v1")
            {
                anyhow::bail!(
                    "providers.{name}.base_url must not end with /v1: ox appends /v1/chat/completions itself"
                );
            }
            if !provider.base_url.starts_with("http://")
                && !provider.base_url.starts_with("https://")
            {
                anyhow::bail!("providers.{name}.base_url must be an http(s) URL");
            }
        }
        for (id, route) in &self.models {
            if route.candidates.is_empty() {
                anyhow::bail!("models.{id}.candidates is empty: list at least one candidate");
            }
            for candidate in &route.candidates {
                if candidate.model.is_empty() {
                    anyhow::bail!("models.{id}: candidate model must not be empty");
                }
                if candidate.provider != DEFAULT_PROVIDER_NAME
                    && self.provider(&candidate.provider).is_none()
                {
                    anyhow::bail!(
                        "models.{id}: candidate provider `{}` is neither under [providers] nor a \
                         credential-resolved builtin",
                        candidate.provider
                    );
                }
            }
        }
        Ok(())
    }

    /// `[context_caps]`: every value is a positive token count.
    fn validate_context_caps(&self) -> Result<()> {
        for (id, cap) in &self.context_caps {
            if *cap == 0 {
                anyhow::bail!("context_caps.{id} must be a positive token count, got 0");
            }
        }
        Ok(())
    }
}
