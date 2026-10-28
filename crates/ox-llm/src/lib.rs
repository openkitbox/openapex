//! LLM endpoint access for ox: the endpoint configuration schema
//! ([`config`]), the model catalogue ([`model_catalog`]), the streaming chat
//! transport with multi-endpoint routing ([`cardinal`]), and the outbound HTTP
//! identity ([`http_client`]).
//!
//! Depends only on the contract crate (`ox-schema`) — never on `ox-core`,
//! `ox-db` or the engine. `ox-core` embeds [`config::LlmConfig`] in its
//! `Config`; the engine and server consume the [`cardinal::chat::Chat`] seam.

pub mod cardinal;
pub mod config;
pub mod model_catalog;

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
