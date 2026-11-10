use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn test_orchestrator(cwd: std::path::PathBuf) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        cwd,
    )
}

#[tokio::test]
async fn seed_read_state_normalizes_bom_and_crlf_to_lf() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seeded.txt");
    std::fs::write(&path, b"\xEF\xBB\xBFalpha\r\nbeta\rgamma\n").expect("write fixture");
    let orch = test_orchestrator(dir.path().to_path_buf());
    let host_mtime_ms = std::fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post epoch")
        .as_millis() as f64
        + 1.0;

    assert!(
        orch.seed_read_state_from_host("seeded.txt", host_mtime_ms)
            .await
    );

    let entry = tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &path)
        .expect("seeded entry present");
    assert_eq!(entry.content, "alpha\nbeta\ngamma\n");
    assert!(!entry.from_read);
    assert!(
        orch.prompt_runtime
            .read_state_map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .model_context_keys()
            .is_empty(),
        "host-seeded content must not appear as model-visible context"
    );
}

#[tokio::test]
async fn rejected_host_seeds_leave_the_existing_read_state_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("seeded.txt");
    std::fs::write(&path, "first").unwrap();
    let orch = test_orchestrator(dir.path().to_path_buf());
    assert!(orch.seed_read_state_from_host("seeded.txt", f64::MAX).await);
    let original = tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &path)
        .expect("initial seed");

    std::fs::write(&path, b"\xff").unwrap();
    assert!(!orch.seed_read_state_from_host("seeded.txt", f64::MAX).await);
    std::fs::write(&path, "new").unwrap();
    assert!(!orch.seed_read_state_from_host("seeded.txt", 0.0).await);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();
    assert!(!orch.seed_read_state_from_host("seeded.txt", f64::MAX).await);

    let retained = tool_api::read_file_state::get(&orch.prompt_runtime.read_state_map, &path)
        .expect("failed seed retains previous entry");
    assert_eq!(retained.content, original.content);
    assert_eq!(retained.mtime_ms, original.mtime_ms);
}
