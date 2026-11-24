//! Cross-process scheduler lease backed by a single SQLite row.
//!
//! Acquisition, renewal and release are all compare-and-swap on owner and
//! token. No shared in-process lock can make two schedulers believe they both
//! hold the lease.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use anyhow::Result;
use ox_db::Db;

static LEASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Holder of the scheduler lease. A lost lease makes every later operation a
/// no-op, so a stale holder cannot write automation state.
pub struct Lease<'a> {
    db: &'a Db,
    name: String,
    owner: String,
    token: String,
    expires_at: AtomicI64,
}

impl<'a> Lease<'a> {
    /// Try to become the holder. A live lease owned by someone else returns
    /// `None`; an expired one is taken over in the same SQL statement.
    pub async fn acquire(
        db: &'a Db,
        name: &str,
        owner: &str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<Option<Self>> {
        let counter = LEASE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let token = format!("{owner}:{}:{counter}", std::process::id());
        let expires_at = now_ms.saturating_add(ttl_ms);
        let tx = db.begin().await?;
        let changed = tx
            .execute(
                "INSERT INTO automation_lease (name, owner, token, expires_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                     owner = excluded.owner,
                     token = excluded.token,
                     expires_at = excluded.expires_at
                 WHERE automation_lease.expires_at <= ?5",
                ox_db::turso::params![name, owner, token.as_str(), expires_at, now_ms],
            )
            .await?;
        tx.commit().await?;
        if changed != 1 {
            return Ok(None);
        }
        Ok(Some(Self {
            db,
            name: name.to_owned(),
            owner: owner.to_owned(),
            token,
            expires_at: AtomicI64::new(expires_at),
        }))
    }

    /// Whether this lease is still valid at `now_ms`. Callers check this with
    /// their injected clock before touching state.
    #[must_use]
    pub fn valid(&self, now_ms: i64) -> bool {
        self.expires_at.load(Ordering::Relaxed) > now_ms
    }

    /// Extend the lease. A `false` result means someone else owns it now, and
    /// invalidates this holder locally so a stale cached expiry cannot license
    /// a later write. Callable while handlers hold a shared borrow of `Lease`.
    pub async fn renew(&self, now_ms: i64, ttl_ms: i64) -> Result<bool> {
        let expires_at = now_ms.saturating_add(ttl_ms);
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "UPDATE automation_lease SET expires_at = ?1
                 WHERE name = ?2 AND owner = ?3 AND token = ?4 AND expires_at > ?5",
                ox_db::turso::params![
                    expires_at,
                    self.name.as_str(),
                    self.owner.as_str(),
                    self.token.as_str(),
                    now_ms,
                ],
            )
            .await?;
        tx.commit().await?;
        if changed == 1 {
            self.expires_at.store(expires_at, Ordering::Relaxed);
        } else {
            self.expires_at.store(i64::MIN, Ordering::Relaxed);
        }
        Ok(changed == 1)
    }

    /// Give up the lease. Returns `false` when a successor already owns it.
    pub async fn release(&self) -> Result<bool> {
        let tx = self.db.begin().await?;
        let changed = tx
            .execute(
                "DELETE FROM automation_lease
                 WHERE name = ?1 AND owner = ?2 AND token = ?3",
                ox_db::turso::params![
                    self.name.as_str(),
                    self.owner.as_str(),
                    self.token.as_str(),
                ],
            )
            .await?;
        tx.commit().await?;
        Ok(changed == 1)
    }
}
