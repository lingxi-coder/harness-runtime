use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use agent::{AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy};
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

/// Listing tests mutate process-global subscription/steer signals; serialize the
/// gate-sensitive tests (every one removes/sets the var under this lock).
static AGENT_LIST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Minimal tool whose only meaningful behavior is its name — used to put an
/// `Agent`-named tool (or not) into the registry for the gate test.
struct NamedTool(&'static str);
#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(
            || serde_json::json!({ "type": "object", "properties": {} }),
        );
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other { reason: "t".into() },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[test]
fn wait_for_mcp_servers_is_visible_only_without_tool_search() {
    let wait: Arc<dyn Tool> = Arc::new(NamedTool("WaitForMcpServers"));
    let search: Arc<dyn Tool> = Arc::new(NamedTool("ToolSearch"));

    let mut direct_tools = vec![Arc::clone(&wait)];
    ConversationOrchestrator::apply_wait_for_mcp_servers_gate(&mut direct_tools);
    assert_eq!(
        direct_tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>(),
        ["WaitForMcpServers"]
    );

    let mut deferred_tools = vec![wait, search];
    ConversationOrchestrator::apply_wait_for_mcp_servers_gate(&mut deferred_tools);
    assert_eq!(
        deferred_tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>(),
        ["ToolSearch"]
    );
}

fn agent_def(agent_type: &str, when_to_use: &str, tools: AgentToolPolicy) -> AgentDefinition {
    AgentDefinition {
        omit_instructions: false,
        cache_ttl: None,
        agent_type: agent_type.into(),
        when_to_use: when_to_use.into(),
        tools,
        max_turns: 1,
        model: AgentModel::Inherit,
        permission_mode: AgentPermissionMode::Bubble,
        source: AgentSource::BuiltIn,
        base_dir: "/tmp".into(),
        system_prompt: None,
        mcp_servers: vec![],
        frontmatter_hooks: vec![],
        icon: None,
        allowed_tools: vec![],
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
        offer_provider: None,
    }
}

/// Build an orchestrator with the given tools + optional agent catalog.
fn orch_with(
    tools: ToolRegistry,
    catalog: Option<Arc<tokio::sync::RwLock<Vec<AgentDefinition>>>>,
) -> ConversationOrchestrator {
    let api = Arc::new(MockApiClient::new(vec![]));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    if let Some(c) = catalog {
        orch = orch.with_agent_catalog(c);
    }
    orch
}

fn reg_with_agent_tool() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register_builtin(Arc::new(NamedTool("Agent")));
    reg
}

#[tokio::test]
async fn removed_inline_catalog_override_does_not_suppress_reminder() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The obsolete inline-catalog override cannot suppress the current reminder.

    std::env::set_var("LINGXI_AGENT_LIST_IN_MESSAGES", "false");
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![agent_def(
        "general-purpose",
        "anything",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    )]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog));
    let got = orch.agent_listing_reminder_message().await;
    std::env::remove_var("LINGXI_AGENT_LIST_IN_MESSAGES");
    assert!(
        got.is_some(),
        "the current catalog reminder is always enabled when Agent is available"
    );
}

#[tokio::test]
async fn gate_on_no_catalog_still_announces_builtins() {
    // Binary `aLe` builds the delta from `activeAgents` (built-ins +
    // user/project), gating ONLY on the Agent tool's presence — NOT on a
    // wired DISK catalog. So a session with the gate ON, the Agent tool
    // present, and NO disk catalog still announces the BUILT-IN agents
    // (e.g. general-purpose). (Previously this early-returned `None`,
    // suppressing built-ins under the gate — a divergence from `aLe`.)
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let orch = orch_with(reg_with_agent_tool(), None);
    let got = orch.agent_listing_reminder_message().await;
    let text = got
        .expect("built-ins must be announced even with no disk catalog")
        .text_content();
    assert!(text.starts_with("<system-reminder>"), "got: {text}");
    assert!(
        text.contains("Available agent types for the Agent tool:"),
        "turn-0 initial header expected; got: {text}"
    );
    assert!(
        text.contains("- general-purpose:"),
        "built-ins must be listed with no disk catalog; got: {text}"
    );
}

#[tokio::test]
async fn gate_on_but_agent_tool_absent_is_none() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![agent_def(
        "general-purpose",
        "anything",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    )]));
    // Empty registry — the Agent tool is not present this turn.
    let orch = orch_with(ToolRegistry::new(), Some(catalog));
    let got = orch.agent_listing_reminder_message().await;
    assert!(got.is_none(), "Agent tool absent ⇒ no reminder");
}

#[tokio::test]
async fn gate_on_turn0_full_listing_with_initial_header() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Catalog supplies a custom type; built-ins are merged in too.
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![agent_def(
        "custom-agent",
        "a project agent",
        AgentToolPolicy::Explicit(vec!["Read".into()]),
    )]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog));

    let msg = orch.agent_listing_reminder_message().await;
    let text = msg.expect("turn-0 reminder present").text_content();

    assert!(text.starts_with("<system-reminder>"), "got: {text}");
    assert!(text.ends_with("</system-reminder>"), "got: {text}");
    assert!(
        text.contains("Available agent types for the Agent tool:"),
        "turn-0 must use the is_initial header; got: {text}"
    );
    // formatAgentLine for the catalog entry.
    assert!(
        text.contains("- custom-agent: a project agent (Tools: Read)"),
        "missing catalog line; got: {text}"
    );
    // Built-ins are merged in (e.g. general-purpose).
    assert!(
        text.contains("- general-purpose:"),
        "built-ins must be merged into the listing; got: {text}"
    );
}

#[tokio::test]
async fn gate_on_later_turn_no_new_types_is_none() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![agent_def(
        "custom-agent",
        "a project agent",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    )]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog));

    // Turn 0 emits the full listing.
    let t0 = orch.agent_listing_reminder_message().await;
    assert!(t0.is_some(), "turn-0 must emit");
    // Turn 1 with the same catalog ⇒ nothing new ⇒ None.
    let t1 = orch.agent_listing_reminder_message().await;
    assert!(t1.is_none(), "no new types ⇒ no reminder");
}

#[tokio::test]
async fn gate_on_newly_added_type_emits_delta_with_new_header_only() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![agent_def(
        "alpha-agent",
        "the alpha agent",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    )]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog.clone()));

    // Turn 0: full listing (contains alpha-agent + built-ins).
    let t0 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-0")
        .text_content();
    assert!(t0.contains("- alpha-agent:"));
    assert!(!t0.contains("- gamma-agent:"));

    // A brand-new agent type appears in the catalog.
    catalog.write().await.push(agent_def(
        "gamma-agent",
        "the gamma agent",
        AgentToolPolicy::Explicit(vec!["Read".into(), "Edit".into()]),
    ));

    // Turn 1: ONLY the new type, with the "New agent types…" header.
    let t1 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-1 delta")
        .text_content();

    assert!(
        t1.contains("New agent types are now available for the Agent tool:"),
        "delta must use the non-initial header; got: {t1}"
    );
    assert!(
        !t1.contains("Available agent types for the Agent tool:"),
        "delta must NOT use the is_initial header; got: {t1}"
    );
    assert!(
        t1.contains("- gamma-agent: the gamma agent (Tools: Read, Edit)"),
        "delta must contain the new agent line; got: {t1}"
    );
    assert!(
        !t1.contains("- alpha-agent:"),
        "delta must NOT re-emit an already-announced type; got: {t1}"
    );
}

/// AGT-15 — the INITIAL listing carries the concurrency note when the plan
/// is not Pro and the subagent steer is `default`
/// (`showConcurrencyNote:Cc()!=="pro"&&DZ()==="default"`, oracle
/// @296530704), and a later DELTA never does (`e.isInitial` guard,
/// @296704484). Sections are joined by a BLANK line.
#[tokio::test]
async fn gate_on_initial_listing_carries_the_concurrency_note() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![agent_def(
        "alpha-agent",
        "the alpha agent",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    )]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog.clone()));

    let t0 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-0")
        .text_content();
    const NOTE: &str = "When you launch multiple agents for independent work, send them in a single message with multiple tool uses so they run concurrently.";
    assert!(
        t0.contains(NOTE),
        "initial listing must carry it; got: {t0}"
    );
    // Its own section ⇒ a blank line separates it from the listing lines.
    assert!(
        t0.contains(&format!("\n\n{NOTE}\n</system-reminder>")),
        "got: {t0}"
    );

    catalog.write().await.push(agent_def(
        "gamma-agent",
        "the gamma agent",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    ));
    let t1 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-1 delta")
        .text_content();
    assert!(
        !t1.contains(NOTE),
        "the note is gated on isInitial; got: {t1}"
    );
}

/// AGT-15 — an agent type that DISAPPEARS from the catalog emits the
/// `removedTypes` section plus the shared ambient-context trailer, and is
/// dropped from the announced set so it is re-announced if it returns
/// (oracle `s.delete(p)` replay @296530704).
#[tokio::test]
async fn gate_on_removed_type_emits_the_removal_branch_and_the_ambient_trailer() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![
        agent_def(
            "zeta-agent",
            "the zeta agent",
            AgentToolPolicy::All {
                use_exact_tools: false,
            },
        ),
        agent_def(
            "beta-agent",
            "the beta agent",
            AgentToolPolicy::All {
                use_exact_tools: false,
            },
        ),
    ]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog.clone()));

    let t0 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-0")
        .text_content();
    assert!(t0.contains("- zeta-agent:") && t0.contains("- beta-agent:"));

    // Both project agents vanish (a plugin/MCP catalog reload).
    catalog.write().await.clear();
    let t1 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-1 removal")
        .text_content();
    assert!(
        t1.contains(
            "The following agent types are no longer available:\n- beta-agent\n- zeta-agent"
        ),
        "removed list must be plain-sorted; got: {t1}"
    );
    assert!(
        t1.contains(crate::prompt::memory_update::AMBIENT_CONTEXT_TRAILER),
        "the removal branch pushes the ambient trailer; got: {t1}"
    );
    // No ADDED section this turn ⇒ neither header appears.
    assert!(!t1.contains("agent types for the Agent tool:"), "got: {t1}");

    // The type comes back ⇒ it is re-announced (the announced set dropped it).
    catalog.write().await.push(agent_def(
        "beta-agent",
        "the beta agent",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    ));
    let t2 = orch
        .agent_listing_reminder_message()
        .await
        .expect("turn-2 re-announce")
        .text_content();
    assert!(
        t2.contains("New agent types are now available for the Agent tool:")
            && t2.contains("- beta-agent:"),
        "a returning type must be re-announced; got: {t2}"
    );
}

#[tokio::test]
async fn agent_offer_withheld_agent_is_absent_from_model_reminder() {
    let _g = AGENT_LIST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("agent-offer.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('agent.offer', { agent: 'hidden-agent' }, () => ({ isOffered: false }));
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("agent-offer", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host);

    let mut hidden = agent_def(
        "hidden-agent",
        "not offered to the model",
        AgentToolPolicy::All {
            use_exact_tools: false,
        },
    );
    hidden.source = AgentSource::Settings(lingxi_core::types::SettingsScope::User);
    let catalog = Arc::new(tokio::sync::RwLock::new(vec![
        hidden,
        agent_def(
            "visible-agent",
            "available to the model",
            AgentToolPolicy::All {
                use_exact_tools: false,
            },
        ),
    ]));
    let orch = orch_with(reg_with_agent_tool(), Some(catalog))
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)));

    let reminder = orch
        .agent_listing_reminder_message()
        .await
        .expect("the offered listing should be sent on the initial turn")
        .text_content();

    assert!(reminder.contains("- visible-agent: available to the model"));
    assert!(reminder.contains("- general-purpose:"));
    assert!(
        !reminder.contains("hidden-agent"),
        "a Mod-withheld candidate must be absent from the model reminder: {reminder}"
    );
}
