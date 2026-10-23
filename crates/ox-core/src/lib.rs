//! Shared core for ox: configuration, the model catalogue, the event bus and
//! its projector, the outbound HTTP identity, and the `§N§` tag stripper.
//!
//! Depends only on the contract (`ox-schema`) and storage (`ox-db`) crates —
//! everything engine-shaped (runner, tools, mctx) sits above it.

pub mod config;
pub mod copied_tags;
pub mod event_bus;
pub mod model_catalog;
pub mod projector;
pub mod reminder_tags;
pub mod which;

/// How ox identifies itself to everything it calls: the LLM gateway, the model
/// catalogue, the embeddings endpoint. One value, because a request's origin
/// must be unambiguous at the far end, and `ox/<version>` is what tells two ox
/// builds apart in a gateway's logs.
pub const USER_AGENT: &str = concat!("ox/", env!("CARGO_PKG_VERSION"));

/// The reqwest builder every outbound client starts from.
///
/// Identity and nothing else: each client still sets its own timeouts, because
/// a streaming completion and a 30-second embed call do not share one. Going
/// through here is what keeps a new client from sending no `User-Agent` at all,
/// which is reqwest's default and therefore an easy mistake to inherit.
pub fn http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder().user_agent(USER_AGENT)
}
