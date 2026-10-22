use ox_client::Client;

use crate::ui::home::Chrome;

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

/// Home 的静态装饰数据：默认 agent、默认 model、目录缩写、git 分支。
/// 全部尽力而为——任何一项取不到就留空，home 按上游降级链隐藏对应段。
pub async fn chrome(client: &Client, directory: &str) -> Chrome {
    let mut out = Chrome {
        agent: None,
        model: None,
        directory: abbreviate_home(directory),
        branch: git_branch(directory),
    };
    // 默认 agent：第一个可见的 primary 条目，显示名首字母大写。
    if let Ok(agents) = client.agents().await {
        let agent = agents
            .into_iter()
            .find(|agent| !agent.hidden && agent.mode.as_deref() != Some("subagent"));
        if let Some(agent) = agent {
            out.agent = Some(display_agent(&agent.name));
        }
    }
    // model 行：`<modelID> <provider 显示名>`（provider 名缺省时只显 model）。
    if let Ok(model) = client.default_model().await {
        let provider = client
            .providers()
            .await
            .ok()
            .and_then(|providers| {
                providers
                    .into_iter()
                    .find(|provider| provider.id == model.provider_id)
            })
            .map(|provider| provider.name);
        out.model = Some(model_label(&model.model_id, provider.as_deref()));
    }
    out
}

/// 显示名：首字母大写（上游 `build` → `Build`）。
fn display_agent(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `<modelID> <provider>`；provider 显示名缺省时只显 model。
fn model_label(model_id: &str, provider: Option<&str>) -> String {
    match provider {
        Some(provider) if !provider.is_empty() => format!("{model_id} {provider}"),
        _ => model_id.to_owned(),
    }
}

/// `~` 缩写家目录前缀（footer 左段的路径展示）。
fn abbreviate_home(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_owned();
    };
    let home = home.to_string_lossy().to_string();
    if home.is_empty() || !path.starts_with(&home) {
        return path.to_owned();
    }
    let rest = &path[home.len()..];
    if rest.is_empty() {
        "~".to_owned()
    } else {
        format!("~{rest}")
    }
}

/// 当前分支名；非 git 目录或 git 缺失时 None。
fn git_branch(directory: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(directory)
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!branch.is_empty()).then_some(branch)
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

#[cfg(test)]
mod tests {
    use super::{abbreviate_home, display_agent, model_label};

    #[test]
    fn agent_display_name_capitalizes_the_first_letter() {
        assert_eq!(display_agent("build"), "Build");
        assert_eq!(display_agent("code"), "Code");
        assert_eq!(display_agent(""), "");
    }

    #[test]
    fn model_label_joins_the_provider_display_name() {
        assert_eq!(model_label("k3-256k", Some("Cardinal")), "k3-256k Cardinal");
        assert_eq!(model_label("k3-256k", None), "k3-256k");
        assert_eq!(model_label("k3-256k", Some("")), "k3-256k");
    }

    #[test]
    fn home_dir_is_abbreviated_to_tilde() {
        unsafe { std::env::set_var("HOME", "/Users/john") };
        assert_eq!(
            abbreviate_home("/Users/john/Developer/openkitbox/ox"),
            "~/Developer/openkitbox/ox"
        );
        assert_eq!(abbreviate_home("/Users/john"), "~");
        assert_eq!(abbreviate_home("/opt/bin"), "/opt/bin");
    }
}
