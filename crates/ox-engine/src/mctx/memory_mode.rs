//! Per-session memory switch: the control a user has over what a sensitive
//! conversation reads from and leaves in long-term memory.
//!
//! With the switch [`MemoryMode::Off`], the session neither *reads* memory
//! (no `<project-memory>` / `<user-profile>` in the head, no memory hits in
//! the automatic `<recall>` block) nor *leaves* any behind (the historian's
//! `<facts>` and `<user_observations>` are not promoted). Folding is
//! unaffected — the session's own compartments are its own business — and so
//! is `ctx_memory`: a model writing a memory on purpose is a decision the
//! user can see in the transcript, not an extraction.
//!
//! The mode is one `kv` row per session, `mctx_memory_mode:{session}`,
//! present only while the switch is off (the default needs no row). It is
//! read where the two sides of memory meet the session: the pass that builds
//! the head ([`crate::mctx::pass`]), the opening request's recall, and the
//! historian's publish. [`Withheld`] folds the external-content taint
//! ([`crate::mctx::taint`]) into the same question — "may this session's
//! extractions become memory?" — so the historian asks it once.
//!
//! Reference: codex's per-thread `memory_mode` and `memory/reset`.

use anyhow::{Context as _, Result};
use ox_db::{Db, optional};
use ox_schema::id::now_millis;
use serde::{Deserialize, Serialize};

/// Whether a session takes part in long-term memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryMode {
    /// The default: memories are injected and extractions promoted.
    #[default]
    On,
    /// The session is memory-blind: nothing injected, nothing promoted.
    Off,
}

impl MemoryMode {
    /// Whether memory takes part in this session at all.
    #[must_use]
    pub fn is_on(self) -> bool {
        matches!(self, Self::On)
    }

    /// Parse the wire/CLI spelling (`on` / `off`, case-insensitive).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// Why a session's historian extractions are not promoted to memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Withheld {
    /// The user switched memory off for the session.
    MemoryOff,
    /// The session carries external content ([`crate::mctx::taint`]).
    Tainted,
}

impl Withheld {
    /// The reason as it reads in a log line.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MemoryOff => "session memory is off",
            Self::Tainted => "session carries external content",
        }
    }
}

/// Set the session's switch. `On` deletes the row (the default is implicit),
/// `Off` writes it; both are idempotent.
pub async fn set(db: &Db, session_id: &str, mode: MemoryMode) -> Result<()> {
    let conn = db.acquire().await?;
    let key = ox_schema::support::memory_mode_key(session_id);
    match mode {
        MemoryMode::On => {
            conn.execute("DELETE FROM kv WHERE key = ?1", [key.as_str()])
                .await
                .with_context(|| format!("switch memory on for session {session_id}"))?;
        }
        MemoryMode::Off => {
            let now = now_millis();
            conn.execute(
                "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?3",
                (key.as_str(), MemoryMode::Off.as_str(), now),
            )
            .await
            .with_context(|| format!("switch memory off for session {session_id}"))?;
        }
    }
    Ok(())
}

/// The session's switch (`On` when no row exists).
pub async fn mode_of(db: &Db, session_id: &str) -> Result<MemoryMode> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare("SELECT 1 FROM kv WHERE key = ?1")
        .await
        .context("read session memory mode")?;
    let row = optional(
        &mut stmt,
        [ox_schema::support::memory_mode_key(session_id).as_str()],
    )
    .await
    .context("read session memory mode")?;
    Ok(if row.is_some() {
        MemoryMode::Off
    } else {
        MemoryMode::On
    })
}

/// Whether the session's extractions may become memory, and if not, why.
/// The switch is checked first: it is the user's explicit word.
pub(crate) async fn promotion_withheld(db: &Db, session_id: &str) -> Result<Option<Withheld>> {
    if !mode_of(db, session_id).await?.is_on() {
        return Ok(Some(Withheld::MemoryOff));
    }
    if crate::mctx::taint::is_tainted(db, session_id).await? {
        return Ok(Some(Withheld::Tainted));
    }
    Ok(None)
}
