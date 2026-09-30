//! Scope upgrade: 403 with `required_scopes` re-triggers PKCE preserving the
//! existing `refresh_token`. On PKCE failure, the old `refresh_token` is preserved.

use lingxi_core::types::Secret;
use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
use lingxi_llm_client::auth::oauth::anthropic::{parse_scope_upgrade, ScopeUpgradeRequired};
use llm_runtime::auth::anthropic::refresh::AuthState;
use llm_runtime::auth::anthropic::scope_upgrade::run_scope_upgrade;
use llm_runtime::auth::anthropic::OAuthError;
use std::time::{Duration, SystemTime};

#[test]
fn parse_scope_upgrade_returns_some_for_required_scopes_body() {
    let body =
        r#"{"required_scopes":["read:projects","write:billing"],"granted_scopes":["read:user"]}"#;
    let parsed = parse_scope_upgrade(body).expect("must parse");
    assert_eq!(
        parsed.required,
        vec!["read:projects".to_string(), "write:billing".into()]
    );
    assert_eq!(parsed.granted, vec!["read:user".to_string()]);
}

#[test]
fn parse_scope_upgrade_returns_none_for_unrelated_403() {
    let body = r#"{"error":"rate_limited"}"#;
    assert!(parse_scope_upgrade(body).is_none());
}

#[test]
fn parse_scope_upgrade_returns_none_for_invalid_json() {
    assert!(parse_scope_upgrade("not json").is_none());
}

#[test]
fn parse_scope_upgrade_tolerates_missing_granted_scopes() {
    let body = r#"{"required_scopes":["read:projects"]}"#;
    let p = parse_scope_upgrade(body).expect("must parse");
    assert_eq!(p.required, vec!["read:projects".to_string()]);
    assert!(p.granted.is_empty());
}

#[tokio::test]
async fn run_scope_upgrade_on_pkce_failure_preserves_refresh_token() {
    // Build a state with a known refresh_token.
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state = AuthState::new_for_test(
        cfg,
        Secret::new("OLD_ACCESS".into()),
        Some(Secret::new("OLD_REFRESH_PRESERVED".into())),
        SystemTime::now() + Duration::from_secs(3600),
    );

    // Inject a PKCE runner that fails — simulates user closing the browser.
    let failing_runner = llm_runtime::auth::anthropic::scope_upgrade::PkceFailingRunner;

    let r = run_scope_upgrade(
        &state,
        ScopeUpgradeRequired {
            required: vec!["read:projects".into(), "write:billing".into()],
            granted: vec!["read:user".into()],
        },
        &failing_runner,
    )
    .await;

    // PKCE failed → ScopeRejected.
    assert!(matches!(r, Err(OAuthError::ScopeRejected { .. })));

    // CRITICAL: the refresh_token must STILL be the original "OLD_REFRESH_PRESERVED".
    let token = state.token.read().await;
    let refresh = token
        .refresh_token
        .as_ref()
        .expect("refresh preserved")
        .expose_secret()
        .clone();
    assert_eq!(refresh, "OLD_REFRESH_PRESERVED");
}

#[tokio::test]
async fn run_scope_upgrade_on_pkce_success_rotates_tokens_and_scopes() {
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state = AuthState::new_for_test(
        cfg,
        Secret::new("OLD_ACCESS".into()),
        Some(Secret::new("OLD_REFRESH".into())),
        SystemTime::now() + Duration::from_secs(3600),
    );

    let success_runner = llm_runtime::auth::anthropic::scope_upgrade::PkceFakeSuccessRunner::new(
        "NEW_ACCESS",
        "NEW_REFRESH",
        vec![
            "read:user".into(),
            "read:projects".into(),
            "write:billing".into(),
        ],
        SystemTime::now() + Duration::from_secs(7200),
    );

    let r = run_scope_upgrade(
        &state,
        ScopeUpgradeRequired {
            required: vec!["read:projects".into(), "write:billing".into()],
            granted: vec!["read:user".into()],
        },
        &success_runner,
    )
    .await;
    assert!(r.is_ok(), "scope upgrade with successful PKCE: {r:?}");

    // Tokens rotated to the values returned by the PKCE runner.
    let token = state.token.read().await;
    assert_eq!(token.access_token.expose_secret(), "NEW_ACCESS");
    assert_eq!(
        token.refresh_token.as_ref().unwrap().expose_secret(),
        "NEW_REFRESH"
    );
    assert!(token.scopes.contains(&"read:projects".to_string()));
    assert!(token.scopes.contains(&"write:billing".to_string()));
}
