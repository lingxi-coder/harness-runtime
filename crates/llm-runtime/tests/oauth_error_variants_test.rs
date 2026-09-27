//! Spec §5 §5: `OAuthError` must extend with `RefreshExpired` / `ScopeRejected` /
//! `ProactiveFailed`. Display strings are byte-locked.

use llm_runtime::oauth::anthropic::OAuthError;

#[test]
fn refresh_expired_display_is_byte_locked() {
    let e = OAuthError::RefreshExpired;
    assert_eq!(format!("{e}"), "Session expired. Re-authenticate?");
}

#[test]
fn scope_rejected_display_is_byte_locked() {
    let e = OAuthError::ScopeRejected {
        required: vec!["read:projects".into()],
        granted: vec![],
    };
    assert_eq!(format!("{e}"), "Scope upgrade denied by provider");
}

#[test]
fn proactive_failed_display_wraps_source() {
    let inner = OAuthError::RefreshExpired;
    let e = OAuthError::ProactiveFailed {
        source: Box::new(inner),
    };
    assert_eq!(
        format!("{e}"),
        "proactive refresh failed: Session expired. Re-authenticate?",
    );
}

#[test]
fn existing_callback_variant_preserved() {
    let e = OAuthError::Callback("oops".into());
    assert!(format!("{e}").contains("callback failed: oops"));
}

#[test]
fn existing_token_exchange_variant_preserved() {
    let e = OAuthError::TokenExchange("bad".into());
    assert!(format!("{e}").contains("token exchange failed: bad"));
}
