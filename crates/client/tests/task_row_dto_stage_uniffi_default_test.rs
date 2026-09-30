//! Regression for review finding B7#3: `TaskRowDto.stage` (F005) is a 7th,
//! additive field on a `#[derive(uniffi::Record)]` type. UniFFI's generated
//! Kotlin/Swift bindings use a memberwise/positional constructor with NO
//! default values UNLESS the field carries `#[uniffi(default = …)]` — the
//! sole existing precedent in this crate is `images: Vec<MessageImageDto>`
//! at `message.rs:27` (`uniffi(default = [])`).
//!
//! Without a default on `stage`, every hand-written 6-arg Kotlin/Swift
//! `TaskRowDto(...)` call site (`ChatViewModelReducerTest.kt:620,844,852`,
//! `BackgroundTasksPanelTests.swift:38`) stops compiling the moment the
//! gitignored mobile bindings are regenerated — the Rust workspace has no
//! way to build those Kotlin/Swift targets to prove it directly, so this
//! test asserts the one thing that actually drives that codegen: the
//! compile-time UniFFI metadata buffer for `TaskRowDto` must carry a
//! `default = None` literal for the `stage` field.

#![cfg(feature = "uniffi")]

use client::protocol::listings::UNIFFI_META_CLIENT_RECORD_TASKROWDTO;
use uniffi_meta::{DefaultValueMetadata, LiteralMetadata, Metadata};

#[test]
fn task_row_dto_stage_field_carries_a_uniffi_none_default() {
    let metadata = uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_RECORD_TASKROWDTO)
        .expect("TaskRowDto's compile-time UniFFI metadata buffer decodes");
    let record = match metadata {
        Metadata::Record(record) => record,
        other => panic!("expected `TaskRowDto` to decode as Metadata::Record, got {other:?}"),
    };
    assert_eq!(record.name, "TaskRowDto");

    let stage_field = record
        .fields
        .iter()
        .find(|field| field.name == "stage")
        .unwrap_or_else(|| {
            panic!(
                "TaskRowDto's UniFFI metadata has no `stage` field; fields present: {:?}",
                record.fields.iter().map(|f| &f.name).collect::<Vec<_>>()
            )
        });

    assert_eq!(
        stage_field.default,
        Some(DefaultValueMetadata::Literal(LiteralMetadata::None)),
        "TaskRowDto.stage has UniFFI default {:?}, expected `Some(DefaultValueMetadata::Literal(LiteralMetadata::None))`. \
         Without that default, UniFFI's generated Kotlin/Swift bindings give `stage` a \
         REQUIRED trailing constructor parameter, and every existing 6-arg hand-written \
         `TaskRowDto(...)` call site in the mobile test targets \
         (ChatViewModelReducerTest.kt:620,844,852; BackgroundTasksPanelTests.swift:38) stops \
         compiling the moment the gitignored bindings are regenerated. Add \
         `#[cfg_attr(feature = \"uniffi\", uniffi(default = None))]` to `stage` in \
         client/src/protocol/listings.rs (the `images: Vec<..>` precedent at message.rs:27).",
        stage_field.default,
    );
}

#[test]
fn task_row_kind_is_appended_and_optional_in_generated_constructors() {
    let metadata = uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_RECORD_TASKROWDTO).unwrap();
    let Metadata::Record(record) = metadata else {
        panic!("record metadata")
    };
    let field = record
        .fields
        .iter()
        .find(|field| field.name == "kind")
        .unwrap();
    assert_eq!(
        record.fields[9].name, "kind",
        "later additions must preserve kind's positional index"
    );
    assert_eq!(
        field.default,
        Some(DefaultValueMetadata::Literal(LiteralMetadata::None))
    );
}

#[test]
fn task_row_display_fields_are_trailing_and_have_generated_constructor_defaults() {
    let metadata = uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_RECORD_TASKROWDTO).unwrap();
    let Metadata::Record(record) = metadata else {
        panic!("record metadata")
    };
    let names = record
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "task_id",
            "task_type",
            "status",
            "description",
            "can_resume",
            "started_at_ms",
            "error",
            "stage",
            "awaiting_plan_approval",
            "kind",
            "unread",
            "model",
            "effort",
            "agent_id"
        ],
        "new display fields may only extend the existing Kotlin/Swift constructor order"
    );
    assert_eq!(
        record.fields[10].default,
        Some(DefaultValueMetadata::Literal(LiteralMetadata::Boolean(
            false
        )))
    );
    assert_eq!(
        record.fields[11].default,
        Some(DefaultValueMetadata::Literal(LiteralMetadata::None))
    );
    assert_eq!(
        record.fields[12].default,
        Some(DefaultValueMetadata::Literal(LiteralMetadata::None))
    );
    assert_eq!(
        record.fields[13].default,
        Some(DefaultValueMetadata::Literal(LiteralMetadata::None))
    );
}
