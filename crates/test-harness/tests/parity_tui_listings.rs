//! M6-07 parity — locks `/mcp`, `/hooks`, `/agents` empty-state literals
//! and one non-empty sample for each.

use command_api::builtins::agents::AgentsHandler;
use command_api::builtins::hooks::HooksHandler;
use command_api::builtins::mcp::McpHandler;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use orchestrator::test_support::MockOrchestratorHandle;
use platform_api::{AgentInfo, HookInfo, McpServerInfo, McpStatus};
use serde_json::Value;
use std::sync::Arc;

const FIXTURE: &str = include_str!("../src/parity/fixtures/tui_listings.json");

fn args(name: &str) -> ParsedSlashCommand {
    ParsedSlashCommand {
        name: name.into(),
        raw_args: String::new(),
        positional_args: vec![],
    }
}

fn load_fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture parses")
}

#[tokio::test]
async fn empty_states_match_fixture() {
    let fixture = load_fixture();
    let empty = &fixture["empty_states"];

    let mock = Arc::new(MockOrchestratorHandle::new());
    let r = McpHandler::new(mock.clone()).handle(&args("mcp")).await;
    match r {
        CommandResult::Done { display: Some(s) } => {
            assert_eq!(s, empty["mcp"].as_str().unwrap());
        }
        other => panic!("got {other:?}"),
    }

    let r = HooksHandler::new(mock.clone()).handle(&args("hooks")).await;
    match r {
        CommandResult::Done { display: Some(s) } => {
            assert_eq!(s, empty["hooks"].as_str().unwrap());
        }
        other => panic!("got {other:?}"),
    }

    let r = AgentsHandler::new(mock).handle(&args("agents")).await;
    match r {
        CommandResult::Done { display: Some(s) } => {
            assert_eq!(s, empty["agents"].as_str().unwrap());
        }
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn non_empty_mcp_matches_fixture() {
    let fixture = load_fixture();
    let mock = Arc::new(MockOrchestratorHandle::new());
    mock.set_mcp_servers(vec![
        McpServerInfo {
            name: "filesystem".into(),
            status: McpStatus::Disconnected,
            transport: "stdio".into(),
        },
        McpServerInfo {
            name: "memory".into(),
            status: McpStatus::Connected,
            transport: "stdio".into(),
        },
    ]);
    let r = McpHandler::new(mock).handle(&args("mcp")).await;
    let expected = fixture["non_empty_sample"]["mcp"]["output"]
        .as_str()
        .unwrap();
    match r {
        CommandResult::Done { display: Some(s) } => assert_eq!(s, expected),
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn non_empty_hooks_matches_fixture() {
    let fixture = load_fixture();
    let mock = Arc::new(MockOrchestratorHandle::new());
    mock.set_hooks(vec![HookInfo {
        name: "./fmt.sh".into(),
        event: "PreToolUse".into(),
        matcher: Some("Write|Edit".into()),
        timeout_ms: 30_000,
        ..HookInfo::default()
    }]);
    let r = HooksHandler::new(mock).handle(&args("hooks")).await;
    let expected = fixture["non_empty_sample"]["hooks"]["output"]
        .as_str()
        .unwrap();
    match r {
        CommandResult::Done { display: Some(s) } => assert_eq!(s, expected),
        other => panic!("got {other:?}"),
    }
}

#[tokio::test]
async fn non_empty_agents_matches_fixture() {
    let fixture = load_fixture();
    let mock = Arc::new(MockOrchestratorHandle::new());
    mock.set_agents(vec![AgentInfo {
        name: "reviewer".into(),
        description: "Reviews code".into(),
        tools_allowed: vec!["Read".into(), "Grep".into()],
        wildcard_tools: false,
        ..AgentInfo::default()
    }]);
    let r = AgentsHandler::new(mock).handle(&args("agents")).await;
    let expected = fixture["non_empty_sample"]["agents"]["output"]
        .as_str()
        .unwrap();
    match r {
        CommandResult::Done { display: Some(s) } => assert_eq!(s, expected),
        other => panic!("got {other:?}"),
    }
}
