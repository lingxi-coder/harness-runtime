use hooks::definition::{HookCondition, HookExecutor, HookSource};
use hooks::events::HookEventType;
use hooks::{HookDefinition, HookRegistry};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::OrchestratorHandle;
use protocol::HookId;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

fn hk(
    name: &str,
    event: HookEventType,
    matcher: Option<&str>,
    timeout: Option<Duration>,
) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: name.into(),
        events: vec![event],
        if_condition: matcher.map(|m| HookCondition {
            pattern: m.into(),
            match_tool_name: true,
            match_input: false,
            if_pattern: None,
        }),
        executor: HookExecutor::Builtin {
            handler_id: "noop".into(),
        },
        source: HookSource::Settings(protocol::SettingsScope::User),
        blocking: true,
        timeout,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    }
}

fn build_orch() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn list_hooks_returns_empty_when_no_registry() {
    let orch = Arc::new(build_orch());
    assert!(orch.list_hooks().await.is_empty());
}

#[tokio::test]
async fn list_hooks_returns_one_pretooluse_entry() {
    let mut reg = HookRegistry::new();
    reg.register(hk(
        "./fmt.sh",
        HookEventType::PreToolUse,
        Some("Write|Edit"),
        Some(Duration::from_secs(30)),
    ));
    let reg = Arc::new(RwLock::new(reg));

    let orch = Arc::new(build_orch().with_hook_registry(reg));
    let v = orch.list_hooks().await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "./fmt.sh");
    assert_eq!(v[0].event, "PreToolUse");
    assert_eq!(v[0].matcher.as_deref(), Some("Write|Edit"));
    assert_eq!(v[0].timeout_ms, 30_000);
}

#[tokio::test]
async fn list_hooks_default_timeout_is_60000ms() {
    let mut reg = HookRegistry::new();
    reg.register(hk("./x.sh", HookEventType::Stop, None, None));
    let reg = Arc::new(RwLock::new(reg));

    let orch = Arc::new(build_orch().with_hook_registry(reg));
    let v = orch.list_hooks().await;
    assert_eq!(v[0].timeout_ms, 60_000);
    assert_eq!(v[0].event, "Stop");
    assert!(v[0].matcher.is_none());
}

#[tokio::test]
async fn list_hooks_maps_executor_type_content_and_source() {
    // (hooks-detail-fields-divergent)
    let mut reg = HookRegistry::new();
    reg.register(HookDefinition {
        id: HookId::new(),
        name: "fmt".into(),
        events: vec![HookEventType::PostToolUse],
        if_condition: None,
        executor: HookExecutor::Command {
            command: "prettier".into(),
            args: vec!["--write".into()],
            env: std::collections::HashMap::new(),
            cwd: None,
            shell: None,
        },
        source: HookSource::Settings(protocol::SettingsScope::Project),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: Some("Formatting…".into()),
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });
    let reg = Arc::new(RwLock::new(reg));

    let orch = Arc::new(build_orch().with_hook_registry(reg));
    let v = orch.list_hooks().await;
    assert_eq!(v[0].hook_type, "command");
    assert_eq!(v[0].content, "prettier --write");
    assert_eq!(v[0].source, "Project settings (.lingxi/settings.json)");
    assert_eq!(v[0].status_message.as_deref(), Some("Formatting…"));
}
