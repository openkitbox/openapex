//! The tool permission gate adapter: one authorization call per tool execution.
//!
//! Pending requests, replies and saved approvals are owned by
//! [`crate::permission::PermissionService`], which is shared with the HTTP
//! layer. This adapter only carries the per-turn facts (config generation,
//! agent rules and the unattended marker) into that service.

use std::path::Path;
use std::sync::Arc;

use ox_core::config::Config;
use ox_schema::permission::Rule;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::permission::PermissionService;
use crate::permission::gate::AuthorizeParams;
pub use crate::permission::{Decision, SourceRef};

/// Everything one authorization needs, cloned out of the drain context so the
/// check runs inside the spawned tool task.
#[derive(Clone)]
pub(crate) struct Gate {
    pub service: Arc<PermissionService>,
    pub config: Arc<Config>,
    pub agent: &'static ox_tools::agent::Agent,
    /// `Some` only for an automation turn: no pending request is created.
    pub unattended: Option<Vec<Rule>>,
    pub source: Option<SourceRef>,
}

impl Gate {
    pub(crate) async fn authorize(
        &self,
        session_id: &str,
        directory: &Path,
        tool: &str,
        input: &Value,
        is_mcp: bool,
        cancellation: &CancellationToken,
    ) -> Decision {
        self.service
            .authorize(AuthorizeParams {
                config: &self.config,
                agent: self.agent,
                session_id,
                directory,
                tool,
                input,
                source: self.source.clone(),
                unattended: self.unattended.as_deref(),
                is_mcp,
                cancellation,
            })
            .await
    }
}
