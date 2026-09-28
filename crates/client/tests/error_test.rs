//! F1-07 — `ClientError` round-trip + FFI-flatness tests.
//!
//! `ClientError` is the flat, `#[derive(uniffi::Error)]`-**ready** error type
//! that mirrors command failure modes (governing plan F1-07). It is DISTINCT
//! from [`client::protocol::events::ErrorKindDto`] — that is the coarse class
//! carried by the streaming `ClientEvent::Error`; `ClientError` is the typed
//! result error returned across an FFI / RPC `submit()` boundary (used by the
//! mobile `submit(command) -> Result<(), ClientError>` entry point in F3-05).
//!
//! Per plan F1-07, the actual `#[derive(uniffi::Error)]` is added in F3-01 (once
//! `uniffi` is vendored/pinned offline in F3-00). What F1-07 freezes is the
//! FLAT SHAPE: every variant carries a single `message: String` field — no
//! nested non-FFI payloads — so the derive lands without a shape change. (A
//! struct variant, not a tuple variant, so the §0.1 `#[serde(tag = "type")]`
//! convention holds: serde cannot internally-tag a `String` newtype variant.)
//! The check here is the Display strings + serde round-trip; the compile-time
//! flatness becomes the real `uniffi::Error` check in F3-01.
//!
//! `serde_json` is a DEV-ONLY dep — the contract crate itself never depends on
//! `serde_json::Value` (§0.4).

use client::protocol::error::ClientError;

/// Every `ClientError` variant round-trips byte-stable through serde and is
/// internally tagged on `type` with `snake_case` variant names (the frozen serde
/// convention every DTO inherits, §0.1).
#[test]
fn client_error_round_trips() {
    let cases = [
        ClientError::Transport {
            message: "connection reset".to_string(),
        },
        ClientError::Protocol {
            message: "malformed frame".to_string(),
        },
        ClientError::Rejected {
            message: "permission denied by user".to_string(),
        },
        ClientError::NotFound {
            message: "session 7f3a not found".to_string(),
        },
        ClientError::Internal {
            message: "unexpected nil orchestrator".to_string(),
        },
    ];

    for err in cases {
        let json = serde_json::to_value(&err).expect("serialize ClientError");
        // Internally tagged on `type`, snake_case variant names.
        assert!(
            json.get("type").and_then(|t| t.as_str()).is_some(),
            "ClientError must be internally tagged on `type`: {json}"
        );
        // The single flat `message` field is carried alongside the tag.
        assert!(
            json.get("message").and_then(|m| m.as_str()).is_some(),
            "ClientError must carry a flat `message` field: {json}"
        );
        let back: ClientError = serde_json::from_value(json).expect("deserialize ClientError");
        assert_eq!(back, err, "round-trip must be byte-stable");
    }
}

/// The variant tags serialize `snake_case` (matches `ErrorKindDto`).
#[test]
fn client_error_tags_are_snake_case() {
    let cases = [
        (
            ClientError::Transport {
                message: "x".into(),
            },
            "transport",
        ),
        (
            ClientError::Protocol {
                message: "x".into(),
            },
            "protocol",
        ),
        (
            ClientError::Rejected {
                message: "x".into(),
            },
            "rejected",
        ),
        (
            ClientError::NotFound {
                message: "x".into(),
            },
            "not_found",
        ),
        (
            ClientError::Internal {
                message: "x".into(),
            },
            "internal",
        ),
    ];
    for (err, tag) in cases {
        let json = serde_json::to_value(&err).expect("serialize ClientError");
        assert_eq!(json["type"], tag, "ClientError::{err:?} tag mismatch");
    }
}

/// `error_variants_are_ffi_flat` — the F1-07 FFI-readiness anchor. Every variant
/// is flat (unit or single-`String` tuple), so the F3-01 `uniffi::Error` derive
/// lands without a shape change. We exercise that flat shape by constructing one
/// of each variant and asserting its `thiserror` `Display` string. Under
/// `--features uniffi` in F3-01 the actual derive turns this into a real
/// compile-time check; here it asserts the human-readable failure messages.
#[test]
fn error_variants_are_ffi_flat() {
    assert_eq!(
        ClientError::Transport {
            message: "connection reset".into()
        }
        .to_string(),
        "transport error: connection reset"
    );
    assert_eq!(
        ClientError::Protocol {
            message: "malformed frame".into()
        }
        .to_string(),
        "protocol error: malformed frame"
    );
    assert_eq!(
        ClientError::Rejected {
            message: "denied by user".into()
        }
        .to_string(),
        "request rejected: denied by user"
    );
    assert_eq!(
        ClientError::NotFound {
            message: "session 7f3a".into()
        }
        .to_string(),
        "not found: session 7f3a"
    );
    assert_eq!(
        ClientError::Internal {
            message: "unexpected nil orchestrator".into()
        }
        .to_string(),
        "internal error: unexpected nil orchestrator"
    );
}

/// `ClientError` is a real `std::error::Error` (via `thiserror`), so it composes
/// with `?` and `Box<dyn Error>` at the transport/FFI call sites.
#[test]
fn client_error_is_std_error() {
    fn _accepts<E: std::error::Error + Send + Sync + 'static>(_e: E) {}
    _accepts(ClientError::Internal {
        message: "boom".into(),
    });
}
