//! `RTK_DISABLED=1` is the process-wide escape hatch for the RTK rewrite.
//!
//! This lives in its own integration-test binary on purpose: the variable is
//! process-global, so it is set once at startup in a process that runs no
//! other test (and therefore no other environment reader).

use std::path::PathBuf;
use std::sync::Arc;

use ox::runner::{Tool, ToolContext, ToolRegistry};
use ox::tool::rtk::RtkRewriter;
use ox::tool::shell::ShellTool;
use ox::tool::shell::sessions::ShellSessions;
use serde_json::json;

#[tokio::test]
async fn rtk_disabled_env_skips_the_hook_entirely() {
    // Set before any tool/session exists; this process runs only this test.
    unsafe { std::env::set_var("RTK_DISABLED", "1") };

    let dir = tempfile::tempdir().unwrap();
    let binary: PathBuf = dir.path().join("rtk-mock.sh");
    let log: PathBuf = dir.path().join("rtk-mock.log");
    std::fs::write(
        &binary,
        format!("#!/bin/sh\nprintf '%s\\n' \"$3\" >> \"{}\"\nprintf '{{\"command\":\"echo rewritten-output\"}}\\n'\n", log.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let tool = ShellTool::with_sessions(Arc::new(ShellSessions::default()))
        .with_rewriter(RtkRewriter::with_binary(binary));
    let ctx = ToolContext {
        session_id: "ses_rtk_disabled".to_owned(),
        directory: dir.path().to_path_buf(),
        call_id: "call_rtk_disabled".to_owned(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        registry: Arc::new(ToolRegistry::new()),
    };
    let output = tool
        .execute(ctx, json!({"command": "git status", "timeout": 20_000}))
        .await;
    assert!(output.error.is_none(), "{output:?}");
    assert!(
        !output
            .content
            .as_str()
            .unwrap_or_default()
            .contains("rewritten-output"),
        "RTK_DISABLED=1 must run the original command: {}",
        output.content
    );
    assert!(
        !log.exists(),
        "the hook must not even be invoked when disabled"
    );
}
