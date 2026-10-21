mod support;

use ox_client::Client;

#[tokio::test]
async fn client_holds_base_and_directory() {
    let client = Client::new("http://127.0.0.1:1/", "/tmp/proj");
    assert_eq!(client.directory, "/tmp/proj");
    let _ = support::MockOx::default();
}
