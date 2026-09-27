//! T6 tests — exact-string Display locks for `LoaderError` variants.

use session::jsonl::LoaderError;
use uuid::Uuid;

#[test]
fn session_not_found_display_is_locked() {
    let e = LoaderError::SessionNotFound {
        arg: "11111111-1111-1111-1111-111111111111".into(),
    };
    assert_eq!(
        format!("{e}"),
        "Session 11111111-1111-1111-1111-111111111111 was not found."
    );
}

#[test]
fn chain_broken_display_is_locked() {
    let e = LoaderError::ChainBroken {
        arg: "11111111-1111-1111-1111-111111111111".into(),
        at_uuid: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
    };
    assert_eq!(
        format!("{e}"),
        "Session 11111111-1111-1111-1111-111111111111 corrupted: parentUuid chain broken at message 22222222-2222-2222-2222-222222222222."
    );
}

#[test]
fn session_id_mismatch_display_is_locked() {
    let e = LoaderError::SessionIdMismatch {
        arg: "aa".into(),
        expected: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
        got: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
    };
    assert_eq!(
        format!("{e}"),
        "Session aa corrupted: sessionId mismatch (expected 11111111-1111-1111-1111-111111111111, got 22222222-2222-2222-2222-222222222222)."
    );
}

#[test]
fn invalid_selection_display_is_locked() {
    let e = LoaderError::InvalidSelection;
    assert_eq!(format!("{e}"), "Invalid selection (3 attempts). Aborting.");
}

#[test]
fn empty_directory_display_is_locked() {
    let e = LoaderError::EmptyDirectory;
    assert_eq!(format!("{e}"), "No conversations found to resume.");
}

#[test]
fn io_display_wraps_source() {
    let e = LoaderError::Io {
        arg: "x.jsonl".into(),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "perm"),
    };
    let s = format!("{e}");
    assert!(s.starts_with("Session x.jsonl I/O error: "), "{s}");
    assert!(s.contains("perm"), "{s}");
}
