//! What the 2.1.267 sweep established, pinned against the port.
//!
//! Unlike its `parity_claude_2_1_252.rs` sibling this file does NOT pin
//! version-facing identifiers: `lingxi_core::host::CLAUDE_CODE_VERSION` is
//! deliberately still `2.1.252`, because this port is not wholly at 2.1.267 and
//! advertising that it is would be a false claim. What is pinned here is the
//! set of ORACLE FACTS the sweep read out of the 2.1.267 binary and then built
//! behaviour on.
//!
//! 🚨 Why this file exists at all: four of those facts are gate DEFAULTS, and
//! three "this is not a gap" rulings in `src/parity/fixtures/accepted_divergences.json`
//! rest on
//! them. If upstream flips `tengu_lively_waffle` to true, the SubagentHandback
//! ruling silently becomes wrong. This file cannot detect that on its own — the
//! oracle binary is not in the repo — but it keeps the recorded value, its
//! provenance and its sha256 next to the behaviour, so a re-check is a diff
//! rather than an excavation.
//!
//! Fixture provenance is in the `capture` block: which binary, which sha256,
//! and that the facts were read from the plain-JS chunks rather than
//! `claude-code/src`, which is stale enough to manufacture findings.

use serde_json::Value;

const FACTS: &str = include_str!("../src/parity/fixtures/cc_2_1_267_oracle_facts.json");

fn facts() -> Value {
    serde_json::from_str(FACTS).expect("2.1.267 oracle facts fixture parses")
}

#[test]
fn the_fixture_names_the_binary_it_came_from() {
    // A fixture's "oracle fact" is only as good as its capture block.
    let f = facts();
    let capture = &f["capture"];
    assert_eq!(capture["version"], "2.1.267");
    assert_eq!(
        capture["sha256"],
        "a681f3008f0050029aeebcab3af51bb6a55ddeb625a3af3141a4416d43cd2558"
    );
    assert!(
        capture["method"]
            .as_str()
            .expect("method")
            .contains("NOT from claude-code/src"),
        "the capture must say where the facts did NOT come from"
    );
}

#[test]
fn the_recorded_gate_defaults_are_the_ones_the_rulings_rest_on() {
    let f = facts();
    let gates = &f["gate_defaults"];
    // Live upstream ⇒ a missing implementation here is a real gap.
    assert_eq!(gates["tengu_mcp_legacy_sse_fallback"]["default"], true);
    assert_eq!(gates["tengu_surface_failed_mcp_servers"]["default"], true);
    assert_eq!(
        gates["tengu_keybinding_customization_release"]["default"],
        true
    );
    // Dormant upstream ⇒ absence here is alignment, not a gap.
    assert_eq!(gates["tengu_lively_waffle"]["default"], false);
}

#[test]
fn the_allowlist_exempt_scopes_match_the_ported_rule() {
    // claude `XJ(e){return FTn.includes(e)}` with FTn=["enterprise","managed"].
    let f = facts();
    let scopes: Vec<&str> = f["mcp_policy"]["allowlist_exempt_scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|s| s.as_str().expect("scope"))
        .collect();
    assert_eq!(scopes, vec!["enterprise", "managed"]);

    // And the port really does exempt exactly those two: an org-delivered
    // server that read no environment is allowed by a list that does not name
    // it, while the same server at user scope is not.
    let policy = mcp::enterprise_policy::McpPolicy {
        denied: None,
        allowed: Some(vec![mcp::enterprise_policy::McpServerMatcher {
            server_name: Some("something-else".to_string()),
            server_command: None,
            server_url: None,
        }]),
    };
    for (scope, want) in [
        (mcp::connection::ConfigScope::Enterprise, true),
        (
            mcp::connection::ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
            true,
        ),
        (
            mcp::connection::ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            false,
        ),
        (
            mcp::connection::ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
            false,
        ),
    ] {
        let config = mcp::connection::McpServerConfig {
            name: "org-tool".to_string(),
            spec: lingxi_core::host::McpTransportSpec::Http {
                url: "https://x.test/mcp".to_string(),
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
            scope,
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: Default::default(),
        };
        assert_eq!(
            mcp::enterprise_policy::is_server_allowed(&config, &policy),
            want,
            "{scope:?} exemption must match the recorded FTn"
        );
    }
}

#[test]
fn the_managed_settings_key_errors_are_byte_exact() {
    let f = facts();
    let key = &f["mcp_policy"]["managed_settings_key"];
    assert_eq!(
        key["shape_error"].as_str().expect("shape error"),
        mcp::enterprise_policy::MANAGED_MCP_SERVERS_SHAPE_INVALID
    );
    assert_eq!(
        key["name_error"].as_str().expect("name error"),
        mcp::enterprise_policy::MANAGED_MCP_SERVER_NAME_INVALID
    );
    // Only remote transports: an organization may push a url to every user, not
    // a program to run on their machine.
    let types: Vec<&str> = key["accepted_types"]
        .as_array()
        .expect("types")
        .iter()
        .map(|t| t.as_str().expect("type"))
        .collect();
    assert_eq!(types, vec!["http", "sse"]);
}

#[test]
fn the_project_scope_refusal_copy_is_byte_exact() {
    let f = facts();
    assert_eq!(
        f["mcp_policy"]["legacy_sse_rescue"]["project_scope_refusal"]
            .as_str()
            .expect("refusal"),
        mcp::server_gate::PROJECT_UNRESOLVED_ENV_REF_REFUSAL
    );
}

#[test]
fn the_goal_cleared_reasons_are_the_six_upstream_passes() {
    use lingxi_core::host::GoalClearedReason::*;
    let f = facts();
    let recorded: Vec<&str> = f["goal_cleared_reasons"]["values"]
        .as_array()
        .expect("values")
        .iter()
        .map(|v| v.as_str().expect("reason"))
        .collect();
    let ported: Vec<&str> = [
        UserClear,
        Superseded,
        ContextLimit,
        ApiError,
        SessionClear,
        ResumeSwap,
    ]
    .iter()
    .map(|r| r.as_str())
    .collect();
    assert_eq!(
        recorded, ported,
        "every reason kB is called with must have a ported variant, in order"
    );
}

#[test]
fn the_bundled_name_set_is_read_off_registrations_not_counts() {
    let f = facts();
    let bundled = &f["bundled_skills"];
    let names = bundled["names"].as_array().expect("names");
    assert_eq!(names.len(), 21);
    assert_eq!(bundled["count"], 21);

    // 🚨 The trap this records: `stuck` has ~50 occurrences in the binary and no
    // registration. It, `skillify` and `lorem-ipsum` must never appear in the
    // upstream set — the first from a miscounted substring, the other two from
    // the stale TS mirror.
    let names: Vec<&str> = names.iter().map(|n| n.as_str().expect("name")).collect();
    for ghost in bundled["absent_despite_the_stale_mirror"]
        .as_array()
        .expect("ghosts")
    {
        let ghost = ghost.as_str().expect("ghost");
        assert!(
            !names.contains(&ghost),
            "{ghost} is not an upstream bundled skill and must not be listed as one"
        );
    }
}

#[test]
fn the_legacy_sse_rescue_triggers_on_exactly_three_statuses() {
    let f = facts();
    let rescue = &f["mcp_policy"]["legacy_sse_rescue"];
    let statuses: Vec<u64> = rescue["trigger_statuses"]
        .as_array()
        .expect("statuses")
        .iter()
        .map(|s| s.as_u64().expect("status"))
        .collect();
    assert_eq!(statuses, vec![400, 404, 405]);
    // The half that is easy to drop, recorded next to the statuses so it cannot
    // be read as "status alone is the trigger".
    assert!(rescue["predicate_from"]
        .as_str()
        .expect("predicate")
        .contains("does NOT parse as a JSON-RPC message"));
    assert_eq!(rescue["absent_body_falls_back"], true);
    assert_eq!(rescue["budget_ms"]["min"], 1000);
    assert_eq!(rescue["budget_ms"]["max"], 5000);
}
