// lingxi-code/crates/core/tests/settings_module_exists.rs
//! Smoke test: the settings module compiles and exports `SettingsError`.
//! This is the very first failing test of M3-01.

#[test]
fn settings_module_re_exports_error_type() {
    // The mere fact this compiles is the assertion.
    fn _accepts(_: lingxi_core::settings::SettingsError) {}
}
