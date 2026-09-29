use agent::definition::{
    AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy,
};
use lingxi_core::host::OrchestratorHandle;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tokio::sync::RwLock;

fn mk(name: &str, desc: &str, tools: Vec<String>) -> AgentDefinition {
    AgentDefinition {
        cache_ttl: None,
        agent_type: name.into(),
        when_to_use: desc.into(),
        tools: AgentToolPolicy::Explicit(tools.clone()),
        max_turns: 100,
        model: AgentModel::Inherit,
        permission_mode: AgentPermissionMode::Bubble,
        source: AgentSource::Settings(lingxi_core::types::SettingsScope::User),
        base_dir: std::path::PathBuf::from("/tmp"),
        system_prompt: None,
        mcp_servers: vec![],
        frontmatter_hooks: vec![],
        icon: None,
        allowed_tools: tools,
        worktree_requirement: None,
        disallowed_tools: vec![],
        skills: vec![],
        required_mcp_servers: vec![],
        background: false,
        isolation: None,
        memory: None,
        effort: None,
        initial_prompt: None,
        color: None,
        observer: None,
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
async fn list_agents_returns_empty_when_no_catalog() {
    let orch = Arc::new(build_orch());
    assert!(orch.list_agents().await.is_empty());
}

#[tokio::test]
async fn list_agents_returns_one_entry() {
    let cat = Arc::new(RwLock::new(vec![mk(
        "reviewer",
        "Reviews code",
        vec!["Read".into(), "Grep".into()],
    )]));
    let orch = Arc::new(build_orch().with_agent_catalog(cat));
    let v = orch.list_agents().await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].name, "reviewer");
    assert_eq!(v[0].description, "Reviews code");
    assert_eq!(
        v[0].tools_allowed,
        vec!["Read".to_string(), "Grep".to_string()]
    );
}

#[tokio::test]
async fn list_agents_sorts_by_name() {
    let cat = Arc::new(RwLock::new(vec![
        mk("zeta", "z", vec![]),
        mk("alpha", "a", vec![]),
    ]));
    let orch = Arc::new(build_orch().with_agent_catalog(cat));
    let v = orch.list_agents().await;
    assert_eq!(v[0].name, "alpha");
    assert_eq!(v[1].name, "zeta");
}

#[tokio::test]
async fn list_agents_maps_source_to_group_label() {
    // (agents-08) AgentSource → AGENT_SOURCE_GROUPS display label.
    let mut user = mk("u", "x", vec![]);
    user.source = AgentSource::Settings(lingxi_core::types::SettingsScope::User);
    let mut builtin = mk("b", "x", vec![]);
    builtin.source = AgentSource::BuiltIn;
    let mut project = mk("p", "x", vec![]);
    project.source = AgentSource::Settings(lingxi_core::types::SettingsScope::Project);
    let cat = Arc::new(RwLock::new(vec![user, builtin, project]));
    let orch = Arc::new(build_orch().with_agent_catalog(cat));
    let v = orch.list_agents().await;
    let group = |name: &str| {
        v.iter()
            .find(|a| a.name == name)
            .map(|a| a.source_group.clone())
            .unwrap()
    };
    assert_eq!(group("u"), "User agents");
    assert_eq!(group("b"), "Built-in agents");
    assert_eq!(group("p"), "Project agents");
}
