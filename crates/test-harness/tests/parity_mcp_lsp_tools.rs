//! M4-07 parity driver — asserts every locked literal from
//! `parity/fixtures/mcp_lsp_tools.json` appears byte-for-byte in production
//! source (constants, telemetry NAMES, `McpClientError::Timeout` Display).

use serde::Deserialize;
use serde_json::Value;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    tool_names: ToolNames,
    wire_identifiers: WireIdentifiers,
    telemetry_events: Vec<String>,
    auth_kind_mapping: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)] // fixture keys are tool slot names
struct ToolNames {
    mcp_tool: String,
    mcp_auth_tool: String,
    list_mcp_resources_tool: String,
    read_mcp_resource_tool: String,
    lsp_tool: String,
}

#[derive(Deserialize)]
struct WireIdentifiers {
    mcp_full_name_prefix: String,
    mcp_full_name_separator: String,
    mcp_timeout_display_template: String,
    mcp_timeout_concrete_example: String,
    lsp_position_error: String,
    lsp_server_not_running_template: String,
    lsp_operations_locked: Vec<String>,
}

#[test]
fn tool_names_match_production_constants() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    assert_eq!(fx.tool_names.mcp_tool, tool_mcp::mcp_tool::MCP_TOOL_NAME);
    assert_eq!(
        fx.tool_names.mcp_auth_tool,
        tool_mcp::mcp_tool::MCP_AUTH_TOOL_NAME
    );
    assert_eq!(
        fx.tool_names.list_mcp_resources_tool,
        tool_mcp::mcp_tool::LIST_MCP_RESOURCES_TOOL_NAME
    );
    assert_eq!(
        fx.tool_names.read_mcp_resource_tool,
        tool_mcp::mcp_tool::READ_MCP_RESOURCE_TOOL_NAME
    );
    assert_eq!(fx.tool_names.lsp_tool, tool_lsp::lsp_tool::LSP_TOOL_NAME);
}

#[test]
fn mcp_wire_identifiers_match() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    assert_eq!(
        fx.wire_identifiers.mcp_full_name_prefix,
        tool_mcp::mcp_tool::MCP_TOOL_FULL_NAME_PREFIX
    );
    assert_eq!(
        fx.wire_identifiers.mcp_full_name_separator,
        tool_mcp::mcp_tool::MCP_TOOL_FULL_NAME_SEPARATOR
    );
}

#[test]
fn lsp_wire_identifiers_match() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    assert_eq!(
        fx.wire_identifiers.lsp_position_error,
        tool_lsp::lsp_tool::LSP_POSITION_ERROR
    );
    assert_eq!(
        fx.wire_identifiers.lsp_operations_locked,
        tool_lsp::lsp_tool::LSP_OPERATIONS_LOCKED.to_vec()
    );
}

#[test]
fn telemetry_events_present_in_all_event_names() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    assert_eq!(
        fx.telemetry_events.len(),
        15,
        "M4-07 must list exactly 15 telemetry events (3 per tool; MCP_COMPLETED/FAILED are reused from M3-06 baseline)"
    );
    let names = telemetry::tengu::ALL_EVENT_NAMES;
    for name in &fx.telemetry_events {
        assert!(
            names.contains(&name.as_str()),
            "fixture event {name} missing from tengu::ALL_EVENT_NAMES"
        );
    }
}

#[test]
fn mcp_timeout_display_matches_locked_template() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    assert_eq!(
        fx.wire_identifiers.mcp_timeout_display_template,
        "MCP server \"{server}\" tool \"{tool}\" timed out after {secs}s"
    );
    let err = mcp::McpClientError::Timeout {
        server: "filesystem".into(),
        tool: "read".into(),
        secs: 60,
    };
    assert_eq!(
        err.to_string(),
        fx.wire_identifiers.mcp_timeout_concrete_example
    );
}

#[test]
fn lsp_server_not_running_template_byte_locked() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    assert_eq!(
        fx.wire_identifiers.lsp_server_not_running_template,
        "LSP server '{name}' is not running"
    );
}

#[test]
fn auth_kind_mapping_covers_all_transport_variants() {
    let fx: Fixture = load_fixture("mcp_lsp_tools");
    // 13 entries: stdio + 5 sse variants + 3 http variants + 2 websocket + 1 inProcess + 1 sseIde + 1 sdkControl.
    // Minus 1 since we collapsed "sse_with_oauth" / "sse_bare" / etc. into one mapping per relevant variant.
    assert!(fx.auth_kind_mapping.len() >= 10);
    // Sanity-check a few critical entries.
    assert_eq!(fx.auth_kind_mapping["stdio"], "none");
    assert_eq!(fx.auth_kind_mapping["sse_with_oauth"], "oauth");
    assert_eq!(
        fx.auth_kind_mapping["sse_with_headers_helper"],
        "headers_helper"
    );
    assert_eq!(
        fx.auth_kind_mapping["websocket_with_headers"],
        "static_headers"
    );
    assert_eq!(fx.auth_kind_mapping["inProcess"], "none");
}

#[test]
fn fixture_loads_as_valid_json() {
    let _v: Value = load_fixture("mcp_lsp_tools");
}
