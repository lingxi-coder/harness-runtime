//! Verify the real metadata consumed by generated Swift and Kotlin bindings.

#![cfg(feature = "uniffi")]

use client::protocol::commands::UNIFFI_META_CLIENT_ENUM_CLIENTCOMMAND;
use client::protocol::events::UNIFFI_META_CLIENT_ENUM_CLIENTEVENT;
use uniffi_meta::{Metadata, Type};

#[test]
fn attach_command_and_boundaries_keep_native_field_types_and_ordinals() {
    let Metadata::Enum(commands) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_CLIENTCOMMAND).unwrap()
    else {
        panic!("expected ClientCommand enum metadata");
    };
    let attach = commands
        .variants
        .iter()
        .find(|variant| variant.name == "AttachTurn")
        .expect("AttachTurn metadata");
    assert_eq!(
        attach
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["session_id", "turn_id", "after_sequence", "request_id"]
    );
    assert!(matches!(&attach.fields[0].ty, Type::String));
    assert!(matches!(&attach.fields[1].ty, Type::UInt64));
    assert!(matches!(
        &attach.fields[2].ty,
        Type::Optional { inner_type } if matches!(inner_type.as_ref(), Type::UInt64)
    ));
    assert!(matches!(&attach.fields[3].ty, Type::String));
    assert_eq!(attach.fields[0].default, None);
    assert_eq!(attach.fields[3].default, None);

    for (name, expected_fields) in [
        (
            "SendPrompt",
            &[
                "session_id",
                "text",
                "prompt_mode",
                "images",
                "turn_id",
                "visualization_context",
            ][..],
        ),
        (
            "RunSlashCommand",
            &["session_id", "raw", "images", "turn_id"][..],
        ),
    ] {
        let variant = commands.variants.iter().find(|v| v.name == name).unwrap();
        assert_eq!(
            variant
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            expected_fields
        );
        assert!(matches!(
            &variant.fields[0].ty,
            Type::Optional { inner_type } if matches!(inner_type.as_ref(), Type::String)
        ));
        assert!(matches!(
            &variant.fields[1].ty,
            Type::Record { name, .. } if name == "ClientTextDto"
        ));
    }

    let cancel = commands
        .variants
        .iter()
        .find(|v| v.name == "Cancel")
        .unwrap();
    assert_eq!(
        cancel
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["session_id", "turn_id"]
    );
    assert!(matches!(
        &cancel.fields[0].ty,
        Type::Optional { inner_type } if matches!(inner_type.as_ref(), Type::String)
    ));
    assert!(matches!(
        &cancel.fields[1].ty,
        Type::Optional { inner_type } if matches!(inner_type.as_ref(), Type::UInt64)
    ));
    for (name, expected_fields) in [
        ("ResumeTurn", &["session_id", "turn_id"][..]),
        ("PauseTurn", &["session_id", "turn_id", "reason"][..]),
    ] {
        let variant = commands.variants.iter().find(|v| v.name == name).unwrap();
        assert_eq!(
            variant
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            expected_fields
        );
        assert!(matches!(&variant.fields[0].ty, Type::String));
        assert_eq!(variant.fields[0].default, None);
        assert!(matches!(&variant.fields[1].ty, Type::UInt64));
    }

    let Metadata::Enum(events) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ENUM_CLIENTEVENT).unwrap()
    else {
        panic!("expected ClientEvent enum metadata");
    };
    let tail = &events.variants[events.variants.len() - 4..];
    assert_eq!(
        tail.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        [
            "ModSessionEvent",
            "TurnAttachStarted",
            "TurnAttachFinished",
            "QueuedTurnStatus"
        ]
    );
    assert_eq!(
        tail[1]
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["request_id", "snapshot"]
    );
    assert!(matches!(&tail[1].fields[0].ty, Type::String));
    assert!(matches!(
        &tail[1].fields[1].ty,
        Type::Record { name, .. } if name == "TurnRecoverySnapshotDto"
    ));
    assert_eq!(
        tail[2]
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["request_id", "session_id", "turn_id"]
    );
    assert!(matches!(&tail[2].fields[0].ty, Type::String));
    assert!(matches!(&tail[2].fields[1].ty, Type::String));
    assert!(matches!(&tail[2].fields[2].ty, Type::UInt64));
}
