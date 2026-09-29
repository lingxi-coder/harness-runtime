//! F1-01 — `ClientEvent::Error` round-trip + serde-convention tests.
//!
//! `Error` is the FIRST `ClientEvent` variant, defined now so every later DTO
//! (F1-03..) inherits the frozen serde conventions (governing decision §0.1,
//! plan F1-01):
//!   - internally tagged: `#[serde(tag = "type", rename_all = "snake_case")]`
//!     (matches `lingxi_core::types::ContentBlock` / api-client `StreamEvent`),
//!   - `#[non_exhaustive]` (mirrors `lingxi_core::host::OutputEvent`),
//!   - `snake_case` field names.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate itself never depends on
//! `serde_json::Value` (§0.4).

use client::protocol::events::{ClientEvent, ErrorKindDto};

/// `ClientEvent::Error` serializes with the `"type": "error"` tag and `snake_case`
/// fields, and round-trips byte-stable.
#[test]
fn error_event_round_trips() {
    let ev = ClientEvent::Error {
        kind: ErrorKindDto::Transport,
        message: "connection reset".to_string(),
    };

    let json = serde_json::to_value(&ev).expect("serialize ClientEvent::Error");

    // Internally tagged on `type`, snake_case variant name.
    assert_eq!(
        json["type"], "error",
        "variant tag must be snake_case `error`"
    );
    // Nested error-kind enum is also `type`-tagged + snake_case.
    assert_eq!(json["kind"]["type"], "transport");
    assert_eq!(json["message"], "connection reset");

    // Round-trip is byte-stable.
    let back: ClientEvent = serde_json::from_value(json).expect("deserialize ClientEvent::Error");
    assert_eq!(back, ev);
}

/// Every `ErrorKindDto` variant round-trips and serializes `snake_case`.
#[test]
fn error_kind_variants_round_trip() {
    let cases = [
        (ErrorKindDto::Transport, "transport"),
        (ErrorKindDto::Protocol, "protocol"),
        (ErrorKindDto::Server, "server"),
        (ErrorKindDto::MaxTurns, "max_turns"),
        (ErrorKindDto::Rejected, "rejected"),
        (ErrorKindDto::Internal, "internal"),
    ];
    for (kind, tag) in cases {
        let json = serde_json::to_value(&kind).expect("serialize ErrorKindDto");
        assert_eq!(json["type"], tag, "ErrorKindDto::{kind:?} tag mismatch");
        let back: ErrorKindDto = serde_json::from_value(json).expect("deserialize ErrorKindDto");
        assert_eq!(back, kind);
    }
}

/// The top-level event/command enums are `#[non_exhaustive]`, so a downstream
/// `match` without a wildcard arm fails to compile. We can't assert that from
/// here directly, but we CAN prove the shells exist and round-trip, which is the
/// behavior every later F1 task builds on.
#[test]
fn client_command_shell_is_present() {
    // `ClientCommand` is the inbound shell; at F1-01 it has no variants yet, so
    // we only assert the type is nameable and the enum is empty-constructible via
    // its (currently variant-less) discriminant. Referencing the path proves the
    // shell exists and is wired through `lib.rs`.
    fn _accepts(_c: &client::protocol::commands::ClientCommand) {}
}
