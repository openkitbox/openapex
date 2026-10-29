//! Retry policy for transient provider failures.
//!
//! Owns two decisions and nothing else: which [`CardinalError`]s are worth
//! retrying ([`classify`]) and how long to wait, or whether to give up
//! ([`Backoff::on_failure`]). Pure, no I/O; the caller (`step.rs`) owns
//! events, sleeping and settlement.
//!
//! [`Backoff`] owns three independent counters for one step:
//!
//! - `network`: consecutive [`RetryKind::Network`] failures; drives the delay
//!   ladder `min(2s * 2^(n-1), 10s)`, never exhausts, and restarts whenever
//!   an attempt received model output (progress was made).
//! - `bounded`: [`RetryKind::Bounded`] failures, capped at
//!   [`MAX_BOUNDED_RETRIES`]; never reset, not even by progress. A long
//!   network outage does not consume this allowance.
//! - `continuations`: attempts that failed after receiving model output,
//!   capped at [`MAX_CONTINUATIONS`], whatever the error kind.
//!
//! Deliberate deviations from upstream `model-retry.ts`: network-class errors
//! retry without limit, the per-step continuation cap, and there is no jitter
//! and no `retry-after` handling (`CardinalError::Http` carries no headers;
//! single user, no thundering herd).

use std::time::Duration;

use ox_llm::cardinal::CardinalError;

/// Maximum consecutive failures for [`RetryKind::Bounded`] errors (upstream
/// `model-retry.ts` `recurs(10)`).
pub(crate) const MAX_BOUNDED_RETRIES: usize = 10;

/// Maximum attempts per step that fail after receiving model output. A
/// flapping upstream that cuts every stream after a few tokens must not loop
/// forever: each cycle costs a billed request, a partial assistant row and a
/// synthetic continue message.
pub(crate) const MAX_CONTINUATIONS: usize = 10;

const BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(10);

/// How an error should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryKind {
    /// Fail immediately.
    Never,
    /// Retry up to [`MAX_BOUNDED_RETRIES`] consecutive failures.
    Bounded,
    /// Retry until it recovers (or the user interrupts).
    Network,
}

/// Classify a provider error. Exhaustive on purpose: a new variant must get an
/// explicit decision here.
pub(crate) fn classify(error: &CardinalError) -> RetryKind {
    match error {
        CardinalError::Transport(e) => {
            // `is_decode()` is a body read failure (reqwest wraps every
            // `Response::chunk()` error as decode; ox-llm never parses JSON
            // through reqwest), so it falls through to `Network`.
            if e.is_builder() {
                RetryKind::Never
            } else if e.is_redirect() {
                RetryKind::Bounded
            } else {
                RetryKind::Network
            }
        }
        CardinalError::EarlyClose => RetryKind::Network,
        CardinalError::Http { status, .. } => match *status {
            408 | 429 | 500.. => RetryKind::Bounded,
            _ => RetryKind::Never,
        },
        CardinalError::Upstream(_) | CardinalError::Sse(_) => RetryKind::Bounded,
        CardinalError::NoRoute { .. } => RetryKind::Never,
    }
}

/// Delay for the `n`-th consecutive failure: `min(2s * 2^(n-1), 10s)`;
/// `n == 0` counts as 1.
fn ladder(n: usize) -> Duration {
    // Clamp the shift: 2s * 2^4 already exceeds the cap.
    let shift = u32::try_from(n.max(1) - 1).unwrap_or(u32::MAX).min(4);
    BASE_DELAY.saturating_mul(1 << shift).min(MAX_DELAY)
}

/// Retry state of one step: decides the delay before the next attempt, or
/// `None` when the step must fail. See the module doc for the counters.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    network: usize,
    bounded: usize,
    continuations: usize,
}

impl Backoff {
    /// Record a failed attempt and return the delay before the next one, or
    /// `None` to give up. `produced` says the attempt received model output
    /// before failing.
    ///
    /// - `Never` => `None`, counters untouched.
    /// - `produced` => `continuations += 1` and the network ladder restarts;
    ///   more than [`MAX_CONTINUATIONS`] => `None`, for any kind.
    /// - `Network` => `ladder(network)`, never exhausts.
    /// - `Bounded` => `ladder(bounded)`; more than [`MAX_BOUNDED_RETRIES`]
    ///   => `None`. Never reset, not even by progress.
    pub(crate) fn on_failure(&mut self, kind: RetryKind, produced: bool) -> Option<Duration> {
        if kind == RetryKind::Never {
            return None;
        }
        if produced {
            self.continuations = self.continuations.saturating_add(1);
            self.network = 0;
            if self.continuations > MAX_CONTINUATIONS {
                return None;
            }
        }
        match kind {
            RetryKind::Never => None,
            RetryKind::Network => {
                self.network = self.network.saturating_add(1);
                Some(ladder(self.network))
            }
            RetryKind::Bounded => {
                self.bounded = self.bounded.saturating_add(1);
                (self.bounded <= MAX_BOUNDED_RETRIES).then(|| ladder(self.bounded))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn secs(n: u64) -> Option<Duration> {
        Some(Duration::from_secs(n))
    }

    fn backoff_delays(kind: RetryKind, n: usize) -> Vec<Option<Duration>> {
        let mut b = Backoff::default();
        (0..n).map(|_| b.on_failure(kind, false)).collect()
    }

    #[test]
    fn network_delay_sequence_caps_at_ten_seconds() {
        assert_eq!(
            backoff_delays(RetryKind::Network, 6),
            [secs(2), secs(4), secs(8), secs(10), secs(10), secs(10)]
        );
    }

    #[test]
    fn ladder_zero_counts_as_one_and_huge_does_not_overflow() {
        assert_eq!(ladder(0), ladder(1));
        assert_eq!(ladder(usize::MAX), Duration::from_secs(10));
    }

    #[test]
    fn network_never_gives_up() {
        let mut b = Backoff::default();
        for i in 0..10_000 {
            assert!(b.on_failure(RetryKind::Network, false).is_some(), "{i}");
        }
    }

    #[test]
    fn bounded_gives_up_after_the_cap() {
        let mut b = Backoff::default();
        for i in 1..=MAX_BOUNDED_RETRIES {
            assert!(b.on_failure(RetryKind::Bounded, false).is_some(), "{i}");
        }
        assert_eq!(b.on_failure(RetryKind::Bounded, false), None);
        assert_eq!(b.on_failure(RetryKind::Bounded, false), None);
        assert_eq!(backoff_delays(RetryKind::Bounded, 1), [secs(2)]);
    }

    #[test]
    fn network_storm_does_not_consume_the_bounded_allowance() {
        let mut b = Backoff::default();
        for _ in 0..50 {
            b.on_failure(RetryKind::Network, false);
        }
        for i in 1..=MAX_BOUNDED_RETRIES {
            assert!(b.on_failure(RetryKind::Bounded, false).is_some(), "{i}");
        }
        assert_eq!(b.on_failure(RetryKind::Bounded, false), None);
    }

    #[test]
    fn progress_restarts_the_network_ladder_but_not_the_bounded_cap() {
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.on_failure(RetryKind::Network, false);
        }
        assert_eq!(b.on_failure(RetryKind::Network, true), secs(2));
        assert_eq!(b.on_failure(RetryKind::Network, false), secs(4));

        let mut b = Backoff::default();
        for _ in 0..MAX_BOUNDED_RETRIES - 1 {
            b.on_failure(RetryKind::Bounded, false);
        }
        assert!(b.on_failure(RetryKind::Bounded, true).is_some());
        assert_eq!(b.on_failure(RetryKind::Bounded, false), None);
    }

    #[test]
    fn network_continuations_are_capped() {
        // Only a network-class failure shows the continuation cap on its own:
        // for `Bounded` the bounded counter reaches its limit on the same call.
        let mut b = Backoff::default();
        for i in 1..=MAX_CONTINUATIONS {
            assert!(b.on_failure(RetryKind::Network, true).is_some(), "{i}");
        }
        assert_eq!(b.on_failure(RetryKind::Network, true), None);
        // The cap only applies to attempts that produced output.
        assert!(b.on_failure(RetryKind::Network, false).is_some());
    }

    #[test]
    fn never_has_no_delay_and_touches_no_counter() {
        let mut b = Backoff::default();
        for produced in [false, true] {
            assert_eq!(b.on_failure(RetryKind::Never, produced), None);
        }
        assert_eq!(b.on_failure(RetryKind::Network, false), secs(2));
        for i in 1..=MAX_CONTINUATIONS {
            assert!(b.on_failure(RetryKind::Network, true).is_some(), "{i}");
        }
    }

    #[test]
    fn counters_saturate_instead_of_overflowing() {
        let mut b = Backoff {
            network: usize::MAX,
            bounded: usize::MAX,
            continuations: usize::MAX,
        };
        assert_eq!(b.on_failure(RetryKind::Network, false), secs(10));
        assert_eq!(b.on_failure(RetryKind::Bounded, false), None);
        assert_eq!(b.on_failure(RetryKind::Network, true), None);
    }

    fn http(status: u16) -> CardinalError {
        CardinalError::Http {
            status,
            body: String::new(),
        }
    }

    #[test]
    fn http_statuses() {
        for s in [400, 401, 403, 404] {
            assert_eq!(classify(&http(s)), RetryKind::Never, "{s}");
        }
        for s in [408, 429, 500, 502, 503, 529] {
            assert_eq!(classify(&http(s)), RetryKind::Bounded, "{s}");
        }
    }

    #[test]
    fn non_transport_variants() {
        assert_eq!(classify(&CardinalError::EarlyClose), RetryKind::Network);
        assert_eq!(
            classify(&CardinalError::Upstream("x".into())),
            RetryKind::Bounded
        );
        assert_eq!(
            classify(&CardinalError::Sse("x".into())),
            RetryKind::Bounded
        );
        assert_eq!(
            classify(&CardinalError::NoRoute { model: "m".into() }),
            RetryKind::Never
        );
    }

    /// One-shot loopback server: accepts a connection, optionally reads the
    /// request, then writes `reply` (or holds the socket open forever when
    /// `None`). Returns the base URL.
    async fn serve_once(reply: Option<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            match reply {
                Some(r) => {
                    let _ = sock.write_all(r.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
                None => std::future::pending::<()>().await,
            }
        });
        url
    }

    #[tokio::test]
    async fn connect_error_is_network() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let e = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}"))
            .send()
            .await
            .unwrap_err();
        assert!(e.is_connect());
        assert_eq!(classify(&CardinalError::Transport(e)), RetryKind::Network);
    }

    #[tokio::test]
    async fn timeout_error_is_network() {
        let url = serve_once(None).await;
        let e = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap_err();
        assert!(e.is_timeout());
        assert_eq!(classify(&CardinalError::Transport(e)), RetryKind::Network);
    }

    #[tokio::test]
    async fn builder_error_is_never() {
        let e = reqwest::Client::new()
            .get("not a url")
            .send()
            .await
            .unwrap_err();
        assert!(e.is_builder());
        assert_eq!(classify(&CardinalError::Transport(e)), RetryKind::Never);
    }

    /// reqwest wraps every body-read failure from `Response::chunk()` as
    /// `Kind::Decode`; ox-llm only reads responses chunk-wise, so a decode
    /// error is a connection/body read failure, not bad JSON.
    #[tokio::test]
    async fn truncated_body_read_is_network() {
        let url = serve_once(Some(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nhello",
        ))
        .await;
        let mut response = reqwest::Client::new().get(url).send().await.unwrap();
        let e = loop {
            match response.chunk().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("truncated body ended cleanly"),
                Err(e) => break e,
            }
        };
        assert!(e.is_decode());
        assert_eq!(classify(&CardinalError::Transport(e)), RetryKind::Network);
    }

    #[tokio::test]
    async fn redirect_error_is_bounded() {
        let url = serve_once(Some(
            "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ))
        .await;
        let e = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(0))
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap_err();
        assert!(e.is_redirect());
        assert_eq!(classify(&CardinalError::Transport(e)), RetryKind::Bounded);
    }
}
