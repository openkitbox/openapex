//! Persistent per-model variant preferences for the TUI.
//!
//! The preference file is user-owned state, not session state. A missing or
//! unreadable file produces an empty preference set. Writes replace the file
//! through a sibling temporary file, so readers observe either the old or the
//! complete new JSON document.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::from_str;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The key used to isolate a preference by provider and model.
#[must_use]
pub fn model_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// User preferences loaded from one private JSON file.
#[derive(Debug)]
pub struct ModelPreference {
    path: PathBuf,
    variants: BTreeMap<String, String>,
}

impl ModelPreference {
    /// Load preferences from `path`; missing and corrupt files are empty.
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let variants = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| from_str::<BTreeMap<String, String>>(&text).ok())
            .unwrap_or_default();
        Self { path, variants }
    }

    /// Return the saved variant, including the explicit `"default"` value.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<&str> {
        self.variants.get(key).map(String::as_str)
    }

    /// Return the saved variant for a provider/model pair.
    #[must_use]
    pub fn lookup_model(&self, provider: &str, model: &str) -> Option<&str> {
        self.lookup(&model_key(provider, model))
    }

    /// Update and persist one provider/model preference atomically.
    pub fn update_model(
        &mut self,
        provider: &str,
        model: &str,
        variant: &str,
    ) -> std::io::Result<()> {
        let key = model_key(provider, model);
        let mut candidate = self.variants.clone();
        candidate.insert(key, variant.to_owned());
        self.save_variants(&candidate)?;
        self.variants = candidate;
        Ok(())
    }

    fn save_variants(&self, variants: &BTreeMap<String, String>) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(variants).map_err(std::io::Error::other)?;
        ensure_parent(&self.path)?;
        replace_private(&self.path, &body)
    }
}

/// Default location for TUI model preferences.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/state/ox-tui/model.json"))
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent)
    }
}

fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn replace_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{sequence}", std::process::id()));
    let result = private_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(body)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("ox-tui-model-preference-{name}-{nonce}.json"))
    }

    #[test]
    fn model_key_and_lookup_are_provider_specific() {
        let path = test_path("lookup");
        let mut preferences = ModelPreference::load(&path);
        preferences
            .update_model("one", "model", "high")
            .expect("write preference");
        assert_eq!(model_key("one", "model"), "one/model");
        assert_eq!(preferences.lookup_model("one", "model"), Some("high"));
        assert_eq!(preferences.lookup_model("two", "model"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_and_corrupt_files_are_empty() {
        let path = test_path("corrupt");
        std::fs::write(&path, b"not json").expect("write corrupt fixture");
        let preferences = ModelPreference::load(&path);
        assert_eq!(preferences.lookup("one/model"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn malformed_preference_value_invalidates_the_file() {
        let path = test_path("malformed-value");
        std::fs::write(&path, br#"{"one/model":"high","bad/model":42}"#)
            .expect("write malformed fixture");
        let preferences = ModelPreference::load(&path);
        assert_eq!(preferences.lookup("one/model"), None);
        assert_eq!(preferences.lookup("bad/model"), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn explicit_default_round_trips_through_atomic_save() {
        let path = test_path("default");
        let mut preferences = ModelPreference::load(&path);
        preferences
            .update_model("provider", "model", "default")
            .expect("write default");
        let loaded = ModelPreference::load(&path);
        assert_eq!(loaded.lookup_model("provider", "model"), Some("default"));
        let _ = std::fs::remove_file(path);
    }
}
