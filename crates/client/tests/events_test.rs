//! F1-03 — live-turn event DTO round-trip tests.
//!
//! Freezes the per-turn streaming events the `client::adapter` emits (plan
//! F1-03). Each variant gets a serialize → assert-tag → deserialize → assert-eq
//! round-trip so the wire shape is locked before the F1-08 snapshot golden is
//! generated. The engine sources are noted per variant in `events.rs`.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate itself never depends on
//! `serde_json::Value` (governing decision §0.4): every tool payload here is a
//! JSON **String** (`input_json`/`result_json`).

use client::protocol::controls::{
    ControlDisabledReasonDto, ConversationControlsDto, PermissionControlStateDto,
    PermissionModeOptionDto, ReasoningControlSpecDto, ReasoningControlStateDto, ReasoningOptionDto,
    ReasoningSelectionDto,
};
use client::protocol::events::{ClientEvent, CostDto, TurnOutcomeDto};
use client::protocol::listings::SessionAgentSummaryDto;
use client::protocol::local_apps::{
    AppBridgeResponseDto, AppCapabilityKindDto, AppCapabilityRequestDto, AppCheckpointDto,
    AppCheckpointKindDto, AppDependencyChangeConfirmationRequestDto, AppDependencyChangeDto,
    AppDependencyChangeKindDto, AppErrorCodeDto, AppEventDto, AppRecordDto, AppRuntimeModeDto,
    AppRuntimeProfileDto, AppRuntimeProfileOptionDto, AppRuntimeProfilePackageDto,
    AppRuntimeRecoveryStateDto, AppRuntimeStateDto, AppRuntimeSuspensionReasonDto, AppSurfaceDto,
    AppUiActionKindDto, AppUiRequestDto, AppWorkflowStateDto, LocalAppCreateConfirmationRequestDto,
    LocalAppGateStatusDto, LocalAppMcpProposalApprovalRequestDto, LocalAppMcpToolChangeKindDto,
    LocalAppMcpToolDiffDto, LocalAppMcpToolFieldDto, LocalAppMcpToolSurfaceDto,
    LocalAppPluginErrorCodeDto, LocalAppRejectedCandidateDto, LocalAppTemplateSummaryDto,
    LocalAppVerificationStatusDto, LocalAppVerificationSummaryDto, ManagedLocalAppMcpServerDto,
    ManagedLocalAppMcpStatusDto, McpAppWidgetDto,
};
use client::protocol::message::{MessageBlockDto, MessageDto};
use client::protocol::permission::PermissionResolutionDto;
use client::protocol::tool_display::{ToolHeaderDto, ToolVerbDto};

#[test]
fn permission_request_resolution_is_authoritative_event() {
    let event = ClientEvent::PermissionRequestResolved {
        request_id: 9,
        resolution: PermissionResolutionDto::Expired,
    };
    let value = serde_json::to_value(event).expect("serialize permission resolution");
    assert_eq!(value["type"], "permission_request_resolved");
    assert_eq!(value["request_id"], 9);
    assert_eq!(value["resolution"], "expired");
}

#[test]
fn tool_header_icon_is_optional_for_older_wire_payloads() {
    let old = serde_json::json!({
        "verb": "read",
        "label": "Read",
        "title": "Read(file.txt)"
    });
    let header: ToolHeaderDto = serde_json::from_value(old).expect("old header remains readable");
    assert_eq!(header.verb, ToolVerbDto::Read);
    assert_eq!(header.icon, None);
}

#[test]
fn session_agent_events_round_trip() {
    let summary = SessionAgentSummaryDto {
        agent_id: "agent:00000000-0000-0000-0000-000000000001".into(),
        name: "researcher".into(),
        agent_type: "explorer".into(),
        model: Some("deepseek-flash".into()),
        model_profile: Some("deepseek".into()),
        status: "running".into(),
        latest_activity: Some("Reading protocol files".into()),
        updated_at_ms: Some(1_750_000_000_000),
    };
    let list = ClientEvent::SessionAgentList {
        session_id: "00000000-0000-0000-0000-000000000002".into(),
        agents: vec![summary.clone()],
    };
    let json = serde_json::to_value(&list).expect("serialize SessionAgentList");
    assert_eq!(json["type"], "session_agent_list");
    assert_eq!(serde_json::from_value::<ClientEvent>(json).unwrap(), list);

    let message = MessageDto {
        loop_wakeup: None,
        role: "assistant".into(),
        blocks: vec![MessageBlockDto::Text {
            text: "done".into(),
        }],
        images: Vec::new(),
    };
    for event in [
        ClientEvent::SessionAgentTranscript {
            session_id: "session".into(),
            agent_id: summary.agent_id.clone(),
            messages: vec![message.clone()],
            next_message_index: 1,
            revision: 1,
        },
        ClientEvent::SessionAgentUpdated {
            session_id: "session".into(),
            agent: summary.clone(),
        },
        ClientEvent::SessionAgentMessage {
            session_id: "session".into(),
            agent_id: summary.agent_id.clone(),
            message_index: 0,
            message: message.clone(),
        },
    ] {
        let json = serde_json::to_value(&event).expect("serialize session-agent event");
        let back: ClientEvent = serde_json::from_value(json).expect("deserialize event");
        assert_eq!(back, event);
    }
}

#[test]
fn conversation_controls_changed_round_trips() {
    let event = ClientEvent::ConversationControlsChanged {
        controls: ConversationControlsDto {
            qualified_model: "openai/gpt-5".into(),
            permission: PermissionControlStateDto {
                requested: "auto".into(),
                effective: "acceptEdits".into(),
                options: vec![
                    PermissionModeOptionDto {
                        mode: "acceptEdits".into(),
                        available: true,
                        disabled_reason: None,
                    },
                    PermissionModeOptionDto {
                        mode: "bypassPermissions".into(),
                        available: false,
                        disabled_reason: Some(ControlDisabledReasonDto {
                            code: "not_yet_available".into(),
                            message: Some("Mobile host does not expose bypass".into()),
                        }),
                    },
                ],
            },
            reasoning: ReasoningControlStateDto {
                requested: ReasoningSelectionDto::Automatic,
                effective: ReasoningSelectionDto::Level {
                    id: "medium".into(),
                },
                spec: ReasoningControlSpecDto {
                    options: vec![
                        ReasoningOptionDto {
                            selection: ReasoningSelectionDto::Automatic,
                            persistable: true,
                        },
                        ReasoningOptionDto {
                            selection: ReasoningSelectionDto::Level {
                                id: "medium".into(),
                            },
                            persistable: true,
                        },
                    ],
                    budget_range: None,
                    provider_default: ReasoningSelectionDto::Level {
                        id: "medium".into(),
                    },
                    forced_reasoning: false,
                    editable: true,
                    disabled_reason: None,
                },
            },
        },
    };
    let json = serde_json::to_value(&event).expect("serialize ConversationControlsChanged");
    assert_eq!(json["type"], "conversation_controls_changed");
    assert_eq!(json["controls"]["qualified_model"], "openai/gpt-5");
    assert_eq!(json["controls"]["permission"]["requested"], "auto");
    assert_eq!(
        json["controls"]["reasoning"]["requested"]["type"],
        "automatic"
    );
    let back: ClientEvent =
        serde_json::from_value(json).expect("deserialize ConversationControlsChanged");
    assert_eq!(back, event);
}

#[test]
fn fast_mode_changed_round_trips() {
    let event = ClientEvent::FastModeChanged { enabled: true };
    let json = serde_json::to_value(&event).expect("serialize FastModeChanged");
    assert_eq!(
        json,
        serde_json::json!({ "type": "fast_mode_changed", "enabled": true })
    );
    assert_eq!(serde_json::from_value::<ClientEvent>(json).unwrap(), event);
}

#[test]
fn ask_user_question_resolved_round_trips() {
    let ev = ClientEvent::AskUserQuestionResolved { request_id: 9 };
    let json = serde_json::to_value(&ev).expect("serialize AskUserQuestionResolved");
    assert_eq!(json["type"], "ask_user_question_resolved");
    assert_eq!(json["request_id"], 9);
    let back: ClientEvent =
        serde_json::from_value(json).expect("deserialize AskUserQuestionResolved");
    assert_eq!(back, ev);
}

#[test]
fn slash_command_result_round_trips() {
    let ev = ClientEvent::SlashCommandResult {
        turn_id: Some(9),
        display: "Switched model to opus".to_string(),
        is_error: false,
    };
    let json = serde_json::to_value(&ev).expect("serialize SlashCommandResult");
    assert_eq!(json["type"], "slash_command_result");
    assert_eq!(json["turn_id"], 9);
    assert_eq!(json["display"], "Switched model to opus");
    assert!(
        json.get("is_error").is_none(),
        "default false is_error must be skipped"
    );
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize SlashCommandResult");
    assert_eq!(back, ev);

    let minimal = ClientEvent::SlashCommandResult {
        turn_id: None,
        display: "Unknown command".to_string(),
        is_error: true,
    };
    let json = serde_json::to_value(&minimal).expect("serialize minimal SlashCommandResult");
    assert!(json.get("turn_id").is_none());
    assert_eq!(json["is_error"], true);
    let back: ClientEvent =
        serde_json::from_value(json).expect("deserialize minimal SlashCommandResult");
    assert_eq!(back, minimal);
}

/// `TextDelta` — 1:1 `OutputStream::emit_text`. Carries plain assistant text.
#[test]
fn text_delta_round_trips() {
    let ev = ClientEvent::TextDelta {
        text: "hello world".to_string(),
    };
    let json = serde_json::to_value(&ev).expect("serialize TextDelta");
    assert_eq!(json["type"], "text_delta");
    assert_eq!(json["text"], "hello world");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize TextDelta");
    assert_eq!(back, ev);
}

/// `ToolUseStarted` — 1:1 `emit_tool_call`; the `serde_json::Value` input is
/// lowered to a JSON **String** (`input_json`) per §0.4.
#[test]
fn tool_use_started_round_trips() {
    let ev = ClientEvent::ToolUseStarted {
        id: "tu_01".to_string(),
        tool: "Read".to_string(),
        input_json: r#"{"file_path":"/tmp/x"}"#.to_string(),
        header: None,
    };
    let json = serde_json::to_value(&ev).expect("serialize ToolUseStarted");
    assert_eq!(json["type"], "tool_use_started");
    assert_eq!(json["id"], "tu_01");
    assert_eq!(json["tool"], "Read");
    // The payload is a JSON String on the wire, NOT a nested object.
    assert!(
        json["input_json"].is_string(),
        "input_json must be a String"
    );
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize ToolUseStarted");
    assert_eq!(back, ev);
}

/// `ToolUseResult` — 1:1 `emit_tool_result`; fires in COMPLETION order (clients
/// key by id). `result_json` is a JSON String; `is_error` flags tool failure.
#[test]
fn tool_use_result_round_trips() {
    let ev = ClientEvent::ToolUseResult {
        id: "tu_01".to_string(),
        tool: "Read".to_string(),
        result_json: r#"{"content":"ok"}"#.to_string(),
        is_error: false,
        display: None,
    };
    let json = serde_json::to_value(&ev).expect("serialize ToolUseResult");
    assert_eq!(json["type"], "tool_use_result");
    assert_eq!(json["id"], "tu_01");
    assert_eq!(json["tool"], "Read");
    assert!(
        json["result_json"].is_string(),
        "result_json must be a String"
    );
    assert_eq!(json["is_error"], false);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize ToolUseResult");
    assert_eq!(back, ev);
}

/// `MessageComplete` — synthesized (no engine message-boundary event). Carries
/// an optional [`MessageDto`] reproducing the assistant message block set.
#[test]
fn message_complete_round_trips() {
    let ev = ClientEvent::MessageComplete {
        stop_reason: Some("end_turn".to_string()),
        message: Some(MessageDto {
            loop_wakeup: None,
            role: "assistant".to_string(),
            blocks: vec![
                MessageBlockDto::Text {
                    text: "done".to_string(),
                },
                MessageBlockDto::ToolUse {
                    id: "tu_01".to_string(),
                    tool: "Read".to_string(),
                    input_json: "{}".to_string(),
                    header: None,
                },
            ],
            images: Vec::new(),
        }),
    };
    let json = serde_json::to_value(&ev).expect("serialize MessageComplete");
    assert_eq!(json["type"], "message_complete");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["message"]["role"], "assistant");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize MessageComplete");
    assert_eq!(back, ev);
}

/// `MessageComplete` with both optional fields absent skips them from the wire
/// (the `skip_serializing_if = "Option::is_none"` forward-compat convention).
#[test]
fn message_complete_omits_none_fields() {
    let ev = ClientEvent::MessageComplete {
        stop_reason: None,
        message: None,
    };
    let json = serde_json::to_value(&ev).expect("serialize MessageComplete");
    assert_eq!(json["type"], "message_complete");
    assert!(
        json.get("stop_reason").is_none(),
        "None stop_reason must be skipped"
    );
    assert!(
        json.get("message").is_none(),
        "None message must be skipped"
    );
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize MessageComplete");
    assert_eq!(back, ev);
}

/// `TurnStarted` — adapter-synthesized on `SendPrompt` receipt (no engine
/// source). `turn_id` is optional.
#[test]
fn turn_started_round_trips() {
    let ev = ClientEvent::TurnStarted { turn_id: Some(7) };
    let json = serde_json::to_value(&ev).expect("serialize TurnStarted");
    assert_eq!(json["type"], "turn_started");
    assert_eq!(json["turn_id"], 7);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize TurnStarted");
    assert_eq!(back, ev);

    // None turn_id is skipped.
    let ev_none = ClientEvent::TurnStarted { turn_id: None };
    let json_none = serde_json::to_value(&ev_none).expect("serialize TurnStarted none");
    assert!(json_none.get("turn_id").is_none());
    let back_none: ClientEvent =
        serde_json::from_value(json_none).expect("deserialize TurnStarted none");
    assert_eq!(back_none, ev_none);
}

/// `TurnEnded` — 1:1 `emit_end_turn`. Carries the outcome, stop reason, and the
/// lowered [`CostDto`].
#[test]
fn turn_ended_round_trips() {
    let ev = ClientEvent::TurnEnded {
        outcome: TurnOutcomeDto::EndTurn,
        stop_reason: Some("end_turn".to_string()),
        cost: CostDto {
            total_usd: 0.0123,
            input_tokens: 100,
            output_tokens: 200,
            api_calls: 3,
            session_duration_secs: 42,
            formatted: "$0.0123".to_string(),
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize TurnEnded");
    assert_eq!(json["type"], "turn_ended");
    assert_eq!(json["outcome"]["type"], "end_turn");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["cost"]["input_tokens"], 100);
    assert_eq!(json["cost"]["session_duration_secs"], 42);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize TurnEnded");
    assert_eq!(back, ev);
}

/// `CostUpdate` — the cumulative cost snapshot lowered (`Duration` → secs).
#[test]
fn cost_update_round_trips() {
    let ev = ClientEvent::CostUpdate {
        total_usd: 1.5,
        input_tokens: 10,
        output_tokens: 20,
        api_calls: 2,
        session_duration_secs: 99,
        formatted: "$1.5000".to_string(),
    };
    let json = serde_json::to_value(&ev).expect("serialize CostUpdate");
    assert_eq!(json["type"], "cost_update");
    assert_eq!(json["total_usd"], 1.5);
    assert_eq!(json["api_calls"], 2);
    assert_eq!(json["session_duration_secs"], 99);
    assert_eq!(json["formatted"], "$1.5000");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize CostUpdate");
    assert_eq!(back, ev);
}

/// `CompactionCompleted` — 1:1 `emit_compaction_completed`.
#[test]
fn compaction_completed_round_trips() {
    let ev = ClientEvent::CompactionCompleted {
        messages_before: 50,
        messages_after: 12,
        bytes_saved: 4096,
        summary: "Summary:\nkept context".to_string(),
    };
    let json = serde_json::to_value(&ev).expect("serialize CompactionCompleted");
    assert_eq!(json["type"], "compaction_completed");
    assert_eq!(json["messages_before"], 50);
    assert_eq!(json["messages_after"], 12);
    assert_eq!(json["bytes_saved"], 4096);
    assert_eq!(json["summary"], "Summary:\nkept context");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize CompactionCompleted");
    assert_eq!(back, ev);
}

/// `ThinkingDelta` — now LIVE-FED (§0.7 follow-up): `event_router` emits it per
/// `HistoryContentDelta::ThinkingDelta` chunk. The wire shape is unchanged, so the
/// frozen round-trip still holds (the live stream carries `signature: None`).
#[test]
fn thinking_delta_round_trips() {
    let ev = ClientEvent::ThinkingDelta {
        thinking: "let me think".to_string(),
        signature: Some("sig".to_string()),
    };
    let json = serde_json::to_value(&ev).expect("serialize ThinkingDelta");
    assert_eq!(json["type"], "thinking_delta");
    assert_eq!(json["thinking"], "let me think");
    assert_eq!(json["signature"], "sig");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize ThinkingDelta");
    assert_eq!(back, ev);

    // signature is optional and skipped when None.
    let ev_no_sig = ClientEvent::ThinkingDelta {
        thinking: "x".to_string(),
        signature: None,
    };
    let json_no_sig = serde_json::to_value(&ev_no_sig).expect("serialize ThinkingDelta no-sig");
    assert!(json_no_sig.get("signature").is_none());
    let back_no_sig: ClientEvent =
        serde_json::from_value(json_no_sig).expect("deserialize ThinkingDelta no-sig");
    assert_eq!(back_no_sig, ev_no_sig);
}

/// `UsageUpdate` — now LIVE-FED (§0.7 follow-up): `event_router` emits it from
/// the `MessageStart` / `MessageDelta` usage fields. The wire shape is
/// unchanged, so the frozen round-trip still holds.
#[test]
fn usage_update_round_trips() {
    let ev = ClientEvent::UsageUpdate {
        is_snapshot: None,
        input_tokens: 11,
        output_tokens: 22,
        cache_read_tokens: 3,
        cache_creation_tokens: 4,
    };
    let json = serde_json::to_value(&ev).expect("serialize UsageUpdate");
    assert_eq!(json["type"], "usage_update");
    assert!(json.get("is_snapshot").is_none());
    assert_eq!(json["input_tokens"], 11);
    assert_eq!(json["output_tokens"], 22);
    assert_eq!(json["cache_read_tokens"], 3);
    assert_eq!(json["cache_creation_tokens"], 4);
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize UsageUpdate");
    assert_eq!(back, ev);
}

/// `AppsChanged` — carries the full local-app record set (bare-string
/// state enum; an absent `conversation_id` skipped).
#[test]
fn apps_changed_round_trips() {
    let ev = ClientEvent::AppsChanged {
        apps: vec![AppRecordDto {
            id: "habits-1a2b".to_string(),
            name: "Habits".to_string(),
            brief: "Track daily habits".to_string(),
            git_enabled: true,
            created_at_ms: 1_750_000_000_000,
            updated_at_ms: 1_750_000_000_001,
            workflow_state: AppWorkflowStateDto::Draft,
            conversation_id: None,
            init_session_id: None,
            workspace_rel: "apps/habits-1a2b/workspace".to_string(),
            scaffolded: true,
        }],
    };
    let json = serde_json::to_value(&ev).expect("serialize AppsChanged");
    assert_eq!(json["type"], "apps_changed");
    assert_eq!(json["apps"][0]["id"], "habits-1a2b");
    assert_eq!(json["apps"][0]["workflow_state"], "draft");
    assert!(
        json["apps"][0].get("conversation_id").is_none(),
        "None conversation_id must be skipped"
    );
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AppsChanged");
    assert_eq!(back, ev);
}

/// `AppWorkflowChanged` — bare-string state + an optional detail (skipped when
/// `None`).
#[test]
fn app_workflow_changed_round_trips() {
    let ev = ClientEvent::AppWorkflowChanged {
        app_id: "habits-1a2b".to_string(),
        state: AppWorkflowStateDto::PublishedUnverified,
        detail: Some("catalog promoted".to_string()),
    };
    let json = serde_json::to_value(&ev).expect("serialize AppWorkflowChanged");
    assert_eq!(json["type"], "app_workflow_changed");
    assert_eq!(json["state"], "published_unverified");
    assert_eq!(json["detail"], "catalog promoted");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AppWorkflowChanged");
    assert_eq!(back, ev);

    let ev_min = ClientEvent::AppWorkflowChanged {
        app_id: "habits-1a2b".to_string(),
        state: AppWorkflowStateDto::Draft,
        detail: None,
    };
    let json_min = serde_json::to_value(&ev_min).expect("serialize minimal AppWorkflowChanged");
    assert_eq!(json_min["state"], "draft");
    assert!(
        json_min.get("detail").is_none(),
        "None detail must be skipped"
    );
    let back_min: ClientEvent =
        serde_json::from_value(json_min).expect("deserialize minimal AppWorkflowChanged");
    assert_eq!(back_min, ev_min);
}

/// `AppRuntimeChanged` — bare-string runtime state + optional `last_error`.
#[test]
fn app_runtime_changed_round_trips() {
    let ev = ClientEvent::AppRuntimeChanged {
        app_id: "habits-1a2b".to_string(),
        state: AppRuntimeStateDto::Failed,
        details: None,
        last_error: Some("port already in use".to_string()),
    };
    let json = serde_json::to_value(&ev).expect("serialize AppRuntimeChanged");
    assert_eq!(json["type"], "app_runtime_changed");
    assert_eq!(json["state"], "failed");
    assert_eq!(json["last_error"], "port already in use");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AppRuntimeChanged");
    assert_eq!(back, ev);

    let ev_min = ClientEvent::AppRuntimeChanged {
        app_id: "habits-1a2b".to_string(),
        state: AppRuntimeStateDto::Stopped,
        details: None,
        last_error: None,
    };
    let json_min = serde_json::to_value(&ev_min).expect("serialize minimal AppRuntimeChanged");
    assert!(
        json_min.get("last_error").is_none(),
        "None last_error must be skipped"
    );
    let back_min: ClientEvent =
        serde_json::from_value(json_min).expect("deserialize minimal AppRuntimeChanged");
    assert_eq!(back_min, ev_min);
}

#[test]
#[allow(clippy::too_many_lines)] // a flat data table: one row per AppEventDto variant
fn extended_local_app_events_round_trip() {
    let events = vec![
        ClientEvent::AppEvent {
            event: AppEventDto::AppBridgeResponse {
                response: AppBridgeResponseDto {
                    request_id: "bridge-1".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    ok: true,
                    result_json: Some("[]".to_string()),
                    error: None,
                    error_code: None,
                },
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::AppUiRequest {
                request: AppUiRequestDto {
                    request_id: "ui-1".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    action: AppUiActionKindDto::Inspect,
                    target: None,
                    value: None,
                },
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::AppCapabilityRequested {
                request: AppCapabilityRequestDto {
                    request_id: "cap-1".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    capability: AppCapabilityKindDto::NetworkDomain,
                    domain: Some("api.example.com".to_string()),
                    reason: "Fetch app data".to_string(),
                },
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::AppCheckpointsChanged {
                app_id: "habits-1a2b".to_string(),
                checkpoints: vec![],
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::AppRecordChanged {
                record: AppRecordDto {
                    id: "habits-1a2b".to_string(),
                    name: "Habits".to_string(),
                    brief: "a habit tracker".to_string(),
                    git_enabled: true,
                    created_at_ms: 11,
                    updated_at_ms: 22,
                    workflow_state:
                        client::protocol::local_apps::AppWorkflowStateDto::PublishedUnverified,
                    conversation_id: Some("conv-9".to_string()),
                    init_session_id: Some("init-1".to_string()),
                    workspace_rel: "apps/habits-1a2b/workspace".to_string(),
                    scaffolded: true,
                },
            },
        },
    ];
    // Pin the WIRE TAG of every inner `AppEventDto` variant, mirroring
    // `commands_test::extended_local_app_commands_round_trip`. A symmetric
    // `to_value` → `from_value` round-trip alone renames a tag/field in BOTH
    // directions and still succeeds, so the literals below are what make a
    // rename visible here.
    let expected_types = [
        "app_bridge_response",
        "app_ui_request",
        "app_capability_requested",
        "app_checkpoints_changed",
        "app_record_changed",
    ];
    // One leaf field name per variant, so a renamed FIELD (not just a renamed
    // variant tag) is caught too.
    let expected_leaves: [(&str, serde_json::Value); 5] = [
        ("/event/response/result_json", serde_json::Value::from("[]")),
        ("/event/request/action", serde_json::Value::from("inspect")),
        (
            "/event/request/capability",
            serde_json::Value::from("network_domain"),
        ),
        ("/event/app_id", serde_json::Value::from("habits-1a2b")),
        (
            "/event/record/init_session_id",
            serde_json::Value::from("init-1"),
        ),
    ];
    assert_eq!(
        events.len(),
        expected_types.len(),
        "every extended app event must have a pinned wire tag"
    );
    for ((event, expected_type), (leaf_pointer, leaf_value)) in
        events.into_iter().zip(expected_types).zip(expected_leaves)
    {
        let json = serde_json::to_value(&event).expect("serialize extended app event");
        assert_eq!(json["type"], "app_event", "outer envelope tag");
        assert_eq!(
            json["event"]["type"], expected_type,
            "inner AppEventDto tag"
        );
        assert_eq!(
            json.pointer(leaf_pointer),
            Some(&leaf_value),
            "leaf {leaf_pointer} must keep its wire name and value"
        );
        let back: ClientEvent =
            serde_json::from_value(json).expect("deserialize extended app event");
        assert_eq!(back, event);
    }

    let runtime = ClientEvent::AppRuntimeChanged {
        app_id: "habits-1a2b".to_string(),
        state: AppRuntimeStateDto::Running,
        details: Some(client::protocol::local_apps::AppRuntimeDetailsDto {
            state: AppRuntimeStateDto::Running,
            mode: Some(AppRuntimeModeDto::NextProduction),
            loopback_url: Some("http://127.0.0.1:43123".to_string()),
            suspension_reason: Some(AppRuntimeSuspensionReasonDto::Backgrounded),
            recovery_state: Some(AppRuntimeRecoveryStateDto::Recovered),
            last_error: None,
        }),
        last_error: None,
    };
    let json = serde_json::to_value(&runtime).expect("serialize extended runtime");
    assert_eq!(json["details"]["mode"], "next_production");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize extended runtime");
    assert_eq!(back, runtime);
}

/// `AppCheckpointCreated` — carries the recorded checkpoint row.
#[test]
fn app_checkpoint_created_round_trips() {
    let ev = ClientEvent::AppCheckpointCreated {
        app_id: "habits-1a2b".to_string(),
        checkpoint: AppCheckpointDto {
            id: "ckpt-0001".to_string(),
            label: "Preview approved".to_string(),
            kind: AppCheckpointKindDto::PreviewApproved,
            created_at_ms: 1_750_000_000_000,
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize AppCheckpointCreated");
    assert_eq!(json["type"], "app_checkpoint_created");
    assert_eq!(json["checkpoint"]["id"], "ckpt-0001");
    assert_eq!(json["checkpoint"]["kind"], "preview_approved");
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AppCheckpointCreated");
    assert_eq!(back, ev);
}

/// `AppOperationFailed` — bare-string typed code; `app_id` is optional
/// (skipped when the failure addressed no specific app), and so is the
/// `request_id` echoed back from the command that failed.
#[test]
fn app_operation_failed_round_trips() {
    let ev = ClientEvent::AppOperationFailed {
        app_id: Some("habits-1a2b".to_string()),
        code: AppErrorCodeDto::NotYetAvailable,
        message: "app runtime lands in phase 4".to_string(),
        request_id: Some("req-7".to_string()),
    };
    let json = serde_json::to_value(&ev).expect("serialize AppOperationFailed");
    assert_eq!(json["type"], "app_operation_failed");
    assert_eq!(json["app_id"], "habits-1a2b");
    assert_eq!(json["code"], "not_yet_available");
    assert_eq!(json["message"], "app runtime lands in phase 4");
    assert_eq!(
        json["request_id"], "req-7",
        "the failing command's correlation key rides back verbatim"
    );
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize AppOperationFailed");
    assert_eq!(back, ev);

    let ev_global = ClientEvent::AppOperationFailed {
        app_id: None,
        code: AppErrorCodeDto::InvalidRequest,
        message: "bad app id".to_string(),
        request_id: None,
    };
    let json_g = serde_json::to_value(&ev_global).expect("serialize global AppOperationFailed");
    assert!(
        json_g.get("app_id").is_none(),
        "None app_id must be skipped"
    );
    assert!(
        json_g.get("request_id").is_none(),
        "None request_id must be skipped"
    );
    let back_g: ClientEvent =
        serde_json::from_value(json_g).expect("deserialize global AppOperationFailed");
    assert_eq!(back_g, ev_global);
}

#[test]
fn dependency_change_confirmation_round_trips_with_supply_chain_policy() {
    let ev = ClientEvent::AppEvent {
        event: AppEventDto::AppDependencyChangeConfirmationRequested {
            request: AppDependencyChangeConfirmationRequestDto {
                request_id: "dependency-request-1".to_string(),
                app_id: "habits-1a2b".to_string(),
                reason: "pre_resolution_no_network".to_string(),
                changes: vec![AppDependencyChangeDto {
                    kind: AppDependencyChangeKindDto::Add,
                    package: "dayjs".to_string(),
                    version: Some("1.11.13".to_string()),
                    cache_status: "unknown_until_resolution".to_string(),
                    download_status: "may_be_required".to_string(),
                }],
                license_risk: "unknown_until_resolution".to_string(),
                sbom_risk: "unknown_until_resolution".to_string(),
                lifecycle_scripts_blocked: true,
                native_addons_blocked: true,
                rollback_policy: "rollback_on_validation_failure".to_string(),
            },
        },
    };
    let json = serde_json::to_value(&ev).expect("serialize dependency confirmation");
    assert_eq!(
        json["event"]["type"],
        "app_dependency_change_confirmation_requested"
    );
    assert_eq!(json["event"]["request"]["changes"][0]["package"], "dayjs");
    assert_eq!(json["event"]["request"]["lifecycleScriptsBlocked"], true);
    assert_eq!(
        serde_json::from_value::<ClientEvent>(json).expect("deserialize dependency confirmation"),
        ev
    );
}

#[test]
fn phase8_local_app_events_round_trip_with_exact_nested_keys() {
    let tool = LocalAppMcpToolSurfaceDto {
        name: "save_habit".to_string(),
        title: Some("Track habits".to_string()),
        description: Some("Create or update one habit entry.".to_string()),
        input_schema_json:
            r#"{"type":"object","properties":{"date":{"type":"string"}},"required":["date"]}"#
                .to_string(),
        output_schema_json: None,
        annotations_json: Some(r#"{"readOnlyHint":false}"#.to_string()),
        execution_json: None,
        visible_meta_json: None,
        semantic_flow_json: r#"{"flowId":"local-app-save","source":"active"}"#.to_string(),
        permission_ceiling: "ask".to_string(),
    };
    let events = [
        ClientEvent::AppEvent {
            event: AppEventDto::CreateConfirmationRequested {
                request: LocalAppCreateConfirmationRequestDto {
                    request_id: "create-0001".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    name: "Habits".to_string(),
                    brief: "Track streaks and notes".to_string(),
                    selected_template: LocalAppTemplateSummaryDto {
                        template_id: "react-dom-r1".to_string(),
                        surface: AppSurfaceDto::Dom,
                        summary: "Best for forms and lists".to_string(),
                    },
                    runtime_profile: AppRuntimeProfileOptionDto {
                        family: AppRuntimeProfileDto::ReactDom,
                        revision: 1,
                        contract_sha256: "8".repeat(64),
                        surface: AppSurfaceDto::Dom,
                        core_packages: vec![AppRuntimeProfilePackageDto {
                            name: "react".to_string(),
                            version: "19.0.0".to_string(),
                        }],
                        cache_status: "bundled".to_string(),
                        download_status: "bundled".to_string(),
                        available: true,
                        reason: None,
                    },
                    reason: "Compact list app.".to_string(),
                    rejected: vec![LocalAppRejectedCandidateDto {
                        template_id: "three-3d-r1".to_string(),
                        reason: "3D is unnecessary.".to_string(),
                    }],
                    initial_tools: vec![tool.clone()],
                    required_gates: vec![LocalAppGateStatusDto {
                        gate_id: "ui_runner".to_string(),
                        label: "UI runner available".to_string(),
                        status: LocalAppVerificationStatusDto::Pending,
                        available: true,
                        detail: None,
                    }],
                },
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::McpProposalApprovalRequested {
                request: LocalAppMcpProposalApprovalRequestDto {
                    request_id: "proposal-0001".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    workflow_run_id: "wf-0002".to_string(),
                    summary: "Remove summarize_habits".to_string(),
                    proposal_sha256: "3".repeat(64),
                    approval_contract_sha256: "4".repeat(64),
                    tool_surface_sha256: "5".repeat(64),
                    tool_diffs: vec![LocalAppMcpToolDiffDto {
                        kind: LocalAppMcpToolChangeKindDto::Removed,
                        name: "summarize_habits".to_string(),
                        before: Some(tool.clone()),
                        after: None,
                        changed_fields: vec![LocalAppMcpToolFieldDto::Description],
                    }],
                    required_flow_changes: vec!["Add a save step".to_string()],
                    excluded_capabilities: vec!["calendar".to_string()],
                    pending_gates: vec![],
                },
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::ManagedMcpInventoryChanged {
                servers: vec![ManagedLocalAppMcpServerDto {
                    server_name: "local_app_habits-1a2b".to_string(),
                    app_id: "habits-1a2b".to_string(),
                    app_name: "Habits".to_string(),
                    enabled: true,
                    status: ManagedLocalAppMcpStatusDto::Enabled,
                    settings_revision: 6,
                    enabled_tools: vec!["save_habit".to_string()],
                    pinned_to_current_conversation: true,
                    build_id: "build-0001".to_string(),
                    catalog_sha256: "6".repeat(64),
                    tool_surface_sha256: "7".repeat(64),
                    tool_count: 1,
                    authoring_revision: 2,
                    publication_state: AppWorkflowStateDto::PublishedUnverified,
                    mcp_verification: LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Passed,
                        summary: "MCP verification passed.".to_string(),
                        code: None,
                    },
                    ui_verification: LocalAppVerificationSummaryDto {
                        status: LocalAppVerificationStatusDto::Unavailable,
                        summary: "UI runner unavailable.".to_string(),
                        code: Some("verification_unavailable".to_string()),
                    },
                    widget: Some(McpAppWidgetDto {
                        resource_uri:
                            "ui://local-app/habits-1a2b/8888888888888888888888888888888888888888888888888888888888888888/mcp-app.html"
                                .to_string(),
                        mime_type: "text/html;profile=mcp-app".to_string(),
                        resource_sha256: "8".repeat(64),
                    }),
                    tools: vec![tool.clone()],
                }],
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::VerificationSummaryChanged {
                app_id: "habits-1a2b".to_string(),
                publication_state: AppWorkflowStateDto::PublishedVerified,
                mcp_verification: LocalAppVerificationSummaryDto {
                    status: LocalAppVerificationStatusDto::Passed,
                    summary: "MCP verification passed.".to_string(),
                    code: None,
                },
                ui_verification: LocalAppVerificationSummaryDto {
                    status: LocalAppVerificationStatusDto::Unverified,
                    summary: "UI verification not run.".to_string(),
                    code: None,
                },
            },
        },
        ClientEvent::AppEvent {
            event: AppEventDto::LocalAppOperationFailed {
                app_id: None,
                code: LocalAppPluginErrorCodeDto::PluginDisabled,
                message: "Enable the Local App plugin and retry once.".to_string(),
                request_id: Some("plugin-read-1".to_string()),
            },
        },
    ];

    let expected = [
        (
            "create_confirmation_requested",
            // Was `/event/request/receipt/superseded` until
            // r1-backlog-native-confirmation-13 removed the field. Re-cut onto
            // `requiredGates`, which is the record's LAST field now, so this
            // pointer keeps probing the deepest nested leaf the way it did.
            "/event/request/requiredGates/0/gateId",
            serde_json::Value::String("ui_runner".to_string()),
        ),
        (
            "mcp_proposal_approval_requested",
            "/event/request/toolDiffs/0/kind",
            serde_json::Value::String("removed".to_string()),
        ),
        (
            "managed_mcp_inventory_changed",
            "/event/servers/0/status",
            serde_json::Value::String("enabled".to_string()),
        ),
        (
            "verification_summary_changed",
            "/event/ui_verification/status",
            serde_json::Value::String("unverified".to_string()),
        ),
        (
            "local_app_operation_failed",
            "/event/code",
            serde_json::Value::String("plugin_disabled".to_string()),
        ),
    ];

    for (event, (tag, pointer, leaf)) in events.into_iter().zip(expected) {
        let json = serde_json::to_value(&event).expect("serialize phase8 local app event");
        assert_eq!(json["type"], "app_event");
        assert_eq!(json["event"]["type"], tag);
        assert_eq!(json.pointer(pointer), Some(&leaf));
        let back: ClientEvent =
            serde_json::from_value(json).expect("deserialize phase8 local app event");
        assert_eq!(back, event);
    }
}

/// Enumerate every `TurnOutcomeDto` variant and assert the `snake_case` wire
/// tags plus a byte-stable round-trip (`EndTurn | MaxTurns | Cancelled`).
#[test]
fn end_turn_outcome_variants() {
    let cases = [
        (TurnOutcomeDto::EndTurn, "end_turn"),
        (TurnOutcomeDto::MaxTurns, "max_turns"),
        (TurnOutcomeDto::Cancelled, "cancelled"),
    ];
    for (outcome, tag) in cases {
        let json = serde_json::to_value(&outcome).expect("serialize TurnOutcomeDto");
        assert_eq!(
            json["type"], tag,
            "TurnOutcomeDto::{outcome:?} tag mismatch"
        );
        let back: TurnOutcomeDto =
            serde_json::from_value(json).expect("deserialize TurnOutcomeDto");
        assert_eq!(back, outcome);
    }
}

/// An engine request carries a stable resource owner and a local timeout budget.
#[test]
fn audio_request_round_trips_on_the_wire() {
    use client::protocol::audio::{
        AudioOperationDto, AudioOperationIdDto, AudioOperationRequestDto, AudioOwnerDto,
    };
    let request = ClientEvent::AudioRequest {
        request: AudioOperationRequestDto {
            identity: AudioOperationIdDto {
                id: "00000000-0000-4000-8000-000000000007".into(),
                generation: 2,
                service_epoch: 3,
            },
            owner: AudioOwnerDto::Session {
                session_id: "session-1".into(),
            },
            initiator: None,
            timeout_budget_ms: Some(1_000),
            max_payload_bytes: 12_533_760,
            operation: AudioOperationDto::Listen {
                language: Some("zh-CN".into()),
            },
        },
    };
    let json = serde_json::to_value(&request).expect("serialize AudioRequest");
    assert_eq!(json["type"], "audio_request");
    assert_eq!(json["request"]["identity"]["service_epoch"], 3);
    assert_eq!(json["request"]["owner"]["session_id"], "session-1");
    assert_eq!(json["request"]["operation"]["type"], "listen");
    assert_eq!(json["request"]["operation"]["language"], "zh-CN");
    assert_eq!(
        serde_json::from_value::<ClientEvent>(json).unwrap(),
        request
    );
}

/// Progress is independent of successful compaction counts and survives idle commands.
#[test]
fn compaction_status_round_trips_with_optional_error() {
    for (phase, error) in [
        ("preparing", None),
        ("summarizing", None),
        ("restoring", None),
        ("complete", None),
        ("error", Some("summary failed")),
        ("cancelled", Some("Compaction canceled.")),
    ] {
        let event = ClientEvent::CompactionStatus {
            phase: phase.into(),
            error: error.map(str::to_string),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "compaction_status");
        assert_eq!(json["phase"], phase);
        assert_eq!(json.get("error").and_then(serde_json::Value::as_str), error);
        let decoded: ClientEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event);
    }
}

#[test]
fn restored_zero_usage_snapshot_round_trips() {
    let ev = ClientEvent::UsageUpdate {
        is_snapshot: Some(true),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["is_snapshot"], true);
    assert_eq!(serde_json::from_value::<ClientEvent>(json).unwrap(), ev);
}
