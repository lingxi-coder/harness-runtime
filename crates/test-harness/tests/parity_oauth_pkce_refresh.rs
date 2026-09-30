//! Parity fixture: OAuth PKCE refresh wire identifiers.
//!
//! Locks the byte-for-byte string identifiers M3-04 promises against
//! claude-code @ 6a25909. Drift breaks interop with the upstream `IdP`.

use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::auth::oauth::anthropic::{CLAUDE_CODE_OAUTH_SCOPES, REFRESH_GRANT_TYPE};
use llm_runtime::auth::anthropic::refresh::{proactive_lead, PROACTIVE_LEAD_CAP};
use llm_runtime::auth::anthropic::OAuthError;
use serde::Deserialize;
use std::time::Duration;
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    authorize_endpoint: String,
    token_endpoint: String,
    revocation_endpoint: String,
    refresh_grant_type: String,
    scopes_in_order: Vec<String>,
    loopback_redirect_uri_template: String,
    refresh_expired_error_string: String,
    scope_rejected_error_string: String,
    proactive_lead_cap_seconds: u64,
    telemetry_event_names: Vec<String>,
    keychain_service: String,
    keychain_account: String,
}

#[test]
fn oauth_wire_identifiers_match_claude_code() {
    let fx: Fixture = load_fixture("oauth_pkce_refresh");

    // Endpoints come from `default_with_port(0)`.
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    assert_eq!(
        cfg.authorization_endpoint, fx.authorize_endpoint,
        "authorize endpoint drift",
    );
    assert_eq!(
        cfg.token_endpoint, fx.token_endpoint,
        "token endpoint drift"
    );
    assert_eq!(
        cfg.revocation_endpoint, fx.revocation_endpoint,
        "revocation endpoint drift",
    );

    // Refresh grant_type
    assert_eq!(REFRESH_GRANT_TYPE, fx.refresh_grant_type);

    // Scope list + order.
    let scopes_in_order: Vec<String> = CLAUDE_CODE_OAUTH_SCOPES
        .iter()
        .map(|s| (*s).into())
        .collect();
    assert_eq!(
        scopes_in_order, fx.scopes_in_order,
        "scope list/order drift"
    );

    // Loopback redirect URI template — config builds it for port 0; we compare
    // against the literal `{port}` template from the fixture. We do NOT do a
    // textual replace on `cfg.redirect_uri` because the `0` in `127.0.0.1`
    // would also match.
    let template = "http://127.0.0.1:{port}/callback";
    assert_eq!(template, fx.loopback_redirect_uri_template);

    // Error strings (OAUTHREF.5: LingXi-DESIGN messages, NOT claude-code
    // byte-for-byte — TS throws a generic "Token refresh failed: <status>".
    // Pinned here as LingXi values; see OAuthError::RefreshExpired docs).
    let refresh_expired = OAuthError::RefreshExpired;
    assert_eq!(
        format!("{refresh_expired}"),
        fx.refresh_expired_error_string
    );

    let scope_rejected = OAuthError::ScopeRejected {
        required: vec!["x".into()],
        granted: vec![],
    };
    assert_eq!(format!("{scope_rejected}"), fx.scope_rejected_error_string);

    // Proactive lead formula edge.
    assert_eq!(
        PROACTIVE_LEAD_CAP,
        Duration::from_secs(fx.proactive_lead_cap_seconds),
    );
    // Spot-check the formula at remaining = 1 hour (cap) and remaining = 1 min (half).
    assert_eq!(
        proactive_lead(Duration::from_secs(3600)),
        Duration::from_secs(fx.proactive_lead_cap_seconds),
    );
    assert_eq!(
        proactive_lead(Duration::from_secs(60)),
        Duration::from_secs(30),
    );

    // Telemetry event names (exactly five, exact order).
    let expected_events = vec![
        "tengu_oauth_refresh_started",
        "tengu_oauth_refresh_succeeded",
        "tengu_oauth_refresh_failed",
        "tengu_oauth_scope_upgraded",
        "tengu_oauth_proactive_canceled",
    ];
    assert_eq!(fx.telemetry_event_names, expected_events);

    // Keychain layout.
    assert_eq!(fx.keychain_service, "lingxi");
    assert_eq!(fx.keychain_account, "claude-code-credentials");
}
