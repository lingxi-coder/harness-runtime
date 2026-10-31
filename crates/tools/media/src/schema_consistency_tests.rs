use super::*;
use serde_json::{json, Value};

fn assert_required_fields(schema: &Value, pointer: &str, expected: &[&str]) {
    let mut actual = schema
        .pointer(pointer)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing required array at {pointer}"))
        .iter()
        .map(|field| field.as_str().expect("required entries are strings"))
        .collect::<Vec<_>>();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected, "required fields at {pointer}");
}

#[test]
fn transcribe_schema_and_parser_agree_on_live_and_stored_sources() {
    let schema = schema_for(MediaOperationKind::Transcribe, &[]);
    assert_required_fields(
        &schema,
        "/properties/source/oneOf/0/required",
        &["kind", "source_ref"],
    );
    assert_required_fields(
        &schema,
        "/properties/source/oneOf/1/required",
        &["kind", "media_ref"],
    );

    for input in [
        json!({"source":{"kind":"live","source_ref":"mic-1"}}),
        json!({"source":{"kind":"stored","media_ref":"media-1"}}),
    ] {
        assert!(parse_operation(
            MediaOperationKind::Transcribe,
            &generated_projection(&input)
        )
        .is_ok());
    }

    for input in [
        json!({"source":{"kind":"live"}}),
        json!({"source":{"kind":"stored"}}),
        json!({"source":{"kind":"stored","source_ref":"wrong-variant"}}),
    ] {
        assert!(parse_operation(
            MediaOperationKind::Transcribe,
            &generated_projection(&input)
        )
        .is_err());
    }
}

#[test]
fn session_start_schema_and_parser_require_sources_for_each_mode() {
    let schema = schema_for(
        MediaOperationKind::SessionStart,
        &[MediaSessionMode::Audio, MediaSessionMode::AudioVideo],
    );
    assert_required_fields(&schema, "/oneOf/0/required", &["mode", "audio_source_ref"]);
    assert_required_fields(&schema, "/oneOf/1/required", &["mode", "video_source_ref"]);
    assert_required_fields(
        &schema,
        "/oneOf/2/required",
        &["mode", "audio_source_ref", "video_source_ref"],
    );

    for input in [
        json!({"intent":"observation","mode":"audio","audio_source_ref":"mic-1"}),
        json!({"intent":"observation","mode":"video","video_source_ref":"camera-1"}),
        json!({"intent":"observation","mode":"audio_video","audio_source_ref":"mic-1","video_source_ref":"camera-1"}),
    ] {
        assert!(parse_operation(
            MediaOperationKind::SessionStart,
            &generated_projection(&input)
        )
        .is_ok());
    }

    for input in [
        json!({"intent":"observation","mode":"audio"}),
        json!({"intent":"observation","mode":"video"}),
        json!({"intent":"observation","mode":"audio_video","audio_source_ref":"mic-1"}),
        json!({"intent":"observation","mode":"audio_video","video_source_ref":"camera-1"}),
    ] {
        assert!(parse_operation(
            MediaOperationKind::SessionStart,
            &generated_projection(&input)
        )
        .is_err());
    }
}

#[test]
fn session_update_schema_and_parser_require_each_variant_payload() {
    let schema = schema_for(MediaOperationKind::SessionUpdate, &[]);
    assert_required_fields(
        &schema,
        "/properties/update/oneOf/0/required",
        &["type", "kind", "enabled"],
    );
    assert_required_fields(
        &schema,
        "/properties/update/oneOf/1/required",
        &["type", "kind", "source_ref"],
    );
    assert_required_fields(
        &schema,
        "/properties/update/oneOf/2/required",
        &["type", "gain"],
    );

    for input in [
        json!({"session_ref":"session-1","update":{"type":"set_enabled","kind":"audio","enabled":true}}),
        json!({"session_ref":"session-1","update":{"type":"select_source","kind":"video","source_ref":"camera-1"}}),
        json!({"session_ref":"session-1","update":{"type":"set_playback_gain","gain":0.5}}),
    ] {
        assert!(parse_operation(
            MediaOperationKind::SessionUpdate,
            &generated_projection(&input)
        )
        .is_ok());
    }

    for input in [
        json!({"session_ref":"session-1","update":{"type":"set_enabled","kind":"audio"}}),
        json!({"session_ref":"session-1","update":{"type":"select_source","kind":"video"}}),
        json!({"session_ref":"session-1","update":{"type":"set_playback_gain"}}),
    ] {
        assert!(parse_operation(
            MediaOperationKind::SessionUpdate,
            &generated_projection(&input)
        )
        .is_err());
    }
}

#[test]
fn operation_target_schema_requires_host_identity_for_status_and_stop() {
    let status_schema = schema_for(MediaOperationKind::Status, &[]);
    let stop_schema = schema_for(MediaOperationKind::Stop, &[]);
    for (schema, pointer) in [
        (&status_schema, "/properties/target/oneOf/0/required"),
        (&stop_schema, "/properties/target/oneOf/0/required"),
    ] {
        assert_required_fields(
            schema,
            pointer,
            &["kind", "ref", "generation", "service_epoch", "incarnation"],
        );
    }

    for kind in [MediaOperationKind::Status, MediaOperationKind::Stop] {
        let input = generated_projection(&json!({
            "target":{"kind":"operation","ref":"operation-1"}
        }));
        assert!(parse_operation(kind, &input).is_err());

        let missing_incarnation = generated_projection(&json!({
            "target":{
                "kind":"operation",
                "ref":"operation-1",
                "generation":42,
                "service_epoch":7
            }
        }));
        assert!(parse_operation(kind, &missing_incarnation).is_err());

        let input = generated_projection(&json!({
            "target":{
                "kind":"operation",
                "ref":"operation-1",
                "generation":42,
                "service_epoch":7,
                "incarnation":media_api::MediaOperationIncarnation::new().as_string()
            }
        }));
        assert!(parse_operation(kind, &input).is_ok());
    }

    let ordinary_target = generated_projection(&json!({
        "target":{"kind":"session","ref":"session-1"}
    }));
    assert!(parse_operation(MediaOperationKind::Status, &ordinary_target).is_ok());
}
