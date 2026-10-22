use ox_client::Client;

pub struct Bootstrap {
    pub client: Client,
    /// `None` on the home route: the session is created on first submit.
    pub session: Option<String>,
}

/// Fast-fail reachability probe (keeps the old startup behaviour without
/// creating a session).
pub async fn check_reachable(server: &str, directory: &str) -> anyhow::Result<Client> {
    let client = Client::new(server, directory);
    let _location = client.location().await?;
    Ok(client)
}

/// Connect for the `--session` path: probe, then keep (or create) a session.
pub async fn connect(
    server: &str,
    directory: &str,
    session: Option<String>,
) -> anyhow::Result<Bootstrap> {
    let client = check_reachable(server, directory).await?;
    let session = match session {
        Some(session) => Some(session),
        None => Some(client.create_session().await?),
    };
    Ok(Bootstrap { client, session })
}
