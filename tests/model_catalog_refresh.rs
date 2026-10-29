//! `ModelCatalog::refresh` against a mock `/v1/models` endpoint: success
//! replaces the snapshot; when every endpoint then fails the call errors and
//! the previous snapshot is kept.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ox::config::LlmConfig;
use ox::model_catalog::ModelCatalog;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const LIST: &str = r#"{"data":[{"id":"m1","owned_by":"p"}]}"#;

/// Serves the model list on the first connection and HTTP 500 afterwards.
async fn spawn_flaky() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let first = hits.fetch_add(1, Ordering::SeqCst) == 0;
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = if first {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{LIST}",
                    LIST.len()
                )
            } else {
                "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                    .to_owned()
            };
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    url
}

#[tokio::test]
async fn refresh_errors_when_every_endpoint_fails_and_keeps_the_snapshot() {
    let config = LlmConfig {
        cardinal_url: spawn_flaky().await,
        ..LlmConfig::default()
    };
    let catalog = ModelCatalog::new(&config);

    catalog.refresh().await.expect("first fetch succeeds");
    let ids: Vec<_> = catalog
        .cached()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert_eq!(ids, ["m1"]);

    let error = catalog
        .refresh()
        .await
        .expect_err("every endpoint now fails");
    assert!(error.to_string().contains("endpoints failed"), "{error}");
    let ids: Vec<_> = catalog
        .cached()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert_eq!(ids, ["m1"], "the previous snapshot is kept");
}
