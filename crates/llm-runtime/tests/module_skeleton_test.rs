//! Smoke test: the new M3-04 modules compile and export the expected symbols.
//! This is the very first failing test of M3-04.

#[test]
fn refresh_module_exports_auth_state_and_driver() {
    fn _accepts_state(_: &llm_runtime::auth::anthropic::refresh::AuthState) {}
    fn _accepts_driver(_: &llm_runtime::auth::anthropic::refresh::RefreshDriver) {}
}

#[test]
fn scope_upgrade_uses_sdk_parser() {
    let _: Option<lingxi_llm_client::auth::oauth::anthropic::ScopeUpgradeRequired> =
        lingxi_llm_client::auth::oauth::anthropic::parse_scope_upgrade("{}");
}

#[test]
fn config_default_uses_current_claude_code_endpoints() {
    use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
    use lingxi_llm_client::auth::oauth::anthropic::CLAUDE_CODE_OAUTH_SCOPES;
    let c = ClaudeAiOAuthConfig::default_with_port(0);
    assert_eq!(
        c.authorization_endpoint,
        "https://claude.com/cai/oauth/authorize"
    );
    assert_eq!(
        c.token_endpoint,
        "https://platform.claude.com/v1/oauth/token"
    );
    // Scope order is locked.
    assert_eq!(
        c.scopes,
        CLAUDE_CODE_OAUTH_SCOPES
            .iter()
            .map(|scope| (*scope).to_string())
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn auth_state_new_returns_arc() {
    use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
    use llm_runtime::auth::anthropic::refresh::AuthState;
    use protocol::Secret;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    // We only build a state with no I/O hooks attached yet — Task 4 wires the
    // full constructor. For now, the state struct must exist with a `new(...)`
    // signature we can call against in-memory mocks.
    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state: Arc<AuthState> = AuthState::new_for_test(
        cfg,
        Secret::new("initial_access".to_string()),
        Some(Secret::new("initial_refresh".to_string())),
        SystemTime::now() + Duration::from_secs(3600),
    );
    let _ = state;
}

#[tokio::test]
async fn refresh_driver_new_holds_state() {
    use lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig;
    use llm_runtime::auth::anthropic::refresh::{AuthState, RefreshDriver};
    use protocol::Secret;
    use std::time::{Duration, SystemTime};

    let cfg = ClaudeAiOAuthConfig::default_with_port(0);
    let state = AuthState::new_for_test(
        cfg,
        Secret::new("a".to_string()),
        None,
        SystemTime::now() + Duration::from_secs(3600),
    );
    let _driver = RefreshDriver::new(state);
}
