//! F1-06 — `ClientCommand` full-set round-trip + session-lifecycle lock tests.
//!
//! Freezes every inbound command and ENCODES governing decision §0.5:
//! `session_id` is a CONNECTION ATTRIBUTE (carried in `SessionStarted` /
//! `SessionResumed`), NOT a general per-live-command param. `ResumeSession`
//! names a target; mobile durable-turn commands fence an expected owner without
//! switching it.
//!
//! Each command gets a serialize → assert-tag → deserialize → assert-eq
//! round-trip so the wire shape is locked before the F1-08 snapshot golden is
//! generated. The conventions inherited from the F1-01 shell (decision §0.1):
//! internally tagged on `type`, `snake_case`; `#[non_exhaustive]`; every
//! optional field `#[serde(default, skip_serializing_if = "Option::is_none")]`.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate itself never depends on
//! `serde_json::Value` (decision §0.4): image bytes are base64 Strings, tool
//! input lives in JSON Strings elsewhere.

use client::protocol::commands::{
    ClientCommand, CommandResultDto, ImageRefDto, ListingKindDto, PromptModeDto,
    ProviderCredentialSecretDto, UiSurfaceDto,
};
use client::protocol::controls::ReasoningSelectionDto;
use client::protocol::listings::TaskStatusDto;
use client::protocol::permission::PermissionResponseDto;
use client::protocol::text::ClientTextDto;

/// `SendPrompt` — the core inbound command. Carries the text, an optional
/// prompt mode, inline image bytes (decision §0.8), and an optional client
/// turn correlator and a captured mobile session owner.
#[test]
fn send_prompt_round_trips() {
    let cmd = ClientCommand::SendPrompt {
        session_id: Some("session-a".into()),
        text: ClientTextDto::from_utf8("summarize this repo"),
        prompt_mode: Some(PromptModeDto::Plan),
        images: vec![ImageRefDto {
            media_type: "image/png".to_string(),
            base64: "iVBORw0KGgo=".to_string(),
        }],
        turn_id: Some(7),
        visualization_context: None,
    };
    let json = serde_json::to_value(&cmd).expect("serialize SendPrompt");
    assert_eq!(json["type"], "send_prompt");
    assert_eq!(json["session_id"], "session-a");
    assert_eq!(json["text"], "summarize this repo");
    assert_eq!(json["prompt_mode"]["type"], "plan");
    assert_eq!(json["images"][0]["media_type"], "image/png");
    assert_eq!(json["images"][0]["base64"], "iVBORw0KGgo=");
    assert_eq!(json["turn_id"], 7);
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize SendPrompt");
    assert_eq!(back, cmd);
}

/// `SendPrompt` optional fields are skipped when absent, and an `images: []`
/// is preserved (a non-`Option` Vec, always present).
#[test]
fn send_prompt_optional_fields_skip_when_none() {
    let cmd = ClientCommand::SendPrompt {
        session_id: None,
        text: ClientTextDto::from_utf8("hi"),
        prompt_mode: None,
        images: vec![],
        turn_id: None,
        visualization_context: None,
    };
    let json = serde_json::to_value(&cmd).expect("serialize minimal SendPrompt");
    assert!(json.get("session_id").is_none());
    assert!(
        json.get("prompt_mode").is_none(),
        "None prompt_mode must be skipped"
    );
    assert!(
        json.get("turn_id").is_none(),
        "None turn_id must be skipped"
    );
    assert!(
        json["images"].is_array(),
        "images Vec is always present (not optional)"
    );
    // A frame omitting the optionals entirely deserializes with defaults.
    let from_minimal: ClientCommand =
        serde_json::from_str(r#"{"type":"send_prompt","text":"hi","images":[]}"#)
            .expect("deserialize minimal SendPrompt");
    assert_eq!(from_minimal, cmd);
}

/// `PromptModeDto` — every prompt-input mode round-trips with its `snake_case`
/// wire tag (`Normal | Bash | Memory | Plan`).
#[test]
fn prompt_mode_variants_round_trip() {
    let cases = [
        (PromptModeDto::Normal, "normal"),
        (PromptModeDto::Bash, "bash"),
        (PromptModeDto::Memory, "memory"),
        (PromptModeDto::Plan, "plan"),
    ];
    for (mode, tag) in cases {
        let json = serde_json::to_value(mode).expect("serialize PromptModeDto");
        assert_eq!(json["type"], tag, "PromptModeDto::{mode:?} tag mismatch");
        let back: PromptModeDto = serde_json::from_value(json).expect("deserialize PromptModeDto");
        assert_eq!(back, mode);
    }
}

/// `ImageRefDto` — uniform inline `{media_type, base64}` (no transport-leaking
/// enum, decision §0.8).
#[test]
fn image_ref_round_trips() {
    let img = ImageRefDto {
        media_type: "image/jpeg".to_string(),
        base64: "/9j/4AAQ=".to_string(),
    };
    let json = serde_json::to_value(&img).expect("serialize ImageRefDto");
    assert_eq!(json["media_type"], "image/jpeg");
    assert_eq!(json["base64"], "/9j/4AAQ=");
    let back: ImageRefDto = serde_json::from_value(json).expect("deserialize ImageRefDto");
    assert_eq!(back, img);
}

/// `Cancel` preserves both owner-scoped and connection-scoped forms.
#[test]
fn cancel_round_trips() {
    for (session_id, turn_id) in [
        (Some("session-a".to_string()), Some(3u64)),
        (Some("session-a".to_string()), None),
        (None, Some(3u64)),
        (None, None),
    ] {
        let cmd = ClientCommand::Cancel {
            session_id: session_id.clone(),
            turn_id,
        };
        let json = serde_json::to_value(&cmd).expect("serialize Cancel");
        assert_eq!(json["type"], "cancel");
        if let Some(owner) = session_id {
            assert_eq!(json["session_id"], owner);
        } else {
            assert!(json.get("session_id").is_none());
        }
        if turn_id.is_none() {
            assert!(
                json.get("turn_id").is_none(),
                "None turn_id must be skipped"
            );
        }
        let back: ClientCommand = serde_json::from_value(json).expect("deserialize Cancel");
        assert_eq!(back, cmd);
    }
}

/// `ApprovePermission` — correlated by `request_id`, carries the response.
#[test]
fn approve_permission_round_trips() {
    let cmd = ClientCommand::ApprovePermission {
        request_id: 42,
        response: PermissionResponseDto::AllowAlways,
    };
    let json = serde_json::to_value(&cmd).expect("serialize ApprovePermission");
    assert_eq!(json["type"], "approve_permission");
    assert_eq!(json["request_id"], 42);
    assert_eq!(json["response"]["type"], "allow_always");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize ApprovePermission");
    assert_eq!(back, cmd);
}

/// `DenyPermission` — correlated by `request_id` only.
#[test]
fn deny_permission_round_trips() {
    let cmd = ClientCommand::DenyPermission { request_id: 7 };
    let json = serde_json::to_value(&cmd).expect("serialize DenyPermission");
    assert_eq!(json["type"], "deny_permission");
    assert_eq!(json["request_id"], 7);
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize DenyPermission");
    assert_eq!(back, cmd);
}

/// `SetPermissionMode` — names the live mode to apply.
#[test]
fn set_permission_mode_round_trips() {
    let cmd = ClientCommand::SetPermissionMode {
        mode: "acceptEdits".to_string(),
    };
    let json = serde_json::to_value(&cmd).expect("serialize SetPermissionMode");
    assert_eq!(json["type"], "set_permission_mode");
    assert_eq!(json["mode"], "acceptEdits");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize SetPermissionMode");
    assert_eq!(back, cmd);
}

#[test]
fn native_proactivity_commands_keep_initialize_opt_in_separate_from_level_changes() {
    let initialize = ClientCommand::InitializeProactivity {
        proactivity: Some(true),
        reinitialize: None,
    };
    let initialize_json = serde_json::to_value(&initialize).expect("serialize initialize opt-in");
    assert_eq!(initialize_json["type"], "initialize_proactivity");
    assert_eq!(initialize_json["proactivity"], true);
    assert!(initialize_json.get("reinitialize").is_none());
    assert_eq!(
        serde_json::from_value::<ClientCommand>(initialize_json).unwrap(),
        initialize
    );

    let level = ClientCommand::SetProactivityLevel {
        level: "proactive".into(),
        offer_bypass: Some(false),
    };
    let level_json = serde_json::to_value(&level).expect("serialize proactivity level");
    assert_eq!(level_json["type"], "set_proactivity_level");
    assert_eq!(level_json["level"], "proactive");
    assert_eq!(level_json["offer_bypass"], false);
    assert_eq!(
        serde_json::from_value::<ClientCommand>(level_json).unwrap(),
        level
    );
}

#[test]
fn provider_credential_command_round_trips_without_debug_leak() {
    let secret = "sk-provider-super-secret";
    let command = ClientCommand::SetProviderCredential {
        operation_id: 9,
        provider_id: "deepseek".to_string(),
        credential: ProviderCredentialSecretDto::new(secret.to_string()),
    };

    let debug = format!("{command:?}");
    assert!(
        !debug.contains(secret),
        "credential leaked through Debug: {debug}"
    );
    assert!(debug.contains("<redacted>"));

    let json = serde_json::to_value(&command).expect("serialize credential command");
    assert_eq!(json["type"], "set_provider_credential");
    assert_eq!(json["credential"], secret);
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize credential command");
    assert_eq!(back, command);
}

/// `SetModel` — keeps exact model-selection UTF-16 while preserving JSON text.
#[test]
fn set_model_round_trips() {
    let model =
        ClientTextDto::from_code_units("claude-opus-4-8".encode_utf16().chain([0xD800]).collect());
    let cmd = ClientCommand::SetModel {
        model: model.clone(),
    };
    let json = serde_json::to_string(&cmd).expect("serialize SetModel");
    assert!(json.contains("claude-opus-4-8\\ud800"));
    let back: ClientCommand = serde_json::from_str(&json).expect("deserialize SetModel");
    assert_eq!(back, cmd);
    let ClientCommand::SetModel {
        model: round_tripped,
    } = back
    else {
        panic!("SetModel tag must round-trip")
    };
    assert_eq!(round_tripped.code_units, model.code_units);
}

/// `ListModels` — a unit-style pull command.
#[test]
fn list_models_round_trips() {
    let cmd = ClientCommand::ListModels;
    let json = serde_json::to_value(&cmd).expect("serialize ListModels");
    assert_eq!(json["type"], "list_models");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize ListModels");
    assert_eq!(back, cmd);
}

#[test]
fn conversation_controls_commands_round_trip() {
    let get = ClientCommand::GetConversationControls;
    let json = serde_json::to_value(&get).expect("serialize GetConversationControls");
    assert_eq!(json["type"], "get_conversation_controls");
    assert_eq!(serde_json::from_value::<ClientCommand>(json).unwrap(), get);

    let set = ClientCommand::SetReasoningSelection {
        selection: ReasoningSelectionDto::Level {
            id: "high".to_string(),
        },
    };
    let json = serde_json::to_value(&set).expect("serialize SetReasoningSelection");
    assert_eq!(json["type"], "set_reasoning_selection");
    assert_eq!(json["selection"]["type"], "level");
    assert_eq!(json["selection"]["id"], "high");
    assert_eq!(serde_json::from_value::<ClientCommand>(json).unwrap(), set);
}

#[test]
fn session_agent_commands_round_trip() {
    let list = ClientCommand::ListSessionAgents;
    let json = serde_json::to_value(&list).expect("serialize ListSessionAgents");
    assert_eq!(json["type"], "list_session_agents");
    assert_eq!(serde_json::from_value::<ClientCommand>(json).unwrap(), list);

    let load = ClientCommand::LoadSessionAgentTranscript {
        agent_id: "agent:00000000-0000-0000-0000-000000000001".into(),
    };
    let json = serde_json::to_value(&load).expect("serialize LoadSessionAgentTranscript");
    assert_eq!(json["type"], "load_session_agent_transcript");
    assert_eq!(
        json["agent_id"],
        "agent:00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(serde_json::from_value::<ClientCommand>(json).unwrap(), load);
}

/// `RunSlashCommand` optionally carries a client-owned `turn_id` so prompt-like
/// slash commands can reuse the ordinary turn lifecycle.
#[test]
fn run_slash_command_round_trips() {
    let cmd = ClientCommand::RunSlashCommand {
        session_id: Some("session-a".into()),
        raw: ClientTextDto::from_utf8("/model opus"),
        images: vec![ImageRefDto {
            media_type: "image/png".to_string(),
            base64: "aGVsbG8=".to_string(),
        }],
        turn_id: Some(7),
    };
    let json = serde_json::to_value(&cmd).expect("serialize RunSlashCommand");
    assert_eq!(json["type"], "run_slash_command");
    assert_eq!(json["session_id"], "session-a");
    assert_eq!(json["raw"], "/model opus");
    assert_eq!(json["images"][0]["media_type"], "image/png");
    assert_eq!(json["images"][0]["base64"], "aGVsbG8=");
    assert_eq!(json["turn_id"], 7);
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize RunSlashCommand");
    assert_eq!(back, cmd);

    let without_turn = ClientCommand::RunSlashCommand {
        session_id: None,
        raw: ClientTextDto::from_utf8("/help"),
        images: Vec::new(),
        turn_id: None,
    };
    let json = serde_json::to_value(&without_turn).expect("serialize RunSlashCommand");
    assert!(json.get("session_id").is_none());
    assert!(
        json.get("turn_id").is_none(),
        "None turn_id must be skipped"
    );
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize RunSlashCommand");
    assert_eq!(back, without_turn);
}

/// `CommandResultDto` — the reply for `RunSlashCommand` (display text + an
/// optional injected prompt). Round-trips; the optional `injected` is skipped
/// when absent.
#[test]
fn command_result_round_trips() {
    let result = CommandResultDto {
        display: "Switched model to opus".to_string(),
        injected: None,
    };
    let json = serde_json::to_value(&result).expect("serialize CommandResultDto");
    assert_eq!(json["display"], "Switched model to opus");
    assert!(
        json.get("injected").is_none(),
        "None injected must be skipped"
    );
    let back: CommandResultDto =
        serde_json::from_value(json).expect("deserialize CommandResultDto");
    assert_eq!(back, result);

    let with_injection = CommandResultDto {
        display: "Loaded prompt".to_string(),
        injected: Some("Explain the architecture".to_string()),
    };
    let json2 = serde_json::to_value(&with_injection).expect("serialize CommandResultDto injected");
    assert_eq!(json2["injected"], "Explain the architecture");
    let back2: CommandResultDto =
        serde_json::from_value(json2).expect("deserialize CommandResultDto injected");
    assert_eq!(back2, with_injection);
}

/// `RefreshListings` — pulls a set of screen listings by kind.
#[test]
fn refresh_listings_round_trips() {
    let cmd = ClientCommand::RefreshListings {
        which: vec![
            ListingKindDto::Mcp,
            ListingKindDto::Agents,
            ListingKindDto::Sessions,
        ],
    };
    let json = serde_json::to_value(&cmd).expect("serialize RefreshListings");
    assert_eq!(json["type"], "refresh_listings");
    assert_eq!(json["which"][0]["type"], "mcp");
    assert_eq!(json["which"][1]["type"], "agents");
    assert_eq!(json["which"][2]["type"], "sessions");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize RefreshListings");
    assert_eq!(back, cmd);
}

/// `ListingKindDto` — every screen kind round-trips with its `snake_case` tag.
#[test]
fn listing_kind_variants_round_trip() {
    let cases = [
        (ListingKindDto::Sessions, "sessions"),
        (ListingKindDto::Models, "models"),
        (ListingKindDto::Mcp, "mcp"),
        (ListingKindDto::Skills, "skills"),
        (ListingKindDto::Hooks, "hooks"),
        (ListingKindDto::Agents, "agents"),
        (ListingKindDto::SlashCommands, "slash_commands"),
        (ListingKindDto::Memory, "memory"),
        (ListingKindDto::Status, "status"),
        (ListingKindDto::Settings, "settings"),
        (ListingKindDto::Auth, "auth"),
        (ListingKindDto::Doctor, "doctor"),
        (ListingKindDto::Tasks, "tasks"),
        (ListingKindDto::Coordinator, "coordinator"),
    ];
    for (kind, tag) in cases {
        let json = serde_json::to_value(kind).expect("serialize ListingKindDto");
        assert_eq!(json["type"], tag, "ListingKindDto::{kind:?} tag mismatch");
        let back: ListingKindDto =
            serde_json::from_value(json).expect("deserialize ListingKindDto");
        assert_eq!(back, kind);
    }
}

/// `ListingKindDto::Coordinator` — the per-worker roster pull kind (T18). A new
/// variant on the `#[non_exhaustive]` enum is additive (no major bump).
#[test]
fn listing_kind_coordinator_roundtrip() {
    let kind = ListingKindDto::Coordinator;
    let json = serde_json::to_value(kind).expect("serialize ListingKindDto::Coordinator");
    assert_eq!(json["type"], "coordinator");
    let back: ListingKindDto =
        serde_json::from_value(json).expect("deserialize ListingKindDto::Coordinator");
    assert_eq!(back, kind);
}

/// `NewSession` — starts a fresh session. Carries optional `cwd`/`model`, but
/// NO `session_id` (the new id is reported back via `SessionStarted`, §0.5).
#[test]
fn new_session_round_trips() {
    let model = ClientTextDto::from_code_units(
        "claude-sonnet-4-6".encode_utf16().chain([0xD800]).collect(),
    );
    let cmd = ClientCommand::NewSession {
        cwd: Some("/home/dev/proj".to_string()),
        model: Some(model.clone()),
    };
    let json = serde_json::to_string(&cmd).expect("serialize NewSession");
    assert!(json.contains("\"type\":\"new_session\""));
    assert!(json.contains("\"cwd\":\"/home/dev/proj\""));
    assert!(json.contains("\"model\":\"claude-sonnet-4-6\\ud800\""));
    assert!(
        !json.contains("session_id"),
        "NewSession must NOT carry session_id (§0.5)"
    );
    let back: ClientCommand = serde_json::from_str(&json).expect("deserialize NewSession");
    assert_eq!(back, cmd);

    // Both optionals absent ⇒ skipped.
    let minimal = ClientCommand::NewSession {
        cwd: None,
        model: None,
    };
    let json_min = serde_json::to_value(&minimal).expect("serialize minimal NewSession");
    assert!(json_min.get("cwd").is_none(), "None cwd must be skipped");
    assert!(
        json_min.get("model").is_none(),
        "None model must be skipped"
    );
    let back_min: ClientCommand =
        serde_json::from_value(json_min).expect("deserialize minimal NewSession");
    assert_eq!(back_min, minimal);
}

/// `ResumeSession` names a target to resume. Carries an optional `cwd`.
#[test]
fn resume_session_round_trips_and_carries_session_id() {
    let cmd = ClientCommand::ResumeSession {
        session_id: "9f1c2e3a-...".to_string(),
        cwd: None,
    };
    let json = serde_json::to_value(&cmd).expect("serialize ResumeSession");
    assert_eq!(json["type"], "resume_session");
    assert_eq!(
        json["session_id"], "9f1c2e3a-...",
        "ResumeSession names the session target (§0.5)"
    );
    assert!(json.get("cwd").is_none(), "None cwd must be skipped");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize ResumeSession");
    assert_eq!(back, cmd);
}

/// `ListSessions` — carries an optional row limit.
#[test]
fn list_sessions_round_trips() {
    for limit in [Some(20u32), None] {
        let cmd = ClientCommand::ListSessions { limit };
        let json = serde_json::to_value(&cmd).expect("serialize ListSessions");
        assert_eq!(json["type"], "list_sessions");
        if limit.is_none() {
            assert!(json.get("limit").is_none(), "None limit must be skipped");
        }
        let back: ClientCommand = serde_json::from_value(json).expect("deserialize ListSessions");
        assert_eq!(back, cmd);
    }
}

/// The unit-style session/control commands all round-trip with their
/// `snake_case` tags and carry no payload (hence no `session_id`).
#[test]
fn unit_control_commands_round_trip() {
    let cases = [
        (ClientCommand::Login, "login"),
        (ClientCommand::Logout, "logout"),
        (ClientCommand::ForceCompact, "force_compact"),
        (ClientCommand::ClearSession, "clear_session"),
        (ClientCommand::RequestExit, "request_exit"),
    ];
    for (cmd, tag) in cases {
        let json = serde_json::to_value(&cmd).expect("serialize unit command");
        assert_eq!(json["type"], tag, "{cmd:?} tag mismatch");
        let back: ClientCommand = serde_json::from_value(json).expect("deserialize unit command");
        assert_eq!(back, cmd);
    }
}

#[test]
fn ui_surface_lifecycle_commands_round_trip() {
    let commands = [
        (
            ClientCommand::UiAttach {
                surface: UiSurfaceDto::Desktop,
                client_id: "electron-window-1".into(),
            },
            "ui_attach",
        ),
        (
            ClientCommand::UiDetach {
                client_id: "electron-window-1".into(),
            },
            "ui_detach",
        ),
    ];
    for (command, tag) in commands {
        let json = serde_json::to_value(&command).expect("serialize UI lifecycle command");
        assert_eq!(json["type"], tag);
        assert_eq!(json["client_id"], "electron-window-1");
        if tag == "ui_attach" {
            assert_eq!(json["surface"], "desktop");
        }
        let decoded: ClientCommand =
            serde_json::from_value(json).expect("deserialize UI lifecycle command");
        assert_eq!(decoded, command);
    }

    let surface_cases = [
        (UiSurfaceDto::Desktop, "desktop"),
        (UiSurfaceDto::Mobile, "mobile"),
        (UiSurfaceDto::Vscode, "vscode"),
    ];
    for (surface, wire) in surface_cases {
        assert_eq!(serde_json::to_value(surface).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<UiSurfaceDto>(wire.into()).unwrap(),
            surface
        );
    }
}

#[test]
fn mod_ui_control_commands_round_trip_raw_json_and_outer_correlators() {
    let commands = [
        (
            ClientCommand::UiRender {
                request_id: "ui-render-1".into(),
                request_json: r#"{"subtype":"ui_render"}"#.into(),
            },
            "ui_render",
            "ui-render-1",
        ),
        (
            ClientCommand::UiClientModule {
                request_id: "ui-module-1".into(),
                plugin: "review".into(),
            },
            "ui_client_module",
            "ui-module-1",
        ),
        (
            ClientCommand::UiMessage {
                request_id: "ui-message-1".into(),
                request_json: r#"{"subtype":"ui_message"}"#.into(),
            },
            "ui_message",
            "ui-message-1",
        ),
        (
            ClientCommand::UiClientFault {
                request_id: "ui-fault-1".into(),
                request_json: r#"{"subtype":"ui_client_fault"}"#.into(),
            },
            "ui_client_fault",
            "ui-fault-1",
        ),
        (
            ClientCommand::UiClientPress {
                request_id: "ui-press-1".into(),
                request_json: r#"{"subtype":"ui_client_press"}"#.into(),
            },
            "ui_client_press",
            "ui-press-1",
        ),
        (
            ClientCommand::UiPress {
                request_id: "parent-press-1".into(),
                request_json: r#"{"subtype":"ui_press"}"#.into(),
            },
            "ui_press",
            "parent-press-1",
        ),
        (
            ClientCommand::UiInput {
                request_id: "parent-input-1".into(),
                request_json: r#"{"subtype":"ui_input"}"#.into(),
            },
            "ui_input",
            "parent-input-1",
        ),
        (
            ClientCommand::UiSelect {
                request_id: "parent-select-1".into(),
                request_json: r#"{"subtype":"ui_select"}"#.into(),
            },
            "ui_select",
            "parent-select-1",
        ),
        (
            ClientCommand::UiClientOperation {
                request_id: "ui-operation-1".into(),
                operation_json: r#"{"subtype":"mount"}"#.into(),
            },
            "ui_client_operation",
            "ui-operation-1",
        ),
    ];

    for (command, tag, request_id) in commands {
        let json = serde_json::to_value(&command).expect("serialize Mod UI command");
        assert_eq!(json["type"], tag);
        assert_eq!(json["request_id"], request_id);
        match &command {
            ClientCommand::UiRender { request_json, .. }
            | ClientCommand::UiMessage { request_json, .. }
            | ClientCommand::UiClientFault { request_json, .. }
            | ClientCommand::UiClientPress { request_json, .. }
            | ClientCommand::UiPress { request_json, .. }
            | ClientCommand::UiInput { request_json, .. }
            | ClientCommand::UiSelect { request_json, .. } => {
                assert_eq!(json["request_json"], request_json.as_str());
                assert!(json["request_json"].is_string());
            }
            ClientCommand::UiClientModule { plugin, .. } => {
                assert_eq!(json["plugin"], plugin.as_str());
            }
            ClientCommand::UiClientOperation { operation_json, .. } => {
                assert_eq!(json["operation_json"], operation_json.as_str());
                assert!(json["operation_json"].is_string());
            }
            _ => unreachable!("the cases above contain only Mod UI commands"),
        }
        let decoded: ClientCommand =
            serde_json::from_value(json).expect("deserialize Mod UI command");
        assert_eq!(decoded, command);
    }
}

#[test]
fn set_fast_mode_round_trips() {
    let command = ClientCommand::SetFastMode { enabled: true };
    let json = serde_json::to_value(&command).expect("serialize SetFastMode");
    assert_eq!(
        json,
        serde_json::json!({ "type": "set_fast_mode", "enabled": true })
    );
    assert_eq!(
        serde_json::from_value::<ClientCommand>(json).unwrap(),
        command
    );
}

/// `TaskList` — carries an optional status filter (reuses `TaskStatusDto`).
#[test]
fn task_list_round_trips() {
    let cmd = ClientCommand::TaskList {
        request_id: None,
        status_filter: Some(TaskStatusDto::Running),
    };
    let json = serde_json::to_value(&cmd).expect("serialize TaskList");
    assert_eq!(json["type"], "task_list");
    assert_eq!(json["status_filter"]["type"], "running");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize TaskList");
    assert_eq!(back, cmd);

    let unfiltered = ClientCommand::TaskList {
        request_id: None,
        status_filter: None,
    };
    let json_u = serde_json::to_value(&unfiltered).expect("serialize unfiltered TaskList");
    assert!(
        json_u.get("status_filter").is_none(),
        "None status_filter must be skipped"
    );
    let back_u: ClientCommand =
        serde_json::from_value(json_u).expect("deserialize unfiltered TaskList");
    assert_eq!(back_u, unfiltered);
    let correlated = ClientCommand::TaskList {
        request_id: Some("guard-1".into()),
        status_filter: None,
    };
    let json = serde_json::to_value(&correlated).unwrap();
    assert_eq!(json["request_id"], "guard-1");
    assert_eq!(
        serde_json::from_value::<ClientCommand>(json).unwrap(),
        correlated
    );
}

/// `TaskOutput` — pulls a task's output spool from a byte/line offset.
#[test]
fn task_output_round_trips() {
    let cmd = ClientCommand::TaskOutput {
        task_id: "b12345678".to_string(),
        offset: 1024,
    };
    let json = serde_json::to_value(&cmd).expect("serialize TaskOutput");
    assert_eq!(json["type"], "task_output");
    assert_eq!(json["task_id"], "b12345678");
    assert_eq!(json["offset"], 1024);
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize TaskOutput");
    assert_eq!(back, cmd);
}

/// `TaskStop` — stops a running task by id.
#[test]
fn task_stop_round_trips() {
    let cmd = ClientCommand::TaskStop {
        task_id: "b12345678".to_string(),
    };
    let json = serde_json::to_value(&cmd).expect("serialize TaskStop");
    assert_eq!(json["type"], "task_stop");
    assert_eq!(json["task_id"], "b12345678");
    let back: ClientCommand = serde_json::from_value(json).expect("deserialize TaskStop");
    assert_eq!(back, cmd);
}

/// Representative live commands keep the session as a connection attribute.
/// ResumeSession names a target, while durable-turn commands fence their owner.
#[test]
#[allow(clippy::too_many_lines)]
fn no_live_command_carries_session_id() {
    // Representative commands that do not select or fence a session.
    let commands: Vec<ClientCommand> = vec![
        ClientCommand::SendPrompt {
            session_id: None,
            text: ClientTextDto::from_utf8("x"),
            prompt_mode: Some(PromptModeDto::Normal),
            images: vec![ImageRefDto {
                media_type: "image/png".to_string(),
                base64: "AA==".to_string(),
            }],
            turn_id: Some(1),
            visualization_context: None,
        },
        ClientCommand::Cancel {
            session_id: None,
            turn_id: Some(1),
        },
        ClientCommand::ApprovePermission {
            request_id: 1,
            response: PermissionResponseDto::AllowOnce,
        },
        ClientCommand::DenyPermission { request_id: 1 },
        ClientCommand::SetModel {
            model: ClientTextDto::from_utf8("m"),
        },
        ClientCommand::ListModels,
        ClientCommand::GetConversationControls,
        ClientCommand::SetReasoningSelection {
            selection: ReasoningSelectionDto::Automatic,
        },
        ClientCommand::RunSlashCommand {
            session_id: None,
            raw: ClientTextDto::from_utf8("/x"),
            images: Vec::new(),
            turn_id: None,
        },
        ClientCommand::RefreshListings {
            which: vec![ListingKindDto::Status],
        },
        ClientCommand::NewSession {
            cwd: Some("/p".to_string()),
            model: Some(ClientTextDto::from_utf8("m")),
        },
        ClientCommand::ListSessions { limit: Some(5) },
        ClientCommand::Login,
        ClientCommand::Logout,
        ClientCommand::ForceCompact,
        ClientCommand::ClearSession,
        ClientCommand::TaskList {
            request_id: None,
            status_filter: Some(TaskStatusDto::Pending),
        },
        ClientCommand::TaskOutput {
            task_id: "b00000000".to_string(),
            offset: 0,
        },
        ClientCommand::TaskStop {
            task_id: "b00000000".to_string(),
        },
        ClientCommand::RequestExit,
    ];

    for cmd in &commands {
        let json = serde_json::to_value(cmd).expect("serialize command");
        let obj = json
            .as_object()
            .expect("command serializes to a JSON object");
        assert!(
            !obj.contains_key("session_id"),
            "live command {cmd:?} must NOT carry session_id (decision §0.5) — \
             session_id is a connection attribute"
        );
        // Sanity: it actually round-trips too.
        let back: ClientCommand = serde_json::from_value(json).expect("deserialize command");
        assert_eq!(&back, cmd);
    }

    // A session-selecting command does carry session_id.
    let resume = ClientCommand::ResumeSession {
        session_id: "target-id".to_string(),
        cwd: None,
    };
    let resume_json = serde_json::to_value(&resume).expect("serialize ResumeSession");
    assert!(
        resume_json
            .as_object()
            .expect("object")
            .contains_key("session_id"),
        "ResumeSession names a session target (§0.5)"
    );
}

#[test]
fn attach_turn_requires_session_and_attempt_identity() {
    let command = ClientCommand::AttachTurn {
        session_id: "session-a".into(),
        turn_id: 7,
        after_sequence: None,
        request_id: "attempt-1".into(),
    };
    let value = serde_json::to_value(&command).unwrap();
    assert_eq!(value["session_id"], "session-a");
    assert_eq!(value["turn_id"], 7);
    assert_eq!(value["request_id"], "attempt-1");
    assert!(value.get("after_sequence").is_none());
    assert_eq!(
        serde_json::from_value::<ClientCommand>(value.clone()).unwrap(),
        command
    );

    for required in ["session_id", "request_id"] {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(required);
        assert!(
            serde_json::from_value::<ClientCommand>(missing).is_err(),
            "{required}"
        );
    }
}

#[test]
fn resume_and_pause_require_session_identity() {
    for command in [
        ClientCommand::ResumeTurn {
            session_id: "session-a".into(),
            turn_id: 7,
        },
        ClientCommand::PauseTurn {
            session_id: "session-a".into(),
            turn_id: 7,
            reason: "lease_expired".into(),
        },
    ] {
        let mut value = serde_json::to_value(&command).unwrap();
        assert_eq!(value["session_id"], "session-a");
        assert_eq!(
            serde_json::from_value::<ClientCommand>(value.clone()).unwrap(),
            command
        );
        value.as_object_mut().unwrap().remove("session_id");
        assert!(serde_json::from_value::<ClientCommand>(value).is_err());
    }
}

#[test]
fn cron_claim_generation_roundtrips_without_requiring_legacy_records_to_have_it() {
    use client::protocol::commands::CronRunDto;
    let legacy = serde_json::json!({
        "id": "run", "taskId": "task", "scheduledAt": 1,
        "startedAt": 2, "finishedAt": null, "status": "running",
        "model": "provider/model", "reasoning": {"type": "automatic"},
        "sessionId": null, "summary": null, "error": null
    });
    let mut run: CronRunDto = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(run.claim_generation, None);
    assert_eq!(run.manual_occurrence_at, None);
    assert_eq!(serde_json::to_value(&run).unwrap(), legacy);
    run.claim_generation = Some(2);
    run.manual_occurrence_at = Some(1);
    let value = serde_json::to_value(&run).unwrap();
    assert_eq!(value["claimGeneration"], 2);
    assert_eq!(value["manualOccurrenceAt"], 1);
    assert_eq!(serde_json::from_value::<CronRunDto>(value).unwrap(), run);
}
