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

#[cfg(test)]
mod tests {
    use ox_db::Db;

    use super::Lease;

    const NAME: &str = "automation-scheduler";
    const TTL: i64 = 15_000;

    /// Two simulated processes share one database and run the same tick. They
    /// coordinate only through the SQL CAS: no in-process mutex is involved.
    #[tokio::test]
    async fn lease_single_holder() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let process_a = db.clone();
        let process_b = db.clone();

        let holder = Lease::acquire(&process_a, NAME, "host_a", 1_000, TTL)
            .await?
            .expect("the first process becomes holder");
        assert!(holder.valid(1_000));

        let mut actions = 0;
        for now in [1_000, 2_000, 3_000] {
            if let Some(other) = Lease::acquire(&process_b, NAME, "host_b", now, TTL).await? {
                actions += 1;
                drop(other);
            }
        }
        assert_eq!(actions, 0, "only one simulated process may act per tick");

        assert!(
            holder.renew(6_000, TTL).await?,
            "the holder keeps its lease with a heartbeat"
        );
        assert!(
            Lease::acquire(&process_b, NAME, "host_b", 7_000, TTL)
                .await?
                .is_none(),
            "renewal extends the holder's exclusivity"
        );

        assert!(
            !holder.renew(30_000, TTL).await?,
            "a holder that missed its heartbeat loses the lease"
        );
        let successor = Lease::acquire(&process_b, NAME, "host_b", 22_000, TTL)
            .await?
            .expect("an expired lease can be taken over");
        assert!(
            !holder.release().await?,
            "the old holder cannot release someone else's lease"
        );
        assert!(
            Lease::acquire(&process_a, NAME, "host_a", 23_000, TTL)
                .await?
                .is_none(),
            "the stale release did not clear the successor"
        );
        assert!(
            successor.release().await?,
            "the successor can release its lease"
        );
        assert!(
            Lease::acquire(&process_a, NAME, "host_a", 24_000, TTL)
                .await?
                .is_some(),
            "release makes the lease acquirable again"
        );
        Ok(())
    }

    /// A renewal rejected by the SQL CAS must also fail closed locally: the
    /// holder's cached expiry can still be in the future after a takeover.
    #[tokio::test]
    async fn failed_renewal_invalidates_local_lease() -> anyhow::Result<()> {
        let db = Db::open_in_memory().await?;
        let holder = Lease::acquire(&db, NAME, "host_a", 1_000, TTL)
            .await?
            .expect("the first process becomes holder");
        assert!(holder.valid(2_000));

        let conn = db.acquire().await?;
        conn.execute(
            "UPDATE automation_lease SET owner = 'host_b', token = 'stolen' WHERE name = ?1",
            [NAME],
        )
        .await?;
        drop(conn);

        assert!(
            !holder.renew(2_000, TTL).await?,
            "a stolen lease cannot be renewed by its old holder"
        );
        assert!(
            !holder.valid(2_000),
            "a lost lease fails closed locally too"
        );
        Ok(())
    }
}
