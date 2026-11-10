use super::*;
use lingxi_core::host::{prompt_policy::resolve_prompt_policy, session_context::ConversationMode};
use lingxi_llm_client::protocol as wire;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

fn sdk_profile(provider: &str, name: &str, protocol: &str, model: Value) -> wire::ProviderProfile {
    serde_json::from_value(json!({
        "provider_id": provider,
        "profile_name": name,
        "base_url": "https://example.invalid",
        "protocol": protocol,
        "auth": "none",
        "models": [model]
    }))
    .unwrap()
}

fn model(request_model: &str, family: Option<&str>) -> Value {
    json!({
        "display_model": "visible-model",
        "request_model": request_model,
        "billing_model": "claude-sonnet-4-6",
        "aliases": ["selected-alias"],
        "metadata": {"family": family}
    })
}

fn runtime(profiles: Vec<wire::ProviderProfile>) -> ModelRuntime {
    let catalog = crate::catalog::catalog_from_profiles(profiles);
    ModelRuntime::from_config(ClientConfig {
        providers: catalog.providers,
    })
    .unwrap()
}

#[test]
fn production_catalog_keeps_kimi_messages_and_openrouter_claude_separate() {
    let profiles = lingxi_llm_client::builtin_providers()
        .unwrap()
        .into_iter()
        .filter(|profile| {
            matches!(
                profile.profile_name.as_str(),
                "kimi-code-anthropic" | "openrouter"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(profiles.len(), 2);
    let client = runtime(profiles);
    let kimi = client
        .prompt_target("k3", Some("kimi-code-anthropic"), ConversationMode::Code)
        .unwrap();
    assert_eq!(kimi.provider_id, "kimi");
    assert_eq!(kimi.request_model, "k3");
    assert_eq!(kimi.model_family.as_deref(), Some("kimi-k3"));
    assert_eq!(
        client
            .protocol_for_model("k3", Some("kimi-code-anthropic"))
            .unwrap(),
        ProtocolFamily::AnthropicMessages
    );
    assert_eq!(
        resolve_prompt_policy(kimi).bundle_id,
        "generic-code-lean-v1"
    );

    let claude = client
        .prompt_target(
            "anthropic/claude-opus-4.6",
            Some("openrouter"),
            ConversationMode::Chat,
        )
        .unwrap();
    assert_eq!(claude.provider_id, "openrouter");
    assert_eq!(claude.request_model, "anthropic/claude-opus-4.6");
    assert_eq!(claude.canonical_model, "claude-opus-4-6");
    assert_eq!(resolve_prompt_policy(claude).bundle_id, "claude-chat");
}

#[test]
fn exact_profile_selects_provider_and_family_without_using_display_or_billing() {
    let client = runtime(vec![
        sdk_profile(
            "sdk-a",
            "alpha",
            "open_ai_chat",
            model("shared-wire", Some("family-a")),
        ),
        sdk_profile(
            "sdk-b",
            "beta",
            "anthropic_messages",
            model("shared-wire", Some("family-b")),
        ),
    ]);
    assert!(client
        .prompt_target("selected-alias", None, ConversationMode::Code)
        .is_err());
    let alpha = client
        .prompt_target("selected-alias", Some("alpha"), ConversationMode::Code)
        .unwrap();
    let beta = client
        .prompt_target("selected-alias", Some("beta"), ConversationMode::Code)
        .unwrap();
    assert_eq!(alpha.provider_id, "sdk-a");
    assert_eq!(alpha.model_family.as_deref(), Some("family-a"));
    assert_eq!(alpha.profile_name.as_deref(), Some("alpha"));
    assert_eq!(beta.provider_id, "sdk-b");
    assert_eq!(beta.model_family.as_deref(), Some("family-b"));
    assert_eq!(beta.profile_name.as_deref(), Some("beta"));
    assert_eq!(alpha.request_model, "shared-wire");
    assert_eq!(alpha.canonical_model, "shared-wire");
    assert_ne!(
        resolve_prompt_policy(alpha).fingerprint(),
        resolve_prompt_policy(beta).fingerprint()
    );
}

#[test]
fn foundry_uses_explicit_underlying_model_and_keeps_deployment_on_wire() {
    let mut deployment = model("customer-deployment", None);
    deployment["display_model"] = json!("claude-sonnet-4-6");
    deployment["foundry"] = json!({"hosting": "azure", "model_id": "claude-opus-5-5"});
    let client = runtime(vec![sdk_profile(
        "customer-foundry-provider",
        "foundry",
        "foundry_claude",
        deployment,
    )]);
    let target = client
        .prompt_target("selected-alias", Some("foundry"), ConversationMode::Code)
        .unwrap();
    assert_eq!(target.provider_id, "customer-foundry-provider");
    assert_eq!(target.request_model, "customer-deployment");
    assert_eq!(target.canonical_model, "claude-opus-5-5");
    assert_eq!(target.model_family.as_deref(), Some("claude"));
    assert_eq!(resolve_prompt_policy(target).bundle_id, "claude-code");
}

#[test]
fn unknown_namespaces_are_not_inferred_from_protocol_or_claude_substrings() {
    let client = runtime(vec![sdk_profile(
        "custom-sdk-provider",
        "custom",
        "vertex_claude",
        model("vendor/claude-opus-5-5", None),
    )]);
    let target = client
        .prompt_target("selected-alias", Some("custom"), ConversationMode::Chat)
        .unwrap();
    assert_eq!(target.provider_id, "custom-sdk-provider");
    assert_eq!(target.request_model, "vendor/claude-opus-5-5");
    assert_eq!(target.canonical_model, "vendor/claude-opus-5-5");
    assert_eq!(target.model_family, None);
    assert_eq!(
        resolve_prompt_policy(target).bundle_id,
        "generic-chat-lean-v1"
    );
}

#[derive(Debug)]
struct ForbiddenCredentials;

impl CredentialProvider for ForbiddenCredentials {
    fn source<'a>(
        &'a self,
        _: &'a CredentialScope,
    ) -> crate::BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
        panic!("prompt routing must not inspect credential storage");
    }

    fn load<'a>(
        &'a self,
        _: &'a CredentialScope,
    ) -> crate::BoxFuture<'a, Result<Credential, LlmError>> {
        panic!("prompt routing must not load or refresh credentials");
    }
}

#[derive(Debug)]
struct ChangingCatalog {
    reads: AtomicUsize,
    first: Arc<ModelRuntime>,
    second: Arc<ModelRuntime>,
}

impl ModelRuntimeSource for ChangingCatalog {
    fn current(&self, _: &ModelRuntime) -> Arc<ModelRuntime> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first.clone()
        } else {
            self.second.clone()
        }
    }
}

#[tokio::test]
async fn prompt_target_captures_one_catalog_and_never_resolves_credentials() {
    let make = |provider, family| {
        runtime(vec![sdk_profile(
            provider,
            "same-profile",
            "anthropic_messages",
            model("shared-wire", Some(family)),
        )])
        .with_credential_provider(Arc::new(ForbiddenCredentials))
    };
    let first = Arc::new(make("provider-a", "family-a"));
    let second = Arc::new(make("provider-b", "family-b"));
    let source = Arc::new(ChangingCatalog {
        reads: AtomicUsize::new(0),
        first: first.clone(),
        second,
    });
    let client = first.as_ref().clone().with_catalog_source(source.clone());
    crate::catalog_task::scope(async {
        for mode in [ConversationMode::Chat, ConversationMode::Code] {
            let target = client
                .prompt_target("selected-alias", Some("same-profile"), mode)
                .unwrap();
            assert_eq!(target.provider_id, "provider-a");
            assert_eq!(target.model_family.as_deref(), Some("family-a"));
        }
    })
    .await;
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    let next = client
        .prompt_target(
            "selected-alias",
            Some("same-profile"),
            ConversationMode::Code,
        )
        .unwrap();
    assert_eq!(next.provider_id, "provider-b");
    assert_eq!(next.model_family.as_deref(), Some("family-b"));
    assert_eq!(source.reads.load(Ordering::SeqCst), 2);
}
