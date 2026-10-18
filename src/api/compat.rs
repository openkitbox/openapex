//! Catalog endpoints the clients probe at startup that ox answers with
//! empty or trivially-derived data. Shapes follow upstream's protocol
//! (`packages/protocol/openapi.json`); semantics are the ox MVP subset —
//! no plugins, commands, MCP, skills, forms, or integrations exist, and
//! config is not yet modeled.

use salvo::prelude::*;
use serde_json::json;

/// The request's project directory (upstream `location.ts:73-75`):
/// `location[directory]` query, then the percent-encoded
/// /// `x-opencode-directory` header; `None` lets the caller fall back to the
/// server's own directory.
pub(crate) fn request_directory(req: &Request) -> Option<String> {
    if let Some(directory) = req.query::<String>("location[directory]") {
        return Some(directory);
    }
    req.headers()
        .get("x-opencode-directory")
        .and_then(|value| value.to_str().ok())
        .map(percent_decode)
}

/// `decodeURIComponent` semantics with upstream's raw fallback: `%XX` byte
/// sequences decoded into a UTF-8 string; malformed input comes back as-is.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = (
                std::str::from_utf8(&bytes[index + 1..index + 2]).ok(),
                std::str::from_utf8(&bytes[index + 2..index + 3]).ok(),
            );
            if let (Some(high), Some(low)) = hex
                && let (Ok(high), Ok(low)) =
                    (u8::from_str_radix(high, 16), u8::from_str_radix(low, 16))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

/// The `{ location: { directory }, data }` envelope catalog endpoints share.
fn envelope(directory: &str, data: serde_json::Value) -> serde_json::Value {
    json!({ "location": { "directory": directory }, "data": data })
}

/// `GET /api/experimental/migration/v1` — upstream `experimental.migration
/// .v1.status`. ox owns its own database; there is nothing to migrate.
#[handler] // operation_id: experimental.migration.v1.status
pub(crate) async fn migration_status(res: &mut Response) {
    res.render(Json(json!({ "status": "completed" })));
}

/// `GET /api/config` — upstream `config.get`. ox has no user config model;
/// an empty entry list.
#[handler] // operation_id: config.get
pub(crate) async fn config_get(res: &mut Response) {
    res.render(Json(json!([])));
}

/// Empty catalog lists: command / mcp / provider / reference / skill /
/// shell / form / plugin / integration.
macro_rules! empty_catalog {
    ($name:ident, $operation:literal) => {
        #[handler] // operation_id: $operation
        pub(crate) async fn $name(req: &mut Request, res: &mut Response) {
            let directory = request_directory(req).unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .display()
                    .to_string()
            });
            res.render(Json(envelope(&directory, json!([]))));
        }
    };
}

empty_catalog!(command_list, "command.list");
empty_catalog!(mcp_list, "mcp.list");
empty_catalog!(provider_list, "provider.list");
empty_catalog!(reference_list, "reference.list");
empty_catalog!(skill_list, "skill.list");
empty_catalog!(shell_list, "shell.list");
empty_catalog!(form_list, "form.list");
empty_catalog!(plugin_list, "plugin.list");
empty_catalog!(integration_list, "integration.list");

/// `GET /api/mcp/resource` — upstream `mcp.resource.catalog`. ox has no MCP
/// client, so the catalog is empty.
#[handler] // operation_id: mcp.resource.catalog
pub(crate) async fn mcp_resource_catalog(req: &mut Request, res: &mut Response) {
    let directory = request_directory(req).unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });
    res.render(Json(envelope(
        &directory,
        json!({ "resources": [], "templates": [] }),
    )));
}

/// `GET /api/vcs` — upstream `vcs.get`: the directory's git branch read with
/// a plain `git` invocation; not-a-repo degrades to empty branch names.
#[handler] // operation_id: vcs.get
pub(crate) async fn vcs_get(req: &mut Request, res: &mut Response) {
    let directory = request_directory(req).unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });
    let current = tokio::process::Command::new("git")
        .args(["-C", &directory, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    res.render(Json(envelope(
        &directory,
        json!({ "branch": { "current": current, "default": "" } }),
    )));
}

/// Directories skipped by the fs walker (build/VCS noise, never useful in
/// the file picker).
const FS_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
];

/// Collect relative file/dir paths under `root` (bounded walk).
fn walk(root: &std::path::Path, limit: usize) -> Vec<(String, &'static str)> {
    let mut found = Vec::new();
    let mut frontier = vec![std::path::PathBuf::from("")];
    while let Some(relative) = frontier.pop() {
        let absolute = root.join(&relative);
        let Ok(entries) = std::fs::read_dir(&absolute) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let path = if relative.as_os_str().is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", relative.display())
            };
            if is_dir {
                if FS_SKIP.contains(&name.as_str()) {
                    continue;
                }
                frontier.push(std::path::PathBuf::from(&path));
                found.push((path.clone(), "directory"));
            } else {
                found.push((path, "file"));
            }
            if found.len() >= limit {
                return found;
            }
        }
    }
    found
}

fn location_directory(req: &Request) -> String {
    request_directory(req)
        .map(|raw| {
            let path = std::path::PathBuf::from(&raw);
            path.canonicalize().unwrap_or(path).display().to_string()
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string()
        })
}

/// `GET /api/fs/list?location[directory]=X[&path=Y]` — upstream `fs.list`:
/// relative entries under the directory.
#[handler] // operation_id: fs.list
pub(crate) async fn fs_list(req: &mut Request, res: &mut Response) {
    let directory = location_directory(req);
    let root = req
        .query::<String>("path")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&directory));
    let limit = req.query::<usize>("limit").unwrap_or(200).min(2_000);
    let entries = walk(&root, limit)
        .into_iter()
        .map(|(path, kind)| json!({ "path": path, "type": kind }))
        .collect::<Vec<_>>();
    res.render(Json(envelope(&directory, json!(entries))));
}

/// `GET /api/fs/find?location[directory]=X&query=Q[&limit=N]` — upstream
/// `fs.find`: substring match over the walked paths (the @-file picker).
#[handler] // operation_id: fs.find
pub(crate) async fn fs_find(req: &mut Request, res: &mut Response) {
    let directory = location_directory(req);
    let Some(query) = req.query::<String>("query") else {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "Missing query",
        })));
        return;
    };
    let kind = req.query::<String>("type");
    let limit = req.query::<usize>("limit").unwrap_or(20).min(500);
    let needle = query.to_lowercase();
    let entries = walk(std::path::Path::new(&directory), 5_000)
        .into_iter()
        .filter(|(path, entry_kind)| {
            path.to_lowercase().contains(&needle)
                && kind.as_deref().is_none_or(|wanted| wanted == *entry_kind)
        })
        .take(limit)
        .map(|(path, kind)| json!({ "path": path, "type": kind }))
        .collect::<Vec<_>>();
    res.render(Json(envelope(&directory, json!(entries))));
}

/// Mount every compat endpoint under the `/api` router.
pub(crate) fn mount(api: Router) -> Router {
    api.push(
        Router::new()
            .push(Router::with_path("config").get(config_get))
            .push(Router::with_path("vcs").get(vcs_get))
            .push(Router::with_path("command").get(command_list))
            .push(Router::with_path("mcp").get(mcp_list))
            .push(Router::with_path("mcp/resource").get(mcp_resource_catalog))
            .push(Router::with_path("provider").get(provider_list))
            .push(Router::with_path("reference").get(reference_list))
            .push(Router::with_path("skill").get(skill_list))
            .push(Router::with_path("shell").get(shell_list))
            .push(Router::with_path("form").get(form_list))
            .push(Router::with_path("plugin").get(plugin_list))
            .push(Router::with_path("integration").get(integration_list))
            .push(Router::with_path("fs/list").get(fs_list))
            .push(Router::with_path("fs/find").get(fs_find))
            .push(Router::with_path("experimental/migration/v1").get(migration_status)),
    )
}
