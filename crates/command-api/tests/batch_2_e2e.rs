#![cfg(feature = "builtin-handlers")]

//! End-to-end: build a registry with `register_all_builtin_commands` +
//! `register_core_batch_1` + `register_core_batch_2`, dispatch each of the 12
//! batch-2 commands through the `RegistrySlashDispatcher`, and verify
//! behaviour.
//!
//! M5-11 Task 15.

use async_trait::async_trait;
use command_api::builtins::{
    register_all_builtin_commands, register_core_batch_1, register_core_batch_2,
};
use command_api::CommandRegistry;
use command_api::RegistrySlashDispatcher;
use lingxi_core::host::{
    AgentInfo, AuthError, AuthHandle, HookInfo, LoginInfo, McpServerInfo, McpStatus,
    SlashCommandDispatcher, SlashDispatchResult, StatusSnapshot,
};
use orchestrator::test_support::MockOrchestratorHandle;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::RwLock;

struct MockAuth {
    result: StdMutex<Result<LoginInfo, AuthError>>,
}

impl MockAuth {
    fn ok(info: LoginInfo) -> Self {
        Self {
            result: StdMutex::new(Ok(info)),
        }
    }
}

#[async_trait]
impl AuthHandle for MockAuth {
    async fn login(&self) -> Result<LoginInfo, AuthError> {
        self.result.lock().unwrap().clone()
    }
    async fn logout(&self) -> Result<(), AuthError> {
        Ok(())
    }
    async fn current_user(&self) -> Option<LoginInfo> {
        self.result.lock().unwrap().as_ref().ok().cloned()
    }
}

fn fresh() -> (RegistrySlashDispatcher, Arc<MockOrchestratorHandle>) {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    let handle: Arc<MockOrchestratorHandle> = Arc::new(MockOrchestratorHandle::new());
    register_core_batch_1(&mut reg, handle.clone());
    let auth: Arc<dyn AuthHandle> = Arc::new(MockAuth::ok(LoginInfo {
        email: "u@x.com".into(),
        org_id: "org".into(),
    }));
    register_core_batch_2(&mut reg, handle.clone(), auth);
    let d = RegistrySlashDispatcher::new(Arc::new(RwLock::new(reg)));
    (d, handle)
}

/// claude-code parity (#61/#68): built-in command aliases registered in
/// `register_core_batch_1`/`_2` resolve to their target command.
#[test]
fn builtin_command_aliases_resolve_to_targets() {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    let handle: Arc<MockOrchestratorHandle> = Arc::new(MockOrchestratorHandle::new());
    register_core_batch_1(&mut reg, handle.clone());
    let auth: Arc<dyn AuthHandle> = Arc::new(MockAuth::ok(LoginInfo {
        email: "u@x.com".into(),
        org_id: "org".into(),
    }));
    register_core_batch_2(&mut reg, handle, auth);
    for (alias, target) in [
        ("reset", "clear"),
        ("new", "clear"),
        ("quit", "exit"),
        ("settings", "config"),
        ("allowed-tools", "permissions"),
        // #66/#61: cost/stats are now aliases of usage (not standalone cmds).
        ("cost", "usage"),
        ("stats", "usage"),
    ] {
        let cmd = reg
            .resolve(alias)
            .unwrap_or_else(|| panic!("alias /{alias} must resolve"));
        assert_eq!(cmd.name, target, "/{alias} should resolve to /{target}");
    }
}

/// #66/#61: `/cost` is no longer a standalone command — it is an alias of
/// `/usage`. Dispatching `/cost` therefore routes through the alias-aware
/// `get_handler` to the `usage` handler. Without batch-5 wiring (not called by
/// `fresh()`), `usage` is the headless InteractiveOnlyHandler fallback, so
/// `/cost`, `/stats`, and `/usage` all yield the same interactive-only notice.
#[tokio::test]
async fn cost_and_stats_dispatch_through_usage_alias() {
    let (d, _mock) = fresh();
    let expected = "/usage is available in interactive TUI mode only.";
    for raw in ["/cost", "/stats", "/usage"] {
        match d.dispatch(raw).await {
            SlashDispatchResult::Handled { display } => assert_eq!(display, expected, "{raw}"),
            other => panic!("{raw} expected Handled, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn config_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/config").await;
    assert!(matches!(
        r,
        SlashDispatchResult::Handled { ref display } if display.starts_with("Edited ")
    ));
}

#[tokio::test]
async fn model_list_dispatch() {
    let (d, mock) = fresh();
    mock.set_available_models(vec!["claude-opus-4-7".into()]);
    let snap = StatusSnapshot {
        model: "claude-opus-4-7".into(),
        ..StatusSnapshot::default()
    };
    mock.set_status_snapshot(snap);
    let r = d.dispatch("/model").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert_eq!(
            display,
            "Current model: claude-opus-4-7\nAvailable: claude-opus-4-7"
        );
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn model_switch_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/model claude-sonnet-4-6").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert_eq!(display, "Switched to model: claude-sonnet-4-6");
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn permissions_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/permissions").await;
    assert!(matches!(
        r,
        SlashDispatchResult::Handled { ref display } if display.starts_with("Edited ")
    ));
}

#[tokio::test]
async fn mcp_dispatch() {
    let (d, mock) = fresh();
    mock.set_mcp_servers(vec![McpServerInfo {
        name: "m".into(),
        status: McpStatus::Connected,
        transport: "stdio".into(),
    }]);
    let r = d.dispatch("/mcp").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert_eq!(display, "MCP servers (1):\n  m  connected  stdio\n");
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn hooks_dispatch() {
    let (d, mock) = fresh();
    mock.set_hooks(vec![HookInfo {
        name: "fmt".into(),
        event: "PostToolUse".into(),
        matcher: None,
        timeout_ms: 60_000,
        ..HookInfo::default()
    }]);
    let r = d.dispatch("/hooks").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert_eq!(display, "Hooks (1):\n  fmt  PostToolUse  60000ms\n");
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn agents_dispatch() {
    let (d, mock) = fresh();
    mock.set_agents(vec![AgentInfo {
        name: "r".into(),
        description: "x".into(),
        tools_allowed: vec![],
        wildcard_tools: false,
        ..AgentInfo::default()
    }]);
    let r = d.dispatch("/agents").await;
    if let SlashDispatchResult::Handled { display } = r {
        // cc 2.1.198 removed the interactive /agents wizard (M4): the command
        // now emits static guidance regardless of registered agents. Byte-locked
        // against agents.rs::agents_removed_message (`.lingxi/` = accepted
        // branding divergence).
        assert_eq!(
            display,
            "The /agents wizard has been removed.\n\nAsk Claude to create or update subagents for you (e.g. \"create a code-reviewer subagent that ...\"),\nor edit the files directly:\n  \u{2022} .lingxi/agents/       (this project)\n  \u{2022} ~/.lingxi/agents/     (all projects)\n\nDocs: https://code.claude.com/docs/en/sub-agents"
        );
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn login_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/login").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert_eq!(display, "Logged in as u@x.com (org: org).");
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn logout_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/logout").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert_eq!(display, "Logged out.");
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn version_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/version").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert!(display.starts_with("lingxi-cli "));
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn status_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/status").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert!(display.starts_with("Status:\n"));
        assert_eq!(display.matches('\n').count(), 11);
    } else {
        panic!("{r:?}");
    }
}

#[tokio::test]
async fn doctor_dispatch() {
    let (d, _) = fresh();
    let r = d.dispatch("/doctor").await;
    if let SlashDispatchResult::Handled { display } = r {
        assert!(display.starts_with("Doctor:\n"));
        assert!(display.contains("Summary:"));
    } else {
        panic!("{r:?}");
    }
}
