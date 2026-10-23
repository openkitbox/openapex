use ox_client::Client;

use crate::ui::prompt::Chrome;

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

/// Home 的静态装饰数据：默认 agent、默认 model + provider、目录缩写、git 分支。
/// 全部尽力而为——任何一项取不到就留空，prompt 按上游降级链隐藏对应段。
pub async fn chrome(client: &Client, directory: &str) -> Chrome {
    let mut out = Chrome {
        directory: abbreviate_home(directory),
        branch: git_branch(directory),
        ..Chrome::default()
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
    // 上游 metadata 行把 model 与 provider 当作两段：model 是 id，provider 是显示名。
    if let Ok(model) = client.default_model().await {
        out.model = Some(model.model_id.clone());
        if let Ok(providers) = client.providers().await {
            out.provider = providers
                .into_iter()
                .find(|provider| provider.id == model.provider_id)
                .map(|provider| provider.name)
                .filter(|name| !name.is_empty());
        }
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

/// `~` 缩写家目录前缀（footer 左段的路径展示）。
fn abbreviate_home(path: &str) -> String {
    let home = std::env::var_os("HOME").map(|home| home.to_string_lossy().to_string());
    abbreviate_with(path, home.as_deref())
}

/// Pure form of [`abbreviate_home`] so tests never touch the process env.
fn abbreviate_with(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_owned();
    };
    if !path.starts_with(home) {
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
        .args(["--no-optional-locks", "branch", "--show-current"])
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
