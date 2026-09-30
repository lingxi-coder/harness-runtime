//! Regression tests for memory schema test.

use telemetry::tengu::memory;

#[test]
fn all_12_memory_event_names_are_locked() {
    let names: &[&str] = &[
        memory::LOADED,
        memory::LOAD_FAILED,
        memory::CASE_MISMATCH,
        memory::FILE_TOO_LARGE,
        memory::SECRET_REDACTED,
        memory::AGE_PENALTY_APPLIED,
        memory::DROPPED_FOR_AGE,
        memory::RANK_COMPUTED,
        memory::TEAM_SCAN_STARTED,
        memory::TEAM_SCAN_COMPLETED,
        memory::TEAM_SCAN_FAILED,
        memory::CLAUDE_MD_HIERARCHY_WALKED,
    ];
    assert_eq!(names.len(), 12);
    for n in names {
        assert!(n.starts_with("tengu_memory_"));
    }
    // M3-02 plan locks these byte-for-byte.
    assert_eq!(memory::CASE_MISMATCH, "tengu_memory_case_mismatch");
    assert_eq!(memory::FILE_TOO_LARGE, "tengu_memory_file_too_large");
    assert_eq!(memory::SECRET_REDACTED, "tengu_memory_secret_redacted");
}

#[test]
fn case_mismatch_payload_routes_path_via_pii_tagged() {
    use telemetry::{pii::PiiTagged, Verified};
    let p = memory::CaseMismatchPayload {
        actual_name: Verified::assert_safe("claude.md".into()),
        path: PiiTagged::assert_pii_tagged_column("/Users/u/proj/claude.md".into()),
    };
    let _ = serde_json::to_string(&p).unwrap();
}
