//! Drives the trybuild `compile_fail` / `compile_pass` scenarios that exercise
//! the audit logic via the `_audit_source!("...")` doc-hidden macro.

#[test]
fn audit_compile_pass() {
    let t = trybuild::TestCases::new();
    t.pass("tests/compile_pass/good_payload.rs");
}

#[test]
fn audit_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/bare_string.rs");
    t.compile_fail("tests/compile_fail/missing_deny_unknown.rs");
    t.compile_fail("tests/compile_fail/missing_non_exhaustive.rs");
}
