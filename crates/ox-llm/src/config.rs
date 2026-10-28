//! LLM endpoint configuration: the implicit `cardinal_url` endpoint, extra
//! `[providers]`, `[models]` routes, `[context_caps]` and the default model.
//!
//! [`LlmConfig`] is flattened into `ox-core`'s `Config`, so the TOML keys stay
//! at the top level of `ox.toml`.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Deserialize;

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

/// One configured outbound endpoint (`[providers.<name>]`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    /// OpenAI-compatible base URL **without** the `/v1` suffix: ox appends
    /// `/v1/chat/completions` and `/v1/models` itself. For vendors publishing
    /// `…/v1` or `…/v4` bases, configure the part before the suffix
    /// (`https://api.moonshot.ai`, `https://open.bigmodel.cn/api/paas`).
    pub base_url: String,
    /// Bearer key for `Authorization`; omitted for gateways that need none.
    #[serde(default)]
    pub api_key: Option<String>,
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
    /// Additional OpenAI-compatible endpoints (`[providers.<name>]`). Empty:
    /// the implicit `cardinal_url` endpoint serves everything (legacy mode).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
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
            providers: BTreeMap::new(),
            models: BTreeMap::new(),
            context_caps: BTreeMap::new(),
            default_model: None,
        }
    }
}

impl LlmConfig {
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
                anyhow::bail!("providers.{name}.base_url is empty");
            }
            if provider.base_url.ends_with("/v1") {
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
                    && !self.providers.contains_key(&candidate.provider)
                {
                    anyhow::bail!(
                        "models.{id}: candidate provider `{}` is not configured under [providers]",
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
