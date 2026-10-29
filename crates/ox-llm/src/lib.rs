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

use std::time::Duration;

/// How ox identifies itself to everything it calls: the LLM gateway, the model
/// catalogue, the embeddings endpoint. One value, because a request's origin
/// must be unambiguous at the far end, and `ox/<version>` is what tells two ox
/// builds apart in a gateway's logs.
pub const USER_AGENT: &str = concat!("ox/", env!("CARGO_PKG_VERSION"));

/// How long a connection must be idle before the OS sends the first TCP
/// keepalive probe.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);

/// Gap between keepalive probes once probing has started.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Unanswered probes after which the OS declares the peer dead.
const KEEPALIVE_RETRIES: u32 = 3;

/// The reqwest builder every outbound client starts from.
///
/// Identity and transport liveness: each client still sets its own timeouts,
/// because a streaming completion and a 30-second embed call do not share one.
/// Going through here is what keeps a new client from sending no `User-Agent`
/// at all, which is reqwest's default and therefore an easy mistake to inherit.
///
/// reqwest already enables TCP keepalive by default (idle 15s, interval 15s,
/// 3 probes, plus `tcp_user_timeout` 30s on Linux), so a half-open connection
/// (network switch, sleep/wake), which delivers no error, is detected anyway,
/// after about 60s. The explicit settings below only tighten the probe
/// interval from 15s to 5s, bringing detection to about
/// `idle + interval * retries` (≈ 30s); the read then fails with a transport
/// error the step retry loop handles. On Windows hyper-util applies the idle
/// time and interval but not the retry count (a no-op there). This
/// deliberately is not a byte-silence timeout: reasoning models may
/// legitimately stay silent for minutes, and keepalive judges the network,
/// not the model.
pub fn http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .tcp_keepalive(KEEPALIVE_IDLE)
        .tcp_keepalive_interval(KEEPALIVE_INTERVAL)
        .tcp_keepalive_retries(KEEPALIVE_RETRIES)
}
