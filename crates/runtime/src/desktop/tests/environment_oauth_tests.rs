//! A child owns environment mutation; factory and both model drivers are real.
use super::*;
use async_trait::async_trait;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use llm_runtime::{Credential, CredentialProvider};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

const WORKER: &str = "HARNESS_ENVIRONMENT_OAUTH_WORKER";
#[test]
fn environmental_oauth_reaches_factory_and_physical_drivers() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "desktop::environment_oauth_tests::environment_oauth_worker",
            "--nocapture",
        ])
        .env(WORKER, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("environment OAuth: 1744 capture rows, 14 factory/driver scenarios OK"));
}
struct Store {
    inner: Arc<dyn SecureStorage>,
    oauth_reads: AtomicUsize,
    writes: AtomicUsize,
}
#[async_trait]
impl SecureStorage for Store {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: lingxi_core::types::SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.store(service, account, data).await
    }
    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<lingxi_core::types::SecureStorageData>, SecureStorageError> {
        if account.starts_with("anthropic-oauth-") {
            self.oauth_reads.fetch_add(1, Ordering::SeqCst);
            panic!("environment token must prevent stored OAuth acquisition");
        }
        self.inner.retrieve(service, account).await
    }
    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        self.inner.delete(service, account).await
    }
    async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
        self.inner.list(service).await
    }
    fn is_encrypted(&self) -> bool {
        self.inner.is_encrypted()
    }
    fn backend(&self) -> SecureStorageBackend {
        self.inner.backend()
    }
}
#[derive(Default)]
struct Capture(Mutex<Vec<lingxi_llm_client::HttpRequest>>);
#[async_trait]
impl llm_runtime::Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let get = request.method == "GET";
        self.0.lock().unwrap().push(request);
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
fn apply(input: &Value) {
    for (variable, field) in [
        ("CLAUDE_CODE_OAUTH_TOKEN", "environment_token"),
        ("CLAUDE_CODE_OAUTH_SCOPES", "environment_scopes"),
    ] {
        if let Some(value) = input[field].as_str() {
            std::env::set_var(variable, value);
        } else {
            std::env::remove_var(variable);
        }
    }
}
#[tokio::test]
async fn environment_oauth_worker() {
    if std::env::var(WORKER).as_deref() != Ok("1") {
        return;
    }
    for name in [
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_REMOTE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
        "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
        "CLAUDE_CODE_EXTRA_BODY",
        branding::MODEL_CAPABILITIES_ENV,
        branding::DISABLE_FAST_MODE_ENV,
        branding::SKIP_FAST_MODE_ORG_CHECK_ENV,
        branding::SKIP_FAST_MODE_NETWORK_ERRORS_ENV,
    ] {
        std::env::remove_var(name);
    }
    std::env::set_var(
        branding::SUBSCRIPTION_TYPE_ENV,
        "\u{FEFF} enterprise \u{FEFF}",
    );
    std::env::set_var(branding::RATE_LIMIT_TIER_ENV, " default_claude_ai ");
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/oauth_source_2_1_288.json"
    ))
    .unwrap();
    let mut count = 0;
    for row in fixture["cases"].as_array().unwrap() {
        apply(&row["input"]);
        let provider =
            llm_runtime::auth::anthropic::environment::EnvironmentOAuthCredentialProvider::capture(
            );
        let expected = &row["expected"];
        if expected["source"] == "environment" {
            let provider = provider.unwrap();
            assert_eq!(
                serde_json::to_value(provider.scopes()).unwrap(),
                expected["scopes"]
            );
            assert_eq!(provider.subscription_type.as_deref(), Some("enterprise"));
            assert_eq!(
                provider.rate_limit_tier.as_deref(),
                Some("default_claude_ai")
            );
            let scope = llm_runtime::CredentialScope::new(
                llm_runtime::ProviderId::AnthropicFirstParty,
                "anthropic",
            )
            .with_credential_id("anthropic-oauth");
            std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "changed-after-capture");
            let credential = provider.load(&scope).await.unwrap();
            assert!(
                matches!(credential, Credential::AnthropicOAuth {ref access_token, ..} if access_token == expected["access_token"].as_str().unwrap())
            );
            assert!(provider
                .refresh(&scope, &credential)
                .await
                .unwrap()
                .is_none());
            assert!(format!("{provider:?}").contains("access_token: \"[REDACTED]\""));
        } else {
            assert!(provider.is_none());
        }
        count += 1;
    }
    assert_eq!(count, 1744);
    for scopes in [
        None,
        Some("user:inference user:profile"),
        Some("user:inference\u{FEFF}user:profile"),
        Some("user:profile"),
    ] {
        for api_key in [false, true] {
            // Profile-only OAuth is a status source, not model inference auth.
            if scopes == Some("user:profile") && !api_key {
                continue;
            }
            for stream in [false, true] {
                apply(
                    &json!({"environment_token":"\u{FEFF}env-fixture-secret\u{FEFF}","environment_scopes":scopes}),
                );
                let home = tempfile::tempdir().unwrap();
                let mut cfg = DesktopConfig::default();
                cfg.lingxi_home = home.path().into();
                cfg.isolated_credential_storage = true;
                cfg.default_model = "claude-opus-5-5".into();
                cfg.default_model_explicit = true;
                cfg.api_key = if api_key { "fixture-api-key" } else { "" }.into();
                let mut shared = build_shared_credential_stack_for_config(&cfg)
                    .await
                    .unwrap();
                let store = Arc::new(Store {
                    inner: shared.storage.clone(),
                    oauth_reads: AtomicUsize::new(0),
                    writes: AtomicUsize::new(0),
                });
                shared.storage = store.clone();
                shared.credentials = Arc::new(secret::CredentialManager::new(
                    store.clone(),
                    shared.clock.clone(),
                    shared.http.clone(),
                ));
                let mut stack = resolve_llm_stack_with_credentials(&cfg, shared)
                    .await
                    .unwrap();
                assert!(stack.has_oauth_token);
                assert_eq!(stack.is_subscriber, !api_key);
                assert_eq!(
                    stack.credential_origin,
                    if api_key {
                        orchestrator::api_error_copy::CredentialOrigin::EnvApiKey {
                            var: "ANTHROPIC_API_KEY".into(),
                        }
                    } else {
                        orchestrator::api_error_copy::CredentialOrigin::Other
                    }
                );
                let capture = Arc::new(Capture::default());
                stack.llm_transport = capture.clone();
                let service = api_service_from_stack(&cfg, home.path(), stack)
                    .with_interactive_session(false)
                    .with_fast_policy_source(Arc::new(|| {
                        llm_runtime::model::fast_admission::Policy {
                            flag_fast: true,
                            ..Default::default()
                        }
                    }));
                let admitted = api_key || scopes.is_some();
                assert_eq!(
                    service
                        .validate_fast_enable("claude-opus-5-5", Some("anthropic"))
                        .await
                        .is_ok(),
                    admitted
                );
                let mut request = llm_runtime::LlmRequest::new("claude-opus-5-5")
                    .with_user_text("OAuth environment");
                request.profile = Some("anthropic".into());
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
                let calls = capture.0.lock().unwrap();
                assert_eq!(calls.len(), 1 + usize::from(admitted));
                if admitted {
                    let status = &calls[0];
                    assert_eq!(status.method, "GET");
                    // Status retains profile OAuth when the model uses a key.
                    let status_oauth = scopes.is_some();
                    assert_eq!(
                        status
                            .headers
                            .iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                            .map(|(_, value)| value.as_str()),
                        status_oauth.then_some("Bearer env-fixture-secret")
                    );
                    assert_eq!(
                        status
                            .headers
                            .iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
                            .map(|(_, value)| value.as_str()),
                        (!status_oauth).then_some("fixture-api-key")
                    );
                }
                let wire = calls.last().unwrap();
                assert_eq!(wire.method, "POST");
                assert!(wire.headers.iter().any(|(name, value)| if api_key {
                    name.eq_ignore_ascii_case("x-api-key") && value == "fixture-api-key"
                } else {
                    name.eq_ignore_ascii_case("authorization")
                        && value == "Bearer env-fixture-secret"
                }));
                let beta = wire
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or("");
                assert_eq!(beta.contains("oauth-2025-04-20"), !api_key);
                assert_eq!(beta.contains("fast-mode-2026-02-01"), admitted);
                assert_eq!(
                    serde_json::from_slice::<Value>(&wire.body)
                        .unwrap()
                        .get("speed")
                        .and_then(Value::as_str),
                    admitted.then_some("fast")
                );
                assert_eq!(store.oauth_reads.load(Ordering::SeqCst), 0);
                assert_eq!(store.writes.load(Ordering::SeqCst), 0);
            }
        }
    }
    println!("environment OAuth: 1744 capture rows, 14 factory/driver scenarios OK");
}
