//! Connection-local session links and in-flight turn signals. Selection is
//! authoritative in ox, not cached here.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_client_protocol::Error;
use agent_client_protocol::v1::schema::{McpServer, SystemPrompt};
use serde::Deserialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use ox_schema::support::ModelRef;

use crate::internal;

#[derive(Deserialize)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) location: Location,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<ModelRef>,
}

#[derive(Deserialize)]
pub(crate) struct Location {
    pub(crate) directory: String,
}

impl SessionInfo {
    pub(crate) fn directory_header(&self) -> Result<String, Error> {
        directory_header(Path::new(&self.location.directory))
    }
}

#[derive(Default, Clone)]
pub(crate) struct Sessions {
    links: Arc<Mutex<HashMap<String, Link>>>,
}

struct Link {
    directory_header: String,
    turn: Option<Arc<Turn>>,
    restoring: bool,
    restore_generation: u64,
    detaching: bool,
}

#[derive(Default)]
pub(super) struct Turn {
    pub(super) cancel: Notify,
    // A successfully admitted prompt cannot be interrupted before its POST completes.
    pub(super) admission_done: CancellationToken,
    pub(super) detached: CancellationToken,
    pub(super) finished: CancellationToken,
    pub(super) cancelled: CancellationToken,
}

/// Reserves a link while its backend work is being settled. An error leaves
/// the link available for retry rather than losing track of live work.
pub(super) struct Detach<'a> {
    pub(super) sessions: &'a Sessions,
    pub(super) id: &'a str,
    pub(super) directory: String,
    pub(super) turn: Option<Arc<Turn>>,
}

impl Detach<'_> {
    pub(super) async fn finish(self) -> Result<(), Error> {
        if let Some(turn) = &self.turn {
            turn.detached.cancel();
            turn.finished.cancelled().await;
        }
        self.sessions.lock()?.remove(self.id);
        Ok(())
    }
}

impl Drop for Detach<'_> {
    fn drop(&mut self) {
        if let Ok(mut links) = self.sessions.lock()
            && let Some(link) = links.get_mut(self.id)
        {
            link.detaching = false;
        }
    }
}

pub(super) struct Restore<'a> {
    sessions: &'a Sessions,
    id: String,
    created: bool,
    generation: u64,
}

impl Restore<'_> {
    pub(super) fn commit(self, directory_header: String) -> Result<(), Error> {
        let mut links = self.sessions.lock()?;
        let Some(link) = links.get_mut(&self.id) else {
            return Err(internal("ACP session restore was cancelled"));
        };
        if link.detaching || link.restore_generation != self.generation {
            return Err(internal("ACP session restore was cancelled"));
        }
        link.directory_header = directory_header;
        link.restoring = false;
        link.restore_generation = 0;
        Ok(())
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        if let Ok(mut links) = self.sessions.lock()
            && let Some(link) = links.get_mut(&self.id)
            && link.restoring
        {
            if self.created {
                links.remove(&self.id);
            } else {
                link.restoring = false;
                link.restore_generation = 0;
            }
        }
    }
}

impl Sessions {
    pub(crate) fn bind(&self, id: String, directory_header: String) -> Result<(), Error> {
        let mut links = self.lock()?;
        links
            .entry(id)
            .and_modify(|link| link.directory_header.clone_from(&directory_header))
            .or_insert_with(|| Link {
                directory_header,
                turn: None,
                restoring: false,
                restore_generation: 0,
                detaching: false,
            });
        Ok(())
    }

    pub(super) fn begin_restore(
        &self,
        id: &str,
        directory_header: String,
    ) -> Result<Restore<'_>, Error> {
        let mut links = self.lock()?;
        let created = if let Some(link) = links.get_mut(id) {
            if link.detaching || link.restoring {
                return Err(internal("ACP session is already closing or restoring"));
            }
            link.restoring = true;
            link.restore_generation = link.restore_generation.wrapping_add(1);
            false
        } else {
            links.insert(
                id.to_owned(),
                Link {
                    directory_header,
                    turn: None,
                    restoring: true,
                    restore_generation: 1,
                    detaching: false,
                },
            );
            true
        };
        Ok(Restore {
            sessions: self,
            id: id.to_owned(),
            created,
            generation: links
                .get(id)
                .map(|link| link.restore_generation)
                .unwrap_or(0),
        })
    }

    pub(crate) fn directory(&self, id: &str) -> Result<String, Error> {
        self.with_link(id, |link| link.directory_header.clone())
    }

    pub(super) fn begin_turn(&self, id: &str) -> Result<Arc<Turn>, Error> {
        self.with_link(id, |link| {
            if link
                .turn
                .as_ref()
                .is_some_and(|turn| !turn.finished.is_cancelled())
            {
                return Err(internal("ACP session already has an active turn"));
            }
            let turn = Arc::new(Turn::default());
            link.turn = Some(turn.clone());
            Ok(turn)
        })?
    }

    pub(super) fn detach<'a>(&'a self, id: &'a str) -> Result<Option<Detach<'a>>, Error> {
        let mut links = self.lock()?;
        let Some(link) = links.get_mut(id) else {
            return Ok(None);
        };
        if link.detaching {
            return Err(internal("ACP session is already closing"));
        }
        link.detaching = true;
        Ok(Some(Detach {
            sessions: self,
            id,
            directory: link.directory_header.clone(),
            turn: link
                .turn
                .as_ref()
                .filter(|turn| !turn.finished.is_cancelled())
                .cloned(),
        }))
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<String, Error> {
        self.with_link(id, |link| {
            if let Some(turn) = &link.turn {
                turn.cancel.notify_one();
                turn.cancelled.cancel();
            }
            link.directory_header.clone()
        })
    }

    fn with_link<T>(&self, id: &str, f: impl FnOnce(&mut Link) -> T) -> Result<T, Error> {
        let mut links = self.lock()?;
        let link = links
            .get_mut(id)
            .ok_or_else(|| Error::invalid_params().data("unknown ACP session"))?;
        if link.detaching {
            return Err(internal("ACP session is closing"));
        }
        Ok(f(link))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Link>>, Error> {
        self.links
            .lock()
            .map_err(|_| internal("ACP session state is unavailable"))
    }
}

pub(super) fn setup_directory(
    cwd: &Path,
    roots: &[PathBuf],
    mcp_servers: &[McpServer],
    system_prompt: Option<&SystemPrompt>,
) -> Result<String, Error> {
    let directory = directory_header(cwd)?;
    for root in roots {
        validate_directory(root).map_err(|_| Error::invalid_params().data("additionalDirectories must contain absolute UTF-8 paths without control characters"))?;
    }
    // The public HTTP API has no trusted session-scoped way to apply these.
    if !roots.is_empty() {
        return Err(Error::invalid_params()
            .data("additionalDirectories is not supported by the ox backend"));
    }
    // Requested-but-unsupported setup must fail closed: silently omitting it
    // would make the editor's session differ from its declared configuration.
    if !mcp_servers.is_empty() {
        return Err(Error::invalid_params().data(
            "ACP per-session MCP servers are unavailable: ox cannot prove session-local tool isolation",
        ));
    }
    if system_prompt.is_some() {
        return Err(Error::invalid_params().data("systemPrompt is not supported by the ox backend"));
    }
    Ok(directory)
}

/// IDs are interpolated into HTTP path and query segments. ox session IDs are
/// opaque strings; reject separators and dot-only segments before any HTTP I/O.
pub(crate) fn validate_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || matches!(id, "." | "..")
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(Error::invalid_params().data("invalid session ID"));
    }
    Ok(())
}

/// ox percent-decodes this header once; encode literal percent signs, spaces
/// and non-ASCII bytes so the HTTP parser cannot alter the path.
pub(crate) fn directory_header(path: &Path) -> Result<String, Error> {
    let raw = validate_directory(path)?;
    let mut header = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_graphic() && byte != b'%' {
            header.push(char::from(byte));
        } else {
            write!(header, "%{byte:02X}").map_err(internal)?;
        }
    }
    Ok(header)
}

pub(super) fn validate_directory(path: &Path) -> Result<&str, Error> {
    let invalid = || {
        Error::invalid_params()
            .data("cwd must be an absolute UTF-8 path without control characters")
    };
    let raw = path
        .to_str()
        .filter(|path| !path.chars().any(char::is_control))
        .ok_or_else(invalid)?;
    if !path.is_absolute() {
        return Err(invalid());
    }
    Ok(raw)
}
#[cfg(test)]
mod tests {
    use super::Sessions;

    #[tokio::test]
    async fn close_prevents_pending_restore_from_re_registering() {
        let sessions = Sessions::default();
        let restore = sessions
            .begin_restore("ses_one", "/work".to_owned())
            .expect("restore reservation");
        let detached = sessions
            .detach("ses_one")
            .expect("detach pending restore")
            .expect("pending restore is observable");
        assert!(restore.commit("/work".to_owned()).is_err());
        detached.finish().await.expect("finish close");
        assert!(sessions.directory("ses_one").is_err());
    }
}
