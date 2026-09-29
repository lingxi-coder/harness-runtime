//! (settings-status-missing-mcp-and-setting-sources) `get_status_snapshot`'s
//! `setting_sources` reflects real on-disk settings files for the cwd-rooted
//! project tier. The user tier depends on `$HOME`/`$LINGXI_CONFIG_DIR` (a
//! process-global env var), so it is intentionally NOT exercised here to
//! avoid the env-mutation test races this codebase has hit before — the
//! project tier alone is enough to pin the file-existence mapping.

use lingxi_core::host::OrchestratorHandle;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;

fn build_orch(cwd: std::path::PathBuf) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
    )
}

#[tokio::test]
async fn setting_sources_includes_project_tier_when_its_file_exists() {
    let tmp =
        std::env::temp_dir().join(format!("lx-status-setting-sources-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join(".lingxi")).unwrap();
    std::fs::write(tmp.join(".lingxi").join("settings.json"), b"{}").unwrap();

    let orch = build_orch(tmp.clone());
    let snap = orch.get_status_snapshot().await;
    assert!(
        snap.setting_sources
            .contains(&"Project settings (.lingxi/settings.json)".to_string()),
        "got: {:?}",
        snap.setting_sources
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}

#[tokio::test]
async fn setting_sources_excludes_project_tier_when_no_file_exists() {
    let tmp = std::env::temp_dir().join(format!(
        "lx-status-setting-sources-empty-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let orch = build_orch(tmp.clone());
    let snap = orch.get_status_snapshot().await;
    assert!(
        !snap
            .setting_sources
            .iter()
            .any(|s| s.starts_with("Project settings")),
        "got: {:?}",
        snap.setting_sources
    );

    std::fs::remove_dir_all(&tmp).unwrap();
}
