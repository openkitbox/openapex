pub mod error;
pub mod form;
pub mod history;
pub mod rest;
pub mod sse;
pub mod store;
pub mod todo;

pub use error::{Error, Result};

/// Placeholder base URL for unix-socket clients: the host is never resolved,
/// every request goes to the socket given to [`Client::over_unix`].
#[cfg(unix)]
pub const UNIX_BASE: &str = "http://ox";

/// The HTTP types behind [`Client`], for callers that hold a raw response.
pub use reqwest;

/// Protocol client. Holds the base URL and the directory every request must
/// echo (`x-opencode-directory`), because ox/opencode resolve locations from it.
#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    pub base: String,
    pub directory: String,
}

impl Client {
    pub fn new(base: impl Into<String>, directory: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into().trim_end_matches('/').to_owned(),
            directory: directory.into(),
        }
    }

    /// Client that talks to a local server over a unix domain socket. REST and
    /// SSE both go through `http`, so every request URL is `UNIX_BASE`-based.
    #[cfg(unix)]
    pub fn over_unix(
        path: impl AsRef<std::path::Path>,
        directory: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .unix_socket(path.as_ref())
                .build()?,
            base: UNIX_BASE.to_owned(),
            directory: directory.into(),
        })
    }
}
