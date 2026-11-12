use ox_client::{Client, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn server(response: impl Into<String>) -> (Client, tokio::task::JoinHandle<String>) {
    let response = response.into();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        let mut bytes = Vec::new();
        let mut chunk = [0; 1024];
        while !bytes.ends_with(b"\r\n\r\n") {
            let count = stream.read(&mut chunk).await.expect("read request");
            assert!(count > 0, "client closed before sending request headers");
            bytes.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        String::from_utf8(bytes).expect("request headers are UTF-8")
    });
    (
        Client::new(format!("http://{address}"), "/example/project"),
        task,
    )
}

#[tokio::test]
async fn reload_posts_with_directory() {
    let (client, request) = server("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").await;
    client.reload_config().await.expect("reload succeeds");
    let request = request.await.expect("server task").to_ascii_lowercase();
    assert!(
        request.starts_with("post /api/location/reload http/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains("x-opencode-directory: /example/project\r\n"),
        "{request}"
    );
}

#[tokio::test]
async fn reload_reports_server_message_and_falls_back_to_status() {
    let body = r#"{"message":"bad configuration"}"#;
    let response = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (client, request) = server(response).await;
    let error = client.reload_config().await.expect_err("reload fails");
    assert!(error.to_string().contains("bad configuration"), "{error}");
    request.await.expect("server task");

    let (client, request) =
        server("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 3\r\n\r\nbad").await;
    let error = client.reload_config().await.expect_err("reload fails");
    assert!(
        matches!(error, Error::Status(status) if status == 503),
        "{error}"
    );
    request.await.expect("server task");
}

#[tokio::test]
async fn reload_rejects_unexpected_success_status() {
    let (client, request) = server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
    let error = client
        .reload_config()
        .await
        .expect_err("only 204 confirms reload");
    assert!(
        matches!(error, Error::Status(status) if status == 200),
        "{error}"
    );
    request.await.expect("server task");
}
