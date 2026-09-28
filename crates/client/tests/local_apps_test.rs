//! Local-apps DTO round-trip + exact-wire-shape tests.
//!
//! Mirrors `tests/computer_access_test.rs`: each type gets a serialize →
//! assert-shape → deserialize → assert-eq round-trip. The fieldless enums ride
//! as bare wire STRINGS (the `AccessTierDto` precedent — byte-identical to the
//! `local-apps` core enums' canonical `as_str()` values), so several tests
//! assert BYTE-IDENTICAL serialized JSON, not just structural equivalence.

use client::protocol::local_apps::{
    AppCheckpointDto, AppCheckpointKindDto, AppCreateOriginDto, AppErrorCodeDto, AppRecordDto,
    AppRuntimeStateDto, AppWorkflowStateDto,
};

/// `AppWorkflowStateDto` — publication states round-trip as their bare
/// `snake_case` strings.
#[test]
fn workflow_state_serializes_as_bare_string() {
    let cases = [
        (AppWorkflowStateDto::Draft, "\"draft\""),
        (
            AppWorkflowStateDto::PublishedUnverified,
            "\"published_unverified\"",
        ),
        (
            AppWorkflowStateDto::PublishedVerified,
            "\"published_verified\"",
        ),
    ];
    for (state, expected) in cases {
        let json = serde_json::to_string(&state).expect("serialize AppWorkflowStateDto");
        assert_eq!(json, expected, "AppWorkflowStateDto::{state:?} wire form");
        let back: AppWorkflowStateDto =
            serde_json::from_str(&json).expect("deserialize AppWorkflowStateDto");
        assert_eq!(back, state);
    }
}

/// `AppRuntimeStateDto` — every spec-§C state round-trips as its bare string.
#[test]
fn runtime_state_serializes_as_bare_string() {
    let cases = [
        (AppRuntimeStateDto::Stopped, "\"stopped\""),
        (AppRuntimeStateDto::Starting, "\"starting\""),
        (AppRuntimeStateDto::Running, "\"running\""),
        (AppRuntimeStateDto::Stopping, "\"stopping\""),
        (AppRuntimeStateDto::Failed, "\"failed\""),
    ];
    for (state, expected) in cases {
        let json = serde_json::to_string(&state).expect("serialize AppRuntimeStateDto");
        assert_eq!(json, expected, "AppRuntimeStateDto::{state:?} wire form");
        let back: AppRuntimeStateDto =
            serde_json::from_str(&json).expect("deserialize AppRuntimeStateDto");
        assert_eq!(back, state);
    }
}

/// `AppCreateOriginDto` — `"chat"` / `"library"` bare strings.
#[test]
fn create_origin_serializes_as_bare_string() {
    let cases = [
        (AppCreateOriginDto::Chat, "\"chat\""),
        (AppCreateOriginDto::Library, "\"library\""),
    ];
    for (origin, expected) in cases {
        let json = serde_json::to_string(&origin).expect("serialize AppCreateOriginDto");
        assert_eq!(json, expected, "AppCreateOriginDto::{origin:?} wire form");
        let back: AppCreateOriginDto =
            serde_json::from_str(&json).expect("deserialize AppCreateOriginDto");
        assert_eq!(back, origin);
    }
}

/// `AppErrorCodeDto` — every extensible failure code rides as its bare string.
#[test]
fn error_code_serializes_as_bare_string() {
    let cases = [
        (AppErrorCodeDto::NotFound, "\"not_found\""),
        (AppErrorCodeDto::RevisionConflict, "\"revision_conflict\""),
        (
            AppErrorCodeDto::InteractionInvalid,
            "\"interaction_invalid\"",
        ),
        (
            AppErrorCodeDto::WorkflowStateInvalid,
            "\"workflow_state_invalid\"",
        ),
        (AppErrorCodeDto::RuntimeBusy, "\"runtime_busy\""),
        (AppErrorCodeDto::NotYetAvailable, "\"not_yet_available\""),
        (AppErrorCodeDto::StorageCorrupt, "\"storage_corrupt\""),
        (AppErrorCodeDto::InvalidRequest, "\"invalid_request\""),
        (AppErrorCodeDto::Io, "\"io\""),
    ];
    for (code, expected) in cases {
        let json = serde_json::to_string(&code).expect("serialize AppErrorCodeDto");
        assert_eq!(json, expected, "AppErrorCodeDto::{code:?} wire form");
        let back: AppErrorCodeDto =
            serde_json::from_str(&json).expect("deserialize AppErrorCodeDto");
        assert_eq!(back, code);
    }
}

/// `AppCheckpointKindDto` — every checkpoint kind rides as its bare string.
#[test]
fn checkpoint_kind_serializes_as_bare_string() {
    let cases = [
        (
            AppCheckpointKindDto::ScaffoldCreated,
            "\"scaffold_created\"",
        ),
        (
            AppCheckpointKindDto::GenerationValidated,
            "\"generation_validated\"",
        ),
        (
            AppCheckpointKindDto::PreviewApproved,
            "\"preview_approved\"",
        ),
        (AppCheckpointKindDto::UserApproved, "\"user_approved\""),
        (AppCheckpointKindDto::PreRestore, "\"pre_restore\""),
    ];
    for (kind, expected) in cases {
        let json = serde_json::to_string(&kind).expect("serialize AppCheckpointKindDto");
        assert_eq!(json, expected, "AppCheckpointKindDto::{kind:?} wire form");
        let back: AppCheckpointKindDto =
            serde_json::from_str(&json).expect("deserialize AppCheckpointKindDto");
        assert_eq!(back, kind);
    }
}

/// `AppRecordDto` — `snake_case` protocol fields; an absent `conversation_id` is
/// omitted from the wire, while `scaffolded` is REQUIRED (no serde default, so
/// a record that omits it fails to decode rather than silently deciding that a
/// shell is a formed app).
#[test]
fn app_record_round_trips_and_skips_none_conversation() {
    let record = AppRecordDto {
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
        // Deliberately `true`, not `false`: `false` is `bool::default()`, so a
        // future `#[serde(default)]` on this field would still round-trip a
        // `false` fixture and this test would stop noticing.
        scaffolded: true,
    };
    let json = serde_json::to_value(&record).expect("serialize AppRecordDto");
    assert_eq!(json["id"], "habits-1a2b");
    assert_eq!(json["brief"], "Track daily habits");
    assert_eq!(json["git_enabled"], true);
    assert_eq!(json["created_at_ms"], 1_750_000_000_000_u64);
    assert_eq!(json["workflow_state"], "draft");
    assert_eq!(json["workspace_rel"], "apps/habits-1a2b/workspace");
    assert!(
        json.get("conversation_id").is_none(),
        "None conversation_id must be skipped"
    );
    assert_eq!(json["scaffolded"], true);
    let back: AppRecordDto =
        serde_json::from_value(json.clone()).expect("deserialize AppRecordDto");
    assert_eq!(back, record);

    // `scaffolded` carries NO serde default: dropping it from the wire must be
    // a decode ERROR that names the field, never a silent `false`.
    let mut without_scaffolded = json;
    without_scaffolded
        .as_object_mut()
        .expect("record serializes to a JSON object")
        .remove("scaffolded")
        .expect("the field was on the wire to begin with");
    let error = serde_json::from_value::<AppRecordDto>(without_scaffolded)
        .expect_err("a record without `scaffolded` must not decode");
    assert!(
        error.to_string().contains("scaffolded"),
        "the decode error must NAME the missing field, got: {error}"
    );

    let chat_born = AppRecordDto {
        conversation_id: Some("conv-42".to_string()),
        ..record
    };
    let json_c = serde_json::to_value(&chat_born).expect("serialize chat-born AppRecordDto");
    assert_eq!(json_c["conversation_id"], "conv-42");
    let back_c: AppRecordDto =
        serde_json::from_value(json_c).expect("deserialize chat-born AppRecordDto");
    assert_eq!(back_c, chat_born);
}

/// `AppCheckpointDto` — `snake_case` protocol fields + bare-string kind.
#[test]
fn app_checkpoint_round_trips() {
    let checkpoint = AppCheckpointDto {
        id: "ckpt-0001".to_string(),
        label: "Scaffold created".to_string(),
        kind: AppCheckpointKindDto::ScaffoldCreated,
        created_at_ms: 1_750_000_000_000,
    };
    let json = serde_json::to_value(&checkpoint).expect("serialize AppCheckpointDto");
    assert_eq!(json["id"], "ckpt-0001");
    assert_eq!(json["label"], "Scaffold created");
    assert_eq!(json["kind"], "scaffold_created");
    assert_eq!(json["created_at_ms"], 1_750_000_000_000_u64);
    let back: AppCheckpointDto =
        serde_json::from_value(json).expect("deserialize AppCheckpointDto");
    assert_eq!(back, checkpoint);
}
