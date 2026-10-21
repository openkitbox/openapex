use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server returned {0}")]
    Status(reqwest::StatusCode),
    #[error("event stream closed")]
    StreamClosed,
    #[error("no event or heartbeat for 30s")]
    HeartbeatTimeout,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
