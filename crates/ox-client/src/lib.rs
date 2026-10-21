pub mod error;

pub use error::{Error, Result};

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
}
