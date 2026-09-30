//! Regression tests for settings schema test.

use telemetry::tengu::settings;

#[test]
fn all_3_settings_event_names_are_locked() {
    let names: &[&str] = &[
        settings::LOADED,
        settings::INVALID_ENV,
        settings::PARSE_ERROR,
    ];
    assert_eq!(names.len(), 3);
    for n in names {
        assert!(n.starts_with("tengu_settings_"));
    }
    // M3-01 plan locks these byte-for-byte.
    assert_eq!(settings::LOADED, "tengu_settings_loaded");
    assert_eq!(settings::INVALID_ENV, "tengu_settings_invalid_env");
    assert_eq!(settings::PARSE_ERROR, "tengu_settings_parse_error");
}

#[test]
fn registry_is_exactly_410_entries() {
    // The canonical category-by-category recount lives in
    // event_name_completeness_test. Keep this duplicate doctor-facing guard in
    // sync with that registry count.
    //
    // NOTE: this assertion is a DUPLICATE of the one in
    // event_name_completeness_test. Both must move together — the §11 change
    // updated only that one and this file went red, which is the third time a
    // count-lock site was missed on this backlog.
    assert_eq!(telemetry::tengu::ALL_EVENT_NAMES.len(), 410);
}
