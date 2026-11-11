//! Native322 Read count bytes and the separate final-generation UTF-16 boundary.

use super::*;
use crate::{ProviderRequest, ProviderResponse, StreamingResponse};
use futures::future::BoxFuture;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

#[derive(Debug, Default)]
struct CaptureTransport(Mutex<Vec<ProviderRequest>>, std::sync::atomic::AtomicBool);

impl crate::test_support::FixtureTransport for CaptureTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.0.lock().unwrap().push(request.clone());
        if self.1.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async {
                Ok(ProviderResponse::json(
                    401,
                    serde_json::json!({"type":"error","error":{
                        "type":"authentication_error","message":"fixture rejected token"
                    }}),
                ))
            });
        }
        Box::pin(async {
            Ok(ProviderResponse::json(
                200,
                serde_json::json!({"input_tokens": 2500}),
            ))
        })
    }

    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async { panic!("Read counting cannot open a generation stream") })
    }
}
crate::impl_fixture_transport!(CaptureTransport);

fn service(protocol: crate::ProtocolFamily) -> (ApiService, Arc<CaptureTransport>) {
    let name = if protocol == crate::ProtocolFamily::AnthropicMessages {
        "anthropic"
    } else {
        "openai-chat"
    };
    let mut provider = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|provider| provider.profile_name == name)
        .unwrap();
    provider.profile_name = "read-count-fixture".into();
    provider.provider_id = crate::ProviderId::Custom {
        name: "read-count-fixture".into(),
    };
    provider.base_url = "https://read-count.invalid".into();
    provider.wire_profile = None;
    provider.connection = Default::default();
    provider.auth = crate::AuthStrategy::None;
    provider.credential = crate::CredentialConfig::None;
    provider.models.truncate(1);
    let model = &mut provider.models[0];
    model.request_model = "claude-sonnet-4-6".into();
    model.display_model = "read-count-model".into();
    model.billing_model = "read-count-model".into();
    model.aliases.clear();
    model.capabilities.tools = true;
    let client = ModelRuntime::from_config(crate::ClientConfig {
        providers: vec![provider],
    })
    .unwrap();
    let transport = Arc::new(CaptureTransport::default());
    let service = ApiService::new(
        Arc::new(client),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    (service, transport)
}

#[derive(Debug, Default)]
struct CountAccountStore(Mutex<crate::accounts::HealthDocument>);

#[async_trait::async_trait]
impl crate::accounts::AccountPoolStore for CountAccountStore {
    async fn transact(
        &self,
        operation: crate::accounts::PoolOperation,
    ) -> Result<crate::accounts::PoolResult, String> {
        Ok(self.0.lock().unwrap().apply(operation))
    }
}

#[derive(Debug, Default)]
struct NamedAccountCredentials(Mutex<Vec<crate::CredentialScope>>, Arc<CountAccountStore>);

impl crate::CredentialProvider for NamedAccountCredentials {
    fn source<'a>(
        &'a self,
        _: &'a crate::CredentialScope,
    ) -> BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
        Box::pin(async { Ok(crate::CredentialSource::Stored) })
    }

    fn load<'a>(
        &'a self,
        scope: &'a crate::CredentialScope,
    ) -> BoxFuture<'a, Result<crate::Credential, LlmError>> {
        self.0.lock().unwrap().push(scope.clone());
        Box::pin(async move {
            assert!(matches!(
                scope.profile_name.as_str(),
                "account.a" | "account.b"
            ));
            assert_eq!(scope.expected_identity_generation, Some(7));
            assert_eq!(
                scope.credential_id.as_deref(),
                Some(format!("acct.{}", &scope.profile_name[8..]).as_str())
            );
            Ok(crate::Credential::ApiKey(
                format!("fixture-{}-generation-7", scope.profile_name).into(),
            ))
        })
    }

    fn load_versioned<'a>(
        &'a self,
        scope: &'a crate::CredentialScope,
    ) -> BoxFuture<'a, Result<crate::VersionedCredential, crate::VersionedCredentialError>> {
        Box::pin(async move {
            self.load(scope)
                .await
                .map(|credential| crate::VersionedCredential {
                    credential,
                    credential_revision: Some(11),
                })
                .map_err(Into::into)
        })
    }

    fn current_credential_revision<'a>(
        &'a self,
        _: &'a crate::CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async { Ok(Some(11)) })
    }
}

fn named_account_service(
    protocol: crate::ProtocolFamily,
) -> (
    ApiService,
    Arc<CaptureTransport>,
    Arc<crate::accounts::AccountPool>,
    Arc<NamedAccountCredentials>,
) {
    use lingxi_core::host::provider_accounts::{AccountAuthKind, AccountMetadata, CredentialRef};

    let (base, _) = service(protocol);
    let mut binding = base.client().configuration().providers.remove(0);
    binding.auth = crate::AuthStrategy::ApiKey;
    binding.credential = crate::CredentialConfig::Static {
        id: "visible-binding-cannot-authenticate".into(),
    };
    binding.connection.group = Some("read-count-fixture".into());
    let mut providers = vec![binding.clone()];
    let mut accounts = Vec::new();
    for (priority, id, port) in [(0, "a", 39447), (1, "b", 39448)] {
        let mut profile = binding.clone();
        profile.profile_name = format!("account.{id}");
        profile.base_url = format!("http://127.0.0.1:{port}/selected-{id}");
        profile.connection.hidden = true;
        profile.connection.connection_id = Some(id.into());
        profile.connection.order = priority;
        profile.credential = crate::CredentialConfig::Static {
            id: format!("acct.{id}"),
        };
        providers.push(profile);
        accounts.push(AccountMetadata {
            account_id: id.into(),
            provider_id: "read-count-fixture".into(),
            display_name: id.into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: binding.profile_name.clone(),
            credential_ref: CredentialRef {
                store_scope: "read-count-test".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: priority,
            config_revision: 1,
            identity_generation: 7,
        });
    }
    let store = Arc::new(CountAccountStore::default());
    let pool = Arc::new(crate::accounts::AccountPool::new(accounts, store.clone()));
    let credentials = Arc::new(NamedAccountCredentials(Mutex::new(Vec::new()), store));
    let runtime = ModelRuntime::from_config(crate::ClientConfig { providers })
        .unwrap()
        .with_account_pool(pool.clone())
        .with_credential_provider(credentials.clone());
    let transport = Arc::new(CaptureTransport::default());
    let service = ApiService::new(
        Arc::new(runtime),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    );
    (service, transport, pool, credentials)
}

#[tokio::test]
async fn read_token_count_named_accounts_match_generation_endpoint_and_preserve_source_bytes() {
    let (service, transport, _, credentials) =
        named_account_service(crate::ProtocolFamily::AnthropicMessages);
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../scripts/tests/fixtures/file_read_count_322.json"
    ))
    .unwrap();
    let source = Utf16JsonProjection::parse(r#""count \ud800 exactly""#).unwrap();
    for (profile, account) in [
        (None, "a"),
        (Some("read-count-fixture"), "a"),
        (Some("account.b"), "b"),
    ] {
        assert_eq!(
            service
                .count_read_text_tokens_exact("read-count-model", profile, &source)
                .await
                .unwrap(),
            Some(2500)
        );
        let request = transport.0.lock().unwrap().last().unwrap().clone();
        let scope = crate::accounts::AccountResourceScope {
            store_scope: "read-count-test".into(),
            account_id: account.into(),
            identity_generation: 7,
        };
        let mut generation = LlmRequest::new("read-count-model").with_user_text("generation");
        generation.profile = Some(format!("account.{account}"));
        generation.input.max_tokens = Some(4096);
        generation.execution.selected_account = Some(scope);
        let generation = service.client().prepare(&generation).await.unwrap();
        let mut expected_url = url::Url::parse(&generation.provider_request.url).unwrap();
        expected_url.set_path(&format!(
            "{}/count_tokens",
            expected_url.path().trim_end_matches('/')
        ));
        expected_url.set_query(Some("beta=true"));
        assert_eq!(request.url, expected_url.as_str());
        assert_eq!(
            request.headers.get("x-api-key"),
            generation.provider_request.headers.get("x-api-key")
        );
        assert_eq!(
            request.headers.get("x-api-key").map(String::as_str),
            Some(format!("fixture-account.{account}-generation-7").as_str())
        );
        let expected = fixture["requestJsonTemplate"]
            .as_str()
            .unwrap()
            .replace("__SOURCE_JSON__", &source.to_json_string().unwrap());
        assert_eq!(request.wire_body_bytes().unwrap(), expected.as_bytes());
    }
    assert_eq!(transport.0.lock().unwrap().len(), 3);
    assert_eq!(credentials.0.lock().unwrap().len(), 6);
    let health = credentials.1 .0.lock().unwrap();
    assert_eq!(health.model_entitlements.len(), 2);
    assert!(health
        .model_entitlements
        .values()
        .all(|entry| entry.model_id == "tokens/count/claude-sonnet-4-6"));
}

#[tokio::test]
async fn read_token_count_model_billing_override_keeps_generation_eligible_account() {
    use lingxi_core::host::ModelBillingMode::{PerToken, Subscription};

    let (base, transport, pool, credentials) =
        named_account_service(crate::ProtocolFamily::AnthropicMessages);
    let mut config = base.client().configuration();
    for profile in &mut config.providers {
        profile.pricing.billing_mode = Some(if profile.profile_name == "account.b" {
            PerToken
        } else {
            Subscription
        });
        let mut source = crate::upstream::profile(profile).unwrap();
        for model in &mut source.models {
            model.billing_mode = Some(lingxi_llm_client::protocol::BillingMode::PerToken);
        }
        profile.wire_profile = Some(source);
    }
    let mut metered = config
        .providers
        .iter()
        .find(|profile| profile.profile_name == "account.b")
        .unwrap()
        .clone();
    metered.profile_name = "read-count-metered".into();
    metered.connection.hidden = false;
    metered.connection.connection_id = None;
    config.providers.push(metered);
    let mut accounts = pool.snapshot().accounts;
    accounts
        .iter_mut()
        .find(|account| account.account_id == "a")
        .unwrap()
        .enabled = false;
    accounts
        .iter_mut()
        .find(|account| account.account_id == "b")
        .unwrap()
        .execution_binding_id = "read-count-metered".into();
    let pool = Arc::new(crate::accounts::AccountPool::new(
        accounts,
        credentials.1.clone(),
    ));
    assert!(
        pool.snapshot().provider_policies.is_empty(),
        "no metered fallback override"
    );
    let runtime = ModelRuntime::from_config(config)
        .unwrap()
        .with_account_pool(pool)
        .with_credential_provider(credentials.clone());
    let authorized = runtime
        .account_coordinator("read-count-model", Some("read-count-fixture"))
        .unwrap()
        .unwrap();
    assert!(authorized
        .candidate_profiles()
        .contains(&"account.b".into()));
    let service = ApiService::new(
        Arc::new(runtime),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    );
    assert_eq!(
        service
            .count_read_text_tokens_exact(
                "read-count-model",
                Some("read-count-fixture"),
                &Utf16JsonProjection::from("source"),
            )
            .await
            .unwrap(),
        Some(2500)
    );
    let requests = transport.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .url
        .starts_with("http://127.0.0.1:39448/selected-b/"));
    assert_eq!(
        requests[0].headers.get("x-api-key").map(String::as_str),
        Some("fixture-account.b-generation-7")
    );
    assert_eq!(credentials.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn read_token_count_rejection_retains_selected_account_generation_and_credential_revision() {
    let (service, transport, _, credentials) =
        named_account_service(crate::ProtocolFamily::AnthropicMessages);
    transport.1.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = service
        .count_read_text_tokens_exact(
            "read-count-model",
            Some("account.b"),
            &Utf16JsonProjection::from("source"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.classification_source(),
        LlmError::Authentication { .. }
    ));
    assert_eq!(transport.0.lock().unwrap().len(), 1, "no account replay");
    assert_eq!(credentials.0.lock().unwrap().len(), 1);
    let health = credentials.1 .0.lock().unwrap();
    assert_eq!(health.records.len(), 1);
    let (key, record) = health.records.iter().next().unwrap();
    let (account, generation, scope): (String, u64, crate::accounts::FailureScope) =
        serde_json::from_str(key).unwrap();
    assert_eq!(
        account,
        serde_json::to_string(&("read-count-test", "b")).unwrap()
    );
    assert_eq!(generation, 7);
    assert_eq!(scope, crate::accounts::FailureScope::Account);
    assert_eq!(record.status, crate::accounts::HealthStatus::NeedsAuth);
    assert_eq!(record.credential_revision, Some(11));
}

#[tokio::test]
async fn read_token_count_pinned_named_account_never_uses_a_sibling_after_revocation() {
    for replace_identity in [false, true] {
        let (service, transport, pool, credentials) =
            named_account_service(crate::ProtocolFamily::AnthropicMessages);
        let mut snapshot = pool.snapshot();
        snapshot.config_revision += 1;
        let account = snapshot
            .accounts
            .iter_mut()
            .find(|account| account.account_id == "b")
            .unwrap();
        if replace_identity {
            account.identity_generation += 1;
        } else {
            account.enabled = false;
        }
        pool.refresh(snapshot).unwrap();
        assert!(service
            .count_read_text_tokens_exact(
                "read-count-model",
                Some("account.b"),
                &Utf16JsonProjection::from("source"),
            )
            .await
            .is_err());
        assert!(transport.0.lock().unwrap().is_empty());
        assert!(credentials.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn read_token_count_unsupported_named_account_route_never_loads_credentials() {
    let (service, transport, _, credentials) =
        named_account_service(crate::ProtocolFamily::OpenAiChat);
    assert_eq!(
        service
            .count_read_text_tokens_exact(
                "read-count-model",
                Some("read-count-fixture"),
                &Utf16JsonProjection::from("source"),
            )
            .await
            .unwrap(),
        None
    );
    assert!(transport.0.lock().unwrap().is_empty());
    assert!(credentials.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn read_token_count_matches_native_wire_and_retains_exact_utf16_without_generation_policy() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../scripts/tests/fixtures/file_read_count_322.json"
    ))
    .unwrap();
    let recipe = &fixture["sourceRecipe"];
    let prefix = recipe["prefix"].as_str().unwrap();
    let suffix = recipe["suffix"].as_str().unwrap();
    let text = format!(
        "{prefix}{}{suffix}",
        recipe["fill"]
            .as_str()
            .unwrap()
            .repeat(recipe["length"].as_u64().unwrap() as usize - prefix.len() - suffix.len())
    );
    for thinking in [
        crate::model::thinking::ThinkingConfig::Disabled,
        crate::model::thinking::ThinkingConfig::Enabled {
            budget_tokens: 4096,
        },
    ] {
        let (service, transport) = service(crate::ProtocolFamily::AnthropicMessages);
        let service = service.with_thinking(thinking);
        for source in [
            Utf16JsonProjection::from(text.as_str()),
            Utf16JsonProjection::parse(r#""a\ud800b\udfff""#).unwrap(),
        ] {
            let result = service
                .count_read_text_tokens_exact(
                    "read-count-model",
                    Some("read-count-fixture"),
                    &source,
                )
                .await
                .unwrap();
            assert_eq!(result, Some(fixture["stubCount"].as_u64().unwrap()));
            let requests = transport.0.lock().unwrap();
            let request = requests.last().unwrap();
            let expected = fixture["requestJsonTemplate"]
                .as_str()
                .unwrap()
                .replace("__SOURCE_JSON__", &source.to_json_string().unwrap());
            assert_eq!(request.wire_body_bytes().unwrap(), expected.as_bytes());
            assert!(request.url.ends_with("/v1/messages/count_tokens?beta=true"));
            assert_eq!(
                request.body_json.string_at("/model").as_deref(),
                Some("claude-sonnet-4-6")
            );
        }
    }
}

#[tokio::test]
async fn read_token_count_unsupported_routes_and_empty_text_do_not_dispatch() {
    let (service, transport) = service(crate::ProtocolFamily::OpenAiChat);
    assert_eq!(
        service
            .count_read_text_tokens_exact(
                "read-count-model",
                Some("read-count-fixture"),
                &Utf16JsonProjection::from("text")
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        service
            .count_read_text_tokens_exact(
                "read-count-model",
                Some("read-count-fixture"),
                &Utf16JsonProjection::from("")
            )
            .await
            .unwrap(),
        Some(0)
    );
    assert!(service
        .count_read_text_tokens_exact(
            "read-count-model",
            Some("read-count-fixture"),
            &Utf16JsonProjection::parse("{}").unwrap()
        )
        .await
        .is_err());
    assert!(transport.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn read_token_count_generation_history_normalizes_only_final_claude_wire_values() {
    use lingxi_llm_client::protocol::{
        ContentBlock, ConversationMessage, JsonPayload, MessageRole, ToolUseId,
    };

    fn exact_units(payload: &JsonPayload, pointer: &str) -> Option<Vec<u16>> {
        let raw = payload.raw_source_at(pointer)?;
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&raw)
            .ok()?
            .string_units("")
    }

    for protocol in [
        crate::ProtocolFamily::AnthropicMessages,
        crate::ProtocolFamily::OpenAiChat,
    ] {
        let (service, transport) = service(protocol);
        let client = service.client();
        let tool_input = JsonPayload::parse(
            r#"{"old_string":"a\ud800","new_string":"b\ud801","nested":{"\ud805":"v\ud803","pair":"\ud83d\ude03"}}"#,
        )
        .unwrap();
        let result = JsonPayload::string_from_utf16_code_units(&[b'r' as u16, 0xd802]);
        let mut request = LlmRequest::new("read-count-model").with_user_text("start");
        request.profile = Some("read-count-fixture".into());
        request.input.max_tokens = Some(4096);
        request.execution.input_protocol = Some(protocol);
        request.input.messages.extend([
            ConversationMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: ToolUseId::new("toolu_utf16_history"),
                    name: "Edit".into(),
                    input: tool_input.clone(),
                    provider_id: None,
                    caller: None,
                    toolset_name: None,
                    thought_signature: None,
                }],
                native_options: Vec::new(),
            },
            ConversationMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: ToolUseId::new("toolu_utf16_history"),
                    content: result.clone(),
                    is_error: Some(false),
                    blocks: None,
                    cache_reference: None,
                    toolset_name: None,
                }],
                native_options: Vec::new(),
            },
        ]);
        let source_request = request.clone();
        let mut prepared = client.prepare(&request).await.unwrap();
        let draft =
            String::from_utf8(prepared.provider_request.wire_body_bytes().unwrap()).unwrap();
        assert!(draft.contains(r#"\ud800"#));
        assert!(draft.contains(r#"\ud802"#));
        client.seal_prepared(&mut prepared).await.unwrap();
        let wire =
            std::str::from_utf8(&prepared.wire_call.as_ref().unwrap().request().body).unwrap();
        if protocol == crate::ProtocolFamily::AnthropicMessages {
            let body = JsonPayload::parse(wire).unwrap();
            assert_eq!(
                exact_units(&body, "/messages/1/content/0/input/old_string"),
                Some(vec![b'a' as u16, 0xfffd])
            );
            assert_eq!(
                exact_units(&body, "/messages/1/content/0/input/new_string"),
                Some(vec![b'b' as u16, 0xfffd])
            );
            assert_eq!(
                exact_units(&body, "/messages/2/content/0/content"),
                Some(vec![b'r' as u16, 0xfffd])
            );
            assert!(
                wire.contains(r#""\ud805":"v�""#),
                "keys remain source-exact"
            );
            assert_eq!(
                exact_units(&body, "/messages/1/content/0/input/nested/pair"),
                Some(vec![0xd83d, 0xde03])
            );
        } else {
            assert!(wire.contains(r#"\ud800"#), "other provider source contract");
            assert!(wire.contains(r#"\ud802"#), "other provider result contract");
            assert!(!wire.contains('�'));
        }
        assert_eq!(
            request, source_request,
            "sealing cannot rewrite authoritative history"
        );
        assert_eq!(
            exact_units(&tool_input, "/old_string"),
            Some(vec![b'a' as u16, 0xd800])
        );
        assert_eq!(
            result.root_string_code_units(),
            Some(vec![b'r' as u16, 0xd802])
        );
        assert!(
            transport.0.lock().unwrap().is_empty(),
            "sealing does not dispatch"
        );
    }
}
