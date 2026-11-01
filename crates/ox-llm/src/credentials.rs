//! Provider credentials that live outside `ox.toml`.
//!
//! `ox.toml` is a configuration file people read, share and commit; an API key
//! is none of those. `/connect` therefore writes keys to a separate file
//! (`<ox_home>/credentials.toml`, mode `0600`) that ox reads on every config
//! load, so a key entered once survives a restart without touching the config
//! file.
//!
//! Precedence, highest first: a `[providers.<name>] api_key` written in
//! `ox.toml`, then this file, then the provider's environment variable
//! (`DEEPSEEK_API_KEY`, …). The middle position is deliberate: a key the user
//! typed into `/connect` is explicit and recent, so an inherited environment
//! variable from some other tool must not silently shadow it.
//!
//! The file is optional everywhere: a missing file is an empty store, and a
//! malformed one warns and loads empty rather than refusing to start (a broken
//! credential file must not cost the user their session; the warning names the
//! file so it is fixable).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The secrets file, as written by `/connect`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// `[providers.<id>]` tables.
    #[serde(default)]
    providers: BTreeMap<String, Credential>,
}

/// One provider's stored secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// The bearer/`x-api-key` value. Stored as written (ox does not trim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Credentials {
    /// Read `path`, or an empty store when it does not exist. A parse failure
    /// warns and yields an empty store: credentials are optional, and a
    /// half-written file must not stop the server from starting.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "credential file unreadable; providers will fall back to config and environment"
                );
                return Self::default();
            }
        };
        match toml::from_str(&raw) {
            Ok(credentials) => credentials,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "credential file is not valid TOML; providers will fall back to config and environment"
                );
                Self::default()
            }
        }
    }

    /// The stored key for `provider`, if any. An empty value counts as absent
    /// — `/connect` never stores one, but a hand-edited file can.
    #[must_use]
    pub fn key(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)?
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
    }

    /// Store (or replace) `provider`'s key.
    pub fn set(&mut self, provider: &str, api_key: impl Into<String>) {
        let key: String = api_key.into();
        match self.providers.get_mut(provider) {
            Some(credential) => credential.api_key = Some(key),
            None => {
                self.providers
                    .insert(provider.to_owned(), Credential { api_key: Some(key) });
            }
        }
    }

    /// Drop `provider`'s key. Returns whether anything was removed.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers
            .remove(provider)
            .and_then(|credential| credential.api_key)
            .is_some()
    }

    /// Whether any provider has a key — the "is this ox connected to
    /// anything?" question `/connect` and the provider listing ask.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers
            .values()
            .all(|credential| credential.api_key.as_deref().is_none_or(str::is_empty))
    }

    /// Write the store to `path` atomically (temp file + rename) with mode
    /// `0600`, so a concurrent reload never reads a half-written file and
    /// another local user never reads the key at all.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialize credentials")?;
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, body)
            .with_context(|| format!("write {}", temporary.display()))?;
        restrict(&temporary)?;
        std::fs::rename(&temporary, path).with_context(|| format!("install {}", path.display()))?;
        Ok(())
    }
}

/// Make `path` readable by its owner only. A no-op off Unix (where the
/// credential file inherits the user-profile ACL).
fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The credential file path under `home`: `<home>/credentials.toml`.
#[must_use]
pub fn path_in(home: &Path) -> PathBuf {
    home.join("credentials.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ox-credentials-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("credentials.toml")
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let path = temp("missing");
        assert!(Credentials::load(&path).is_empty());
    }

    #[test]
    fn a_saved_key_round_trips_and_survives_a_reload() {
        let path = temp("roundtrip");
        let mut credentials = Credentials::default();
        credentials.set("deepseek", "sk-test");
        credentials.save(&path).expect("save");
        let reloaded = Credentials::load(&path);
        assert_eq!(reloaded.key("deepseek"), Some("sk-test"));
        assert!(!reloaded.is_empty());
    }

    #[test]
    fn the_file_is_owner_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let path = temp("mode");
            Credentials::default().set("deepseek", "sk-test");
            let mut credentials = Credentials::default();
            credentials.set("deepseek", "sk-test");
            credentials.save(&path).expect("save");
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_malformed_file_loads_empty_instead_of_failing() {
        let path = temp("malformed");
        std::fs::write(&path, "not toml at all =").expect("write");
        assert!(Credentials::load(&path).is_empty());
    }

    #[test]
    fn remove_reports_whether_a_key_was_there() {
        let mut credentials = Credentials::default();
        credentials.set("deepseek", "sk-test");
        assert!(credentials.remove("deepseek"));
        assert!(!credentials.remove("deepseek"));
        assert!(credentials.key("deepseek").is_none());
    }
}
