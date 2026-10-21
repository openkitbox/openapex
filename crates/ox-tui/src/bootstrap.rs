use ox_client::Client;

pub struct Bootstrap {
    pub client: Client,
    pub session: String,
}

pub async fn connect(
    server: &str,
    directory: &str,
    session: Option<String>,
) -> anyhow::Result<Bootstrap> {
    let client = Client::new(server, directory);
    // 先确认协议可达（也是 server.connected 之前的快速失败点）。
    let _location = client.location().await?;
    let session = match session {
        Some(session) => session,
        None => client.create_session().await?,
    };
    Ok(Bootstrap { client, session })
}
