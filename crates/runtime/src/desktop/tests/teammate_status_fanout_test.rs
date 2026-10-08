use super::TeammateStatusFanout;
use lingxi_core::host::task_registry::TaskRegistryHandle;
use lingxi_core::types::AgentId;
use std::sync::Arc;
use tasks::handlers::TaskStatusSink;

#[tokio::test]
async fn teammate_status_fanout_exposes_live_registry_to_agent_source_updates() {
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(platform_posix::PosixFileSystem::new(
        dir.path().to_path_buf(),
    ));
    let registry = Arc::new(tasks::registry::TaskRegistry::new(
        Arc::new(platform_posix::PosixRuntime::new()),
        fs.clone(),
        Arc::new(tasks::output_manager::TaskOutputManager::new(
            dir.path().join("output"),
            fs,
        )),
    ));
    let registry_sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    registry_sink.bind(registry.clone());
    let fanout = TeammateStatusFanout {
        task_registry: registry_sink,
        coordinator: Arc::new(coordinator::CoordinatorStatusSink::new(
            Arc::new(coordinator::TeamRegistry::new(AgentId::new())),
            Arc::new(orchestrator::test_support::MockOutputStream::new()),
        )),
    };
    let expected: Arc<dyn TaskRegistryHandle> = registry;
    let exposed = TaskStatusSink::task_registry(&fanout)
        .expect("the production decorator must expose the actual registry");
    assert!(Arc::ptr_eq(&expected, &exposed));
    assert!(
        TaskStatusSink::set_team_member_active(&fanout, AgentId::new(), true)
            .await
            .is_err(),
        "unknown identity must reach the registry instead of the default no-op"
    );
    assert!(
        TaskStatusSink::bind_agent_id(&fanout, "unknown-task", AgentId::new())
            .await
            .is_err(),
        "identity binding must reach the registry instead of the default no-op"
    );
}
