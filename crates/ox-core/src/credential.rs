//! The `credential` table: provider API keys `/connect` stores.
//!
//! `/connect` is the only writer. Keys live in the database rather than
//! `ox.toml` because the config file is meant to be read, shared and
//! committed, and because upstream opencode keeps them in the same table
//! ([`MIGRATION`] mirrors `packages/core/src/credential/sql.ts`), so a database
//! written by either server stays readable by the other.
//!
//! This module owns the SQL for that table; [`ox_llm::credentials::Credentials`]
//! stays the in-memory shape the provider config consumes.
//!
//! Precedence when a key is resolved (`LlmConfig::apply_credentials`, called
//! after [`crate::config::Config::load_from`]): a `[providers.<name>] api_key`
//! written in `ox.toml`, then this table, then the provider's environment
//! variable. A key typed into `/connect` is explicit and recent, so an
//! inherited variable must not silently shadow it.

use anyhow::{Context, Result};
use ox_db::Db;
use ox_llm::credentials::Credentials;
use serde_json::{Value, json};

/// The stored credential value for an API-key integration: upstream's
/// `Credential.Key` (`packages/schema/src/credential.ts:42-46`).
const KEY_VALUE: &str = "key";

/// Every active key, as the in-memory [`Credentials`] the config consumes.
///
/// A row whose `value` is not a JSON key credential (an OAuth or external
/// entry, which ox never writes but an upstream server sharing the file might)
/// is skipped rather than failing: one unknown row must not cost the user
/// every provider.
pub async fn load(db: &Db) -> Result<Credentials> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT integration_id, value FROM credential
             WHERE active = 1 AND integration_id IS NOT NULL",
        )
        .await
        .context("prepare credential select")?;
    let mut rows = stmt.query(()).await.context("query credential")?;
    let mut credentials = Credentials::default();
    while let Some(row) = rows.next().await? {
        let integration_id: Option<String> = row.get(0)?;
        let raw: String = row.get(1)?;
        let Some(integration_id) = integration_id else {
            continue;
        };
        match key_of(&raw) {
            Some(key) => credentials.set(integration_id, key),
            None => tracing::debug!(
                integration = %integration_id,
                "ignoring a credential that is not an API key"
            ),
        }
    }
    Ok(credentials)
}

/// The `key` field of a stored value, when it is a key credential.
fn key_of(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("type")?.as_str()? != KEY_VALUE {
        return None;
    }
    let key = value.get("key")?.as_str()?;
    (!key.is_empty()).then(|| key.to_owned())
}

/// Store `key` as the integration's single active credential, replacing any
/// earlier one. The replace is a delete plus an insert inside one write
/// transaction so a concurrent [`load`] never sees two active rows.
pub async fn set_key(db: &Db, id: &str, integration_id: &str, key: &str) -> Result<()> {
    let now = ox_schema::id::now_millis();
    let value = json!({ "type": KEY_VALUE, "key": key }).to_string();
    let label = format!("{integration_id} API key");
    let tx = db.begin().await?;
    tx.execute(
        "DELETE FROM credential WHERE integration_id = ?1 AND method_id = ?2",
        (integration_id, KEY_VALUE),
    )
    .await
    .context("delete the previous credential")?;
    tx.execute(
        "INSERT INTO credential
           (id, integration_id, label, value, connector_id, method_id, active, time_created, time_updated)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, 1, ?6, ?6)",
        (id, integration_id, label, value, KEY_VALUE, now),
    )
    .await
    .context("insert the credential")?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_key_credentials_yield_a_key() {
        assert_eq!(
            key_of(r#"{"type":"key","key":"sk-test"}"#),
            Some("sk-test".to_owned())
        );
        assert_eq!(key_of(r#"{"type":"key","key":""}"#), None);
        assert_eq!(
            key_of(r#"{"type":"oauth","access":"a","refresh":"r"}"#),
            None
        );
        assert_eq!(key_of("not json"), None);
    }

    #[tokio::test]
    async fn a_key_round_trips_and_a_second_connect_replaces_it() {
        let db = Db::open_in_memory().await.expect("in-memory db");
        set_key(&db, "cred_1", "deepseek", "sk-first")
            .await
            .expect("first key");
        set_key(&db, "cred_2", "deepseek", "sk-second")
            .await
            .expect("replacement key");
        let credentials = load(&db).await.expect("load");
        assert_eq!(
            credentials.key("deepseek"),
            Some("sk-second"),
            "a second /connect must replace the first key, not add a second row"
        );
        let conn = db.acquire().await.expect("conn");
        let mut stmt = conn
            .prepare("SELECT count(*) FROM credential WHERE integration_id = 'deepseek'")
            .await
            .expect("prepare");
        let mut rows = stmt.query(()).await.expect("query");
        let row = rows.next().await.expect("row").expect("one row");
        assert_eq!(row.get::<i64>(0).expect("count"), 1);
    }
}
