//! Provider credentials, as consumed by the endpoint config.
//!
//! This is only the **shape**. The store is the `credential` table, whose SQL
//! lives with the rest of ox's domain storage (`ox_core::credential`);
//! `/connect` writes it and the server reads it after every config load. Keys
//! never go into `ox.toml` unless the operator puts a
//! `[providers.<name>] api_key` there by hand: that file is meant to be read,
//! shared and committed, an API key is none of those.
//!
//! Precedence, highest first: a key written in `ox.toml`, then the credential
//! table, then the provider's environment variable (`DEEPSEEK_API_KEY`, …).
//! The middle position is deliberate: a key the user typed into `/connect` is
//! explicit and recent, so an inherited environment variable from some other
//! tool must not silently shadow it.

use std::collections::BTreeMap;

/// The providers with a stored key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    /// Provider id → secret.
    providers: BTreeMap<String, Credential>,
}

/// One provider's stored secret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credential {
    /// The bearer/`x-api-key` value.
    pub api_key: Option<String>,
}

impl Credentials {
    /// The stored key for `provider`, if any. An empty value counts as absent
    /// — `/connect` never stores one, but a hand-edited database can.
    #[must_use]
    pub fn key(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)?
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
    }

    /// Store (or replace) `provider`'s key.
    pub fn set(&mut self, provider: impl Into<String>, api_key: impl Into<String>) {
        self.providers.insert(
            provider.into(),
            Credential {
                api_key: Some(api_key.into()),
            },
        );
    }

    /// Drop `provider`'s key. Returns whether anything was removed.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers
            .remove(provider)
            .and_then(|credential| credential.api_key)
            .is_some()
    }

    /// Whether no provider has a key — the "is ox connected to anything?"
    /// question the provider listing asks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers
            .values()
            .all(|credential| credential.api_key.as_deref().is_none_or(str::is_empty))
    }
}
