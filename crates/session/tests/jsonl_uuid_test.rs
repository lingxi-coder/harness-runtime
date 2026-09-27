//! `validate_uuid` 1:1 with sessionStoragePortable.ts:23-29 regex.

use session::jsonl::uuid::validate_uuid;

#[test]
fn five_valid_uuids_accepted() {
    assert!(validate_uuid("00000000-0000-0000-0000-000000000000"));
    assert!(validate_uuid("ffffffff-ffff-ffff-ffff-ffffffffffff"));
    assert!(validate_uuid("0a1b2c3d-4e5f-6789-abcd-ef0123456789"));
    // Case-insensitive — uppercase MUST be accepted (claude-code uses /i).
    assert!(validate_uuid("0A1B2C3D-4E5F-6789-ABCD-EF0123456789"));
    // Mixed case.
    assert!(validate_uuid("0a1B2c3D-4e5F-6789-aBCD-eF0123456789"));
}

#[test]
fn five_invalid_uuids_rejected() {
    // Missing a hyphen.
    assert!(!validate_uuid("0a1b2c3d4e5f-6789-abcd-ef0123456789"));
    // Wrong group length.
    assert!(!validate_uuid("0a1b2c3-4e5f-6789-abcd-ef0123456789"));
    // Non-hex char.
    assert!(!validate_uuid("0g1b2c3d-4e5f-6789-abcd-ef0123456789"));
    // Trailing junk.
    assert!(!validate_uuid("0a1b2c3d-4e5f-6789-abcd-ef0123456789x"));
    // Empty.
    assert!(!validate_uuid(""));
}
