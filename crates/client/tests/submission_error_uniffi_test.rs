//! Lock the native result-error variant and its existing ordinal prefix.

#![cfg(feature = "uniffi")]

use client::protocol::error::UNIFFI_META_CLIENT_ERROR_CLIENTERROR;
use uniffi_meta::{EnumShape, Metadata, Type};

#[test]
fn submission_not_admitted_has_a_distinct_native_error_variant() {
    let Metadata::Enum(error) =
        uniffi_meta::read_metadata(&UNIFFI_META_CLIENT_ERROR_CLIENTERROR).unwrap()
    else {
        panic!("expected ClientError metadata");
    };
    assert_eq!(error.name, "ClientError");
    assert!(matches!(error.shape, EnumShape::Error { flat: false }));
    assert_eq!(
        error
            .variants
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Transport",
            "Protocol",
            "Rejected",
            "NotFound",
            "Internal",
            "SubmissionNotAdmitted"
        ]
    );
    let added = error.variants.last().unwrap();
    assert_eq!(added.fields.len(), 1);
    assert_eq!(added.fields[0].name, "message");
    assert!(matches!(&added.fields[0].ty, Type::String));
}
