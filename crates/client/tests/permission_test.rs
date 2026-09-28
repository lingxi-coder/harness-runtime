//! F1-04 — Permission DTO round-trip + reserved-variant tests.
//!
//! Freezes the permission request/response shape sourced from
//! `platform_api::PermissionGate::check` (plan F1-04). Each `PermissionKindDto`
//! variant gets a serialize → assert-tag → deserialize → assert-eq round-trip
//! so the wire shape is locked before the F1-08 snapshot golden is generated.
//!
//! Only `ToolUseConfirm` has a LIVE engine source. `ExitPlanMode` and
//! `BypassPermissionsMode` are RESERVED / feed-deferred (governing decision
//! §0.6) — defined here so the contract freezes now, but they MUST NOT be wired
//! to a live source in the foundation. The tests prove they are *present* and
//! round-trip, not that they are fed.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate itself never depends on
//! `serde_json::Value` (decision §0.4): the tool input is a JSON **String**
//! (`tool_input_json`), and `PromptDefault` is collapsed to `default_allow: bool`.

use client::protocol::permission::{
    AutoModePromptDto, PermissionKindDto, PermissionOwnerDto, PermissionRequest,
    PermissionResolved, PermissionResponseDto, WorkerInfoDto,
};

#[test]
fn permission_request_carries_owner_scope() {
    let request = PermissionRequest {
        request_id: 42,
        kind: PermissionKindDto::ToolUseConfirm {
            tool_name: "Bash".into(),
            tool_input_json: "{}".into(),
            default_allow: false,
        },
        worker: None,
        owner: Some(PermissionOwnerDto {
            session_id: Some("session-a".into()),
            turn_id: Some(7),
            worker_name: None,
        }),
        suppress_always_allow_rule: false,
        auto_mode_prompt: None,
    };

    let value = serde_json::to_value(request).expect("serialize permission request");
    assert_eq!(value["owner"]["session_id"], "session-a");
    assert_eq!(value["owner"]["turn_id"], 7);
    assert!(value["owner"].get("worker_name").is_none());
}

/// `ToolUseConfirm` — the ONE live-sourced kind. Carries the tool name, the
/// tool input lowered to a JSON **String** (§0.4), and the collapsed
/// `PromptDefault` → `default_allow: bool`.
#[test]
fn tool_use_confirm_round_trips() {
    let kind = PermissionKindDto::ToolUseConfirm {
        tool_name: "Bash".to_string(),
        tool_input_json: r#"{"command":"ls"}"#.to_string(),
        default_allow: false,
    };
    let json = serde_json::to_value(&kind).expect("serialize ToolUseConfirm");
    assert_eq!(json["type"], "tool_use_confirm");
    assert_eq!(json["tool_name"], "Bash");
    // The tool input is a JSON String on the wire, NOT a nested object (§0.4).
    assert!(
        json["tool_input_json"].is_string(),
        "tool_input_json must be a String"
    );
    let back: PermissionKindDto = serde_json::from_value(json).expect("deserialize ToolUseConfirm");
    assert_eq!(back, kind);
}

/// `ToolUseConfirm` carries the collapsed `PromptDefault` as a `default_allow:
/// bool` (the `permission::tool_default(name) -> PromptDefault` →
/// `AllowByDefault` ⇒ `true` collapse, decision §0.6). Both polarities
/// round-trip and serialize the boolean (never skipped — it is not optional).
#[test]
fn tool_use_confirm_carries_default_allow_bool() {
    for allow in [true, false] {
        let kind = PermissionKindDto::ToolUseConfirm {
            tool_name: "Read".to_string(),
            tool_input_json: "{}".to_string(),
            default_allow: allow,
        };
        let json = serde_json::to_value(&kind).expect("serialize ToolUseConfirm");
        // `default_allow` is a required bool, present regardless of polarity.
        assert_eq!(
            json["default_allow"], allow,
            "default_allow must serialize the collapsed PromptDefault bool"
        );
        assert!(
            json["default_allow"].is_boolean(),
            "default_allow must be a bool (the collapsed PromptDefault)"
        );
        let back: PermissionKindDto =
            serde_json::from_value(json).expect("deserialize ToolUseConfirm");
        assert_eq!(back, kind);
    }
}

/// `ExitPlanMode` — RESERVED / feed-deferred (§0.6). Present in the frozen enum
/// so it round-trips, but it has NO live source in the foundation.
#[test]
fn exit_plan_mode_round_trips_reserved() {
    let kind = PermissionKindDto::ExitPlanMode {
        plan: "1. do the thing\n2. verify".to_string(),
    };
    let json = serde_json::to_value(&kind).expect("serialize ExitPlanMode");
    assert_eq!(json["type"], "exit_plan_mode");
    assert_eq!(json["plan"], "1. do the thing\n2. verify");
    let back: PermissionKindDto = serde_json::from_value(json).expect("deserialize ExitPlanMode");
    assert_eq!(back, kind);
}

/// `BypassPermissionsMode` — RESERVED / feed-deferred (§0.6). A unit-style
/// variant (no payload); present so the contract freezes now.
#[test]
fn bypass_permissions_mode_round_trips_reserved() {
    let kind = PermissionKindDto::BypassPermissionsMode;
    let json = serde_json::to_value(&kind).expect("serialize BypassPermissionsMode");
    assert_eq!(json["type"], "bypass_permissions_mode");
    let back: PermissionKindDto =
        serde_json::from_value(json).expect("deserialize BypassPermissionsMode");
    assert_eq!(back, kind);
}

/// The three reserved/live kinds are all PRESENT in the frozen enum (the
/// reserved-variant presence check the plan's verify gate names). Asserts the
/// `snake_case` wire tag for each.
#[test]
fn permission_kind_variants_present() {
    let cases = [
        (
            PermissionKindDto::ToolUseConfirm {
                tool_name: "x".to_string(),
                tool_input_json: "{}".to_string(),
                default_allow: true,
            },
            "tool_use_confirm",
        ),
        (
            PermissionKindDto::ExitPlanMode {
                plan: "p".to_string(),
            },
            "exit_plan_mode",
        ),
        (
            PermissionKindDto::BypassPermissionsMode,
            "bypass_permissions_mode",
        ),
    ];
    for (kind, tag) in cases {
        let json = serde_json::to_value(&kind).expect("serialize PermissionKindDto");
        assert_eq!(
            json["type"], tag,
            "PermissionKindDto::{kind:?} tag mismatch"
        );
        let back: PermissionKindDto =
            serde_json::from_value(json).expect("deserialize PermissionKindDto");
        assert_eq!(back, kind);
    }
}

/// `PermissionRequest` — the outbound request. Carries the `request_id`
/// (id-keyed multiplexing of concurrent worker+main requests, F1-14), the
/// `kind`, and the optional `worker`.
#[test]
fn permission_request_round_trips() {
    let req = PermissionRequest {
        request_id: 42,
        kind: PermissionKindDto::ToolUseConfirm {
            tool_name: "Bash".to_string(),
            tool_input_json: r#"{"command":"rm -rf /"}"#.to_string(),
            default_allow: false,
        },
        worker: None,
        owner: None,
        suppress_always_allow_rule: false,
        auto_mode_prompt: None,
    };
    let json = serde_json::to_value(&req).expect("serialize PermissionRequest");
    assert_eq!(json["request_id"], 42);
    assert_eq!(json["kind"]["type"], "tool_use_confirm");
    let back: PermissionRequest =
        serde_json::from_value(json).expect("deserialize PermissionRequest");
    assert_eq!(back, req);
}

/// `worker` is optional and defaults to `None`, skipped from the wire when
/// absent (the `skip_serializing_if = "Option::is_none"` forward-compat
/// convention). When present, the reserved [`WorkerInfoDto`] round-trips.
#[test]
fn worker_is_optional_and_defaults_none() {
    // Absent ⇒ skipped on the wire AND deserializable from a frame without it.
    let req = PermissionRequest {
        request_id: 1,
        kind: PermissionKindDto::BypassPermissionsMode,
        worker: None,
        owner: None,
        suppress_always_allow_rule: false,
        auto_mode_prompt: None,
    };
    let json = serde_json::to_value(&req).expect("serialize PermissionRequest no-worker");
    assert!(
        json.get("worker").is_none(),
        "None worker must be skipped on the wire"
    );
    // A frame omitting `worker` entirely deserializes with worker == None
    // (the `#[serde(default)]` on the field).
    let from_minimal: PermissionRequest =
        serde_json::from_str(r#"{"request_id":1,"kind":{"type":"bypass_permissions_mode"}}"#)
            .expect("deserialize PermissionRequest without worker key");
    assert_eq!(from_minimal.worker, None);
    let back: PermissionRequest =
        serde_json::from_value(json).expect("deserialize PermissionRequest no-worker");
    assert_eq!(back, req);

    // Present ⇒ the reserved WorkerInfoDto round-trips.
    let req_with_worker = PermissionRequest {
        request_id: 2,
        kind: PermissionKindDto::ExitPlanMode {
            plan: "p".to_string(),
        },
        worker: Some(WorkerInfoDto {
            name: "researcher".to_string(),
            color: "cyan".to_string(),
            team: Some("alpha".to_string()),
        }),
        owner: None,
        suppress_always_allow_rule: false,
        auto_mode_prompt: None,
    };
    let json_w =
        serde_json::to_value(&req_with_worker).expect("serialize PermissionRequest worker");
    assert_eq!(json_w["worker"]["name"], "researcher");
    assert_eq!(json_w["worker"]["color"], "cyan");
    assert_eq!(json_w["worker"]["team"], "alpha");
    let back_w: PermissionRequest =
        serde_json::from_value(json_w).expect("deserialize PermissionRequest worker");
    assert_eq!(back_w, req_with_worker);
}

/// `WorkerInfoDto.team` is itself optional and skipped when `None` (reserved
/// DTO, mirrors the TUI-side `WorkerPermissionInfo` at
/// `tui/src/components/permissions/worker.rs:17`).
#[test]
fn worker_info_team_is_optional() {
    let worker = WorkerInfoDto {
        name: "main".to_string(),
        color: "white".to_string(),
        team: None,
    };
    let json = serde_json::to_value(&worker).expect("serialize WorkerInfoDto");
    assert_eq!(json["name"], "main");
    assert!(json.get("team").is_none(), "None team must be skipped");
    let back: WorkerInfoDto = serde_json::from_value(json).expect("deserialize WorkerInfoDto");
    assert_eq!(back, worker);
}

/// `PermissionResponseDto` — the inbound decision. Every variant round-trips
/// with a `snake_case` wire tag (`AllowOnce | AllowAlways | Deny`).
#[test]
fn permission_response_variants_round_trip() {
    let cases = [
        (PermissionResponseDto::AllowOnce, "allow_once"),
        (PermissionResponseDto::AllowAlways, "allow_always"),
        (PermissionResponseDto::AllowAuto, "allow_auto"),
        (PermissionResponseDto::Deny, "deny"),
    ];
    for (resp, tag) in cases {
        // `PermissionResponseDto` is `Copy`, so pass by value (no needless borrow).
        let json = serde_json::to_value(resp).expect("serialize PermissionResponseDto");
        assert_eq!(
            json["type"], tag,
            "PermissionResponseDto::{resp:?} tag mismatch"
        );
        let back: PermissionResponseDto =
            serde_json::from_value(json).expect("deserialize PermissionResponseDto");
        assert_eq!(back, resp);
    }
}

#[test]
fn auto_prompt_metadata_round_trips_and_is_optional() {
    let request = PermissionRequest {
        request_id: 9,
        kind: PermissionKindDto::ToolUseConfirm {
            tool_name: "Bash".into(),
            tool_input_json: r#"{"command":"echo hi"}"#.into(),
            default_allow: false,
        },
        worker: None,
        owner: None,
        suppress_always_allow_rule: false,
        auto_mode_prompt: Some(AutoModePromptDto::WorkflowBash),
    };
    let json = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(json["auto_mode_prompt"], "workflow_bash");
    let back: PermissionRequest = serde_json::from_value(json).expect("deserialize request");
    assert_eq!(back, request);
}

/// `PermissionResolved` — the resolution echoed back, correlated by
/// `request_id` with the originating [`PermissionRequest`].
#[test]
fn permission_resolved_round_trips() {
    let resolved = PermissionResolved {
        request_id: 42,
        response: PermissionResponseDto::AllowOnce,
    };
    let json = serde_json::to_value(&resolved).expect("serialize PermissionResolved");
    assert_eq!(json["request_id"], 42);
    assert_eq!(json["response"]["type"], "allow_once");
    let back: PermissionResolved =
        serde_json::from_value(json).expect("deserialize PermissionResolved");
    assert_eq!(back, resolved);
}
