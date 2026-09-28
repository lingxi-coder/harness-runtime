use super::*;

#[test]
fn callback_requires_the_registered_destination_and_both_parameters() {
    let (code, state) =
        parse_mobile_oauth_callback("lingxi://oauth/callback?code=auth-code&state=csrf-state")
            .expect("valid callback");
    assert_eq!(code, "auth-code");
    assert_eq!(state, "csrf-state");

    for callback in [
        "https://oauth/callback?code=auth-code&state=csrf-state",
        "lingxi://other/callback?code=auth-code&state=csrf-state",
        "lingxi://oauth/other?code=auth-code&state=csrf-state",
        "lingxi://oauth/callback?code=auth-code",
        "lingxi://oauth/callback?state=csrf-state",
    ] {
        assert!(
            parse_mobile_oauth_callback(callback).is_err(),
            "accepted {callback}"
        );
    }
}

#[test]
fn callback_state_and_flow_id_are_bound_to_the_pending_provider_session() {
    let session = PendingMobileOAuthSession {
        provider: MobileOAuthProvider::Anthropic,
        flow_id: "flow-1".to_string(),
        verifier: "verifier-never-exposed".to_string(),
        state: "state-1".to_string(),
        redirect_uri: IOS_OAUTH_REDIRECT_URI.to_string(),
        expires_at: std::time::Instant::now() + MOBILE_OAUTH_SESSION_TTL,
    };
    assert!(validate_mobile_oauth_session(&session, "flow-1", "state-1").is_ok());
    assert!(validate_mobile_oauth_session(&session, "flow-2", "state-1").is_err());
    assert!(validate_mobile_oauth_session(&session, "flow-1", "state-2").is_err());

    let expired = PendingMobileOAuthSession {
        expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
        ..session
    };
    assert!(validate_mobile_oauth_session(&expired, "flow-1", "state-1").is_err());
}

#[test]
fn taking_a_valid_session_consumes_it_but_state_mismatch_does_not() {
    let session = PendingMobileOAuthSession {
        provider: MobileOAuthProvider::Anthropic,
        flow_id: "flow-1".to_string(),
        verifier: "verifier".to_string(),
        state: "state-1".to_string(),
        redirect_uri: IOS_OAUTH_REDIRECT_URI.to_string(),
        expires_at: std::time::Instant::now() + MOBILE_OAUTH_SESSION_TTL,
    };
    let mut pending = Some(session);

    assert!(take_mobile_oauth_session(&mut pending, "flow-1", "wrong-state").is_err());
    assert!(pending.is_some());
    assert!(take_mobile_oauth_session(&mut pending, "flow-1", "state-1").is_ok());
    assert!(pending.is_none());
    assert!(take_mobile_oauth_session(&mut pending, "flow-1", "state-1").is_err());
}

#[test]
fn taking_an_expired_session_clears_it() {
    let session = PendingMobileOAuthSession {
        provider: MobileOAuthProvider::OpenAi,
        flow_id: "flow-1".to_string(),
        verifier: "verifier".to_string(),
        state: "state-1".to_string(),
        redirect_uri: IOS_OAUTH_REDIRECT_URI.to_string(),
        expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
    };
    let mut pending = Some(session);

    assert!(take_mobile_oauth_session(&mut pending, "flow-1", "state-1").is_err());
    assert!(pending.is_none());
}

#[test]
fn provider_aliases_lower_to_the_stable_credential_ids() {
    assert_eq!(
        MobileOAuthProvider::parse("anthropic").unwrap().id(),
        "anthropic"
    );
    assert_eq!(
        MobileOAuthProvider::parse("openai").unwrap().id(),
        "openai-chatgpt"
    );
    assert_eq!(
        MobileOAuthProvider::parse("openai-chatgpt").unwrap().id(),
        "openai-chatgpt"
    );
    assert!(MobileOAuthProvider::parse("openai-api-key").is_err());
}
