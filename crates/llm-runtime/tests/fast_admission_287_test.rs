//! Organization admission reaches the SDK status GET and final message POST.
use async_trait::async_trait;
use llm_runtime::model::{
    fast_admission::Policy, thinking::ThinkingConfig, user_agent::UserAgentEnv,
};
use llm_runtime::{
    ApiService, ClientConfig, Credential, CredentialProvider, CredentialScope, LlmError,
    LlmRequest, ModelRuntime, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

#[derive(Debug)]
struct Credentials(Mutex<String>);
impl CredentialProvider for Credentials {
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            Ok(llm_runtime::AnthropicAuthSnapshot::from_credential(
                scope.clone(),
                self.load(scope).await?,
            ))
        })
    }

    fn load<'a>(
        &'a self,
        _: &'a CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<Credential, LlmError>> {
        let value = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(Credential::ApiKey(value)) })
    }
}
#[derive(Default)]
struct Capture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let status_get = request.method == "GET";
        let enabled = request
            .headers
            .iter()
            .any(|(name, value)| name == "x-api-key" && value == "enabled-key");
        self.0.lock().unwrap().push(request);
        Ok(lingxi_llm_client::HttpResponse {
            status: if status_get { 200 } else { 400 },
            headers: vec![],
            body: if status_get {
                json!({"enabled":enabled,"disabled_reason":"preference"})
                    .to_string()
                    .into_bytes()
            } else {
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"fixture"}}"#
                    .to_vec()
            }
            .into(),
        }
        .into())
    }
}
fn config() -> ClientConfig {
    let providers = [("direct", "https://api.anthropic.com"), ("compatible", "https://fixture.example")].into_iter().map(|(profile, base)| json!({
        "provider_id":"anthropic_first_party","profile_name":profile,"base_url":base,"protocol":"anthropic_messages","auth":"api_key","credential":{"type":"host_managed","id":"test"},
        "models":[{"display_model":"Opus","request_model":"claude-opus-5-5","billing_model":"claude-opus-5-5","aliases":["opus"],"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
    })).collect::<Vec<_>>();
    serde_json::from_value(json!({"providers":providers})).unwrap()
}
fn service(
    credentials: Arc<dyn CredentialProvider>,
    capture: Arc<Capture>,
    policy: Arc<Mutex<Policy>>,
) -> ApiService {
    let client = Arc::new(
        ModelRuntime::from_config(config())
            .unwrap()
            .with_credential_provider(credentials),
    );
    ApiService::new_with_routing(
        client,
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
    .with_fast_policy_source(Arc::new(move || policy.lock().unwrap().clone()))
    .with_interactive_session(false)
    .with_thinking(ThinkingConfig::Disabled)
}
async fn assert_wire(service: &ApiService, capture: &Capture, fast: bool) {
    let mut request = LlmRequest::new("opus").with_user_text("test");
    request.profile = Some("direct".into());
    request.set_speed(Some("fast".into())).unwrap();
    let _ = service.stream_request(request).await;
    let calls = capture.0.lock().unwrap();
    let wire = calls.last().unwrap();
    assert_eq!(wire.method, "POST");
    let body: Value = serde_json::from_slice(&wire.body).unwrap();
    assert_eq!(
        body.get("speed").and_then(Value::as_str),
        fast.then_some("fast")
    );
    let beta = wire
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("");
    assert_eq!(beta.contains("fast-mode-2026-02-01"), fast);
}
#[tokio::test]
async fn account_admission_rotation_policy_and_observer_reach_the_wire() {
    // This dedicated test process owns all Fast environment gates.
    for name in [
        branding::MODEL_CAPABILITIES_ENV,
        branding::DISABLE_FAST_MODE_ENV,
        branding::SKIP_FAST_MODE_ORG_CHECK_ENV,
        branding::SKIP_FAST_MODE_NETWORK_ERRORS_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
    ] {
        std::env::remove_var(name);
    }
    let credentials = Arc::new(Credentials(Mutex::new("enabled-key".into())));
    let capture = Arc::new(Capture::default());
    let policy = Arc::new(Mutex::new(Policy::default()));
    let service = service(credentials.clone(), capture.clone(), policy.clone());
    service
        .validate_fast_enable("opus", Some("direct"))
        .await
        .unwrap();
    service
        .validate_fast_enable("opus", Some("direct"))
        .await
        .unwrap();
    assert_eq!(
        capture.0.lock().unwrap().len(),
        1,
        "30-second prefetch window"
    );
    assert_wire(&service, &capture, true).await;

    *credentials.0.lock().unwrap() = "denied-key".into();
    let refusal = service
        .validate_fast_enable("opus", Some("direct"))
        .await
        .unwrap_err();
    assert!(
        matches!(refusal, LlmError::PermissionDenied { message } if message.contains("disabled by your organization"))
    );
    assert_wire(&service, &capture, false).await;
    policy.lock().unwrap().flag_fast = true;
    assert!(
        service
            .validate_fast_enable("opus", Some("direct"))
            .await
            .is_err(),
        "explicit flag cannot bypass authoritative preference"
    );
    std::env::set_var(branding::SKIP_FAST_MODE_ORG_CHECK_ENV, "true");
    assert!(
        service
            .validate_fast_enable("opus", Some("direct"))
            .await
            .is_err(),
        "skip cannot bypass authoritative preference"
    );
    std::env::remove_var(branding::SKIP_FAST_MODE_ORG_CHECK_ENV);

    *credentials.0.lock().unwrap() = "enabled-key".into();
    service
        .validate_fast_enable("opus", Some("direct"))
        .await
        .unwrap();
    policy.lock().unwrap().policy_fast = Some(false);
    assert!(service
        .validate_fast_enable("opus", Some("direct"))
        .await
        .is_err());
    assert_wire(&service, &capture, false).await;
    policy.lock().unwrap().policy_fast = None;
    policy.lock().unwrap().policy_session_opt_in = Some(true);
    assert!(
        service
            .validate_fast_enable("opus", Some("direct"))
            .await
            .is_err(),
        "managed per-session opt-in restricts the noninteractive toggle"
    );
    policy.lock().unwrap().policy_session_opt_in = None;

    let before = capture.0.lock().unwrap().len();
    assert!(service
        .validate_fast_enable("opus", Some("compatible"))
        .await
        .is_err());
    assert_eq!(
        capture.0.lock().unwrap().len(),
        before,
        "compatible route makes no status GET"
    );
    policy.lock().unwrap().flag_fast = false;
    service.account_change_observer().account_changed();
    assert_wire(&service, &capture, false).await;
    service
        .validate_fast_enable("opus", Some("direct"))
        .await
        .unwrap();
    assert_wire(&service, &capture, true).await;
    let calls = capture.0.lock().unwrap();
    assert_eq!(calls.iter().filter(|call| call.method == "GET").count(), 4);
    drop(calls);
    request_snapshot_binding_reaches_both_drivers().await;
    scoped_oauth_refresh_uses_the_real_sdk_and_credential_driver().await;
    alternate_auth_sources_reach_status_and_preserve_model_auth().await;
}

struct AlternateCapture {
    calls: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    renewed_profile: bool,
    denied: bool,
    unsupported_refresh: bool,
}
#[async_trait]
impl Transport for AlternateCapture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let calls = self.calls.lock().unwrap();
        let previous_gets = calls.iter().filter(|call| call.method == "GET").count();
        drop(calls);
        let (status, body) = if request.method == "GET" {
            let key = request.headers.iter().find(|(name, _)| name == "x-api-key");
            if let Some((_, key)) = key {
                assert_eq!(key, "alternate-key");
                assert!(!request
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("authorization")));
                (
                    200,
                    json!({"enabled":!self.denied,"disabled_reason":"free"}),
                )
            } else if self.unsupported_refresh && previous_gets > 0 {
                (200, json!({"enabled":true}))
            } else {
                assert!(request
                    .headers
                    .iter()
                    .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                        && value == "Bearer original-oauth"));
                (401, json!({"error":"fixture"}))
            }
        } else if request.url.ends_with("/token") {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["grant_type"], "refresh_token");
            (
                200,
                json!({"access_token":"renewed-oauth","refresh_token":"renewed-refresh","expires_in":3600,"scope":if self.renewed_profile {"user:profile user:inference"} else {"user:inference"}}),
            )
        } else {
            assert!(!request.headers.iter().any(|(name, _)| name == "x-api-key"));
            (
                400,
                json!({"type":"error","error":{"type":"invalid_request_error","message":"fixture"}}),
            )
        };
        self.calls.lock().unwrap().push(request);
        Ok(lingxi_llm_client::HttpResponse {
            status,
            headers: vec![],
            body: body.to_string().into_bytes().into(),
        }
        .into())
    }
}
struct AlternateCredentials {
    inner: llm_runtime::auth::anthropic::OAuthCredentialProvider,
    snapshots: AtomicUsize,
    missing_oauth: bool,
    unsupported_refresh: bool,
    snapshot_error: bool,
}
impl std::fmt::Debug for AlternateCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AlternateCredentials")
            .finish_non_exhaustive()
    }
}
impl CredentialProvider for AlternateCredentials {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<Credential, LlmError>> {
        if self.missing_oauth {
            Box::pin(async {
                Err(LlmError::Authentication {
                    message: String::new(),
                })
            })
        } else {
            self.inner.load(scope)
        }
    }
    fn refresh<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> llm_runtime::BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        if self.unsupported_refresh {
            Box::pin(async { Ok(None) })
        } else {
            self.inner.refresh(scope, rejected)
        }
    }
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            scope.provider_id,
            llm_runtime::ProviderId::AnthropicFirstParty
        );
        assert_eq!(scope.profile_name, "direct");
        assert_eq!(scope.credential_id.as_deref(), Some("test"));
        Box::pin(async move {
            let mut snapshot = if self.missing_oauth {
                llm_runtime::AnthropicAuthSnapshot::default()
            } else {
                self.inner.anthropic_auth_snapshot(scope).await?
            };
            snapshot.api_key =
                (!self.snapshot_error).then(|| Credential::ApiKey("alternate-key".into()));
            Ok(snapshot)
        })
    }
}
async fn alternate_auth_sources_reach_status_and_preserve_model_auth() {
    use llm_runtime::auth::anthropic::{
        refresh::AuthState, OAuthCredentialProvider, RefreshDriver,
    };
    // Narrow grant, refreshed grant downgrade (enabled/denied), completed
    // unsupported renewal, missing/empty OAuth and failed alternate lookup.
    for stream in [false, true] {
        for (profile, denied, unsupported_refresh, missing_oauth, empty, snapshot_error) in [
            (false, false, false, false, false, false),
            (true, false, false, false, false, false),
            (true, true, false, false, false, false),
            (true, false, true, false, false, false),
            (false, false, false, true, false, false),
            (true, false, false, false, true, false),
            (false, false, false, false, false, true),
        ] {
            let capture = Arc::new(AlternateCapture {
                calls: Mutex::new(vec![]),
                renewed_profile: false,
                denied,
                unsupported_refresh,
            });
            let state = AuthState::new(
                lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(
                    0,
                ),
                lingxi_core::types::Secret::new(if empty {
                    String::new()
                } else {
                    "original-oauth".into()
                }),
                Some(lingxi_core::types::Secret::new("refresh".into())),
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(5000),
                if profile {
                    vec!["user:profile".into(), "user:inference".into()]
                } else {
                    vec!["user:inference".into()]
                },
                capture.clone(),
                Arc::new(OAuthClock),
                None,
                None,
            );
            let credentials = Arc::new(AlternateCredentials {
                inner: OAuthCredentialProvider::new(Arc::new(RefreshDriver::new(state))),
                snapshots: AtomicUsize::new(0),
                missing_oauth,
                unsupported_refresh,
                snapshot_error,
            });
            let mut config = config();
            for provider in &mut config.providers {
                provider.auth = llm_runtime::AuthStrategy::OAuthBearer;
            }
            let client = Arc::new(
                ModelRuntime::from_config(config)
                    .unwrap()
                    .with_credential_provider(Arc::new(
                        llm_runtime::CopilotExchangeCredentialProvider::new(
                            credentials.clone(),
                            capture.clone(),
                            "github-copilot",
                        ),
                    )),
            );
            let service = ApiService::new_with_routing(
                client,
                capture.clone(),
                SubscriberState::default(),
                UserAgentEnv::default(),
                "test",
                None,
                None,
                None,
                Default::default(),
                Some(0),
                None,
            )
            .with_thinking(ThinkingConfig::Disabled);
            let result = service.validate_fast_enable("opus", Some("direct")).await;
            let fast = !denied && !snapshot_error;
            assert_eq!(result.is_ok(), fast);
            if denied {
                let error = result.unwrap_err();
                assert!(
                    matches!(error, LlmError::PermissionDenied { message } if message == "Fast mode unavailable: Fast mode requires a paid subscription")
                );
            }
            assert_eq!(credentials.snapshots.load(Ordering::SeqCst), 1);
            // A compatible endpoint must not inspect the first-party alternate key.
            assert!(service
                .validate_fast_enable("opus", Some("compatible"))
                .await
                .is_err());
            assert_eq!(credentials.snapshots.load(Ordering::SeqCst), 1);
            let before = capture.calls.lock().unwrap().len();
            let mut request = LlmRequest::new("opus").with_user_text("alternate");
            request.profile = Some("direct".into());
            request.set_speed(Some("fast".into())).unwrap();
            if stream {
                assert!(service.stream_request(request).await.is_err());
            } else {
                assert!(service
                    .execute_non_stream_request(
                        request,
                        llm_runtime::NonStreamingRequestClass::Auxiliary,
                        Default::default()
                    )
                    .await
                    .is_err());
            }
            if !missing_oauth {
                let calls = capture.calls.lock().unwrap();
                assert_eq!(calls.len(), before + 1);
                let wire = calls.last().unwrap();
                let bearer = wire
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .unwrap();
                assert_eq!(
                    bearer.1,
                    if empty {
                        "Bearer "
                    } else if profile && !unsupported_refresh {
                        "Bearer renewed-oauth"
                    } else {
                        "Bearer original-oauth"
                    }
                );
                let body: Value = serde_json::from_slice(&wire.body).unwrap();
                // A cleared/empty OAuth state is absent from the readonly
                // status snapshot. Its alternate key cannot authorize the
                // model's empty bearer credential.
                let model_fast = fast && !empty;
                assert_eq!(
                    body.get("speed").and_then(Value::as_str),
                    model_fast.then_some("fast"),
                    "profile={profile}, denied={denied}, unsupported={unsupported_refresh}, empty={empty}, snapshot_error={snapshot_error}"
                );
                assert_eq!(
                    wire.headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                        .unwrap()
                        .1
                        .contains("fast-mode-2026-02-01"),
                    model_fast
                );
            } else {
                assert_eq!(
                    capture.calls.lock().unwrap().len(),
                    before,
                    "missing model OAuth must not borrow the status API key"
                );
            }
            let calls = capture.calls.lock().unwrap();
            let methods = calls
                .iter()
                .take(before)
                .map(|call| call.method.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                methods,
                if snapshot_error {
                    vec![]
                } else if profile && !empty {
                    if unsupported_refresh {
                        vec!["GET", "GET"]
                    } else {
                        vec!["GET", "POST", "GET"]
                    }
                } else {
                    vec!["GET"]
                }
            );
            assert_eq!(
                credentials.snapshots.load(Ordering::SeqCst),
                1,
                "model preparation must not reload the alternate source"
            );
        }
    }
}

struct OAuthCapture {
    calls: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    first_status: u16,
    revoked: bool,
    repeat_refusal: bool,
    profile: bool,
}
#[async_trait]
impl Transport for OAuthCapture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let bearer = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str());
        let (status, body) = if request.method == "GET" {
            if bearer == Some("Bearer old-oauth") || self.repeat_refusal {
                (
                    self.first_status,
                    if self.revoked {
                        json!("OAuth token has been revoked")
                    } else {
                        json!({"error":"expired"})
                    },
                )
            } else {
                assert_eq!(bearer, Some("Bearer new-oauth"));
                (200, json!({"enabled":true}))
            }
        } else if request.url.ends_with("/token") {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["grant_type"], "refresh_token");
            (
                200,
                json!({"access_token":"new-oauth","refresh_token":"new-refresh","expires_in":3600,"scope":"user:profile user:inference"}),
            )
        } else {
            assert_eq!(
                bearer,
                Some(if self.profile {
                    "Bearer new-oauth"
                } else {
                    "Bearer old-oauth"
                })
            );
            (
                400,
                json!({"type":"error","error":{"type":"invalid_request_error","message":"fixture"}}),
            )
        };
        self.calls.lock().unwrap().push(request);
        Ok(lingxi_llm_client::HttpResponse {
            status,
            headers: vec![],
            body: body.to_string().into_bytes().into(),
        }
        .into())
    }
}
struct OAuthClock;
impl lingxi_core::host::Clock for OAuthClock {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000)
    }
}
async fn scoped_oauth_refresh_uses_the_real_sdk_and_credential_driver() {
    use llm_runtime::auth::anthropic::{
        refresh::AuthState, OAuthCredentialProvider, RefreshDriver,
    };
    for (profile, first_status, revoked, repeat_refusal) in [
        (true, 401, false, false),
        (true, 403, true, false),
        (true, 401, false, true),
        (false, 401, false, false),
    ] {
        let capture = Arc::new(OAuthCapture {
            calls: Mutex::new(vec![]),
            first_status,
            revoked,
            repeat_refusal,
            profile,
        });
        let oauth =
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0);
        let state = AuthState::new(
            oauth,
            lingxi_core::types::Secret::new("old-oauth".into()),
            Some(lingxi_core::types::Secret::new("old-refresh".into())),
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(5000),
            if profile {
                vec!["user:profile".into(), "user:inference".into()]
            } else {
                vec!["user:inference".into()]
            },
            capture.clone(),
            Arc::new(OAuthClock),
            None,
            None,
        );
        let mut config = config();
        for provider in &mut config.providers {
            provider.auth = llm_runtime::AuthStrategy::OAuthBearer;
        }
        let credentials = Arc::new(OAuthCredentialProvider::new(Arc::new(RefreshDriver::new(
            state.clone(),
        ))));
        let client = Arc::new(
            ModelRuntime::from_config(config)
                .unwrap()
                .with_credential_provider(credentials),
        );
        let service = ApiService::new_with_routing(
            client,
            capture.clone(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled);
        // Subscription defaults to false: real token scope still admits renewal.
        let result = service.validate_fast_enable("opus", Some("direct")).await;
        assert_eq!(result.is_ok(), profile && !repeat_refusal);
        assert_eq!(
            state.token.read().await.access_token.expose_secret(),
            if profile { "new-oauth" } else { "old-oauth" }
        );
        let mut request = LlmRequest::new("opus").with_user_text("oauth");
        request.profile = Some("direct".into());
        request.set_speed(Some("fast".into())).unwrap();
        let _ = service.stream_request(request).await;
        let calls = capture.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.method.as_str())
                .collect::<Vec<_>>(),
            if profile {
                vec!["GET", "POST", "GET", "POST"]
            } else {
                vec!["POST"]
            }
        );
        let wire = calls.last().unwrap();
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(
            body.get("speed").and_then(Value::as_str),
            (profile && !repeat_refusal).then_some("fast")
        );
        let beta = wire
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .unwrap();
        assert_eq!(
            beta.1.contains("fast-mode-2026-02-01"),
            profile && !repeat_refusal
        );
        assert!(beta.1.contains("oauth-2025-04-20"));
    }
}

struct Rotating {
    values: Vec<&'static str>,
    loads: AtomicUsize,
    invalidate_on_second: Mutex<Option<Arc<dyn lingxi_core::host::auth::AccountChangeObserver>>>,
}
impl std::fmt::Debug for Rotating {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Rotating").finish_non_exhaustive()
    }
}
impl CredentialProvider for Rotating {
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            Ok(llm_runtime::AnthropicAuthSnapshot::from_credential(
                scope.clone(),
                self.load(scope).await?,
            ))
        })
    }

    fn load<'a>(
        &'a self,
        _: &'a CredentialScope,
    ) -> llm_runtime::BoxFuture<'a, Result<Credential, LlmError>> {
        let index = self.loads.fetch_add(1, Ordering::SeqCst);
        let value = self.values[index.min(self.values.len() - 1)];
        if index == 1 {
            if let Some(observer) = self.invalidate_on_second.lock().unwrap().as_ref() {
                observer.account_changed();
            }
        }
        Box::pin(async move { Ok(Credential::ApiKey(value.into())) })
    }
}
async fn request_snapshot_binding_reaches_both_drivers() {
    for stream in [false, true] {
        for (values, invalidate, expected_key, fast) in [
            (
                vec!["enabled-key", "denied-key"],
                false,
                "denied-key",
                false,
            ),
            (
                vec!["enabled-key", "enabled-key", "denied-key"],
                false,
                "enabled-key",
                true,
            ),
            (
                vec!["enabled-key", "enabled-key"],
                true,
                "enabled-key",
                false,
            ),
        ] {
            let credentials = Arc::new(Rotating {
                values,
                loads: AtomicUsize::new(0),
                invalidate_on_second: Mutex::new(None),
            });
            let capture = Arc::new(Capture::default());
            let service = service(
                credentials.clone(),
                capture.clone(),
                Arc::new(Mutex::new(Policy::default())),
            );
            service
                .validate_fast_enable("opus", Some("direct"))
                .await
                .unwrap();
            if invalidate {
                *credentials.invalidate_on_second.lock().unwrap() =
                    Some(service.account_change_observer());
            }
            let mut request = LlmRequest::new("opus").with_user_text("snapshot");
            request.profile = Some("direct".into());
            request.set_speed(Some("fast".into())).unwrap();
            if stream {
                let _ = service.stream_request(request).await;
            } else {
                let _ = service
                    .execute_non_stream_request(
                        request,
                        llm_runtime::NonStreamingRequestClass::Auxiliary,
                        Default::default(),
                    )
                    .await;
            }
            assert_eq!(
                credentials.loads.load(Ordering::SeqCst),
                2,
                "one explicit enable load and one draft load"
            );
            let calls = capture.0.lock().unwrap();
            assert_eq!(calls.len(), 2, "one status GET and one message POST");
            let wire = &calls[1];
            assert!(wire.headers.iter().any(|(name, value)| name
                .eq_ignore_ascii_case("x-api-key")
                && value == expected_key));
            let body: Value = serde_json::from_slice(&wire.body).unwrap();
            assert_eq!(
                body.get("speed").and_then(Value::as_str),
                fast.then_some("fast"),
                "stream={stream}, invalidation={invalidate}"
            );
            let beta = wire
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            assert_eq!(beta.contains("fast-mode-2026-02-01"), fast);
        }
    }
}
