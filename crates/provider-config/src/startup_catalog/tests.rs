use super::*;
use serde_json::{json, Value};
use std::{net::TcpListener, thread};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/startup-catalog.json")).unwrap()
}
fn parsed(
    value: &Value,
    platform: CatalogPlatform,
    version: &str,
) -> Result<ParsedCatalog, String> {
    parse_catalog(&value.to_string(), platform, version)
}
fn deepseek(catalog: &llm_runtime::BuiltinCatalog) -> &llm_runtime::ProviderProfile {
    catalog
        .providers
        .iter()
        .find(|p| p.profile_name == "deepseek")
        .unwrap()
}
fn context(catalog: &llm_runtime::BuiltinCatalog) -> u64 {
    deepseek(catalog).models[0]
        .metadata
        .context_window_tokens
        .unwrap()
}
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("catalog-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn serve(responses: Vec<(u16, String)>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let start = std::time::Instant::now();
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && start.elapsed() < Duration::from_secs(8) =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    other => panic!("catalog server accept: {other:?}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            requests.push(String::from_utf8(request).unwrap());
            write!(socket,"HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (format!("http://{address}/v1/model-catalog"), task)
}
fn source(home: &Home, endpoint: &str, platform: CatalogPlatform, version: &str) -> StartupCatalog {
    let mut source = StartupCatalog::new(platform, version, &home.0);
    source.endpoint = endpoint.into();
    source
}

// A running mobile engine constructs provider services once. Publish a second
// validated official definition and inspect the actual SDK view it retained.
#[tokio::test]
async fn media_services_follow_the_published_audio_definition() {
    struct NoTransport;
    #[async_trait::async_trait]
    impl lingxi_llm_client::Transport for NoTransport {
        async fn send(
            &self,
            _: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            panic!("this test only inspects SDK snapshots")
        }
    }

    let home = Home::new();
    let source = source(
        &home,
        "http://unused.invalid",
        CatalogPlatform::Android,
        "0.1.0",
    );
    let inputs = crate::AssembleInputs {
        official_catalog: Some((*source.snapshot()).clone()),
        anthropic_api_base: "https://api.anthropic.com".into(),
        anthropic_models: Vec::new(),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers: BTreeMap::new(),
        routing: None,
    };
    let template = llm_runtime::ModelRuntime::from_config(
        crate::assemble_for_region(inputs.clone(), llm_runtime::Region::International)
            .client_config,
    )
    .unwrap()
    .with_catalog_source(source.runtime_source(inputs.clone(), llm_runtime::Region::International));
    let services = template
        .provider_services(llm_runtime::Region::International, Arc::new(NoTransport))
        .unwrap();
    let child_template = llm_runtime::ModelRuntime::from_config(
        crate::assemble_for_region(inputs.clone(), llm_runtime::Region::International)
            .client_config,
    )
    .unwrap()
    .with_catalog_source(source.runtime_source(inputs.clone(), llm_runtime::Region::International));
    let child_services = child_template
        .provider_services(llm_runtime::Region::International, Arc::new(NoTransport))
        .unwrap();
    let other_region_services = child_template
        .provider_services(llm_runtime::Region::ChinaMainland, Arc::new(NoTransport))
        .unwrap();
    let mut different_account =
        crate::assemble_for_region(inputs.clone(), llm_runtime::Region::International)
            .client_config;
    different_account
        .providers
        .iter_mut()
        .find(|provider| provider.profile_name == "openai")
        .unwrap()
        .credential = llm_runtime::CredentialConfig::Static {
        id: "another-account".into(),
    };
    let different_account = llm_runtime::ModelRuntime::from_config(different_account)
        .unwrap()
        .provider_services(llm_runtime::Region::International, Arc::new(NoTransport))
        .unwrap();
    let late_child = llm_runtime::ModelRuntime::from_config(
        crate::assemble_for_region(inputs.clone(), llm_runtime::Region::International)
            .client_config,
    )
    .unwrap()
    .with_catalog_source(source.runtime_source(inputs, llm_runtime::Region::International));
    let late_child_services = late_child
        .provider_services(llm_runtime::Region::International, Arc::new(NoTransport))
        .unwrap();

    let mut next: Value =
        serde_json::from_str(include_str!("../../data/official-catalog.json")).unwrap();
    let openai = next["providers"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|provider| provider["id"] == "openai")
        .unwrap();
    let definition = openai["executionDefinition"]["source"].as_str().unwrap();
    let updated = definition.replace(
        "https://api.openai.com/v1/audio/",
        "https://new-audio.example/v1/audio/",
    );
    assert_ne!(definition, updated);
    openai["executionDefinition"]["sha256"] =
        json!(format!("{:x}", Sha256::digest(updated.as_bytes())));
    openai["executionDefinition"]["source"] = json!(updated);
    let published = parsed(&next, CatalogPlatform::Android, "0.1.0").unwrap();
    llm_runtime::catalog_task::scope(async {
        // Binding and the model selection both pin their owner before publish.
        let _ = template.current();
        let _ = child_template.current();
        assert!(services.profile_is_current("openai"));
        assert!(services.profile_matches_current_of(&child_services, "openai"));
        assert!(!services.profile_is_current("missing"));
        assert!(!services.profile_matches_current_of(&different_account, "openai"));
        assert!(!services.profile_matches_current_of(&other_region_services, "openai"));
        *source.shared.snapshot.write().unwrap() = Some(Arc::new(published.catalog));
        source
            .shared
            .updates
            .send_modify(|generation| *generation += 1);
        // An admitted turn can complete with its original route.
        assert!(services.profile_is_current("openai"));
        assert!(services.profile_matches_current_of(&child_services, "openai"));
        // A different owner first resolved after publication must not inherit
        // the parent's old service merely because the profile string matches.
        assert!(!services.profile_matches_current_of(&late_child_services, "openai"));
    })
    .await;

    let current = template
        .current()
        .provider_services(llm_runtime::Region::International, Arc::new(NoTransport))
        .unwrap();
    let endpoint = |services: &llm_runtime::services::ProviderServices| {
        let snapshot = services.snapshot();
        match &snapshot.profile("openai").unwrap().audio {
            lingxi_llm_client::protocol::ServiceSetting::Enabled(route) => {
                route.transcriptions_endpoint.clone()
            }
            _ => panic!("the OpenAI audio route is unavailable"),
        }
    };
    assert_eq!(
        endpoint(&current),
        "https://new-audio.example/v1/audio/transcriptions"
    );
    assert_eq!(
        endpoint(&services),
        "https://api.openai.com/v1/audio/transcriptions"
    );
    // A subsequent turn is denied instead of silently using the old endpoint.
    assert!(!services.profile_is_current("openai"));
    assert!(!services.profile_matches_current_of(&child_services, "openai"));
}

#[test]
fn all_four_platforms_project_the_same_complete_executable_snapshot() {
    for platform in [
        CatalogPlatform::Desktop,
        CatalogPlatform::Cli,
        CatalogPlatform::Android,
        CatalogPlatform::Ios,
    ] {
        let mut value = fixture();
        value["models"][0]["name"] = json!("Published Flash");
        value["models"][0]["contextWindow"] = json!(333333);
        value["models"][0]["maxOutputTokens"] = json!(8192);
        let result = parsed(&value, platform, "0.1.0").unwrap();
        assert_eq!(context(&result.catalog), 333333);
        let provider = deepseek(&result.catalog);
        let wire = provider.wire_profile.as_ref().unwrap();
        assert!(wire.pricing.peak.is_some());
        assert_eq!(wire.models[0].display_model, "Published Flash");
        assert_eq!(wire.models[0].metadata.max_output_tokens, Some(8192));
        assert!(!wire.connection.protocol_profiles.is_empty());
        assert_eq!(
            serde_json::to_value(&wire.inference).unwrap()["reasoning"],
            "thinking_type"
        );
        assert_eq!(
            wire.models[0].pricing.as_ref().unwrap().input_per_million,
            Some(0.3)
        );
        assert_eq!(
            result.catalog.providers.len(),
            1,
            "cloud inventory does not resurrect embedded providers"
        );
    }
}

#[test]
fn newly_published_model_is_routable_and_visible_without_a_binary_shortlist_update() {
    let mut value = fixture();
    let definition = &mut value["providers"][0]["executionDefinition"];
    let source=format!("{}\n[[model]]\nid = \"deepseek-published-new\"\nname = \"Published new model\"\ncontext_window = 1000000\nmax_output = 8192\ntool_call = true\n",definition["source"].as_str().unwrap());
    definition["sha256"] = json!(format!("{:x}", Sha256::digest(source.as_bytes())));
    definition["source"] = json!(source);
    value["models"][0]["id"] = json!("deepseek-published-new");
    let catalog = parsed(&value, CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog;
    let runtime = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers,
    })
    .unwrap();
    let route = runtime
        .resolve_media_route("deepseek-published-new", Some("deepseek"))
        .unwrap();
    assert_eq!(route.main.request_model, "deepseek-published-new");
    let wire = runtime
        .prompt_cache_profile("deepseek-published-new", Some("deepseek"))
        .unwrap();
    assert!(wire
        .models
        .iter()
        .any(|m| m.request_model == "deepseek-published-new"));
    let listing = runtime
        .available_models()
        .into_iter()
        .find(|m| m.request_model == "deepseek-published-new")
        .unwrap();
    assert!(listing.metadata.catalog_offered);
    let host = lingxi_core::host::ModelListing {
        request_model: listing.request_model,
        provider_id: listing.profile_name,
        metadata: listing.metadata,
        capabilities: lingxi_core::host::ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        lingxi_core::host::curated_model_refs(std::slice::from_ref(&host), &[], "", None),
        vec!["deepseek/deepseek-published-new"]
    );
    assert_eq!(lingxi_core::host::provider_model_catalog(&[host]).len(), 1);
}

#[test]
fn availability_is_inclusive_platform_scoped_and_uses_semver_precedence() {
    let mut value = fixture();
    value["models"][0]["platforms"] = json!(["ios"]);
    value["models"][0]["minAppVersion"] = json!("2.4.1");
    value["models"][0]["maxAppVersion"] = json!("2.4.1");
    let has = |platform, version| {
        parsed(&value, platform, version)
            .unwrap()
            .catalog
            .providers
            .iter()
            .any(|p| p.profile_name == "deepseek")
    };
    assert!(has(CatalogPlatform::Ios, "2.4.1+build.9"));
    assert!(!has(CatalogPlatform::Ios, "2.4.1-rc.1"));
    assert!(!has(CatalogPlatform::Ios, "2.4.2"));
    assert!(!has(CatalogPlatform::Android, "2.4.1"));
    value["models"][0]["minAppVersion"] = json!("2.4.1+build.9");
    value["models"][0]["maxAppVersion"] = json!("2.4.1+build.1");
    assert!(parsed(&value, CatalogPlatform::Ios, "2.4.1")
        .unwrap()
        .catalog
        .providers
        .iter()
        .any(|p| p.profile_name == "deepseek"));
    value["providers"][0]["enabled"] = json!(false);
    let disabled = parsed(&value, CatalogPlatform::Ios, "2.4.1")
        .unwrap()
        .catalog;
    assert!(deepseek(&disabled)
        .models
        .iter()
        .all(|m| m.metadata.status.as_deref() == Some("disabled")));
}
fn has_provider(value: &Value, name: &str) -> bool {
    parsed(value, CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog
        .providers
        .iter()
        .any(|p| p.profile_name == name)
}

#[test]
fn invalid_source_identity_limits_and_unknown_models_are_not_admitted() {
    for mutate in [0, 1, 2, 3, 4, 5] {
        let mut value = fixture();
        match mutate {
            0 => value["providers"][0]["executionDefinition"]["sha256"] = json!("0".repeat(64)),
            1 => value["models"][0]["id"] = json!("not-defined"),
            2 => value["models"][0]["maxOutputTokens"] = json!(999999999),
            3 => value["providers"][0]["baseUrl"] = json!("http://unsafe.invalid"),
            4 => value["models"][0]["maxAppVersion"] = json!("invalid"),
            _ => {
                let copy = value["models"][0].clone();
                value["models"].as_array_mut().unwrap().push(copy);
            }
        }
        assert!(
            parsed(&value, CatalogPlatform::Desktop, "0.1.0").is_err(),
            "case {mutate}"
        );
    }
    let mut value = fixture();
    value["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("executionDefinition");
    assert!(
        parsed(&value, CatalogPlatform::Desktop, "0.1.0").is_err(),
        "execution definitions are mandatory"
    );
    value["providers"][0]["id"] = json!("new-provider");
    value["models"][0]["providerId"] = json!("new-provider");
    assert!(parsed(&value, CatalogPlatform::Desktop, "0.1.0").is_err());
}

#[test]
fn settings_models_connections_credential_ids_and_routing_win_after_refresh() {
    let snapshot = parsed(&fixture(), CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog;
    let users = BTreeMap::from([
        (
            "deepseek".into(),
            json!({"type":"openai","baseUrl":"https://custom.invalid/v1","apiKeyEnv":"PRIVATE_DEEPSEEK_KEY","models":["my-deployment"]}),
        ),
        (
            "private".into(),
            json!({"type":"openai","baseUrl":"https://private.invalid/v1","apiKeyEnv":"PRIVATE_KEY","models":["my-model"]}),
        ),
    ]);
    let before = users.clone();
    let result = crate::assemble(crate::AssembleInputs {
        official_catalog: Some(snapshot),
        anthropic_api_base: "https://api.anthropic.com".into(),
        anthropic_models: Vec::new(),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers: users,
        routing: Some(json!({"aliases":{"mine":"private/my-model"}})),
    });
    let custom = result
        .client_config
        .providers
        .iter()
        .find(|p| p.profile_name == "deepseek")
        .unwrap();
    assert_eq!(custom.base_url, "https://custom.invalid/v1");
    assert_eq!(custom.models[0].request_model, "my-deployment");
    let credential = result
        .credential_sources
        .iter()
        .find(|c| c.profile_name == "deepseek")
        .unwrap();
    assert_eq!(credential.credential_id, "deepseek");
    assert_eq!(credential.env_var.as_deref(), Some("PRIVATE_DEEPSEEK_KEY"));
    assert_eq!(before["private"]["models"][0], "my-model");
    let runtime = llm_runtime::ModelRuntime::from_config(result.client_config).unwrap();
    assert!(runtime
        .available_models()
        .iter()
        .any(|m| m.request_model == "my-model"));
    let custom = runtime
        .available_models()
        .into_iter()
        .find(|m| m.request_model == "my-deployment")
        .unwrap();
    assert!(custom.metadata.catalog_offered);
}

#[tokio::test]
async fn schema_capabilities_and_lifecycle_are_explicit() {
    let mut value = fixture();
    value["schemaVersion"] = json!(1);
    assert!(parsed(&value, CatalogPlatform::Desktop, "0.1.0").is_err());
    let mut value = fixture();
    value["models"][0]["requiredCapabilities"] = json!(["protocol.future"]);
    assert!(!has_provider(&value, "deepseek"));
    value["models"][0]["requiredCapabilities"] = json!([]);
    value["models"][0]["status"] = json!("deprecated");
    let catalog = parsed(&value, CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog;
    assert_eq!(
        deepseek(&catalog).models[0].metadata.status.as_deref(),
        Some("deprecated")
    );
    value["models"][0]["status"] = json!("disabled");
    let catalog = parsed(&value, CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog;
    let runtime = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers,
    })
    .unwrap();
    assert!(!runtime.model_enabled("deepseek-flash", Some("deepseek")));
    assert!(
        runtime
            .resolve_media_route("deepseek-flash", Some("deepseek"))
            .is_ok(),
        "a disabled saved default still has metadata so settings can open"
    );
    let request = llm_runtime::LlmRequest::new("deepseek-flash").with_profile("deepseek");
    assert!(
        matches!(
            runtime.prepare(&request).await,
            Err(llm_runtime::LlmError::ModelUnavailable)
        ),
        "dispatch must reject the disabled model before reading credentials"
    );
}

#[tokio::test]
async fn conditional_get_publishes_to_current_owner_and_keeps_task_snapshot() {
    let home = Home::new();
    let mut next = fixture();
    next["models"][0]["contextWindow"] = json!(333333);
    next["models"][0]["maxOutputTokens"] = json!(8192);
    let (url, server) = serve(vec![(200, next.to_string()), (304, String::new())]);
    let source = source(&home, &url, CatalogPlatform::Android, "0.1.0");
    let task_snapshot = source.snapshot();
    let mut updates = source.subscribe();
    assert!(source.trigger(CatalogTrigger::Manual).await.unwrap());
    updates.changed().await.unwrap();
    assert_eq!(context(&source.snapshot()), 333333);
    assert!(!Arc::ptr_eq(&task_snapshot, &source.snapshot()));
    assert!(
        !source.trigger(CatalogTrigger::Manual).await.unwrap(),
        "304 must not publish another generation"
    );
    let requests = tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    assert!(requests[0].contains("schemaVersion=2"));
    assert!(requests[0].contains("capability=protocol.open_ai_responses"));
    assert!(!requests[0].to_lowercase().contains("authorization:"));
    assert!(requests[1]
        .to_lowercase()
        .contains("if-none-match: \"fixture\""));
}

#[tokio::test]
async fn refresh_policy_respects_foreground_network_ttls_and_failure_backoff() {
    let home = Home::new();
    let (url, server) = serve(vec![
        (200, fixture().to_string()),
        (503, "down".into()),
        (304, String::new()),
    ]);
    let source = source(&home, &url, CatalogPlatform::Ios, "0.1.0");
    source.trigger(CatalogTrigger::Startup).await.unwrap();
    assert!(!source.trigger(CatalogTrigger::Foreground).await.unwrap());
    assert!(!source.trigger(CatalogTrigger::Settings).await.unwrap());
    source.shared.timing.lock().unwrap().last_success =
        Some(std::time::Instant::now() - Duration::from_secs(901));
    let (mut saved, parsed) = source.cached().unwrap();
    saved.checked_at = 0;
    source.save(&saved, parsed.published_at).unwrap();
    assert!(!source.trigger(CatalogTrigger::Foreground).await.unwrap());
    assert!(source.trigger(CatalogTrigger::Settings).await.is_err());
    let active = source.snapshot();
    assert!(!source.trigger(CatalogTrigger::Timer).await.unwrap());
    source.trigger(CatalogTrigger::Background).await.unwrap();
    assert!(!source
        .trigger(CatalogTrigger::NetworkRestored)
        .await
        .unwrap());
    source.trigger(CatalogTrigger::NetworkLost).await.unwrap();
    assert!(!source.trigger(CatalogTrigger::Foreground).await.unwrap());
    source
        .trigger(CatalogTrigger::NetworkRestored)
        .await
        .unwrap();
    assert!(
        Arc::ptr_eq(&active, &source.snapshot()) || context(&active) == context(&source.snapshot())
    );
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn refresh_failure_preserves_cache_and_user_configuration() {
    let home = Home::new();
    fs::write(home.0.join("settings.json"), b"user settings").unwrap();
    let (url, server) = serve(vec![(200, fixture().to_string()), (200, "broken".into())]);
    let source = source(&home, &url, CatalogPlatform::Desktop, "0.1.0");
    source.refresh().await.unwrap();
    let active = source.snapshot();
    assert!(source.refresh().await.is_err());
    assert!(Arc::ptr_eq(&active, &source.snapshot()));
    assert!(source.cached().is_some());
    assert_eq!(
        fs::read(home.0.join("settings.json")).unwrap(),
        b"user settings"
    );
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn execution_source_updates_models_and_keeps_custom_profiles() {
    let home = Home::new();
    let mut next = fixture();
    next["models"][0]["contextWindow"] = json!(333333);
    next["models"][0]["maxOutputTokens"] = json!(8192);
    let (url, server) = serve(vec![(200, next.to_string())]);
    let source = source(&home, &url, CatalogPlatform::Desktop, "0.1.0");
    let inputs = crate::AssembleInputs {
        official_catalog: Some((*source.snapshot()).clone()),
        anthropic_api_base: "https://api.anthropic.com".into(),
        anthropic_models: Vec::new(),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers: BTreeMap::from([(
            "private".into(),
            json!({"type":"openai","baseUrl":"https://custom.invalid/v1","models":["private-model"]}),
        )]),
        routing: None,
    };
    let template =
        llm_runtime::ModelRuntime::from_config(crate::assemble(inputs.clone()).client_config)
            .unwrap()
            .with_catalog_source(source.runtime_source(inputs, llm_runtime::Region::International));
    llm_runtime::catalog_task::scope(async {
        let task_client = template.current();
        source.refresh().await.unwrap();
        assert!(
            Arc::ptr_eq(&task_client, &template.current()),
            "a running task retains its original directory"
        );
    })
    .await;
    let task_client = llm_runtime::ModelRuntime::from_config(
        crate::assemble(crate::AssembleInputs {
            official_catalog: Some(source.embedded_catalog()),
            anthropic_api_base: "https://api.anthropic.com".into(),
            anthropic_models: Vec::new(),
            anthropic_has_api_key: false,
            anthropic_has_oauth: false,
            user_providers: BTreeMap::new(),
            routing: None,
        })
        .client_config,
    )
    .unwrap()
    .current();
    let next_client = template.current();
    assert!(next_client
        .available_models()
        .iter()
        .any(|m| m.request_model == "private-model"));
    assert!(next_client
        .available_models()
        .iter()
        .any(|m| m.profile_name == "deepseek" && m.metadata.context_window_tokens == Some(333333)));
    assert!(!Arc::ptr_eq(&task_client, &next_client));
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn startup_and_corrupt_cache_never_wait_for_network() {
    let home = Home::new();
    let source = source(
        &home,
        "http://127.0.0.1:1/catalog",
        CatalogPlatform::Cli,
        "0.1.0",
    );
    fs::create_dir_all(&source.cache_dir).unwrap();
    fs::write(source.cache_path(), b"broken").unwrap();
    let local = tokio::time::timeout(Duration::from_millis(100), source.load())
        .await
        .unwrap();
    assert!(local.providers.len() > 2);
    assert!(Arc::ptr_eq(&local, &source.snapshot()));
}
