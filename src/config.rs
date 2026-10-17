//! Configuration loaded from `~/.ox/ox.toml` with environment-variable overrides.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_CARDINAL_URL: &str = "http://127.0.0.1:4000";
const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// Environment variable overrides.
pub const ENV_CARDINAL_URL: &str = "OX_CARDINAL_URL";
pub const ENV_CARDINAL_KEY: &str = "OX_CARDINAL_KEY";
pub const ENV_DB_PATH: &str = "OX_DB_PATH";
pub const ENV_LISTEN: &str = "OX_LISTEN";

fn default_cardinal_url() -> String {
    DEFAULT_CARDINAL_URL.to_owned()
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_owned()
}

fn default_db_path() -> PathBuf {
    ox_home().join("ox.db")
}

fn default_models() -> Vec<ModelEntry> {
    Vec::new()
}

/// A statically configured model. Cardinal integration (Task 5) will replace
/// this with a live model catalogue fetched from the gateway.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// Provider identifier, e.g. `anthropic`.
    pub provider: String,
    /// Model identifier, e.g. `claude-sonnet-4-5`.
    pub id: String,
    /// Human-readable display name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Base URL of the cardinal LLM gateway.
    #[serde(default = "default_cardinal_url")]
    pub cardinal_url: String,
    /// API key for the cardinal gateway, if it requires one.
    #[serde(default)]
    pub cardinal_key: Option<String>,
    /// Path to the SQLite database file.
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    /// Address the HTTP server listens on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Statically configured models exposed by `model.list`.
    #[serde(default = "default_models")]
    pub models: Vec<ModelEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cardinal_url: default_cardinal_url(),
            cardinal_key: None,
            db_path: default_db_path(),
            listen: default_listen(),
            models: default_models(),
        }
    }
}

/// The ox home directory (`~/.ox`), falling back to `./.ox` when the home
/// directory cannot be determined.
pub fn ox_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ox")
}

impl Config {
    /// Default config file location: `~/.ox/ox.toml`.
    pub fn default_path() -> PathBuf {
        ox_home().join("ox.toml")
    }

    /// Load config from the default path, then apply env overrides.
    /// A missing config file is fine — defaults apply.
    pub fn load() -> Result<Self> {
        Self::load_from(Self::default_path())
    }

    /// Load config from an explicit path, then apply env overrides.
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse config file {}", path.display()))?
        } else {
            Config::default()
        };

        if let Ok(url) = std::env::var(ENV_CARDINAL_URL) {
            config.cardinal_url = url;
        }
        if let Ok(key) = std::env::var(ENV_CARDINAL_KEY) {
            config.cardinal_key = Some(key);
        }
        if let Ok(db) = std::env::var(ENV_DB_PATH) {
            config.db_path = PathBuf::from(db);
        }
        if let Ok(listen) = std::env::var(ENV_LISTEN) {
            config.listen = listen;
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_applied_when_file_missing() {
        let cfg = Config::load_from(PathBuf::from("/nonexistent/path/ox.toml")).unwrap();
        assert_eq!(cfg.cardinal_url, DEFAULT_CARDINAL_URL);
        assert_eq!(cfg.listen, DEFAULT_LISTEN);
        assert!(cfg.cardinal_key.is_none());
        assert!(cfg.db_path.ends_with("ox.db"));
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn toml_round_trip() {
        let raw = r#"
cardinal_url = "http://localhost:1234"
db_path = "/tmp/ox-test.db"
listen = "0.0.0.0:9999"

[[models]]
provider = "anthropic"
id = "claude-sonnet-4-5"
name = "Claude Sonnet 4.5"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.cardinal_url, "http://localhost:1234");
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/ox-test.db"));
        assert_eq!(cfg.listen, "0.0.0.0:9999");
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].provider, "anthropic");
        assert_eq!(cfg.models[0].id, "claude-sonnet-4-5");
        assert_eq!(cfg.models[0].name.as_deref(), Some("Claude Sonnet 4.5"));
    }
}
