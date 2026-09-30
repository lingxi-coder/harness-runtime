//! Regression tests for oauth schema test.

use telemetry::tengu::oauth;

#[test]
fn all_8_oauth_event_names_are_locked() {
    let names: &[&str] = &[
        oauth::REFRESH_STARTED,
        oauth::REFRESH_SUCCEEDED,
        oauth::REFRESH_FAILED,
        oauth::SCOPE_UPGRADED,
        oauth::PROACTIVE_CANCELED,
        oauth::PKCE_STARTED,
        oauth::PKCE_COMPLETED,
        oauth::PKCE_FAILED,
    ];
    assert_eq!(names.len(), 8);
    for n in names {
        assert!(n.starts_with("tengu_oauth_"));
    }
    // M3-04 plan locks these byte-for-byte.
    assert_eq!(oauth::REFRESH_STARTED, "tengu_oauth_refresh_started");
    assert_eq!(oauth::REFRESH_SUCCEEDED, "tengu_oauth_refresh_succeeded");
    assert_eq!(oauth::REFRESH_FAILED, "tengu_oauth_refresh_failed");
    assert_eq!(oauth::SCOPE_UPGRADED, "tengu_oauth_scope_upgraded");
    assert_eq!(oauth::PROACTIVE_CANCELED, "tengu_oauth_proactive_canceled");
}
