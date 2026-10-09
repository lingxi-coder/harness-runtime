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
use client::protocol::listings::{SessionAgentMessageRowDto, SessionAgentSummaryDto};
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
    let row = SessionAgentMessageRowDto {
        message_index: 0,
        message_uuid: "00000000-0000-0000-0000-000000000003".into(),
        message,
        api_error_json: None,
    };
    for event in [
        ClientEvent::SessionAgentTranscript {
            session_id: "session".into(),
            agent_id: summary.agent_id.clone(),
            messages: vec![row.clone()],
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
            row,
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
