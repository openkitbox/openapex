//! Catalog endpoints the clients probe at startup that ox answers with
//! empty or trivially-derived data. Shapes follow upstream's protocol
//! (`packages/protocol/openapi.json`); semantics are the ox MVP subset —
//! no plugins, commands, skills, forms, or integrations exist; MCP is real
//! (src/mcp), and config is not yet modeled. Unlike upstream, these endpoints
//! answer 200
//! with empty data for unknown locations instead of 404
//! `LocationNotFoundError` — an ox server owns its directory model, so
//! keep that divergence here rather than "fixing" endpoints piecemeal.

use salvo::prelude::*;
use serde_json::json;

/// The request's project directory (upstream `location.ts:73-75`):
/// `location[directory]` query, then the percent-encoded
/// `x-opencode-directory` header; `None` lets the caller fall back to the
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

/// The request's project directory, canonicalized when it exists on disk,
/// falling back to the server's directory. Every catalog envelope echoes
/// this one resolved value so responses stay consistent.
pub(crate) fn directory_of(req: &Request) -> String {
    request_directory(req)
        .map(|raw| {
            let path = std::path::PathBuf::from(&raw);
            path.canonicalize().unwrap_or(path).display().to_string()
        })
        .unwrap_or_else(fallback_directory)
}

fn fallback_directory() -> String {
    std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string()
}

/// `decodeURIComponent` semantics with upstream's raw fallback: `%XX` byte
/// sequences decoded into a UTF-8 string; malformed input comes back as-is.
pub(crate) fn percent_decode(input: &str) -> String {
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
pub(crate) fn envelope(directory: &str, data: serde_json::Value) -> serde_json::Value {
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

/// Empty catalog lists: command / reference / plugin / integration.
/// (`provider` has a real handler in the parent module — the configured
/// model catalog.)
macro_rules! empty_catalog {
    ($name:ident, $operation:literal) => {
        #[handler] // operation_id: $operation
        pub(crate) async fn $name(req: &mut Request, res: &mut Response) {
            let directory = directory_of(req);
            res.render(Json(envelope(&directory, json!([]))));
        }
    };
}

empty_catalog!(command_list, "command.list");
empty_catalog!(reference_list, "reference.list");
empty_catalog!(plugin_list, "plugin.list");
empty_catalog!(integration_list, "integration.list");

/// `GET /api/mcp` — upstream `mcp.list`: every configured server with its
/// live status. The shape is `Mcp.Server[]`; `status` is a tagged object
/// (`{"status":"connected"}` / `{"status":"failed","error":"..."}`).
#[handler] // operation_id: mcp.list
pub(crate) async fn mcp_list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let directory = directory_of(req);
    let data = serde_json::to_value(state.mcp.servers()).unwrap_or_else(|_| json!([]));
    res.render(Json(envelope(&directory, data)));
}

/// `GET /api/mcp/resource` — upstream `mcp.resource.catalog`: the resources
/// and templates discovered from connected servers. Servers without the
/// `resources` capability contribute nothing.
#[handler] // operation_id: mcp.resource.catalog
pub(crate) async fn mcp_resource_catalog(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let directory = directory_of(req);
    let (resources, templates) = state.mcp.resources();
    res.render(Json(envelope(
        &directory,
        json!({ "resources": resources, "templates": templates }),
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
    ".venv",
    "__pycache__",
    ".svn",
];

/// Collect relative file/dir paths under `root`, sorted for deterministic
/// output. Bounded walk: stops at `limit` entries, so very large trees may
/// be truncated silently (the picker is a suggestion surface, not a listing
/// contract).
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
                found.sort();
                return found;
            }
        }
    }
    found.sort();
    found
}

/// `GET /api/fs/list?location[directory]=X[&path=Y]` — upstream `fs.list`:
/// relative entries under the directory. `path` is absolute or relative to
/// the requested location (`Path::join` replaces the base for absolute
/// paths), never relative to the server process.
#[handler] // operation_id: fs.list
pub(crate) async fn fs_list(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let root =
        std::path::Path::new(&directory).join(req.query::<String>("path").unwrap_or_default());
    let limit = req.query::<usize>("limit").unwrap_or(200).min(2_000);
    // The walk is plain filesystem I/O; keep it off the async workers.
    let entries = tokio::task::spawn_blocking(move || walk(&root, limit))
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(path, kind)| json!({ "path": path, "type": kind }))
        .collect::<Vec<_>>();
    res.render(Json(envelope(&directory, json!(entries))));
}

/// `GET /api/fs/find?location[directory]=X&query=Q[&limit=N]` — upstream
/// `fs.find`: case-insensitive substring match over the walked paths (the
/// @-file picker).
#[handler] // operation_id: fs.find
pub(crate) async fn fs_find(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
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
    let root = std::path::PathBuf::from(&directory);
    let entries = tokio::task::spawn_blocking(move || walk(&root, 5_000))
        .await
        .unwrap_or_default()
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
            .push(Router::with_path("command").get(command_list))
            .push(Router::with_path("mcp").get(mcp_list))
            .push(Router::with_path("mcp/resource").get(mcp_resource_catalog))
            .push(Router::with_path("provider").get(super::provider_list))
            .push(Router::with_path("reference").get(reference_list))
            .push(Router::with_path("plugin").get(plugin_list))
            .push(Router::with_path("integration").get(integration_list))
            .push(Router::with_path("fs/list").get(fs_list))
            .push(Router::with_path("fs/find").get(fs_find))
            .push(Router::with_path("experimental/migration/v1").get(migration_status)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_table() {
        let cases = [
            ("%2Ftmp%2Fx", "/tmp/x"),
            ("%2f", "/"),        // lowercase hex
            ("%25", "%"),        // escaped percent
            ("%E4%B8%AD", "中"), // multibyte UTF-8
            ("plain", "plain"),  // nothing to decode
            ("%zz", "%zz"),      // invalid hex → raw
            ("%2", "%2"),        // truncated → raw
            ("a%2Fb%2Fc", "a/b/c"),
        ];
        for (input, expected) in cases {
            assert_eq!(percent_decode(input), expected, "input: {input}");
        }
    }

    #[test]
    fn walk_skips_noise_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        let found = walk(dir.path(), 100);
        let paths = found.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec!["a.txt", "b.txt", "src", "src/lib.rs"],
            "skip target/, sorted"
        );
    }

    #[test]
    fn walk_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        assert_eq!(walk(dir.path(), 3).len(), 3);
    }
}
