//! Isolated process ownership of session environment and real SDK dispatch.
use super::*;
use async_trait::async_trait;
use llm_runtime::{
    ApiService, BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError, ModelRuntime,
    SubscriberState, Transport,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

const WORKER: &str = "HARNESS_FAST_SESSION_TEST_WORKER";
const VARIABLES: [&str; 4] = [
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CODE_ENVIRONMENT_KIND",
    "CLAUDE_CODE_ENTRYPOINT",
];

fn apply(input: &Value) {
    for (variable, key) in
        VARIABLES
            .into_iter()
            .zip(["access_token", "remote", "environment_kind", "entrypoint"])
    {
        if let Some(value) = input[key].as_str() {
            std::env::set_var(variable, value);
        } else {
            std::env::remove_var(variable);
        }
    }
}
#[test]
fn current_session_environment_reaches_production_policy_and_sdk() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fast_settings::session_tests::session_environment_worker",
            "--nocapture",
        ])
        .env(WORKER, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated session worker failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("session identity production matrix and physical drivers: OK"));
}

#[derive(Debug, Default)]
struct Credentials(AtomicUsize);
impl CredentialProvider for Credentials {
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            Ok(llm_runtime::AnthropicAuthSnapshot::from_credential(
                scope.clone(),
                self.load(scope).await?,
            ))
        })
    }

    fn load<'a>(&'a self, _: &'a CredentialScope) -> BoxFuture<'a, Result<Credential, LlmError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Credential::ApiKey("fixture-key".into())) })
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
fn service(
    home: &Path,
    credentials: Arc<Credentials>,
    capture: Arc<Capture>,
    flag: bool,
) -> ApiService {
    let config=serde_json::from_value(json!({"providers":[{"provider_id":"anthropic_first_party","profile_name":"direct","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"api_key","credential":{"type":"host_managed","id":"test"},"models":[{"display_model":"Opus","request_model":"claude-opus-5-5","billing_model":"claude-opus-5-5","aliases":["opus"],"capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]}]})).unwrap();
    let client = Arc::new(
        ModelRuntime::from_config(config)
            .unwrap()
            .with_credential_provider(credentials),
    );
    let home = home.to_path_buf();
    ApiService::new_with_routing(
        client,
        capture,
        SubscriberState::default(),
        llm_runtime::model::user_agent::UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
    .with_thinking(llm_runtime::model::thinking::ThinkingConfig::Disabled)
    .with_fast_policy_source(Arc::new(move || {
        let settings: SettingsJson = serde_json::from_value(json!({"fastMode":flag})).unwrap();
        policy(&home, Some(&settings), &[])
    }))
}

#[tokio::test]
async fn session_environment_worker() {
    if std::env::var(WORKER).as_deref() != Ok("1") {
        return;
    }
    for name in [
        branding::MODEL_CAPABILITIES_ENV,
        branding::DISABLE_FAST_MODE_ENV,
        branding::SKIP_FAST_MODE_ORG_CHECK_ENV,
        branding::SKIP_FAST_MODE_NETWORK_ERRORS_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        std::env::remove_var(name);
    }
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/session_identity_2_1_287.json"
    ))
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut checked = 0;
    for row in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["input"]["cached_access_token"].is_null())
    {
        apply(&row["input"]);
        let actual = policy(home.path(), None, &[]);
        assert_eq!(
            json!({"has_session_token":actual.session_access_token,"no_user_account":actual.no_user_account,"agent_owned_remote":actual.agent_owned_remote}),
            row["expected"],
            "{row}"
        );
        checked += 1;
    }
    assert!(checked > 1400);
    // Real status prefetch must stop before any local credential lookup for
    // service identities and BYOC workers. Presence of null account fields
    // prevents that classification; sub only prevents the service variant.
    for row in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .take(23 * 48)
        .step_by(4)
    {
        apply(&row["input"]);
        let credentials = Arc::new(Credentials::default());
        let capture = Arc::new(Capture::default());
        let api = service(home.path(), credentials.clone(), capture.clone(), false);
        let result = api.validate_fast_enable("opus", Some("direct")).await;
        let skipped = row["expected"]["no_user_account"].as_bool().unwrap();
        assert_eq!(credentials.0.load(Ordering::SeqCst), usize::from(!skipped));
        assert_eq!(capture.0.lock().unwrap().len(), usize::from(!skipped));
        assert_eq!(result.is_ok(), !skipped);
    }
    let owned = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| {
            row["expected"]["no_user_account"] == true
                && row["expected"]["agent_owned_remote"] == true
        })
        .unwrap();
    for stream in [false, true] {
        apply(&Value::Null);
        let credentials = Arc::new(Credentials::default());
        let capture = Arc::new(Capture::default());
        let api = service(home.path(), credentials.clone(), capture.clone(), true);
        api.validate_fast_enable("opus", Some("direct"))
            .await
            .unwrap();
        apply(&owned["input"]);
        api.validate_fast_enable("opus", Some("direct"))
            .await
            .unwrap();
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            capture.0.lock().unwrap().len(),
            1,
            "owned no-user session must preserve earlier server permission without another GET"
        );
        let mut request = llm_runtime::LlmRequest::new("opus").with_user_text("identity");
        request.profile = Some("direct".into());
        request.set_speed(Some("fast".into())).unwrap();
        if stream {
            assert!(api.stream_request(request).await.is_err());
        } else {
            assert!(api
                .execute_non_stream_request(
                    request,
                    llm_runtime::NonStreamingRequestClass::Auxiliary,
                    Default::default()
                )
                .await
                .is_err());
        }
        let calls = capture.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let wire = &calls[1];
        assert_eq!(wire.method, "POST");
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(body["speed"], "fast");
        assert!(wire
            .headers
            .iter()
            .any(|(name, value)| name == "x-api-key" && value == "fixture-key"));
        assert!(wire
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("anthropic-beta")
                && value.contains("fast-mode-2026-02-01")));
    }
    // A persisted local enabled guess cannot manufacture remote-owned server
    // permission, even with explicit flag opt-in and a session token.
    std::fs::write(
        home.path().join(branding::LEGACY_GLOBAL_CONFIG_FILE),
        r#"{"penguinModeOrgEnabled":true}"#,
    )
    .unwrap();
    apply(&owned["input"]);
    let credentials = Arc::new(Credentials::default());
    let capture = Arc::new(Capture::default());
    let api = service(home.path(), credentials.clone(), capture.clone(), true);
    assert!(api
        .validate_fast_enable("opus", Some("direct"))
        .await
        .is_err());
    assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
    assert!(capture.0.lock().unwrap().is_empty());
    std::fs::remove_file(home.path().join(branding::LEGACY_GLOBAL_CONFIG_FILE)).unwrap();
    // OAuth token presence alone is not session-token identity.
    apply(&Value::Null);
    std::env::set_var(
        "CLAUDE_CODE_OAUTH_TOKEN",
        owned["input"]["access_token"].as_str().unwrap(),
    );
    let credentials = Arc::new(Credentials::default());
    let capture = Arc::new(Capture::default());
    let api = service(home.path(), credentials.clone(), capture.clone(), false);
    api.validate_fast_enable("opus", Some("direct"))
        .await
        .unwrap();
    assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
    assert_eq!(capture.0.lock().unwrap().len(), 1);
    println!("session identity production matrix and physical drivers: OK");
}
