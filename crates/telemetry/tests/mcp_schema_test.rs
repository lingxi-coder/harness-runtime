use telemetry::pii::Verified;
use telemetry::tengu::mcp::{
    self, AddPayload, AuthConfigAuthenticatePayload, AuthConfigClearPayload, CommandInlinePayload,
    ConfigInvalidSource, DegradedPayload, DegradedReason, DeletePayload, ElicitationMode,
    ElicitationResponsePayload, ElicitationShownPayload, GetPayload, InputMissingRequiredPayload,
    LargeResultHandledPayload, ListChangedPayload, ListChangedType, ListPaginatedPayload,
    ListenReopenOutcome, ListenReopenPayload, ListenReopenTrigger, OAuthBrowserOpenPayload,
    OAuthFlowErrorPayload, OAuthFlowFailurePayload, OAuthFlowStartPayload, OAuthFlowSuccessPayload,
    OAuthIssuerEchoMismatchPayload, OAuthIssuerEchoMode, OAuthIssuerEchoOutcome,
    OAuthIssuerEchoSite, OAuthIssuerOriginRelation, OAuthRefreshFailurePayload,
    OAuthRefreshSuccessPayload, OAuthTokenPersistFailedPayload, OAuthXaaFlowSuccessPayload,
    PendingCallPayload, ReconcilePayload, ResourceTemplatesFetchedPayload,
    ServerConfigInvalidPayload, ServerConnectionFailedPayload, ServerConnectionSucceededPayload,
    ServerNeedsAuthPayload, ServersPayload, SessionExpiredPayload, StartPayload,
    ToolCallAuthErrorKind, ToolCallAuthErrorPayload, ToolResultEndedTurnPayload,
    ToolsCommandsLoadedPayload, ToolsListedPayload, ToolsRefreshedMidTurnPayload,
};

#[test]
fn all_forty_four_mcp_event_names_are_locked() {
    let names: &[&str] = &[
        mcp::SERVER_CONFIG_INVALID,
        mcp::SERVER_CONNECTION_SUCCEEDED,
        mcp::SERVER_CONNECTION_FAILED,
        mcp::TOOLS_LISTED,
        mcp::DEGRADED,
        // §11 — emitted on a cache hit (with `entryAgeMs`) and, per oracle
        // `Ko` @182515145, on the `Ko`-true miss reasons only (without it).
        mcp::DISCOVERY_SOURCE,
        mcp::LIST_CHANGED,
        mcp::RESOURCE_TEMPLATES_FETCHED,
        mcp::LISTEN_REOPEN,
        mcp::RESET_MCPJSON_CHOICES,
        telemetry::tengu::tool::MCP_TOOL_AUTO_BACKGROUNDED,
        mcp::START,
        mcp::AUTH_CONFIG_AUTHENTICATE,
        mcp::AUTH_CONFIG_CLEAR,
        mcp::OAUTH_BROWSER_OPEN,
        mcp::OAUTH_FLOW_START,
        mcp::OAUTH_FLOW_SUCCESS,
        mcp::OAUTH_FLOW_ERROR,
        mcp::OAUTH_REFRESH_SUCCESS,
        mcp::OAUTH_REFRESH_FAILURE,
        mcp::OAUTH_TOKEN_PERSIST_FAILED,
        mcp::OAUTH_ISSUER_ECHO_MISMATCH,
        mcp::SERVER_NEEDS_AUTH,
        mcp::TOOL_CALL_AUTH_ERROR,
        mcp::ADD,
        mcp::DELETE,
        mcp::GET,
        mcp::LIST,
        mcp::LOGIN,
        mcp::LOGOUT,
        mcp::COMMAND_INLINE,
        mcp::ELICITATION_SHOWN,
        mcp::ELICITATION_RESPONSE,
        mcp::INPUT_MISSING_REQUIRED,
        mcp::LARGE_RESULT_HANDLED,
        mcp::PENDING_CALL,
        mcp::SERVERS,
        mcp::TOOL_RESULT_ENDED_TURN,
        mcp::TOOLS_COMMANDS_LOADED,
        mcp::TOOLS_REFRESHED_MID_TURN,
        mcp::OAUTH_FLOW_FAILURE,
        mcp::SESSION_EXPIRED,
        mcp::LIST_PAGINATED,
        mcp::RECONCILE,
    ];
    assert_eq!(names.len(), 44);
    for n in names {
        assert!(n.starts_with("tengu_mcp_"));
    }
    assert_eq!(mcp::START, "tengu_mcp_start");
    assert_eq!(
        mcp::SERVER_CONFIG_INVALID,
        "tengu_mcp_server_config_invalid"
    );
    assert_eq!(
        mcp::SERVER_CONNECTION_SUCCEEDED,
        "tengu_mcp_server_connection_succeeded"
    );
    assert_eq!(
        mcp::SERVER_CONNECTION_FAILED,
        "tengu_mcp_server_connection_failed"
    );
    assert_eq!(mcp::TOOLS_LISTED, "tengu_mcp_tools_listed");
    assert_eq!(mcp::DEGRADED, "tengu_mcp_degraded");
    assert_eq!(mcp::LIST_CHANGED, "tengu_mcp_list_changed");
    assert_eq!(
        mcp::RESOURCE_TEMPLATES_FETCHED,
        "tengu_mcp_resource_templates_fetched"
    );
    assert_eq!(mcp::LISTEN_REOPEN, "tengu_mcp_listen_reopen");
    assert_eq!(
        mcp::RESET_MCPJSON_CHOICES,
        "tengu_mcp_reset_mcpjson_choices"
    );
    assert_eq!(
        mcp::AUTH_CONFIG_AUTHENTICATE,
        "tengu_mcp_auth_config_authenticate"
    );
    assert_eq!(mcp::AUTH_CONFIG_CLEAR, "tengu_mcp_auth_config_clear");
    assert_eq!(mcp::OAUTH_BROWSER_OPEN, "tengu_mcp_oauth_browser_open");
    assert_eq!(mcp::OAUTH_FLOW_START, "tengu_mcp_oauth_flow_start");
    assert_eq!(mcp::OAUTH_FLOW_SUCCESS, "tengu_mcp_oauth_flow_success");
    assert_eq!(mcp::OAUTH_FLOW_ERROR, "tengu_mcp_oauth_flow_error");
    assert_eq!(
        mcp::OAUTH_REFRESH_SUCCESS,
        "tengu_mcp_oauth_refresh_success"
    );
    assert_eq!(
        mcp::OAUTH_REFRESH_FAILURE,
        "tengu_mcp_oauth_refresh_failure"
    );
    assert_eq!(
        mcp::OAUTH_TOKEN_PERSIST_FAILED,
        "tengu_mcp_oauth_token_persist_failed"
    );
    assert_eq!(
        mcp::OAUTH_ISSUER_ECHO_MISMATCH,
        "tengu_mcp_oauth_issuer_echo_mismatch"
    );
    assert_eq!(mcp::SERVER_NEEDS_AUTH, "tengu_mcp_server_needs_auth");
    assert_eq!(mcp::TOOL_CALL_AUTH_ERROR, "tengu_mcp_tool_call_auth_error");
    assert_eq!(mcp::ADD, "tengu_mcp_add");
    assert_eq!(mcp::DELETE, "tengu_mcp_delete");
    assert_eq!(mcp::GET, "tengu_mcp_get");
    assert_eq!(mcp::LIST, "tengu_mcp_list");
    assert_eq!(mcp::LOGIN, "tengu_mcp_login");
    assert_eq!(mcp::LOGOUT, "tengu_mcp_logout");
    assert_eq!(mcp::COMMAND_INLINE, "tengu_mcp_command_inline");
    assert_eq!(mcp::ELICITATION_SHOWN, "tengu_mcp_elicitation_shown");
    assert_eq!(mcp::ELICITATION_RESPONSE, "tengu_mcp_elicitation_response");
    assert_eq!(
        mcp::INPUT_MISSING_REQUIRED,
        "tengu_mcp_input_missing_required"
    );
    assert_eq!(mcp::LARGE_RESULT_HANDLED, "tengu_mcp_large_result_handled");
    assert_eq!(mcp::PENDING_CALL, "tengu_mcp_pending_call");
    assert_eq!(mcp::SERVERS, "tengu_mcp_servers");
    assert_eq!(
        mcp::TOOL_RESULT_ENDED_TURN,
        "tengu_mcp_tool_result_ended_turn"
    );
    assert_eq!(
        mcp::TOOLS_COMMANDS_LOADED,
        "tengu_mcp_tools_commands_loaded"
    );
    assert_eq!(
        mcp::TOOLS_REFRESHED_MID_TURN,
        "tengu_mcp_tools_refreshed_mid_turn"
    );
    assert_eq!(mcp::OAUTH_FLOW_FAILURE, "tengu_mcp_oauth_flow_failure");
    assert_eq!(mcp::SESSION_EXPIRED, "tengu_mcp_session_expired");
    assert_eq!(mcp::LIST_PAGINATED, "tengu_mcp_list_paginated");
    assert_eq!(mcp::RECONCILE, "tengu_mcp_reconcile");
    assert_eq!(mcp::NAMES, names);
}

#[test]
fn auth_and_oauth_payloads_round_trip() {
    let authenticate = AuthConfigAuthenticatePayload {
        was_authenticated: true,
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&authenticate).unwrap(),
        serde_json::json!({
            "was_authenticated": true,
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );

    let clear = AuthConfigClearPayload {
        transport_type: Verified::assert_safe("sse".to_string()),
        mcp_server_key_hash: Verified::assert_safe("fedcba987654".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&clear).unwrap(),
        serde_json::json!({
            "transport_type": "sse",
            "mcp_server_key_hash": "fedcba987654",
        })
    );

    let browser = OAuthBrowserOpenPayload {
        success: true,
        headless: false,
        platform: Verified::assert_safe("macos".to_string()),
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&browser).unwrap(),
        serde_json::json!({
            "success": true,
            "headless": false,
            "platform": "macos",
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );

    let start = OAuthFlowStartPayload {
        flow_attempt_id: Verified::assert_safe("abc123".to_string()),
        is_oauth_flow: true,
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&start).unwrap(),
        serde_json::json!({
            "flow_attempt_id": "abc123",
            "is_oauth_flow": true,
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );

    let success = OAuthFlowSuccessPayload {
        flow_attempt_id: Verified::assert_safe("abc123".to_string()),
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&success).unwrap(),
        serde_json::json!({
            "flow_attempt_id": "abc123",
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );

    let error = OAuthFlowErrorPayload {
        flow_attempt_id: Verified::assert_safe("abc123".to_string()),
        reason: Verified::assert_safe("timeout".to_string()),
        error_code: Some(Verified::assert_safe("invalid_client".to_string())),
        http_status: Some(401),
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        serde_json::json!({
            "flow_attempt_id": "abc123",
            "reason": "timeout",
            "error_code": "invalid_client",
            "http_status": 401,
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );
}

#[test]
fn oauth_refresh_and_auth_state_payloads_round_trip() {
    let refresh_success = OAuthRefreshSuccessPayload {
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&refresh_success).unwrap(),
        serde_json::json!({
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );

    let refresh_failure = OAuthRefreshFailurePayload {
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
        reason: Verified::assert_safe("invalid_grant".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&refresh_failure).unwrap(),
        serde_json::json!({
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
            "reason": "invalid_grant",
        })
    );

    let persist_failed = OAuthTokenPersistFailedPayload {
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
        reason: Verified::assert_safe("storage_write_failed".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&persist_failed).unwrap(),
        serde_json::json!({
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
            "reason": "storage_write_failed",
        })
    );

    let needs_auth = ServerNeedsAuthPayload {
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
        cause: Some(Verified::assert_safe("discovery_schema".to_string())),
    };
    assert_eq!(
        serde_json::to_value(&needs_auth).unwrap(),
        serde_json::json!({
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
            "cause": "discovery_schema",
        })
    );

    let tool_call = ToolCallAuthErrorPayload {
        error_code: Verified::assert_safe("401".to_string()),
        transport_type: Verified::assert_safe("http".to_string()),
        auth_error_kind: ToolCallAuthErrorKind::TokenExpired,
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&tool_call).unwrap(),
        serde_json::json!({
            "error_code": "401",
            "transport_type": "http",
            "auth_error_kind": "token_expired",
            "mcp_server_key_hash": "0123456789ab",
        })
    );
}

#[test]
fn issuer_echo_payload_and_enums_round_trip() {
    let payload = OAuthIssuerEchoMismatchPayload {
        site: OAuthIssuerEchoSite::Rfc9728Chain,
        mode: OAuthIssuerEchoMode::Observe,
        origin_relation: OAuthIssuerOriginRelation::CrossOrigin,
        outcome: OAuthIssuerEchoOutcome::Proceeded,
        mismatch_facets: vec![Verified::assert_safe("host".to_string())],
        expected_issuer_hash: Verified::assert_safe("aaaabbbbccccdddd".to_string()),
        received_issuer_hash: Some(Verified::assert_safe("1111222233334444".to_string())),
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&payload).unwrap(),
        serde_json::json!({
            "site": "rfc9728_chain",
            "mode": "observe",
            "origin_relation": "cross_origin",
            "outcome": "proceeded",
            "mismatch_facets": ["host"],
            "expected_issuer_hash": "aaaabbbbccccdddd",
            "received_issuer_hash": "1111222233334444",
            "transport_type": "http",
            "mcp_server_key_hash": "0123456789ab",
        })
    );
}

#[test]
fn start_payload_round_trips() {
    let payload = StartPayload {
        transport: Verified::assert_safe("stdio".to_string()),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(json, serde_json::json!({ "transport": "stdio" }));
}

#[test]
fn start_payload_rejects_unknown_fields() {
    let result: Result<StartPayload, _> = serde_json::from_value(serde_json::json!({
        "transport": "stdio",
        "transport_type": "stdio",
    }));
    assert!(result.is_err(), "start payload is transport-only");
}

#[test]
fn server_connection_succeeded_payload_round_trips() {
    let payload = ServerConnectionSucceededPayload {
        connection_duration_ms: 42,
        transport_type: Verified::assert_safe("http".to_string()),
        scope: Verified::assert_safe("project".to_string()),
        is_plugin: true,
        negotiation_mode: Some(Verified::assert_safe("auto".to_string())),
        protocol_era: Some(Verified::assert_safe("modern".to_string())),
        negotiated_protocol_version: Some(Verified::assert_safe("2026-07-28".to_string())),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "connection_duration_ms": 42,
            "transport_type": "http",
            "scope": "project",
            "is_plugin": true,
            "negotiation_mode": "auto",
            "protocol_era": "modern",
            "negotiated_protocol_version": "2026-07-28",
        })
    );
}

#[test]
fn server_connection_failed_payload_round_trips() {
    let payload = ServerConnectionFailedPayload {
        transport_type: Verified::assert_safe("stdio".to_string()),
        scope: Verified::assert_safe("project".to_string()),
        is_plugin: false,
        connection_duration_ms: Some(7),
        negotiation_mode: Some(Verified::assert_safe("legacy".to_string())),
        error_code: Some(Verified::assert_safe("INVALID_CONFIG".to_string())),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "transport_type": "stdio",
            "scope": "project",
            "is_plugin": false,
            "connection_duration_ms": 7,
            "negotiation_mode": "legacy",
            "error_code": "INVALID_CONFIG",
        })
    );
}

#[test]
fn server_config_invalid_payload_round_trips() {
    let payload = ServerConfigInvalidPayload {
        transport_type: Verified::assert_safe("stdio".to_string()),
        field: Verified::assert_safe("url".to_string()),
        source: ConfigInvalidSource::Connect,
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "transport_type": "stdio",
            "field": "url",
            "source": "connect",
        })
    );
    let round_tripped: ServerConfigInvalidPayload =
        serde_json::from_value(json).expect("deserialize");
    assert_eq!(round_tripped.transport_type.as_str(), "stdio");
}

#[test]
fn server_config_invalid_payload_rejects_unknown_fields() {
    let json = serde_json::json!({
        "transport_type": "stdio",
        "field": "url",
        "source": "connect",
        "extra_unexpected_field": true,
    });
    let result: Result<ServerConfigInvalidPayload, _> = serde_json::from_value(json);
    assert!(
        result.is_err(),
        "deny_unknown_fields must reject an unrecognized key"
    );
}

#[test]
fn config_invalid_source_values_are_snake_case() {
    let loader = serde_json::to_value(ConfigInvalidSource::Loader).unwrap();
    let connect = serde_json::to_value(ConfigInvalidSource::Connect).unwrap();
    assert_eq!(loader, serde_json::json!("loader"));
    assert_eq!(connect, serde_json::json!("connect"));
}

#[test]
fn tools_listed_payload_round_trips() {
    let payload = ToolsListedPayload {
        transport_type: Verified::assert_safe("http".to_string()),
        list_duration_ms: 42,
        tool_count: 7,
        always_load_count: 2,
        discovery_source: Verified::assert_safe("live".to_string()),
        mcp_server_name: Some(Verified::assert_safe("my-server".to_string())),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "transport_type": "http",
            "list_duration_ms": 42,
            "tool_count": 7,
            "always_load_count": 2,
            "discovery_source": "live",
            "mcp_server_name": "my-server",
        })
    );
}

#[test]
fn tools_listed_payload_rejects_unknown_fields() {
    // Guards against silently reintroducing the byte-alignment doc's
    // unconfirmed `normalizedCount` / `keptCount` fields without first
    // tracing their real producer (see mcp.rs's module doc).
    let json = serde_json::json!({
        "transport_type": "stdio",
        "list_duration_ms": 1,
        "tool_count": 1,
        "always_load_count": 0,
        "discovery_source": "live",
        "mcp_server_name": "s",
        "normalizedCount": 3,
    });
    let result: Result<ToolsListedPayload, _> = serde_json::from_value(json);
    assert!(result.is_err(), "unconfirmed extra field must be rejected");
}

#[test]
fn list_changed_payload_round_trips() {
    let payload = ListChangedPayload {
        kind: ListChangedType::Tools,
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
        cause: Verified::assert_safe("notification".to_string()),
        previous_count: Some(2),
        new_count: Some(3),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "kind": "tools",
            "mcp_server_key_hash": "0123456789ab",
            "cause": "notification",
            "previous_count": 2,
            "new_count": 3,
        })
    );
}

#[test]
fn list_changed_type_wire_strings_match_serde() {
    for kind in [
        ListChangedType::Tools,
        ListChangedType::Prompts,
        ListChangedType::Resources,
    ] {
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::json!(kind.wire_str())
        );
    }
}

#[test]
fn resource_templates_fetched_payload_round_trips() {
    let payload = ResourceTemplatesFetchedPayload { template_count: 5 };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(json, serde_json::json!({"template_count": 5}));
}

#[test]
fn listen_reopen_payload_round_trips() {
    let payload = ListenReopenPayload {
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
        outcome: ListenReopenOutcome::Reopened,
        attempts: 3,
        trigger: ListenReopenTrigger::Graceful,
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "mcp_server_key_hash": "0123456789ab",
            "outcome": "reopened",
            "attempts": 3,
            "trigger": "graceful",
        })
    );
}

#[test]
fn listen_reopen_enums_use_snake_case() {
    for (outcome, expected) in [
        (ListenReopenOutcome::OpenedFromZero, "opened_from_zero"),
        (ListenReopenOutcome::Reopened, "reopened"),
        (ListenReopenOutcome::GaveUp, "gave_up"),
        (ListenReopenOutcome::BudgetExhausted, "budget_exhausted"),
        (ListenReopenOutcome::Parked, "parked"),
    ] {
        assert_eq!(
            serde_json::to_value(outcome).unwrap(),
            serde_json::json!(expected)
        );
        assert_eq!(outcome.wire_str(), expected);
    }
    for (trigger, expected) in [
        (ListenReopenTrigger::Connect, "connect"),
        (ListenReopenTrigger::Remote, "remote"),
        (ListenReopenTrigger::Graceful, "graceful"),
    ] {
        assert_eq!(
            serde_json::to_value(trigger).unwrap(),
            serde_json::json!(expected)
        );
        assert_eq!(trigger.wire_str(), expected);
    }
}

#[test]
fn command_and_elicitation_payloads_round_trip() {
    let add = AddPayload {
        scope: Verified::assert_safe("project".to_string()),
        source: Verified::assert_safe("command".to_string()),
        server_type: Some(Verified::assert_safe("http".to_string())),
        transport: Some(Verified::assert_safe("http".to_string())),
        transport_explicit: Some(true),
        looks_like_url: Some(true),
        platform: None,
    };
    assert_eq!(
        serde_json::to_value(&add).unwrap(),
        serde_json::json!({
            "scope": "project",
            "source": "command",
            "type": "http",
            "transport": "http",
            "transportExplicit": true,
            "looksLikeUrl": true,
        })
    );

    let delete = DeletePayload {
        name: Verified::assert_safe("sample".to_string()),
        scope: Verified::assert_safe("user".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&delete).unwrap(),
        serde_json::json!({
            "name": "sample",
            "scope": "user",
        })
    );

    let get = GetPayload {
        name: Verified::assert_safe("sample".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&get).unwrap(),
        serde_json::json!({ "name": "sample" })
    );

    let inline = CommandInlinePayload {
        action: Verified::assert_safe("reconnect".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&inline).unwrap(),
        serde_json::json!({ "action": "reconnect" })
    );

    assert_eq!(ElicitationMode::Form.wire_str(), "form");
    assert_eq!(ElicitationMode::Url.wire_str(), "url");
    assert_eq!(
        serde_json::to_value(ElicitationMode::Url).unwrap(),
        serde_json::json!("url")
    );

    let shown = ElicitationShownPayload {
        mode: ElicitationMode::Form,
    };
    assert_eq!(
        serde_json::to_value(&shown).unwrap(),
        serde_json::json!({ "mode": "form" })
    );

    let response = ElicitationResponsePayload {
        mode: ElicitationMode::Url,
        action: Verified::assert_safe("accept".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::json!({
            "mode": "url",
            "action": "accept",
        })
    );
}

#[test]
fn mcp_runtime_payloads_round_trip() {
    let input_missing = InputMissingRequiredPayload {
        tool_name: Verified::assert_safe("mcp_tool".to_string()),
        is_mcp: true,
        tool_use_id: Some(Verified::assert_safe("toolu_123".to_string())),
        message_id: Verified::assert_safe("123e4567-e89b-12d3-a456-426614174000".to_string()),
        tool_input_size_bytes: 128,
        required_count: 3,
        missing_count: 1,
        present_key_count: 2,
        max_string_value_len: 64,
        has_pseudo_tag_debris: false,
        query_chain_id: Some(Verified::assert_safe("chain-1".to_string())),
        query_depth: Some(2),
        mcp_server_type: Some(Verified::assert_safe("http".to_string())),
        mcp_server_base_url: Some(Verified::assert_safe("https://example.com".to_string())),
        request_id: Some(Verified::assert_safe("req_123".to_string())),
    };
    assert_eq!(
        serde_json::to_value(&input_missing).unwrap(),
        serde_json::json!({
            "toolName": "mcp_tool",
            "isMcp": true,
            "toolUseID": "toolu_123",
            "messageID": "123e4567-e89b-12d3-a456-426614174000",
            "toolInputSizeBytes": 128,
            "requiredCount": 3,
            "missingCount": 1,
            "presentKeyCount": 2,
            "maxStringValueLen": 64,
            "hasPseudoTagDebris": false,
            "queryChainId": "chain-1",
            "queryDepth": 2,
            "mcpServerType": "http",
            "mcpServerBaseUrl": "https://example.com",
            "requestId": "req_123",
        })
    );

    let unavailable_ids = InputMissingRequiredPayload {
        tool_name: Verified::assert_safe("mcp_tool".to_string()),
        is_mcp: true,
        tool_use_id: None,
        message_id: Verified::assert_safe("123e4567-e89b-12d3-a456-426614174001".to_string()),
        tool_input_size_bytes: 2,
        required_count: 1,
        missing_count: 1,
        present_key_count: 0,
        max_string_value_len: 0,
        has_pseudo_tag_debris: false,
        query_chain_id: None,
        query_depth: None,
        mcp_server_type: None,
        mcp_server_base_url: None,
        request_id: None,
    };
    let unavailable_ids = serde_json::to_value(unavailable_ids).unwrap();
    assert!(unavailable_ids.get("toolUseID").is_none());
    assert_eq!(
        unavailable_ids.get("messageID"),
        Some(&serde_json::json!("123e4567-e89b-12d3-a456-426614174001"))
    );

    let large = LargeResultHandledPayload {
        outcome: Verified::assert_safe("persisted".to_string()),
        reason: Verified::assert_safe("file_saved".to_string()),
        size_estimate_tokens: 2048,
        persisted_size_chars: Some(8192),
        result_type: Some(Verified::assert_safe("content_array".to_string())),
        block_count: Some(4),
        persisted_as: Some(Verified::assert_safe("json".to_string())),
    };
    assert_eq!(
        serde_json::to_value(&large).unwrap(),
        serde_json::json!({
            "outcome": "persisted",
            "reason": "file_saved",
            "sizeEstimateTokens": 2048,
            "persistedSizeChars": 8192,
            "resultType": "content_array",
            "blockCount": 4,
            "persistedAs": "json",
        })
    );

    let pending = PendingCallPayload {
        requested_count: 5,
        connected_count: 2,
        cached_count: 1,
        failed_count: 0,
        pending_count: 1,
        needs_auth_count: 1,
        disabled_count: 0,
        unconfigured_count: 0,
        unknown_count: 0,
        wait_ms: 500,
        matched: true,
        match_type: Verified::assert_safe("wait".to_string()),
        success: true,
    };
    assert_eq!(
        serde_json::to_value(&pending).unwrap(),
        serde_json::json!({
            "requestedCount": 5,
            "connectedCount": 2,
            "cachedCount": 1,
            "failedCount": 0,
            "pendingCount": 1,
            "needsAuthCount": 1,
            "disabledCount": 0,
            "unconfiguredCount": 0,
            "unknownCount": 0,
            "waitMs": 500,
            "matched": true,
            "matchType": "wait",
            "success": true,
        })
    );

    let servers = ServersPayload {
        enterprise: 1,
        global: 2,
        project: 3,
        user: 4,
        plugin: 5,
        agent: 6,
        claudeai: 7,
    };
    assert_eq!(
        serde_json::to_value(&servers).unwrap(),
        serde_json::json!({
            "enterprise": 1,
            "global": 2,
            "project": 3,
            "user": 4,
            "plugin": 5,
            "agent": 6,
            "claudeai": 7,
        })
    );

    let turn_end = ToolResultEndedTurnPayload {
        query_chain_id: Verified::assert_safe("chain-1".to_string()),
        query_depth: 0,
        source: Verified::assert_safe("mcp_meta".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&turn_end).unwrap(),
        serde_json::json!({
            "queryChainId": "chain-1",
            "queryDepth": 0,
            "source": "mcp_meta",
        })
    );

    let loaded = ToolsCommandsLoadedPayload {
        tools_count: 12,
        commands_count: 3,
        commands_metadata_length: 144,
    };
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::json!({
            "tools_count": 12,
            "commands_count": 3,
            "commands_metadata_length": 144,
        })
    );

    let refreshed = ToolsRefreshedMidTurnPayload {
        old_mcp_count: 0,
        new_mcp_count: 2,
        recovered: true,
    };
    assert_eq!(
        serde_json::to_value(&refreshed).unwrap(),
        serde_json::json!({
            "oldMcpCount": 0,
            "newMcpCount": 2,
            "recovered": true,
        })
    );
}

#[test]
fn mcp_oauth_and_reconcile_payloads_round_trip() {
    let failure = OAuthFlowFailurePayload {
        auth_method: Verified::assert_safe("xaa".to_string()),
        xaa_failure_stage: Verified::assert_safe("discovery".to_string()),
        id_token_cache_hit: true,
    };
    assert_eq!(
        serde_json::to_value(&failure).unwrap(),
        serde_json::json!({
            "authMethod": "xaa",
            "xaaFailureStage": "discovery",
            "idTokenCacheHit": true,
        })
    );

    let xaa_success = OAuthXaaFlowSuccessPayload {
        auth_method: Verified::assert_safe("xaa".to_string()),
        id_token_cache_hit: false,
    };
    assert_eq!(
        serde_json::to_string(&xaa_success).unwrap(),
        r#"{"authMethod":"xaa","idTokenCacheHit":false}"#
    );

    let expired = SessionExpiredPayload {
        error_code: Some(Verified::assert_safe("404".to_string())),
        transport_type: Verified::assert_safe("http".to_string()),
        mcp_server_key_hash: Verified::assert_safe("0123456789ab".to_string()),
        mcp_server_base_url: Some(Verified::assert_safe("abcdef012345".to_string())),
    };
    assert_eq!(
        serde_json::to_value(&expired).unwrap(),
        serde_json::json!({
            "errorCode": "404",
            "transportType": "http",
            "mcpServerKeyHash": "0123456789ab",
            "mcpServerBaseUrl": "abcdef012345",
        })
    );
    assert_eq!(
        serde_json::to_string(&expired).unwrap(),
        r#"{"errorCode":"404","transportType":"http","mcpServerBaseUrl":"abcdef012345","mcpServerKeyHash":"0123456789ab"}"#
    );

    let paginated = ListPaginatedPayload {
        method: Verified::assert_safe("tools/list".to_string()),
        page_count: None,
        item_count: 25,
        outcome: Verified::assert_safe("complete".to_string()),
        source: Some(Verified::assert_safe("aggregate".to_string())),
    };
    assert_eq!(
        serde_json::to_value(&paginated).unwrap(),
        serde_json::json!({
            "method": "tools/list",
            "itemCount": 25,
            "outcome": "complete",
            "source": "aggregate",
        })
    );

    let reconcile = ReconcilePayload {
        caller: Verified::assert_safe("reload_plugins".to_string()),
        desired_count: 4,
        current_count: 3,
        to_remove_count: 1,
        to_add_count: 2,
        to_replace_count: 0,
        retained_plugin_count: 2,
    };
    assert_eq!(
        serde_json::to_value(&reconcile).unwrap(),
        serde_json::json!({
            "caller": "reload_plugins",
            "desiredCount": 4,
            "currentCount": 3,
            "toRemoveCount": 1,
            "toAddCount": 2,
            "toReplaceCount": 0,
            "retainedPluginCount": 2,
        })
    );
}

#[test]
fn degraded_normalized_payload_carries_only_normalized_count() {
    let payload = DegradedPayload {
        reason: DegradedReason::ToolSchemaNormalized,
        transport_type: Some(Verified::assert_safe("stdio".to_string())),
        normalized_count: Some(3),
        skipped_count: None,
        kept_count: None,
        mcp_server_name: Some(Verified::assert_safe("my-server".to_string())),
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "reason": "tool_schema_normalized",
            "transport_type": "stdio",
            "normalized_count": 3,
            "mcp_server_name": "my-server",
        })
    );
}

#[test]
fn degraded_schema_validator_unavailable_carries_no_per_server_fields() {
    let payload = DegradedPayload {
        reason: DegradedReason::SchemaValidatorUnavailable,
        transport_type: None,
        normalized_count: None,
        skipped_count: None,
        kept_count: None,
        mcp_server_name: None,
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({"reason": "schema_validator_unavailable"})
    );
}

#[test]
fn degraded_reason_values_are_snake_case() {
    for (variant, wire) in [
        (
            DegradedReason::ToolSchemaNormalized,
            "tool_schema_normalized",
        ),
        (DegradedReason::ToolsListFailed, "tools_list_failed"),
        (DegradedReason::ResourcesListFailed, "resources_list_failed"),
        (DegradedReason::PromptsListFailed, "prompts_list_failed"),
        (
            DegradedReason::ToolSchemaNormalizeGated,
            "tool_schema_normalize_gated",
        ),
        (
            DegradedReason::ToolSchemaUnsupported,
            "tool_schema_unsupported",
        ),
        (DegradedReason::ToolSchemaInvalid, "tool_schema_invalid"),
        (
            DegradedReason::ToolPropertyKeyInvalid,
            "tool_property_key_invalid",
        ),
        (
            DegradedReason::ToolSchemaInvalidGated,
            "tool_schema_invalid_gated",
        ),
        (
            DegradedReason::ToolPropertyKeyInvalidGated,
            "tool_property_key_invalid_gated",
        ),
        (
            DegradedReason::SchemaValidatorUnavailable,
            "schema_validator_unavailable",
        ),
    ] {
        assert_eq!(
            serde_json::to_value(variant).unwrap(),
            serde_json::json!(wire)
        );
        assert_eq!(
            variant.wire_str(),
            wire,
            "wire_str must match the serde rendering"
        );
    }
}

#[test]
fn degraded_payload_rejects_unknown_fields() {
    let json = serde_json::json!({
        "reason": "tool_schema_normalized",
        "transport_type": "stdio",
        "normalized_count": 1,
        "mcp_server_name": "s",
        "extra": true,
    });
    let result: Result<DegradedPayload, _> = serde_json::from_value(json);
    assert!(
        result.is_err(),
        "deny_unknown_fields must reject an unrecognized key"
    );
}

// ── Round-1 review regressions ──────────────────────────────────────────────

/// `mcpServerName` is `EA(ln(e.name),HT(e.name,e.config))` at the oracle:
/// `EA(n,e){return e?Vo(n):void 0}`, and `undefined` is DROPPED by the
/// object spread. For an ordinary user-configured server `HT` is false, so
/// the key must not be in the payload at all. Modelling it as required and
/// always-populated (the shipped shape before this fix) exfiltrated the
/// user's private server name as an analytics dimension on every connect.
#[test]
fn tools_listed_omits_the_server_name_when_the_first_party_gate_is_off() {
    let payload = ToolsListedPayload {
        transport_type: Verified::assert_safe("stdio".to_string()),
        list_duration_ms: 5,
        tool_count: 1,
        always_load_count: 0,
        discovery_source: Verified::assert_safe("live".to_string()),
        mcp_server_name: None,
    };
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "transport_type": "stdio",
            "list_duration_ms": 5,
            "tool_count": 1,
            "always_load_count": 0,
            "discovery_source": "live",
        }),
        "the mcp_server_name key must be ABSENT, not null/empty, when the gate is off"
    );
    // ...and the absence must be representable on the way back in, too:
    // `deny_unknown_fields` plus a required field would make the oracle's own
    // payload shape un-deserializable.
    let back: ToolsListedPayload = serde_json::from_value(json).expect("deserialize");
    assert!(back.mcp_server_name.is_none());
}

/// Oracle `HT` gates on FIRST-PARTY-ness only — every arm tests a built-in
/// Anthropic server, a `claudeai-proxy` transport, or an Anthropic URL. No
/// ordinary transport kind may open the gate.
#[test]
fn server_name_gate_is_closed_for_every_user_configurable_transport() {
    for kind in [
        "stdio",
        "sse",
        "http",
        "websocket",
        "inprocess",
        "sse-ide",
        "sdk-control",
    ] {
        assert!(
            !mcp::server_name_gate(kind),
            "{kind} is a user-configurable transport; the oracle's HT gate is false for it, \
             so the raw server name must never be attached"
        );
    }
    // The one `$M` arm this port can spell (`e === "claudeai-proxy"`).
    assert!(mcp::server_name_gate("claudeai-proxy"));
}

/// `connected_zero_tools` is the FIRST statement of the oracle's `yn`
/// (@182316780) — 20 lines above the seven tool-schema counters an earlier
/// revision transcribed verbatim while calling that set complete. It is the
/// single most common silent-MCP-failure signal.
#[test]
fn connected_zero_tools_is_a_modelled_degraded_reason() {
    let parsed: DegradedReason = serde_json::from_value(serde_json::json!("connected_zero_tools"))
        .expect("`connected_zero_tools` must be a modelled DegradedReason");
    assert_eq!(parsed, DegradedReason::ConnectedZeroTools);
    assert_eq!(parsed.wire_str(), "connected_zero_tools");
    assert_eq!(
        serde_json::to_value(parsed).unwrap(),
        serde_json::json!("connected_zero_tools")
    );
}

/// Oracle payload for that reason is `{reason,transportType,mcpServerName,..._}`
/// — no count field of any kind.
#[test]
fn connected_zero_tools_payload_carries_no_count_field() {
    let payload = DegradedPayload {
        reason: DegradedReason::ConnectedZeroTools,
        transport_type: Some(Verified::assert_safe("stdio".to_string())),
        normalized_count: None,
        skipped_count: None,
        kept_count: None,
        mcp_server_name: None,
    };
    assert_eq!(
        serde_json::to_value(&payload).expect("serialize"),
        serde_json::json!({ "reason": "connected_zero_tools", "transport_type": "stdio" })
    );
}

#[test]
fn list_rpc_failure_reasons_are_modelled_and_carry_no_counts() {
    for reason in [
        DegradedReason::ToolsListFailed,
        DegradedReason::ResourcesListFailed,
        DegradedReason::PromptsListFailed,
    ] {
        let payload = DegradedPayload {
            reason,
            transport_type: Some(Verified::assert_safe("stdio".to_string())),
            normalized_count: None,
            skipped_count: None,
            kept_count: None,
            mcp_server_name: None,
        };
        assert_eq!(
            serde_json::to_value(&payload).expect("serialize"),
            serde_json::json!({
                "reason": reason.wire_str(),
                "transport_type": "stdio",
            }),
            "{reason:?} must not populate any count field"
        );
    }
}
