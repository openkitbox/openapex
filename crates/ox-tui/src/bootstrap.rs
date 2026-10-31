use ox_client::Client;
use ox_client::rest::McpRef;
use ox_client::rest::McpStatus;
use ox_client::rest::{ProviderRef, SessionInfo};

use crate::ui::prompt::Chrome;
use crate::ui::prompt::McpSummary;

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

/// 会话自己记住的 agent / model / provider / variant，已转成 metadata 行的展示形式。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub variant: Option<String>,
    /// 会话标题（`/rename` 的预填值）。
    pub title: Option<String>,
}

/// 纯函数：存储的会话 + provider 目录 → 展示用选择。provider 取目录里的显示名，
/// 找不到时退回 id；`default` variant 与上游 `normalizeModelVariant` 一样视为无。
pub fn selection_of(info: &SessionInfo, providers: &[ProviderRef]) -> Selection {
    let provider = info.provider_id.as_deref().map(|id| {
        providers
            .iter()
            .find(|provider| provider.id == id && !provider.name.is_empty())
            .map_or_else(|| id.to_owned(), |provider| provider.name.clone())
    });
    Selection {
        agent: info
            .agent
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(display_agent),
        model: info.model_id.clone(),
        provider,
        variant: info
            .variant
            .clone()
            .filter(|variant| !variant.is_empty() && variant != "default"),
        title: info.title.clone(),
    }
}

/// Home 的静态装饰数据：默认 agent、默认 model + provider、目录缩写、git 分支。
/// 全部尽力而为——任何一项取不到就留空，prompt 按上游降级链隐藏对应段。
pub async fn chrome(client: &Client, directory: &str, session: Option<&str>) -> Chrome {
    // Home 的 footer 显示服务端解析后的 location（符号链接已展开）；会话页显示
    // session 自带的 location（上游 `session()?.location`，原样存储）。
    let info = match session {
        Some(session) => client.session_info(session).await.ok(),
        None => None,
    };
    let stored = info.as_ref().and_then(|info| info.directory.clone());
    let located = client.location().await.ok().map(|info| info.directory);
    let shown = stored.unwrap_or_else(|| canonical_directory(located.as_deref(), directory));
    let mut out = Chrome {
        directory: abbreviate_home(&shown),
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
    // 会话页：agent / model / variant 以会话自己记住的为准（上游 `promptDisplay`），
    // 取不到的部分保留上面的默认值。
    if let Some(info) = &info {
        let providers = client.providers().await.unwrap_or_default();
        let selection = selection_of(info, &providers);
        if selection.agent.is_some() {
            out.agent = selection.agent;
        }
        if selection.model.is_some() {
            out.model = selection.model;
            out.provider = selection.provider;
            out.variant = selection.variant;
        }
    }
    // MCP 数量（home footer 左下角，上游 `home_footer` 的 Mcp 段）：取不到就整段隐藏。
    out.mcp = client
        .mcp()
        .await
        .ok()
        .and_then(|servers| mcp_summary(&servers));
    out
}

/// 上游 `home_footer` 的 Mcp 段：没有配置任何 server 时为 `None`（整段隐藏，
/// 而不是显示 0）；否则给出 `connected` 数与 `failed` 数——任一失败时上游把
/// 圆点染红并把文案换成失败数。
pub fn mcp_summary(servers: &[McpRef]) -> Option<McpSummary> {
    if servers.is_empty() {
        return None;
    }
    let connected = servers
        .iter()
        .filter(|server| server.status == McpStatus::Connected)
        .count();
    let failed = servers
        .iter()
        .filter(|server| matches!(server.status, McpStatus::Failed { .. }))
        .count();
    Some(McpSummary { connected, failed })
}

/// 显示名：首字母大写（上游 `build` → `Build`）。
pub fn display_agent(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// 展示用目录：优先服务端报告的 location（上游 footer 的来源，已解析符号链接），
/// 其次本地 `canonicalize`，最后原样使用传入的路径。
fn canonical_directory(reported: Option<&str>, given: &str) -> String {
    if let Some(reported) = reported.filter(|path| !path.is_empty()) {
        return reported.to_owned();
    }
    std::fs::canonicalize(given)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| given.to_owned())
}

/// `~` 缩写家目录前缀（footer 左段的路径展示）。
fn abbreviate_home(path: &str) -> String {
    let home = std::env::var_os("HOME").map(|home| home.to_string_lossy().to_string());
    abbreviate_with(path, home.as_deref())
}

/// Pure form of [`abbreviate_home`] so tests never touch the process env.
fn abbreviate_with(path: &str, home: Option<&str>) -> String {
    let home = home.map_or("", |home| home.trim_end_matches('/'));
    match path.strip_prefix(home) {
        Some("") if !home.is_empty() => "~".to_owned(),
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::abbreviate_with;
    use super::canonical_directory;
    use super::display_agent;
    use super::mcp_summary;
    use crate::ui::prompt::McpSummary;
    use ox_client::rest::{McpRef, McpStatus};

    fn server(name: &str, status: McpStatus) -> McpRef {
        McpRef {
            name: name.to_owned(),
            status,
        }
    }

    #[test]
    fn mcp_summary_is_hidden_without_configured_servers() {
        assert_eq!(mcp_summary(&[]), None);
    }

    #[test]
    fn mcp_summary_counts_connected_and_failed() {
        // Upstream counts only `connected` entries; anything failed flips the
        // segment to the failure count. `pending`/`disabled` affect neither.
        let servers = [
            server("a", McpStatus::Connected),
            server("b", McpStatus::Connected),
            server("c", McpStatus::Pending),
            server("d", McpStatus::Disabled),
            server(
                "e",
                McpStatus::Failed {
                    error: "boom".into(),
                },
            ),
        ];
        assert_eq!(
            mcp_summary(&servers),
            Some(McpSummary {
                connected: 2,
                failed: 1
            })
        );
    }

    #[test]
    fn the_displayed_directory_prefers_the_servers_resolved_location() {
        assert_eq!(
            canonical_directory(Some("/private/tmp/x"), "/tmp/x"),
            "/private/tmp/x"
        );
        // Without a server answer the local canonical path is used...
        let dir = std::env::temp_dir();
        let given = dir.join(".");
        assert_eq!(
            canonical_directory(None, &given.to_string_lossy()),
            std::fs::canonicalize(&dir).unwrap().to_string_lossy()
        );
        // ...and a path that does not exist is shown as given.
        assert_eq!(
            canonical_directory(Some(""), "/no/such/dir"),
            "/no/such/dir"
        );
    }

    #[test]
    fn agent_display_name_capitalizes_the_first_letter() {
        assert_eq!(display_agent("build"), "Build");
        assert_eq!(display_agent("code"), "Code");
        assert_eq!(display_agent(""), "");
    }

    #[test]
    fn home_dir_is_abbreviated_to_tilde() {
        let home = Some("/Users/john");
        assert_eq!(
            abbreviate_with("/Users/john/Developer/openkitbox/ox", home),
            "~/Developer/openkitbox/ox"
        );
        assert_eq!(abbreviate_with("/Users/john", home), "~");
        assert_eq!(abbreviate_with("/opt/bin", home), "/opt/bin");
        assert_eq!(abbreviate_with("/opt/bin", None), "/opt/bin");
    }

    #[test]
    fn tilde_abbreviation_stops_at_a_path_boundary() {
        let home = Some("/Users/john");
        assert_eq!(
            abbreviate_with("/Users/johnny/code", home),
            "/Users/johnny/code"
        );
        assert_eq!(abbreviate_with("/Users/john/", home), "~/");
        assert_eq!(abbreviate_with("/x", Some("/")), "/x");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::{Selection, selection_of};
    use ox_client::rest::{ProviderRef, SessionInfo};

    fn info(variant: Option<&str>) -> SessionInfo {
        SessionInfo {
            agent: Some("build".into()),
            model_id: Some("k3-256k".into()),
            provider_id: Some("cardinal".into()),
            variant: variant.map(str::to_owned),
            ..SessionInfo::default()
        }
    }

    fn providers() -> Vec<ProviderRef> {
        vec![ProviderRef {
            id: "cardinal".into(),
            name: "Cardinal".into(),
        }]
    }

    #[test]
    fn the_stored_session_becomes_display_names() {
        assert_eq!(
            selection_of(&info(Some("high")), &providers()),
            Selection {
                agent: Some("Build".into()),
                model: Some("k3-256k".into()),
                provider: Some("Cardinal".into()),
                variant: Some("high".into()),
                title: None,
            }
        );
    }

    #[test]
    fn default_variant_is_none_and_unknown_provider_falls_back_to_its_id() {
        let selection = selection_of(&info(Some("default")), &[]);
        assert_eq!(selection.variant, None);
        assert_eq!(selection.provider.as_deref(), Some("cardinal"));
    }
}
