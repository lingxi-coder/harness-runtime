//! The main adapter uses the selected profile and native environment gate.
use async_trait::async_trait;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::{ProviderApiAdapter, StreamingApiClient};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicBool, Arc, Mutex};

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn own() -> Self {
        let names = [
            branding::MODEL_CAPABILITIES_ENV,
            branding::DISABLE_FAST_MODE_ENV,
            "CLAUDE_CODE_EXTRA_BODY",
            "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        ];
        let values = names
            .iter()
            .map(|&name| (name, std::env::var_os(name)))
            .collect();
        for name in names {
            std::env::remove_var(name);
        }
        Self(values)
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
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
        self.0.lock().unwrap().push(request);
        Ok(lingxi_llm_client::HttpResponse {
            status: 400,
            headers: vec![],
            body:
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"fixture"}}"#
                    .to_vec()
                    .into(),
        }
        .into())
    }
}
#[tokio::test]
async fn main_fast_toggle_reaches_sdk_using_the_explicit_profile() {
    let _environment = Environment::own();
    let providers = [("direct", "https://api.anthropic.com"), ("compatible", "https://fixture-gateway.example")].into_iter().map(|(profile, base)| json!({
        "provider_id":"anthropic_first_party","profile_name":profile,"base_url":base,"protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},
        "models":[{"display_model":"Picker Label","request_model":"claude-opus-4-7","billing_model":"claude-opus-4-7","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
    })).collect::<Vec<_>>();
    let config: ClientConfig = serde_json::from_value(json!({"providers":providers})).unwrap();
    let client = Arc::new(ModelRuntime::from_config(config).unwrap());
    let capture = Arc::new(Capture::default());
    let service = Arc::new(
        ApiService::new_with_routing(
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
        .with_fast_policy_source(Arc::new(|| llm_runtime::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        }))
        .with_thinking(ThinkingConfig::Disabled),
    );
    for (capability, disabled) in [
        ("fast_mode", false),
        ("-fast_mode", false),
        ("fast_mode", true),
    ] {
        std::env::set_var(branding::MODEL_CAPABILITIES_ENV, capability);
        std::env::set_var(branding::DISABLE_FAST_MODE_ENV, disabled.to_string());
        for profile in ["direct", "compatible"] {
            for toggle in [false, true] {
                capture.0.lock().unwrap().clear();
                let adapter = ProviderApiAdapter::new(service.clone())
                    .with_fast_mode(Arc::new(AtomicBool::new(toggle)));
                let _ = StreamingApiClient::stream(
                    &adapter,
                    "Picker Label",
                    Some(profile),
                    None,
                    vec![],
                    vec![],
                    "sdk", false, None,
                )
                .await;
                let calls = capture.0.lock().unwrap();
                assert_eq!(
                    calls.len(),
                    1,
                    "{profile}, {capability}, {disabled}, {toggle}"
                );
                let wire = &calls[0];
                let body: Value = serde_json::from_slice(&wire.body).unwrap();
                let allowed =
                    profile == "direct" && toggle && capability == "fast_mode" && !disabled;
                assert_eq!(body["model"], "claude-opus-4-7");
                assert_eq!(
                    body.get("speed").and_then(Value::as_str),
                    allowed.then_some("fast")
                );
                let beta = wire
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or("");
                assert_eq!(beta.contains("fast-mode-2026-02-01"), allowed);
            }
        }
    }
    // The public toggle awaits admission and preserves the detailed native
    // refusal; disabling remains possible after policy changes.
    use lingxi_core::host::OrchestratorHandle;
    use orchestrator::test_support::{
        noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
    };
    std::env::set_var(branding::MODEL_CAPABILITIES_ENV, "fast_mode");
    std::env::remove_var(branding::DISABLE_FAST_MODE_ENV);
    let policy = Arc::new(Mutex::new(llm_runtime::model::fast_admission::Policy {
        cached_org_enabled: true,
        ..Default::default()
    }));
    let live_policy = policy.clone();
    let guarded = Arc::new(
        Arc::try_unwrap(service)
            .unwrap_or_else(|_| panic!("all request adapters were dropped"))
            .with_fast_policy_source(Arc::new(move || live_policy.lock().unwrap().clone())),
    );
    let adapter = Arc::new(ProviderApiAdapter::new(guarded));
    let orch = orchestrator::ConversationOrchestrator::new(
        orchestrator::OrchestratorConfig {
            model: "Picker Label".into(),
            ..Default::default()
        },
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::path::PathBuf::from("/tmp"),
    );
    orch.seed_initial_model_profile("Picker Label", "direct")
        .await;
    orch.set_fast_mode(true).await.unwrap();
    assert!(orch.fast_mode().await);
    orch.set_fast_mode(false).await.unwrap();
    policy.lock().unwrap().policy_fast = Some(false);
    let error = orch.set_fast_mode(true).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("Fast mode unavailable: Fast mode has been disabled by your organization"));
    assert!(!orch.fast_mode().await);
    orch.set_fast_mode(false).await.unwrap();
}
