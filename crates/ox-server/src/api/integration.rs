//! `/connect`'s server side, minimal subset: builtin providers and API keys.
//!
//! The upstream dialog reads `GET /api/integration`
//! (`packages/tui/src/component/dialog-integration.tsx:110-117` via
//! `packages/client/src/solid/data.ts:2117`) and submits a key to
//! `POST /api/integration/{integrationID}/connect/key`, which answers **204**
//! (`packages/client/src/promise/generated/client.ts:1816-1832`). Field shapes
//! follow `packages/client/src/promise/generated/types.ts` (`IntegrationInfo`
//! :2738, `IntegrationKeyMethod` :2669, `ConnectionCredentialInfo` :751).
//!
//! A connect inserts the key into the `credential` table through
//! [`ox_core::credential::set_key`] (one active key per integration, replacing
//! the previous one inside a write transaction) and then reuses
//! [`crate::reload::run`]. The reload re-reads `ox.toml`, applies the stored
//! credentials and rebuilds the router and model catalogue, so the new
//! provider is usable **without a restart** — required, because the TUI's
//! `connected()` (dialog-integration.tsx:1249-1271) re-syncs the integration,
//! model and provider lists as soon as the 204 lands and then opens the model
//! picker. Keys never touch `ox.toml`; the table mirrors upstream's own
//! `credential` store, and `Db::open` keeps the database file owner-only.
//!
//! Out of scope, deliberately: OAuth, command and external methods, the
//! credential activate/update/remove endpoints and `GET /api/integration/{id}`.
//! Known deviation: the TUI still offers rename / switch / remove on a
//! connected row, and those calls get a 404.

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use ox_core::credential;
use ox_llm::config::{self, BuiltinProvider};

/// How the running server authenticates to a builtin. Derived from the running
/// config on every request; no second "connected" flag is ever persisted, so
/// the dialog cannot disagree with the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connection {
    /// The builtin is not materialized: no credential, no provider, no models.
    None,
    /// Materialized from the credential file (what `/connect` writes).
    Credential,
    /// Materialized from the provider's environment variable.
    Environment,
}

/// `materialized` is whether the running config carries the builtin;
/// `stored` is whether the credential file holds its key.
fn connection_of(materialized: bool, stored: bool) -> Connection {
    if !materialized {
        return Connection::None;
    }
    if stored {
        Connection::Credential
    } else {
        Connection::Environment
    }
}

/// One `IntegrationInfo`. Only the `key` method exists, so the dialog skips
/// its method picker and opens the key prompt directly.
fn info(builtin: &BuiltinProvider, connection: Connection) -> Value {
    let label = format!("{} API key", builtin.label);
    let connections = match connection {
        Connection::None => json!([]),
        Connection::Credential => json!([{
            "type": "credential",
            "id": builtin.id,
            "label": label,
            "method": "key",
        }]),
        Connection::Environment => json!([{ "type": "env", "name": builtin.env_key }]),
    };
    json!({
        "id": builtin.id,
        "name": builtin.label,
        "methods": [{ "type": "key", "label": label }],
        "connections": connections,
    })
}

/// `GET /api/integration` — every builtin, connected or not: the dialog's
/// purpose is to offer the ones that are not connected yet.
#[handler] // operation_id: integration.list
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let directory = super::compat::directory_of(req);
    let services = state.services();
    let credentials = match credential::load(&state.db).await {
        Ok(credentials) => credentials,
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "reading stored credentials failed");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({
                "_tag": "StorageError",
                "message": format!("could not read stored credentials: {error:#}"),
            })));
            return;
        }
    };
    let data: Vec<Value> = config::BUILTINS
        .iter()
        .map(|builtin| {
            let materialized = services.config.llm.builtins.contains_key(builtin.id);
            info(
                builtin,
                connection_of(materialized, credentials.key(builtin.id).is_some()),
            )
        })
        .collect();
    res.render(Json(super::compat::envelope(&directory, json!(data))));
}

#[derive(Deserialize)]
struct ConnectKeyBody {
    key: String,
}

/// `POST /api/integration/{integrationID}/connect/key` — store the key and
/// activate the builtin. Unknown providers and empty keys are refused before
/// anything is written; the key itself is never logged.
#[handler] // operation_id: integration.connect.key
pub(crate) async fn connect_key(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(state) = super::state(depot, res) else {
        return;
    };
    let directory = super::compat::directory_of(req);
    let id = req.param::<String>("integrationID").unwrap_or_default();
    let Some(builtin) = config::builtin(&id) else {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Json(json!({
            "_tag": "NotFoundError",
            "message": format!("unknown integration: {id}"),
        })));
        return;
    };
    let Ok(body) = req.parse_json::<ConnectKeyBody>().await else {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "expected a JSON body with a \"key\" string",
            "field": "key",
        })));
        return;
    };
    if body.key.trim().is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(json!({
            "_tag": "InvalidRequestError",
            "message": "API key must not be empty",
            "field": "key",
        })));
        return;
    }

    let id = ox_schema::id::id("cred");
    if let Err(error) = credential::set_key(&state.db, &id, builtin.id, &body.key).await {
        tracing::error!(error = %format!("{error:#}"), "saving the provider key failed");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(json!({
            "_tag": "StorageError",
            "message": format!("could not store the key: {error:#}"),
        })));
        return;
    }

    match crate::reload::run(state, &directory).await {
        Ok(()) => {
            res.status_code(StatusCode::NO_CONTENT);
        }
        Err(error) => {
            // The key is already on disk, so a retry is a no-op write; the
            // failure only means this process is still on the old services.
            tracing::warn!(error = %format!("{error:#}"), "provider key saved but activation failed");
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Json(json!({
                "_tag": "ServiceUnavailableError",
                "message": format!(
                    "key stored, but activating {} failed: {error:#}; restart ox to use it",
                    builtin.id,
                ),
                "service": "provider",
            })));
        }
    }
}
