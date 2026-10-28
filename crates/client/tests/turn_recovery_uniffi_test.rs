//! Lock the native constructor contract for the required recovery marker.

#![cfg(feature = "uniffi")]

use client::protocol::events::UNIFFI_META_CLIENT_RECORD_TURNRECOVERYSNAPSHOTDTO;
use uniffi_meta::{Metadata, Type};

#[test]
fn projection_uncertainty_is_a_required_boolean_in_recovery_record_metadata() {
    let Metadata::Record(record) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_RECORD_TURNRECOVERYSNAPSHOTDTO).unwrap()
    else {
        panic!("expected TurnRecoverySnapshotDto record metadata");
    };
    assert_eq!(record.name, "TurnRecoverySnapshotDto");
    assert_eq!(
        record
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        [
            "session_id",
            "turn_id",
            "revision",
            "state",
            "first_sequence",
            "last_sequence",
            "safe_to_resume",
            "projection_uncertain",
            "reason",
        ]
    );
    let field = &record.fields[7];
    assert!(matches!(&field.ty, Type::Boolean));
    assert_eq!(
        field.default, None,
        "native callers must supply uncertainty"
    );
}
