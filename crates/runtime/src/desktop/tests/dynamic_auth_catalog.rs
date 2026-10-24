use super::*;

fn assembled(api_base: &str, api_key: bool, oauth: bool) -> provider_config::Assembled {
    provider_config::assemble(assembly_inputs(api_base, api_key, oauth))
}

fn assembly_inputs(api_base: &str, api_key: bool, oauth: bool) -> provider_config::AssembleInputs {
    provider_config::AssembleInputs {
        official_catalog: None,
        anthropic_api_base: api_base.to_string(),
        anthropic_models: Vec::new(),
        anthropic_has_api_key: api_key,
        anthropic_has_oauth: oauth,
        user_providers: Default::default(),
        routing: None,
    }
}

fn refresher(
    config: &provider_config::Assembled,
    credentials: Arc<secret::CredentialManager>,
    api_key: bool,
    oauth: bool,
    oauth_slot: bool,
) -> FusionCatalogRefresher {
    FusionCatalogRefresher {
        availability: Default::default(),
        availability_probe_completed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        mutation_epoch: Default::default(),
        credentials,
        credential_sources: config
            .credential_sources
            .iter()
            .filter(|source| {
                matches!(
                    source.profile_name.as_str(),
                    "anthropic" | "anthropic-chat" | "deepseek"
                ) || source.profile_name.starts_with("anthropic#")
            })
            .cloned()
            .collect(),
        anthropic_has_api_key: fusion_route_flag(api_key),
        anthropic_has_oauth: fusion_route_flag(oauth),
        anthropic_dynamic_auth: super::super::fusion_services::anthropic_dynamic_auth_profile(
            &config.client_config.providers,
        ),
        anthropic_oauth_delegate_slot: oauth_slot,
        openai_chatgpt_available: fusion_route_flag(false),
        openai_chatgpt_session_slot: false,
        isolated: true,
    }
}

fn available(refresher: &FusionCatalogRefresher, profile: &str) -> bool {
    *refresher.availability.read().unwrap().get(profile).unwrap()
}

#[tokio::test]
async fn fusion_production_managed_boot_ignores_scalar_and_adopts_first_host_key() {
    let (_home, mut config) = test_config(true);
    config.isolated_credential_storage = true;
    config.api_key = "legacy-scalar-must-be-suppressed".to_string();
    let shared = build_shared_credential_stack_for_config(&config)
        .await
        .unwrap();
    shared.credentials.use_host_managed_provider_keys();
    let credentials = shared.credentials.clone();
    let stack =
        super::super::credentials::resolve_llm_stack_with_credentials(&config, shared, None)
            .await
            .unwrap();
    assert!(stack.resolved_anthropic_api_key.is_none());
    assert!(!stack.provider_availability["anthropic"]);
    assert!(stack.fusion_catalog_refresher.anthropic_dynamic_auth);
    credentials
        .set_provider_key_ephemeral("anthropic", "host-key")
        .await;
    assert!(
        stack
            .fusion_catalog_refresher
            .refresh_after_credential_write("anthropic")
            .await
    );
    assert!(available(&stack.fusion_catalog_refresher, "anthropic"));
}

#[tokio::test]
async fn fusion_api_boot_keeps_only_canonical_oauth_after_managed_key_delete() {
    let config = assembled("https://api.anthropic.com", true, false);
    let credentials = fusion_test_credentials_with_stored_anthropic_key();
    assert!(credentials.has_provider_key("anthropic").await.unwrap());
    credentials.use_host_managed_provider_keys();
    credentials
        .set_provider_key_ephemeral("anthropic", "host-key")
        .await;
    // The stored OAuth delegate was installed even though the key won at boot.
    let refresher = refresher(&config, credentials.clone(), true, true, true);
    assert!(refresher.anthropic_dynamic_auth);
    refresher.refresh().await;
    assert!(available(&refresher, "anthropic"));
    assert!(available(&refresher, "anthropic-chat"));

    credentials
        .suppress_provider_key_ephemeral("anthropic")
        .await;
    refresher.mark_credential_removed("anthropic").await;
    assert!(available(&refresher, "anthropic"));
    assert!(!available(&refresher, "anthropic-chat"));
    assert!(!credentials
        .has_provider_key("anthropic-api-key")
        .await
        .unwrap());
    // An unrelated later refresh must not resurrect the native API-key slot
    // or suppress the canonical route whose live request now uses OAuth.
    credentials
        .set_provider_key_ephemeral("deepseek", "other-key")
        .await;
    assert!(refresher.refresh_after_credential_write("deepseek").await);
    assert!(available(&refresher, "anthropic"));
    assert!(!available(&refresher, "anthropic-chat"));

    refresher.mark_credential_removed("anthropic-oauth").await;
    refresher.refresh().await;
    assert!(!available(&refresher, "anthropic"));
    // The delegate slot stays installed across logout/login.
    assert!(
        refresher
            .refresh_after_credential_write("anthropic-oauth")
            .await
    );
    assert!(available(&refresher, "anthropic"));
    assert!(!available(&refresher, "anthropic-chat"));
}

#[tokio::test]
async fn fusion_oauth_boot_adopts_api_key_without_restart_and_survives_oauth_logout() {
    let config = assembled("https://api.anthropic.com", false, true);
    let credentials = fusion_delete_test_credentials();
    credentials.use_host_managed_provider_keys();
    let refresher = refresher(&config, credentials.clone(), false, true, true);
    refresher.refresh().await;
    assert!(available(&refresher, "anthropic"));
    assert!(!available(&refresher, "anthropic-chat"));

    credentials
        .set_provider_key_ephemeral("anthropic-api-key", "first-key")
        .await;
    assert!(
        refresher
            .refresh_after_credential_write("anthropic-api-key")
            .await,
        "the resolved canonical route can adopt this key without restart"
    );
    refresher.mark_credential_removed("anthropic-oauth").await;
    refresher.refresh().await;
    assert!(available(&refresher, "anthropic"));
    assert!(available(&refresher, "anthropic-chat"));

    credentials
        .set_provider_key_ephemeral("anthropic", "rotated-key")
        .await;
    assert!(refresher.refresh_after_credential_write("anthropic").await);
    credentials
        .suppress_provider_key_ephemeral("anthropic")
        .await;
    refresher.mark_credential_removed("anthropic").await;
    refresher.refresh().await;
    assert!(!available(&refresher, "anthropic"));
    assert!(!available(&refresher, "anthropic-chat"));
}

#[tokio::test]
async fn fusion_cold_official_route_adopts_first_key_without_inventing_an_oauth_delegate() {
    let config = assembled("https://api.anthropic.com", false, false);
    let credentials = fusion_delete_test_credentials();
    credentials.use_host_managed_provider_keys();
    let refresher = refresher(&config, credentials.clone(), false, false, false);
    refresher.refresh().await;
    assert!(!available(&refresher, "anthropic"));
    assert!(
        !refresher
            .mark_credential_established("anthropic-oauth")
            .await
    );
    assert!(!available(&refresher, "anthropic"));
    credentials
        .set_provider_key_ephemeral("anthropic", "first-key")
        .await;
    assert!(refresher.refresh_after_credential_write("anthropic").await);
    assert!(available(&refresher, "anthropic"));
}

#[tokio::test]
async fn fusion_custom_anthropic_endpoint_never_inherits_account_oauth() {
    let config = assembled("https://gateway.example", true, true);
    let credentials = fusion_delete_test_credentials();
    credentials.use_host_managed_provider_keys();
    credentials
        .set_provider_key_ephemeral("anthropic", "gateway-key")
        .await;
    let refresher = refresher(&config, credentials.clone(), true, true, true);
    assert!(!refresher.anthropic_dynamic_auth);
    refresher.refresh().await;
    assert!(available(&refresher, "anthropic"));
    credentials
        .suppress_provider_key_ephemeral("anthropic")
        .await;
    refresher.mark_credential_removed("anthropic").await;
    assert!(
        !refresher
            .mark_credential_established("anthropic-oauth")
            .await
    );
    refresher.refresh().await;
    assert!(!available(&refresher, "anthropic"));
    assert!(!available(&refresher, "anthropic-chat"));
}

#[test]
fn fusion_dynamic_auth_capability_requires_resolved_origin_protocol_and_identity() {
    let config = assembled("https://api.anthropic.com/", false, true);
    let profile = config
        .client_config
        .providers
        .iter()
        .find(|profile| profile.profile_name == "anthropic")
        .unwrap()
        .clone();
    let capable =
        |profile| super::super::fusion_services::anthropic_dynamic_auth_profile(&[profile]);
    assert!(capable(profile.clone()));
    let mut custom = profile.clone();
    custom.base_url = "https://api.anthropic.com.example".to_string();
    assert!(!capable(custom));
    let mut compatibility = profile.clone();
    compatibility.protocol = llm_runtime::ProtocolFamily::OpenAiChat;
    assert!(!capable(compatibility));
    let mut renamed = profile.clone();
    renamed.profile_name = "another-profile".to_string();
    assert!(!capable(renamed));
    let mut another_provider = profile.clone();
    another_provider.provider_id = llm_runtime::ProviderId::OpenAI;
    assert!(!capable(another_provider));
    let mut another_key = profile;
    let mut unauthenticated = another_key.clone();
    unauthenticated.auth = llm_runtime::AuthStrategy::None;
    assert!(!capable(unauthenticated));
    another_key.credential = llm_runtime::CredentialConfig::Static {
        id: "other-key".to_string(),
    };
    assert!(!capable(another_key));
}

#[tokio::test]
async fn fusion_managed_generic_delete_ignores_environment_fallback() {
    const VARIABLE: &str = "LINGXI_TEST_FUSION_MANAGED_DELETE_ENV";
    std::env::set_var(VARIABLE, "ambient-key");
    let config = assembled("https://api.anthropic.com", false, false);
    let credentials = fusion_delete_test_credentials();
    credentials.use_host_managed_provider_keys();
    credentials
        .set_provider_key_ephemeral("deepseek", "host-key")
        .await;
    let mut refresher = refresher(&config, credentials.clone(), false, false, false);
    refresher.isolated = false;
    refresher
        .credential_sources
        .iter_mut()
        .find(|source| source.profile_name == "deepseek")
        .unwrap()
        .env_var = Some(VARIABLE.to_string());
    assert!(refresher.refresh_after_credential_write("deepseek").await);
    credentials
        .suppress_provider_key_ephemeral("deepseek")
        .await;
    refresher.mark_credential_removed("deepseek").await;
    assert!(!available(&refresher, "deepseek"));
    refresher.refresh().await;
    assert!(!available(&refresher, "deepseek"));
    std::env::remove_var(VARIABLE);
}

async fn exercise_independent_profile_and_global_key(
    config: &provider_config::Assembled,
    independent_profile: &str,
    independent_credential: &str,
) {
    let credentials = fusion_test_credentials_with_stored_anthropic_key();
    credentials.use_host_managed_provider_keys();
    credentials
        .set_provider_key_ephemeral(independent_credential, "independent-key")
        .await;
    let refresher = refresher(config, credentials.clone(), false, false, false);
    refresher.refresh().await;
    assert!(available(&refresher, independent_profile));
    assert!(!available(&refresher, "anthropic-chat"));
    if independent_profile == "anthropic" {
        assert_eq!(
            refresher.credential_route_accepts_hot_write("anthropic-oauth"),
            None
        );
        let epoch = refresher.current_mutation_epoch();
        assert!(
            refresher
                .mark_credential_established("anthropic-oauth")
                .await
        );
        refresher.mark_credential_removed("anthropic-oauth").await;
        assert_eq!(refresher.current_mutation_epoch(), epoch);
        assert!(available(&refresher, independent_profile));
        assert!(!available(&refresher, "anthropic-chat"));
    }

    for global_id in ["anthropic", "anthropic-api-key"] {
        credentials
            .set_provider_key_ephemeral(global_id, "first-global-key")
            .await;
        assert!(
            refresher.mark_credential_established(global_id).await,
            "the compatibility route loads the global key without a restart"
        );
        assert!(available(&refresher, independent_profile));
        assert!(available(&refresher, "anthropic-chat"));
        credentials
            .set_provider_key_ephemeral(global_id, "rotated-global-key")
            .await;
        assert!(refresher.refresh_after_credential_write(global_id).await);
        assert!(available(&refresher, independent_profile));
        assert!(available(&refresher, "anthropic-chat"));

        credentials.suppress_provider_key_ephemeral(global_id).await;
        refresher.mark_credential_removed(global_id).await;
        assert!(available(&refresher, independent_profile));
        assert!(!available(&refresher, "anthropic-chat"));
        refresher.refresh().await;
        assert!(available(&refresher, independent_profile));
        assert!(!available(&refresher, "anthropic-chat"));
        assert!(
            !credentials
                .has_provider_key("anthropic-api-key")
                .await
                .unwrap(),
            "the native global slot remains suppressed after unrelated probes"
        );
        assert_eq!(
            credentials
                .get_provider_key(independent_credential)
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "independent-key"
        );
    }

    credentials
        .set_provider_key_ephemeral("anthropic", "last-global-key")
        .await;
    assert!(refresher.refresh_after_credential_write("anthropic").await);
    credentials
        .suppress_provider_key_ephemeral(independent_credential)
        .await;
    refresher
        .mark_credential_removed(independent_credential)
        .await;
    assert!(!available(&refresher, independent_profile));
    assert!(available(&refresher, "anthropic-chat"));
    refresher.refresh().await;
    assert!(!available(&refresher, independent_profile));
    assert!(available(&refresher, "anthropic-chat"));
    credentials
        .set_provider_key_ephemeral("anthropic", "another-global-key")
        .await;
    assert!(refresher.refresh_after_credential_write("anthropic").await);
    assert!(!available(&refresher, independent_profile));
    assert!(available(&refresher, "anthropic-chat"));
}

#[tokio::test]
async fn fusion_real_bedrock_override_preserves_independent_canonical_credential() {
    let mut inputs = assembly_inputs("https://api.anthropic.com", false, false);
    inputs.user_providers.insert(
        "anthropic".to_string(),
        serde_json::json!({
            "type": "bedrock-claude", "region": "us-east-1", "models": ["custom-claude"]
        }),
    );
    let config = provider_config::assemble(inputs);
    let canonical = config
        .credential_sources
        .iter()
        .find(|source| source.profile_name == "anthropic")
        .unwrap();
    assert_eq!(
        canonical.provider_id,
        llm_runtime::ProviderId::BedrockClaude
    );
    assert_eq!(canonical.credential_id, "bedrock_sigv4");
    assert!(
        !super::super::fusion_services::anthropic_dynamic_auth_profile(
            &config.client_config.providers
        )
    );
    exercise_independent_profile_and_global_key(&config, "anthropic", "bedrock_sigv4").await;
}

#[tokio::test]
async fn fusion_real_credential_ids_profile_is_independent_of_global_key_mutations() {
    let mut inputs = assembly_inputs("https://api.anthropic.com", false, false);
    inputs.user_providers.insert(
        "anthropic".to_string(),
        serde_json::json!({
            "type": "anthropic", "baseUrl": "https://gateway.example",
            "credentialIds": ["gateway-key"], "models": ["custom-claude"]
        }),
    );
    let config = provider_config::assemble(inputs);
    assert!(config
        .credential_sources
        .iter()
        .any(|source| source.profile_name == "anthropic"
            && source.credential_id == "anthropic-api-key"));
    assert!(config.credential_sources.iter().any(
        |source| source.profile_name == "anthropic#0" && source.credential_id == "gateway-key"
    ));
    exercise_independent_profile_and_global_key(&config, "anthropic#0", "gateway-key").await;
}
