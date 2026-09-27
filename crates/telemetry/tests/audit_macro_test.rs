//! Positive case: the audit macro accepts the production tengu tree.
//!
//! The macro is invoked at the bottom of `tengu/mod.rs`; if the tree is
//! invalid, the telemetry crate fails to compile and this test never runs.
//! Reaching this test at all proves the audit accepted the schema.

#[test]
fn audit_macro_path_resolves() {
    // No-op — if the macro path were wrong, `tengu/mod.rs` would fail at
    // compile time and we'd never get here.
    assert!(!telemetry::tengu::ALL_EVENT_NAMES.is_empty());
}
