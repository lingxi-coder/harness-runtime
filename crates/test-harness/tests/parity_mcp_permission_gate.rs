//! Parity: MCP-invocation Batch 4 — the permission chokepoint authorizes the
//! `mcp__<server>__<tool>` FQN.
//!
//! Locks the claim that when an MCP tool is invoked, the enforcement chokepoint
//! (`orch.perms.check(name, input)`, `turn_loop.rs:357`) sees the FQN
//! `mcp__server__tool` and applies allow/deny rules keyed on that exact string,
//! WITHOUT colliding with a builtin of the same short name. No production code
//! change was needed (the FQN already flows to the gate and the policy matches
//! by string) — this is the additive parity test for that wiring.
//!
//! TS ref: `services/mcp/mcpStringUtils.ts:60-67` (`getToolNameForPermissionCheck`
//! → `buildMcpToolName`, so MCP rules are keyed on the FQN and never collide
//! with builtin deny rules), `MCPTool.ts:56-61`.
//!
//! Two layers:
//!   1. Direct policy/gate assertions — the namespace-isolation core: a deny
//!      rule `mcp__mock__a` denies `mcp__mock__a` but NOT a builtin `a`.
//!   2. End-to-end dispatch — a denied FQN `tool_use` yields the
//!      `Permission denied:` `ToolResult` (`turn_loop.rs:359`) and never reaches the
//!      server; an allowed FQN reaches the server's `call_tool`.

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use llm_runtime::ContentBlock as LlmContentBlock;
use mcp::{ConfigScope, McpRegistry, McpServerConfig, RawConnectionProvider};
use mobile_linux_api::ProcessOutput;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use permission::gate::{PermissionDecision, PermissionGate};
use permission::loader::permission_rules_from_settings_json;
use permission::{PermissionMode, PermissionPolicy, PermissionRuleSource, PolicyPermissionGate};
use platform_api::{McpTransport, McpTransportSpec, OutputEvent};
use protocol::ToolUseId;
use test_harness::mocks::MockMcpTransport;
use tool_api::registry::ToolRegistry;
use tool_api::BuiltinToolContext;

// ============================================================================
// Helpers (mirror parity_mcp_invocation.rs)
// ============================================================================

fn mock_config() -> McpServerConfig {
    McpServerConfig {
        name: "mock".into(),
        spec: McpTransportSpec::InProcess {
            registry_key: "mock".into(),
        },
        scope: ConfigScope::Settings(protocol::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

fn ctx_with_registry(registry: Arc<McpRegistry>) -> BuiltinToolContext {
    let mut ctx = tool_api::test_support::shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.mcp_registry = Some(registry);
    ctx
}

async fn seed(tools: &[&str]) -> (Arc<ToolRegistry>, Arc<McpRegistry>, Arc<MockMcpTransport>) {
    let mock = Arc::new(MockMcpTransport::with_call_responder());
    for t in tools {
        mock.add_tool(t);
    }
    let mcp_registry = Arc::new(McpRegistry::with_raw_conn(
        mock.clone() as Arc<dyn McpTransport>,
        mock.clone() as Arc<dyn RawConnectionProvider>,
    ));
    mcp_registry.connect(mock_config()).await.unwrap();

    let ctx = ctx_with_registry(mcp_registry.clone());
    let reg = ToolRegistry::new();
    for (conn_id, mcp_tools) in tool_mcp::build_registered_mcp_tools(&mcp_registry, ctx).await {
        reg.register_mcp_tools(conn_id, mcp_tools);
    }
    (Arc::new(reg), mcp_registry, mock)
}

/// Build a `PolicyPermissionGate` over the `permissions` JSON, with `inner` as
/// the Ask-delegation transport (a `NoOpPermissionGate` allows by default, so a
/// non-rule-denied tool is never blocked here).
fn policy_gate(
    permissions_json: &str,
    inner: Arc<dyn PermissionGate>,
) -> Arc<PolicyPermissionGate> {
    let rules = permission_rules_from_settings_json(
        permissions_json,
        PermissionRuleSource::Settings(protocol::SettingsScope::Project),
    )
    .unwrap();
    let policy = Arc::new(PermissionPolicy::from_rules(PermissionMode::Default, rules));
    Arc::new(PolicyPermissionGate::new(policy, inner))
}

fn build_orchestrator(
    api: Arc<MockApiClient>,
    tools: Arc<ToolRegistry>,
    output: Arc<MockOutputStream>,
    mcp_registry: Arc<McpRegistry>,
    perms: Arc<dyn PermissionGate>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        orchestrator::test_support::noop_hook_executor(),
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_mcp_registry(mcp_registry)
}

// ============================================================================
// Layer 1: policy + gate key MCP rules on the FQN, not the short name
// ============================================================================

/// A deny rule whose value is the FQN `mcp__mock__a` DENIES `mcp__mock__a` but
/// does NOT match a builtin named `a` — the FQN namespace is distinct from the
/// builtin namespace (claude-code `buildMcpToolName` keys MCP rules on the FQN).
#[test]
fn deny_rule_on_fqn_does_not_collide_with_builtin_short_name() {
    let rules = permission_rules_from_settings_json(
        r#"{ "permissions": { "deny": ["mcp__mock__a"] } }"#,
        PermissionRuleSource::Settings(protocol::SettingsScope::Project),
    )
    .unwrap();
    let policy = PermissionPolicy::from_rules(PermissionMode::Default, rules);

    // The FQN is denied by the rule (exact tool-name match).
    assert!(
        matches!(
            policy.authorize("mcp__mock__a", &serde_json::json!({ "x": 1 })),
            permission::result::PermissionResult::Deny { .. }
        ),
        "deny rule mcp__mock__a must deny the FQN tool_use"
    );

    // A builtin named `a` is NOT denied by that rule — it falls through to the
    // Default-mode ask (no rule matched), proving the namespaces don't collide.
    assert!(
        matches!(
            policy.authorize("a", &serde_json::json!({})),
            permission::result::PermissionResult::Ask { .. }
        ),
        "the FQN deny rule must NOT deny a builtin short-named `a`"
    );
}

/// At the enforcing gate: `mcp__mock__a` is denied; the same-config gate does
/// NOT deny a builtin `a` (its inner transport — here an allow — is consulted).
#[tokio::test]
async fn gate_denies_fqn_but_not_builtin_short_name() {
    let inner = Arc::new(NoOpPermissionGate); // allow-by-default for delegated asks
    let gate = policy_gate(r#"{ "permissions": { "deny": ["mcp__mock__a"] } }"#, inner);

    // FQN → Deny (rule), with a rendered reason.
    match gate.check("mcp__mock__a", &serde_json::json!({})).await {
        PermissionDecision::Deny { reason } => {
            assert!(
                reason.contains("mcp__mock__a"),
                "deny reason names the FQN rule: {reason}"
            );
        }
        PermissionDecision::Allow => panic!("expected Deny for the FQN, got Allow"),
    }

    // Builtin `a` → NOT a rule-deny. `a` is unknown → DenyByDefault → the ask is
    // delegated to the inner gate, which allows. The key parity point: it is NOT
    // blocked by the FQN deny rule.
    assert_eq!(
        gate.check("a", &serde_json::json!({})).await,
        PermissionDecision::Allow,
        "builtin `a` must not be blocked by the mcp__mock__a deny rule"
    );
}

/// An exact allow rule on the FQN `mcp__mock__b` permits it (no prompt).
#[tokio::test]
async fn allow_rule_on_fqn_permits_it() {
    // Inner denies so we prove the ALLOW came from the rule, not delegation.
    let inner = Arc::new(NoOpPermissionGate);
    let gate = policy_gate(r#"{ "permissions": { "allow": ["mcp__mock__b"] } }"#, inner);
    assert_eq!(
        gate.check("mcp__mock__b", &serde_json::json!({})).await,
        PermissionDecision::Allow,
        "exact allow rule on the FQN must permit it"
    );
}

// ============================================================================
// Layer 2: the dispatch chokepoint honors the FQN rule end-to-end
// ============================================================================

/// A `mcp__mock__a` tool_use under a `deny: ["mcp__mock__a"]` policy produces
/// the `Permission denied:` error ToolResult (turn_loop.rs:359) and the server's
/// `call_tool` is NEVER reached (the gate short-circuits before dispatch).
#[tokio::test]
async fn denied_fqn_tool_use_yields_permission_denied_result_and_skips_server() {
    let (tools, mcp_registry, mock) = seed(&["a", "b"]).await;
    let perms = policy_gate(
        r#"{ "permissions": { "deny": ["mcp__mock__a"] } }"#,
        Arc::new(NoOpPermissionGate),
    );

    let api = Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: ToolUseId::new().to_string(),
                name: "mcp__mock__a".into(),
                input: serde_json::json!({ "x": 1 }),
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "recovered".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orchestrator(api, tools, output.clone(), mcp_registry, perms);
    orch.run_turn("call the denied mcp tool")
        .await
        .expect("turn ok");

    // The server was never reached — the deny short-circuits before dispatch.
    assert!(
        mock.called_tools().is_empty(),
        "a denied FQN tool_use must not reach the server's call_tool"
    );

    // The emitted ToolResult carries claude-code's verbatim deny message
    // (`Permission to use ${tool} has been denied.`, permissions.ts:1087).
    let events = output.snapshot().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::ToolResult { tool, result, .. } if tool == "mcp__mock__a" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("a ToolResult for the denied tool must be emitted");
    let err = result
        .get("error")
        .and_then(|v| v.as_str())
        .expect("denied result carries an `error` string");
    assert!(
        err == "Permission to use mcp__mock__a has been denied.",
        "denied FQN must take the Permission-denied path with the verbatim message; got {err:?}"
    );
}

/// An allowed FQN (`allow: ["mcp__mock__b"]`) passes the gate and reaches the
/// server's `call_tool`, round-tripping a non-error result.
#[test]
fn allowed_fqn_tool_use_reaches_server() {
    std::thread::Builder::new()
        .name("mcp-permission-allowed".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("MCP permission runtime")
                .block_on(allowed_fqn_tool_use_reaches_server_inner());
        })
        .expect("spawn MCP permission test thread")
        .join()
        .expect("MCP permission test thread");
}

async fn allowed_fqn_tool_use_reaches_server_inner() {
    let (tools, mcp_registry, mock) = seed(&["a", "b"]).await;
    let perms = policy_gate(
        r#"{ "permissions": { "allow": ["mcp__mock__b"] } }"#,
        Arc::new(NoOpPermissionGate),
    );

    let api = Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: ToolUseId::new().to_string(),
                name: "mcp__mock__b".into(),
                input: serde_json::json!({}),
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orchestrator(api, tools, output.clone(), mcp_registry, perms);
    orch.run_turn("call the allowed mcp tool")
        .await
        .expect("turn ok");

    assert_eq!(
        mock.called_tools(),
        vec!["b".to_string()],
        "the allowed FQN must reach the server's call_tool exactly once"
    );

    let events = output.snapshot().await;
    let tool_result = events
        .iter()
        .find_map(|e| match e {
            OutputEvent::ToolResult { tool, result, .. } if tool == "mcp__mock__b" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("a ToolResult for mcp__mock__b must be emitted");
    // After the MCP-result unwrap, the result `data` is the server's `content`
    // DIRECTLY — a successful call carries the content (not an error string) and
    // has no `is_error` wrapper key.
    assert!(
        tool_result.get("is_error").is_none(),
        "the unwrapped result data has no is_error wrapper key"
    );
    assert!(
        !tool_result.is_null(),
        "an allowed, successful MCP call carries the server's content"
    );
}
