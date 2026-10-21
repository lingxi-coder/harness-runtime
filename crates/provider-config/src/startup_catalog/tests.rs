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
        assert!(result
            .catalog
            .providers
            .iter()
            .any(|p| p.profile_name == "openai-chatgpt"));
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
fn retired_official_default_can_fall_back_without_rewriting_user_configuration() {
    let catalog = parsed(&fixture(), CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog;
    assert_eq!(
        retired_default("anthropic/claude-sonnet-5", &catalog.providers),
        Some(("deepseek-flash".into(), "deepseek".into()))
    );
    assert!(retired_default("deepseek/deepseek-flash", &catalog.providers).is_none());
    assert!(retired_default("private/my-model", &catalog.providers).is_none());
}

#[test]
fn an_unlisted_primary_format_does_not_rebind_a_surviving_formats_credential() {
    let mut value = fixture();
    let wire = presets::builtin_provider_definition("deepseek-search")
        .unwrap()
        .unwrap()
        .profile;
    value["providers"][0]["id"] = json!("deepseek-search");
    value["providers"][0]["baseUrl"] = json!(wire.base_url);
    value["providers"][0]["protocol"] = serde_json::to_value(wire.protocol).unwrap();
    value["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("executionDefinition");
    value["models"][0]["providerId"] = json!("deepseek-search");
    let snapshot = parsed(&value, CatalogPlatform::Desktop, "0.1.0")
        .unwrap()
        .catalog;
    let result = crate::assemble(crate::AssembleInputs {
        official_catalog: Some(snapshot),
        anthropic_api_base: "https://api.anthropic.com".into(),
        anthropic_models: Vec::new(),
        anthropic_has_api_key: false,
        anthropic_has_oauth: false,
        user_providers: BTreeMap::new(),
        routing: None,
    });
    let credential = result
        .credential_sources
        .iter()
        .find(|c| c.profile_name == "deepseek-search")
        .unwrap();
    assert_eq!(credential.credential_id, "deepseek");
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
    assert!(!parsed(&value, CatalogPlatform::Ios, "2.4.1")
        .unwrap()
        .catalog
        .providers
        .iter()
        .any(|p| p.profile_name == "deepseek"));
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
        has_provider(&value, "deepseek"),
        "an existing SDK definition can supply complete facts"
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
async fn conditional_get_304_cold_cache_and_owner_snapshot_are_consistent() {
    let home = Home::new();
    let body = fixture().to_string();
    let (url, server) = serve(vec![
        (200, body),
        (304, String::new()),
        (304, String::new()),
    ]);
    let first = source(&home, &url, CatalogPlatform::Android, "0.1.0");
    first.refresh().await.unwrap();
    let snapshot = first.load().await;
    assert_eq!(deepseek(&snapshot).models.len(), 1);
    assert!(Arc::ptr_eq(&snapshot, &first.clone().load().await));
    let cold = source(&home, &url, CatalogPlatform::Android, "0.1.0");
    let restored = cold.load().await;
    assert_eq!(context(&snapshot), context(&restored));
    let requests = tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    assert!(
        requests[0].contains("platform=android")
            && requests[0].contains("appVersion=0.1.0")
            && requests[0].contains("schemaVersion=1")
    );
    assert!(!requests[0].to_lowercase().contains("authorization:"));
    assert!(requests[1]
        .to_lowercase()
        .contains("if-none-match: \"fixture\""));
    assert_ne!(
        first.cache_path(),
        source(&home, &url, CatalogPlatform::Ios, "0.1.0").cache_path()
    );
    assert_ne!(
        first.cache_path(),
        source(&home, &url, CatalogPlatform::Android, "0.2.0").cache_path()
    );
}

#[tokio::test]
async fn malformed_refresh_and_outage_preserve_valid_cache_without_touching_settings() {
    let home = Home::new();
    let settings = home.0.join("settings.json");
    fs::write(&settings, b"user-owned settings and connection ids").unwrap();
    let mut value = fixture();
    value["models"][0]["contextWindow"] = json!(333333);
    value["models"][0]["maxOutputTokens"] = json!(8192);
    let (url, server) = serve(vec![
        (200, value.to_string()),
        (200, "broken JSON".into()),
        (503, "down".into()),
    ]);
    source(&home, &url, CatalogPlatform::Desktop, "0.1.0")
        .refresh()
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            context(
                source(&home, &url, CatalogPlatform::Desktop, "0.1.0")
                    .load()
                    .await
                    .as_ref()
            ),
            333333
        );
    }
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    assert_eq!(
        fs::read(&settings).unwrap(),
        b"user-owned settings and connection ids"
    );
    assert!(fs::read_dir(home.0.join("cache/model-catalog"))
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
}

#[tokio::test]
async fn invalid_304_and_corrupt_cache_fall_back_to_embedded_definitions() {
    let home = Home::new();
    let (url, server) = serve(vec![(304, String::new()), (304, String::new())]);
    let src = source(&home, &url, CatalogPlatform::Cli, "0.1.0");
    fs::create_dir_all(&src.cache_dir).unwrap();
    fs::write(src.cache_path(), b"{broken").unwrap();
    assert!(src.load().await.providers.len() > 2);
    let mut other = source(&home, &url, CatalogPlatform::Cli, "0.1.0");
    other.endpoint = url;
    assert!(other.load().await.providers.len() > 2);
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn background_refresh_is_used_by_new_owners_and_never_swaps_an_active_snapshot() {
    let home = Home::new();
    let mut old = fixture();
    old["models"][0]["contextWindow"] = json!(333333);
    old["models"][0]["maxOutputTokens"] = json!(8192);
    let mut new = old.clone();
    new["publishedAt"] = json!(2000);
    new["models"][0]["contextWindow"] = json!(444444);
    let (url, server) = serve(vec![
        (200, old.to_string()),
        (200, new.to_string()),
        (304, String::new()),
    ]);
    let first = source(&home, &url, CatalogPlatform::Ios, "2.4.1");
    first.refresh().await.unwrap();
    let active = first.load().await;
    assert_eq!(context(&active), 333333);
    for _ in 0..100 {
        if first.cached().is_some_and(|(_, p)| p.published_at == 2000) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(first.cached().unwrap().1.published_at, 2000);
    assert!(Arc::ptr_eq(&active, &first.load().await));
    assert_eq!(context(&active), 333333);
    let newest = source(&home, &url, CatalogPlatform::Ios, "2.4.1")
        .load()
        .await;
    assert_eq!(context(&newest), 444444);
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn canceling_a_cold_prefetch_does_not_poison_the_startup_owner() {
    let home = Home::new();
    let (url, server) = serve(vec![(200, fixture().to_string())]);
    let source = source(&home, &url, CatalogPlatform::Cli, "0.9.0");
    drop(source.prefetch());
    tokio::task::yield_now().await;
    assert!(source.settled.get().is_none());
    // A cold owner settles embedded definitions without waiting for HTTP.
    assert!(source.load().await.providers.len() > 2);
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn cold_start_does_not_wait_for_a_stalled_network() {
    let home = Home::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    // Keep the socket listening without accepting or sending any HTTP response.
    let source = source(&home, &endpoint, CatalogPlatform::Desktop, "0.1.0");
    let local = tokio::time::timeout(Duration::from_secs(1), source.load())
        .await
        .expect("cold startup must settle local data without HTTP");
    assert!(local.providers.len() > 2);
    assert!(Arc::ptr_eq(&local, &source.load().await));
    assert!(source.cached().is_none());
    drop(listener);
}
