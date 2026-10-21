//! The model catalogue: cardinal's `GET /v1/models`, not ox's config file.
//!
//! cardinal is ox's only model source and it already reports what a client
//! needs — the id, the provider (`owned_by`) and the context window
//! (`context_length` / `context_window` / `max_input_tokens`) — so keeping a
//! `[[models]]` list in `ox.toml` was a second source of truth that drifted the
//! moment a model was added upstream. The file now names at most a *default*
//! (`default_model`); everything the client lists comes from here.
//!
//! The cache is deliberately simple: one snapshot, refreshed when older than
//! [`CATALOG_TTL`], served stale if a refresh fails — the previous list is far
//! more useful than an empty one, and the failure is logged once per attempt,
//! not per request.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::config::{Config, ModelEntry};

/// How long a fetched catalogue is considered fresh. Model lists change when
/// someone adds a provider upstream, so minutes rather than seconds; a client
/// that wants a fresh list after a change restarts or waits one TTL.
const CATALOG_TTL: Duration = Duration::from_secs(300);

/// `GET /v1/models` — the OpenAI list shape cardinal serves.
const MODELS_PATH: &str = "/v1/models";

/// The provider id used when the gateway does not report `owned_by`.
const FALLBACK_PROVIDER: &str = "cardinal";

/// A fetched model catalogue with a TTL cache.
pub struct ModelCatalog {
    client: reqwest::Client,
    /// Base URL of the gateway (no trailing slash).
    base_url: String,
    api_key: Option<String>,
    cache: RwLock<Snapshot>,
}

#[derive(Default)]
struct Snapshot {
    entries: Arc<Vec<ModelEntry>>,
    fetched_at: Option<Instant>,
}

impl ModelCatalog {
    /// A catalogue for `config`'s gateway. Nothing is fetched yet: [`Self::warm`]
    /// does that, so building state never blocks on the network.
    pub fn new(config: &Config) -> Arc<Self> {
        let client = crate::http_client()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            client,
            base_url: config.cardinal_url.trim_end_matches('/').to_owned(),
            api_key: config.cardinal_key.clone().filter(|key| !key.is_empty()),
            cache: RwLock::new(Snapshot::default()),
        })
    }

    /// A catalogue seeded with `entries` and no fetch — the seam tests use to
    /// pin a model list without a gateway.
    pub fn from_entries(entries: Vec<ModelEntry>) -> Self {
        Self {
            client: crate::http_client().build().unwrap_or_default(),
            base_url: String::new(),
            api_key: None,
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
    pub fn default_entry(&self, config: &Config) -> Option<ModelEntry> {
        let entries = self.cached();
        if let Some(qualified) = &config.default_model {
            let (provider, id) = qualified
                .split_once('/')
                .unwrap_or((FALLBACK_PROVIDER, qualified.as_str()));
            if let Some(found) = entries
                .iter()
                .find(|entry| entry.id == id && entry.provider == provider)
            {
                return Some(found.clone());
            }
            return Some(ModelEntry {
                provider: provider.to_owned(),
                id: id.to_owned(),
                name: None,
                context: None,
                output: None,
            });
        }
        entries.first().cloned()
    }

    /// The context window the gateway reports for `id` (the bare model id, as
    /// cardinal reports it), or `None` when the catalogue does not know the
    /// model or was never fetched. Never refreshes: callers on a step boundary
    /// must not wait on the network.
    pub fn window_of(&self, id: &str) -> Option<u32> {
        self.cached()
            .iter()
            .find(|entry| entry.id == id)?
            .context
            .and_then(|context| u32::try_from(context).ok())
            .filter(|window| *window > 0)
    }

    /// Fetch `GET /v1/models` and replace the snapshot.
    async fn refresh(&self) -> Result<()> {
        let mut request = self.client.get(format!("{}{MODELS_PATH}", self.base_url));
        if let Some(key) = &self.api_key {
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
        let mut entries: Vec<ModelEntry> =
            list.data.into_iter().map(ModelRow::into_entry).collect();
        // A stable order: the default falls back to the first entry, so the
        // order must not depend on how the gateway happened to serialise.
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        entries.dedup_by(|left, right| left.id == right.id && left.provider == right.provider);
        let count = entries.len();
        {
            let mut cache = self.cache.write().expect("catalog lock");
            cache.entries = Arc::new(entries);
            cache.fetched_at = Some(Instant::now());
        }
        tracing::debug!(models = count, "model catalogue refreshed");
        Ok(())
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
    #[serde(default)]
    owned_by: Option<String>,
    /// cardinal reports all three; other gateways may report one.
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
}

impl ModelRow {
    fn into_entry(self) -> ModelEntry {
        ModelEntry {
            provider: self
                .owned_by
                .unwrap_or_else(|| FALLBACK_PROVIDER.to_owned()),
            name: None,
            context: self
                .context_length
                .or(self.context_window)
                .or(self.max_input_tokens),
            output: None,
            id: self.id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(default_model: Option<&str>) -> Config {
        Config {
            default_model: default_model.map(str::to_owned),
            ..Config::default()
        }
    }

    /// A stub gateway serving one `/v1/models` body.
    async fn spawn_gateway(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = [0u8; 4096];
                    let _ = socket.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn entries_come_from_the_gateway_with_its_limits() {
        let url = spawn_gateway(
            r#"{"data":[
                {"id":"k3-256k","owned_by":"cardinal","context_length":262144},
                {"id":"k4-1m","owned_by":"cardinal","context_window":1048576},
                {"id":"bare"}
            ]}"#,
        )
        .await;
        let catalog = ModelCatalog::new(&Config {
            cardinal_url: url,
            ..Config::default()
        });
        let entries = catalog.entries().await;
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["bare", "k3-256k", "k4-1m"],
            "sorted by id so the default is deterministic"
        );
        assert_eq!(entries[1].context, Some(262144));
        assert_eq!(
            entries[2].context,
            Some(1_048_576),
            "context_window counts too"
        );
        assert_eq!(entries[0].provider, FALLBACK_PROVIDER, "owned_by defaults");
    }

    #[tokio::test]
    async fn an_unreachable_gateway_keeps_the_previous_list() {
        // Point at a closed port: every fetch fails, so the catalogue stays
        // empty rather than erroring out of `entries()`.
        let catalog = ModelCatalog::new(&Config {
            cardinal_url: "http://127.0.0.1:1".to_owned(),
            ..Config::default()
        });
        assert!(catalog.entries().await.is_empty());
        assert!(catalog.default_entry(&config(None)).is_none());
    }

    #[tokio::test]
    async fn the_default_prefers_the_configured_model_then_the_first_entry() {
        let url = spawn_gateway(
            r#"{"data":[
                {"id":"k3-256k","owned_by":"cardinal","context_length":262144},
                {"id":"k4-1m","owned_by":"cardinal","context_length":1048576}
            ]}"#,
        )
        .await;
        let catalog = ModelCatalog::new(&Config {
            cardinal_url: url,
            ..Config::default()
        });
        catalog.warm().await;

        // Configured and known: the catalogue entry (limits included).
        let chosen = catalog
            .default_entry(&config(Some("cardinal/k4-1m")))
            .expect("a default");
        assert_eq!(chosen.id, "k4-1m");
        assert_eq!(chosen.context, Some(1_048_576));

        // Configured but unknown: the reference verbatim, so the gateway can
        // still route it.
        let unknown = catalog
            .default_entry(&config(Some("cardinal/not-yet-served")))
            .expect("a default");
        assert_eq!(unknown.id, "not-yet-served");
        assert_eq!(unknown.provider, "cardinal");

        // Unset: the first entry by id.
        assert_eq!(
            catalog.default_entry(&config(None)).expect("a default").id,
            "k3-256k"
        );
    }
}
