use async_trait::async_trait;
use lingxi_core::host::{
    RegisterRepoRootRequest, RepoRootReloadOutcome, RepoRootReloadRequest, RepoRootReloader,
};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tool_api::{SessionCwd, ToolRegistry};

struct RecordingReloader {
    cwd: Arc<SessionCwd>,
    requests: tokio::sync::Mutex<Vec<RepoRootReloadRequest>>,
}

#[async_trait]
impl RepoRootReloader for RecordingReloader {
    async fn reload(&self, request: RepoRootReloadRequest) -> RepoRootReloadOutcome {
        assert!(
            self.cwd.trusted_dirs().contains(&request.root),
            "sandbox/trusted roots must be refreshed before catalog reload"
        );
        self.requests.lock().await.push(request);
        RepoRootReloadOutcome {
            skills_reloaded: true,
            plugins_reloaded: true,
            errors: Vec::new(),
        }
    }
}

fn build_orchestrator(
    cwd: std::path::PathBuf,
    session_cwd: Arc<SessionCwd>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
    )
    .with_session_cwd(session_cwd)
}

#[tokio::test]
async fn register_root_refreshes_security_boundary_before_catalogs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("repo");
    let root = temp.path().join("extra");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&root).expect("extra root");
    let cwd = std::fs::canonicalize(cwd).expect("canonical cwd");
    let root = std::fs::canonicalize(root).expect("canonical extra root");
    let session_cwd = SessionCwd::new(cwd.clone(), vec![cwd.clone()]);
    let reloader = Arc::new(RecordingReloader {
        cwd: session_cwd.clone(),
        requests: tokio::sync::Mutex::new(Vec::new()),
    });
    let orch = build_orchestrator(cwd, session_cwd).with_repo_root_reloader(reloader.clone());

    let outcome = orch
        .register_repo_root(RegisterRepoRootRequest {
            path: root.to_string_lossy().into_owned(),
            reload_claude_md: false,
            reload_skills: true,
            reload_plugins: true,
        })
        .await
        .expect("register root");

    assert_eq!(outcome.directory, root);
    assert!(outcome.added);
    assert!(outcome.skills_reloaded);
    assert!(outcome.plugins_reloaded);
    assert!(outcome.reload_errors.is_empty());
    let requests = reloader.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].root, outcome.directory);
    assert!(requests[0].reload_skills);
    assert!(requests[0].reload_plugins);
}

#[tokio::test]
async fn unavailable_catalog_reloader_is_reported_not_silently_ignored() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("repo");
    let root = temp.path().join("extra");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&root).expect("extra root");
    let cwd = std::fs::canonicalize(cwd).expect("canonical cwd");
    let root = std::fs::canonicalize(root).expect("canonical extra root");
    let session_cwd = SessionCwd::new(cwd.clone(), vec![cwd.clone()]);
    let orch = build_orchestrator(cwd, session_cwd);

    let outcome = orch
        .register_repo_root(RegisterRepoRootRequest {
            path: root.to_string_lossy().into_owned(),
            reload_claude_md: false,
            reload_skills: true,
            reload_plugins: false,
        })
        .await
        .expect("root registration remains committed");

    assert!(outcome.added);
    assert!(!outcome.skills_reloaded);
    assert_eq!(outcome.reload_errors.len(), 1);
    assert!(outcome.reload_errors[0].contains("unavailable"));
}
