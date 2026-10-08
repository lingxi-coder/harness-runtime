//! Status sources are independent; model authorization remains request-owned.
use async_trait::async_trait;
use llm_runtime::model::{
    fast_admission::Policy, thinking::ThinkingConfig, user_agent::UserAgentEnv,
};
use llm_runtime::{
    AnthropicAuthSnapshot, ApiService, BoxFuture, Credential, CredentialProvider, CredentialScope,
    LlmError, LlmRequest, ModelRuntime, SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
fn scope(id: &str) -> CredentialScope {
    CredentialScope::new(llm_runtime::ProviderId::AnthropicFirstParty, "direct")
        .with_credential_id(id)
}
#[derive(Debug)]
struct Sources {
    model: Mutex<Credential>,
    status: Mutex<AnthropicAuthSnapshot>,
    loads: AtomicUsize,
    snapshots: AtomicUsize,
}
impl CredentialProvider for Sources {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        assert_eq!(scope.credential_id.as_deref(), Some("model-key"));
        self.loads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(self.model.lock().unwrap().clone()) })
    }
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        _: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<AnthropicAuthSnapshot, LlmError>> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(self.status.lock().unwrap().clone()) })
    }
}
#[derive(Default)]
struct Capture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        r: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let get = r.method == "GET";
        self.0.lock().unwrap().push(r);
        Ok(lingxi_llm_client::HttpResponse {
            status: if get { 200 } else { 400 },
            headers: vec![],
            body: if get {
                br#"{"enabled":true}"#.to_vec()
            } else {
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"fixture"}}"#
                    .to_vec()
            }
            .into(),
        }
        .into())
    }
}
fn service(
    provider: Arc<dyn CredentialProvider>,
    capture: Arc<dyn Transport>,
    policy: Policy,
) -> ApiService {
    service_with_credential(
        provider,
        capture,
        policy,
        json!({"type":"host_managed","id":"model-key"}),
    )
}
fn service_with_credential(
    provider: Arc<dyn CredentialProvider>,
    capture: Arc<dyn Transport>,
    policy: Policy,
    credential: Value,
) -> ApiService {
    let config=serde_json::from_value(json!({"providers":[{"provider_id":"anthropic_first_party","profile_name":"direct","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"api_key","credential":credential,"models":[{"display_model":"Opus","request_model":"claude-opus-5-5","billing_model":"claude-opus-5-5","aliases":["opus"],"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]}]})).unwrap();
    ApiService::new_with_routing(
        Arc::new(
            ModelRuntime::from_config(config)
                .unwrap()
                .with_credential_provider(provider),
        ),
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
    .with_fast_policy_source(Arc::new(move || policy.clone()))
    .with_interactive_session(false)
    .with_thinking(ThinkingConfig::Disabled)
}
async fn model_call(api: &ApiService, stream: bool) {
    let mut r = LlmRequest::new("opus").with_user_text("independent sources");
    r.profile = Some("direct".into());
    r.set_speed(Some("fast".into())).unwrap();
    if stream {
        assert!(api.stream_request(r).await.is_err());
    } else {
        assert!(api
            .execute_non_stream_request(
                r,
                llm_runtime::NonStreamingRequestClass::Auxiliary,
                Default::default()
            )
            .await
            .is_err());
    }
}
fn header<'a>(r: &'a lingxi_llm_client::HttpRequest, n: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(n))
        .map(|(_, v)| v.as_str())
}
fn assert_model(r: &lingxi_llm_client::HttpRequest, key: &str, fast: bool) {
    assert_eq!(r.method, "POST");
    assert_eq!(header(r, "x-api-key"), Some(key));
    assert!(header(r, "authorization").is_none());
    assert_eq!(
        serde_json::from_slice::<Value>(&r.body)
            .unwrap()
            .get("speed")
            .and_then(Value::as_str),
        fast.then_some("fast")
    );
    assert_eq!(
        header(r, "anthropic-beta")
            .unwrap_or("")
            .contains("fast-mode-2026-02-01"),
        fast
    );
}
#[tokio::test]
async fn native_independent_sources_reach_both_physical_drivers() {
    // This dedicated binary owns the environment.
    for n in [
        branding::MODEL_CAPABILITIES_ENV,
        branding::DISABLE_FAST_MODE_ENV,
        branding::SKIP_FAST_MODE_ORG_CHECK_ENV,
        branding::SKIP_FAST_MODE_NETWORK_ERRORS_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
    ] {
        std::env::remove_var(n);
    }
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/fast_status_sources_2_1_288.json")).unwrap();
    let mut scenarios = 0;
    for row in fixture["cases"].as_array().unwrap() {
        for stream in [false, true] {
            let i = &row["input"];
            let key = i["key"]
                .as_str()
                .filter(|v| !v.is_empty())
                .unwrap_or("model-only-key");
            let status = AnthropicAuthSnapshot {
                oauth: i["token"].as_str().map(|token| {
                    (
                        scope("status-oauth"),
                        Credential::AnthropicOAuth {
                            access_token: token.into(),
                            scopes: serde_json::from_value(i["scopes"].clone()).unwrap(),
                        },
                    )
                }),
                api_key: i["key"].as_str().map(|key| Credential::ApiKey(key.into())),
            };
            let source = Arc::new(Sources {
                model: Mutex::new(Credential::ApiKey(key.into())),
                status: Mutex::new(status),
                loads: AtomicUsize::new(0),
                snapshots: AtomicUsize::new(0),
            });
            let capture = Arc::new(Capture::default());
            let no_user = i["no_user_account"].as_bool().unwrap();
            let api = service(
                source.clone(),
                capture.clone(),
                Policy {
                    session_access_token: i["session_token"].as_bool().unwrap(),
                    no_user_account: no_user,
                    ..Default::default()
                },
            );
            let expected = &row["expected"];
            let admitted = !no_user && !expected.is_null();
            assert_eq!(
                api.validate_fast_enable("opus", Some("direct"))
                    .await
                    .is_ok(),
                admitted,
                "{row}"
            );
            assert_eq!(
                source.loads.load(Ordering::SeqCst),
                0,
                "status must not load model credentials"
            );
            assert_eq!(
                source.snapshots.load(Ordering::SeqCst),
                usize::from(!no_user)
            );
            if admitted {
                let calls = capture.0.lock().unwrap();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].method, "GET");
                if let Some(token) = expected["accessToken"].as_str() {
                    assert_eq!(
                        header(&calls[0], "authorization"),
                        Some(format!("Bearer {token}").as_str())
                    );
                    assert!(header(&calls[0], "x-api-key").is_none());
                } else {
                    assert_eq!(header(&calls[0], "x-api-key"), expected["apiKey"].as_str());
                }
            }
            model_call(&api, stream).await;
            let calls = capture.0.lock().unwrap();
            assert_eq!(calls.len(), 1 + usize::from(admitted));
            let observed_fast = serde_json::from_slice::<Value>(&calls.last().unwrap().body)
                .unwrap()["speed"]
                == "fast";
            assert_eq!(observed_fast, admitted, "row={row}, stream={stream}");
            assert_model(calls.last().unwrap(), key, admitted);
            assert_eq!(source.loads.load(Ordering::SeqCst), 1);
            scenarios += 1;
        }
    }
    assert_eq!(scenarios, 1152);
    for stream in [false, true] {
        std::env::set_var("HARNESS_STATUS_MODEL_API_KEY", "environment-model-key");
        let source = Arc::new(Sources {
            model: Mutex::new(Credential::ApiKey("unused-model-key".into())),
            status: Mutex::new(AnthropicAuthSnapshot {
                oauth: Some((
                    scope("status-oauth"),
                    Credential::AnthropicOAuth {
                        access_token: "independent-oauth".into(),
                        scopes: vec!["user:profile".into()],
                    },
                )),
                api_key: None,
            }),
            loads: AtomicUsize::new(0),
            snapshots: AtomicUsize::new(0),
        });
        let capture = Arc::new(Capture::default());
        let api = service_with_credential(
            source.clone(),
            capture.clone(),
            Policy::default(),
            json!({"type":"env","var":"HARNESS_STATUS_MODEL_API_KEY"}),
        );
        api.validate_fast_enable("opus", Some("direct"))
            .await
            .unwrap();
        model_call(&api, stream).await;
        let calls = capture.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            header(&calls[0], "authorization"),
            Some("Bearer independent-oauth")
        );
        assert_model(&calls[1], "environment-model-key", true);
        assert_eq!(source.loads.load(Ordering::SeqCst), 0);
        assert_eq!(source.snapshots.load(Ordering::SeqCst), 1);
        std::env::remove_var("HARNESS_STATUS_MODEL_API_KEY");
    }
    // An OAuth-owned organization observation does not become an API-key observation.
    // Rotation inside the same account preserves it; account observers invalidate it.
    for stream in [false, true] {
        let source = Arc::new(Sources {
            model: Mutex::new(Credential::ApiKey("A".into())),
            status: Mutex::new(AnthropicAuthSnapshot {
                oauth: Some((
                    scope("status-oauth"),
                    Credential::AnthropicOAuth {
                        access_token: "oauth".into(),
                        scopes: vec!["user:profile".into()],
                    },
                )),
                api_key: Some(Credential::ApiKey("A".into())),
            }),
            loads: AtomicUsize::new(0),
            snapshots: AtomicUsize::new(0),
        });
        let capture = Arc::new(Capture::default());
        let api = service(source.clone(), capture.clone(), Policy::default());
        api.validate_fast_enable("opus", Some("direct"))
            .await
            .unwrap();
        *source.model.lock().unwrap() = Credential::ApiKey("B".into());
        model_call(&api, stream).await;
        assert_model(capture.0.lock().unwrap().last().unwrap(), "B", true);
        api.account_change_observer().account_changed();
        model_call(&api, stream).await;
        assert_model(capture.0.lock().unwrap().last().unwrap(), "B", false);
    }
    readonly_expiry_and_refresh_scope().await;
    println!("1152 native source/driver scenarios, both-driver account epoch checks, readonly expired OAuth and scoped real refresh OK");
}
struct Clock;
impl lingxi_core::host::Clock for Clock {
    fn now(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000)
    }
}
struct RenewCapture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
#[async_trait]
impl Transport for RenewCapture {
    async fn send(
        &self,
        r: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let (status, body) = if r.url.ends_with("/token") {
            assert_eq!(
                serde_json::from_slice::<Value>(&r.body).unwrap()["grant_type"],
                "refresh_token"
            );
            (
                200,
                json!({"access_token":"renewed","refresh_token":"refresh-next","expires_in":3600,"scope":"user:profile user:inference"}),
            )
        } else if r.method == "GET" {
            if header(&r, "authorization") == Some("Bearer expired") {
                (401, json!({"error":"expired"}))
            } else {
                assert_eq!(header(&r, "authorization"), Some("Bearer renewed"));
                (200, json!({"enabled":true}))
            }
        } else {
            assert_eq!(header(&r, "x-api-key"), Some("model-key"));
            (
                400,
                json!({"type":"error","error":{"type":"invalid_request_error","message":"fixture"}}),
            )
        };
        self.0.lock().unwrap().push(r);
        Ok(lingxi_llm_client::HttpResponse {
            status,
            headers: vec![],
            body: body.to_string().into_bytes().into(),
        }
        .into())
    }
}
#[derive(Debug)]
struct DriverSources {
    oauth: llm_runtime::auth::anthropic::OAuthCredentialProvider,
    refreshes: AtomicUsize,
}
impl CredentialProvider for DriverSources {
    fn load<'a>(&'a self, _: &'a CredentialScope) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async { Ok(Credential::ApiKey("model-key".into())) })
    }
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        s: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            let os = s.clone().with_credential_id("status-oauth");
            let mut snapshot = self.oauth.anthropic_auth_snapshot(&os).await?;
            snapshot.api_key = Some(Credential::ApiKey("model-key".into()));
            Ok(snapshot)
        })
    }
    fn refresh<'a>(
        &'a self,
        s: &'a CredentialScope,
        c: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        assert_eq!(s.credential_id.as_deref(), Some("status-oauth"));
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        self.oauth.refresh(s, c)
    }
}
async fn readonly_expiry_and_refresh_scope() {
    for stream in [false, true] {
        let capture = Arc::new(RenewCapture(Mutex::new(vec![])));
        let state = llm_runtime::auth::anthropic::refresh::AuthState::new(
            lingxi_llm_client::auth::oauth::anthropic::ClaudeAiOAuthConfig::default_with_port(0),
            lingxi_core::types::Secret::new("expired".into()),
            Some(lingxi_core::types::Secret::new("refresh".into())),
            std::time::UNIX_EPOCH,
            vec!["user:profile".into()],
            capture.clone(),
            Arc::new(Clock),
            None,
            None,
        );
        let source = Arc::new(DriverSources {
            oauth: llm_runtime::auth::anthropic::OAuthCredentialProvider::new(Arc::new(
                llm_runtime::auth::anthropic::RefreshDriver::new(state.clone()),
            )),
            refreshes: AtomicUsize::new(0),
        });
        let snapshot = source
            .anthropic_auth_snapshot(&scope("model-key"))
            .await
            .unwrap();
        assert!(
            matches!(snapshot.oauth.unwrap().1,Credential::AnthropicOAuth{access_token,..} if access_token=="expired")
        );
        assert!(capture.0.lock().unwrap().is_empty());
        let api = service(
            source.clone(),
            capture.clone(),
            Policy {
                flag_fast: true,
                ..Default::default()
            },
        );
        api.validate_fast_enable("opus", Some("direct"))
            .await
            .unwrap();
        model_call(&api, stream).await;
        let calls = capture.0.lock().unwrap();
        assert_eq!(
            calls.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
            vec!["GET", "POST", "GET", "POST"]
        );
        assert_model(calls.last().unwrap(), "model-key", true);
        assert_eq!(source.refreshes.load(Ordering::SeqCst), 1);
        drop(calls);
        state.invalidate().await;
        assert!(source
            .anthropic_auth_snapshot(&scope("model-key"))
            .await
            .unwrap()
            .oauth
            .is_none());
        assert_eq!(
            capture.0.lock().unwrap().len(),
            4,
            "logout snapshot must not renew"
        );
    }
}
