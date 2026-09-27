//! F1-00 scaffold test.
//!
//! The "red" at F1-00 is structural: before the crate is created and wired into
//! the workspace `members`, this integration test cannot compile or be run at
//! all (`cargo test -p client-protocol` errors with "package not found"). Once
//! the crate exists, builds, and is workspace-wired, the trivial assertion below
//! goes green — proving the crate links as an engine-tier dependency target.

/// The crate compiles and links. Subsequent F1-* tasks add real DTO tests; this
/// is the foundation gate that the empty, workspace-wired crate builds.
#[test]
fn crate_compiles() {
    // Referencing the crate forces it to link. The F1-00 `SCAFFOLD_OK` marker was
    // replaced by the real `CLIENT_PROTOCOL_VERSION` const in F1-01 (per the
    // F1-00 plan note), so we reference that instead.
    assert!(!client_protocol::version::CLIENT_PROTOCOL_VERSION.is_empty());
}
