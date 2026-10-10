//! M4-05 parity driver — asserts every locked literal from
//! `parity/fixtures/agent_task_tools.json`, excluding removed Agent aliases, appears in
//! production source (constants, telemetry NAMES array, task-id regex).

#![allow(clippy::unwrap_used)]

use serde::Deserialize;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct ToolNames {
    agent: String,
    send_message: String,
    task_create: String,
    task_get: String,
    task_list: String,
    task_update: String,
    task_stop: String,
    task_output: String,
}

#[derive(Deserialize)]
struct ConstantsLock {
    #[serde(rename = "AGENT_TOOL_NAME")]
    agent_tool_name: String,
    #[serde(rename = "SEND_MESSAGE_TOOL_NAME")]
    send_message_tool_name: String,
    #[serde(rename = "SEND_MESSAGE_CLAIM_WINDOW_RUST")]
    send_message_claim_window_rust: String,
    #[serde(rename = "SUBAGENT_BUDGET_DENIED_PREFIX")]
    subagent_budget_denied_prefix: String,
    #[serde(rename = "M3_05_BYTE_LOCKED_BUDGET_EXAMPLE")]
    m3_05_byte_locked_budget_example: String,
}

#[derive(Deserialize)]
struct Fixture {
    tool_names: ToolNames,
    builtin_subagent_types: Vec<String>,
    send_message_claim_window_secs: u64,
    task_id_regex: String,
    task_types: Vec<String>,
    task_status_values: Vec<String>,
    telemetry_events: Vec<String>,
    constants_lock: ConstantsLock,
}

fn fx() -> Fixture {
    load_fixture("agent_task_tools")
}

#[test]
fn agent_task_tool_names_match_production_constants() {
    let f = fx();
    assert_eq!(f.tool_names.agent, tool_agent::agent::AGENT_TOOL_NAME);
    assert_eq!(
        f.tool_names.send_message,
        tool_ui::send_message::SEND_MESSAGE_TOOL_NAME
    );
    assert_eq!(
        f.tool_names.task_create,
        tool_task::task::TASK_CREATE_TOOL_NAME
    );
    assert_eq!(f.tool_names.task_get, tool_task::task::TASK_GET_TOOL_NAME);
    assert_eq!(f.tool_names.task_list, tool_task::task::TASK_LIST_TOOL_NAME);
    assert_eq!(
        f.tool_names.task_update,
        tool_task::task::TASK_UPDATE_TOOL_NAME
    );
    assert_eq!(f.tool_names.task_stop, tool_task::task::TASK_STOP_TOOL_NAME);
    assert_eq!(
        f.tool_names.task_output,
        tool_task::task::TASK_OUTPUT_TOOL_NAME
    );
}

#[test]
fn four_builtin_subagent_types_match_production() {
    // Re-captured 2026-08-06 from the REAL 2.1.220/221/223 binaries: the old
    // 2026-03-31 TS snapshot's `verification` agent is a phantom (0-hit in
    // every local oracle), and the oracle's `claude-code-guide` + `claude`
    // catch-all are excluded as the user-confirmed multi-provider divergence.
    let f = fx();
    let prod: Vec<String> = tool_agent::agent::BUILTIN_SUBAGENT_TYPES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(f.builtin_subagent_types, prod);
    assert_eq!(f.builtin_subagent_types.len(), 4);
}

#[test]
fn send_message_claim_window_matches_production() {
    let f = fx();
    assert_eq!(f.send_message_claim_window_secs, 30);
    assert_eq!(
        tool_ui::send_message::SEND_MESSAGE_CLAIM_WINDOW.as_secs(),
        f.send_message_claim_window_secs
    );
}

#[test]
fn task_id_regex_matches_validate_task_id_acceptance() {
    let f = fx();
    let re = regex::Regex::new(&f.task_id_regex).unwrap();
    // Probe with a few well-formed prefixes (one per TaskType).
    // One probe per TaskType prefix, incl. 's' (oracle `monitor_ws:"s"`
    // @242497270), 'k' (`mcp_task`) and 'e' (`auto_mode_scan`, oracle table `k`
    // in src_160932144.js). Without a probe per prefix a widened production
    // regex passes this test while accepting an id the fixture never pinned.
    for prefix in ['b', 'a', 'r', 't', 'w', 'm', 'd', 'k', 's', 'f', 'e'] {
        let id = format!("{prefix}12345678");
        assert!(
            re.is_match(&id),
            "fixture regex must accept {id} (prefix={prefix})"
        );
        assert!(
            tool_task::task::validate_task_id(&id).is_ok(),
            "validate_task_id must accept {id}"
        );
    }
    // Negative samples — bad prefix, uppercase suffix, wrong length.
    assert!(!re.is_match("X12345678"));
    assert!(!re.is_match("b1234567Z"));
    assert!(!re.is_match("toolong0000"));
}

#[test]
fn task_types_match_production() {
    let f = fx();
    let prod: Vec<String> = tool_task::task::TASK_TYPES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(f.task_types, prod);
}

#[test]
fn task_status_values_match_production() {
    let f = fx();
    let prod: Vec<String> = tool_task::task::TASK_STATUSES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(f.task_status_values, prod);
}

#[test]
fn telemetry_events_present_in_tengu_tool_names_array() {
    let f = fx();
    assert_eq!(f.telemetry_events.len(), 24, "M4-05 locks 24 events");
    for name in &f.telemetry_events {
        assert!(
            telemetry::tengu::ALL_EVENT_NAMES.contains(&name.as_str()),
            "fixture event {name} missing from tengu::ALL_EVENT_NAMES"
        );
    }
}

#[test]
fn budget_denied_byte_lock_matches_m3_05_format() {
    let f = fx();
    // Production formatter mirrors `cost/src/budget.rs` test fixture string.
    let built = tool_agent::agent::format_budget_denied(150_750_000_000);
    assert_eq!(built, f.constants_lock.m3_05_byte_locked_budget_example);
    assert_eq!(built, "Budget exceeded ($150.75); stopped.");
}

#[test]
fn constants_lock_block_matches_production() {
    let f = fx();
    assert_eq!(
        f.constants_lock.agent_tool_name,
        tool_agent::agent::AGENT_TOOL_NAME
    );
    assert_eq!(
        f.constants_lock.send_message_tool_name,
        tool_ui::send_message::SEND_MESSAGE_TOOL_NAME
    );
    assert_eq!(
        f.constants_lock.send_message_claim_window_rust,
        "Duration::from_secs(30)"
    );
    assert_eq!(
        f.constants_lock.subagent_budget_denied_prefix,
        tool_agent::agent::SUBAGENT_BUDGET_DENIED_PREFIX
    );
}
