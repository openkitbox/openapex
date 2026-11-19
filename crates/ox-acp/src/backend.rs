//! Select the ox HTTP backend and manage its lifetime for one ACP connection.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use ox_core::config::Config;
use reqwest::{Client, Response};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const SOCKET_ORIGIN: &str = "http://localhost";

pub(super) struct Backend {
    pub(super) http: Client,
    pub(super) base: String,
    pub(super) lifetime: Lifetime,
}

pub(super) enum Lifetime {
    Attached,
    Leased(Response),
    Owned {
        stop: CancellationToken,
        served: JoinHandle<Result<()>>,
    },
}

impl Backend {
    pub(super) async fn connect(explicit_base: Option<String>, config: Config) -> Result<Self> {
        if let Some(base) = explicit_base {
            let base = base.trim_end_matches('/').to_owned();
            let http = Client::builder().build()?;
            let status = probe(&http, &base)
                .await
                .context("ox acp: could not reach --server")?;
            ensure!(
                status.is_success(),
                "ox acp: --server /api/info returned {status}"
            );
            return Ok(Self {
                http,
                base,
                lifetime: Lifetime::Attached,
            });
        }
        if let Some(backend) = Self::local_socket().await? {
            return Ok(backend);
        }
        let base = format!("http://{}", config.listen);
        let http = Client::builder().build()?;
        if probe(&http, &base)
            .await
            .is_ok_and(|status| status.is_success())
        {
            return Ok(Self {
                http,
                base,
                lifetime: Lifetime::Attached,
            });
        }
        Self::start(http, config).await
    }

    async fn local_socket() -> Result<Option<Self>> {
        let socket = ox_schema::paths::socket_path();
        let http = Client::builder()
            .unix_socket(socket.as_path())
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        // Bound the wait for headers, not the lifetime of the streaming lease.
        let lease = tokio::time::timeout(
            PROBE_TIMEOUT,
            http.get(format!("{SOCKET_ORIGIN}/ox/lease")).send(),
        )
        .await;
        match lease {
            Ok(Ok(lease)) if lease.status().is_success() => {
                return Ok(Some(Self {
                    http,
                    base: SOCKET_ORIGIN.to_owned(),
                    lifetime: Lifetime::Leased(lease),
                }));
            }
            Ok(Ok(response)) => {
                tracing::debug!(status = %response.status(), path = %socket.display(), "local ACP socket is unavailable");
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, path = %socket.display(), "local ACP socket probe failed");
            }
            Err(_) => {
                tracing::debug!(path = %socket.display(), "local ACP socket probe timed out");
            }
        }
        // Automatic discovery can still use the configured TCP endpoint.
        Ok(None)
    }

    async fn start(http: Client, config: Config) -> Result<Self> {
        use salvo::conn::Listener as _;

        // Bind before composing so a bind failure cannot leave an open DB or children.
        let acceptor = salvo::conn::TcpListener::new("127.0.0.1:0")
            .try_bind()
            .await
            .context("ox acp: binding an ephemeral port failed")?;
        let addr = acceptor
            .local_addr()
            .context("ox acp: reading the bound port failed")?;
        let (state, router) = ox_server::compose(config)
            .await
            .context("ox acp: composing the in-process server failed")?;
        let stop = state.runtime.stop().clone();
        let served = tokio::spawn(ox_server::serve_on(acceptor, state, router));
        tracing::info!(%addr, "ox acp owns an in-process server");
        Ok(Self {
            http,
            base: format!("http://{addr}"),
            lifetime: Lifetime::Owned { stop, served },
        })
    }
}

impl Lifetime {
    pub(super) async fn shutdown(self) {
        match self {
            Self::Attached => {}
            Self::Leased(lease) => drop(lease),
            Self::Owned { stop, served } => {
                stop.cancel();
                match served.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::error!(%error, "ox acp: in-process server failed"),
                    Err(error) => tracing::error!(%error, "ox acp: serve task did not finish"),
                }
            }
        }
    }
}

async fn probe(http: &Client, base: &str) -> reqwest::Result<reqwest::StatusCode> {
    let response = http
        .get(format!("{base}/api/info"))
        .timeout(PROBE_TIMEOUT)
        .send()
        .await?;
    Ok(response.status())
}
