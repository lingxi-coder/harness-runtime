//! A failing connection must hand the SAME model to the next connection.
//!
//! Before this existed, `DriveStep::Fallback` had exactly one production site —
//! `model/retry.rs`, inside the 529 arm — so a 429 never advanced anything; and
//! the streaming drive passed `None` for fallback outright, which is the path
//! desktop and mobile actually run. Both are covered here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    ApiService, AuthStrategy, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig,
    FailoverTriggers, LlmError, ModelProfile, ModelRuntime, PricingConfig, ProtocolFamily,
    ProviderId, ProviderProfile, SubscriberState,
};
use llm_runtime::{BoxFuture, ProviderRequest, ProviderResponse, StreamingResponse};

const INTL: &str = "https://intl.example.com/v1";
const CN: &str = "https://cn.example.com/v1";

/// Records every request URL, and answers with a scripted status sequence.
struct ScriptedTransport {
    statuses: Vec<u16>,
    urls: Mutex<Vec<String>>,
}

impl ScriptedTransport {
    fn new(statuses: Vec<u16>) -> Arc<Self> {
        Arc::new(Self {
            statuses,
            urls: Mutex::new(Vec::new()),
        })
    }

    /// Which connection each attempt went to, in order.
    fn connections(&self) -> Vec<String> {
        self.urls
            .lock()
            .unwrap()
            .iter()
            .map(|u| {
                if u.starts_with(INTL) {
                    "intl".to_string()
                } else if u.starts_with(CN) {
                    "cn".to_string()
                } else {
                    format!("unknown({u})")
                }
            })
            .collect()
    }

    fn next_status(&self, url: &str) -> u16 {
        let mut urls = self.urls.lock().unwrap();
        urls.push(url.to_string());
        let idx = (urls.len() - 1).min(self.statuses.len() - 1);
        self.statuses[idx]
    }
}

fn ok_body() -> serde_json::Value {
    serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "shared-model",
        "content": [{ "type": "text", "text": "hi" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
}

fn messages_request(model: &str, profile: Option<&str>) -> llm_runtime::MessagesCreateRequest {
    let mut request =
        llm_runtime::MessagesCreateRequest::new(model, profile, None, Vec::new(), Vec::new());
    request.opts.max_output_tokens = Some(4096);
    request
}

impl llm_runtime::test_support::FixtureTransport for ScriptedTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let status = self.next_status(&request.url);
        Box::pin(async move {
            Ok(ProviderResponse {
                status,
                headers: BTreeMap::new(),
                body_json: lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                    &if status == 200 && request.url.ends_with("/chat/completions") {
                        assert!(request
                            .body_json
                            .as_json_source()
                            .find("cache_control")
                            .is_none());
                        serde_json::json!({"id":"chat_1","model":"shared-model","choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
                    } else if status == 200 {
                        ok_body()
                    } else {
                        serde_json::json!({ "error": { "message": "boom" } })
                    },
                )
                .expect("fixture response is valid JSON"),
                request_id: None,
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        // Fail the CONNECT phase with a status, exactly like a real 429 on open.
        let status = self.next_status(&request.url);
        Box::pin(async move {
            if status == 200 {
                // Terminal and NOT a failover trigger, so the drive stops the
                // moment the good connection is reached: the recorded URL list
                // is then exactly the hops taken, with no retry noise.
                return Err(LlmError::InvalidRequest {
                    message: "reached-the-good-connection".to_string(),
                });
            }
            Ok(StreamingResponse {
                status,
                headers: BTreeMap::new(),
                frames: Box::new(TailFrames([serde_json::json!({"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}})].into())),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(ScriptedTransport);

fn connection(conn_id: &str, base_url: &str, order: u32) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: lingxi_llm_client::protocol::Region::all(),
        provider_id: ProviderId::OpenAICompatible {
            name: "grouped".to_string(),
        },
        profile_name: format!("grouped:{conn_id}"),
        base_url: base_url.to_string(),
        protocol: ProtocolFamily::AnthropicMessages,
        auth: AuthStrategy::None,
        credential: CredentialConfig::None,
        models: vec![ModelProfile {
            display_model: "shared-model".to_string(),
            request_model: "shared-model".to_string(),
            billing_model: "shared-model".to_string(),
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                ..Default::default()
            },
        }],
        pricing: PricingConfig::default(),
        signing: None,
        azure: None,
        supports_websockets: false,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: None,
        vision_delegate: None,
        connection: ConnectionSpec {
            group: Some("grouped".to_string()),
            connection_id: Some(conn_id.to_string()),
            order,
            hidden: false,
            failover: FailoverTriggers::DEFAULT,
        },
    }
}

fn service(transport: Arc<ScriptedTransport>) -> ApiService {
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![connection("intl", INTL, 0), connection("cn", CN, 1)],
        })
        .expect("client"),
    );
    ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    )
}

/// Non-streaming: 429 on the first connection, 200 on the second.
#[tokio::test]
async fn a_rate_limited_connection_hands_the_request_to_the_next_one() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let api = service(transport.clone());

    let response = api
        .messages_create(messages_request("shared-model", None))
        .await
        .expect("the second connection must serve the request");

    assert_eq!(
        transport.connections(),
        vec!["intl".to_string(), "cn".to_string()],
        "the 429 must move to the next CONNECTION, not burn the retry ladder on the first"
    );
    assert_eq!(response.model, "shared-model", "the model is unchanged");
}

/// The shape a real session sends: the picker hands back a CONNECTION profile,
/// so `req.profile` is `grouped:intl`, not `None`.
///
/// Every other case here resolves unscoped, which is why the chain being empty
/// under a connection scope went unnoticed — the feature was dead on the only
/// path that matters.
#[tokio::test]
async fn a_request_scoped_to_a_connection_still_fails_over() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let api = service(transport.clone());

    let _ = api
        .messages_create(messages_request("shared-model", Some("grouped:intl")))
        .await;

    assert_eq!(
        transport.connections(),
        vec!["intl".to_string(), "cn".to_string()],
        "a session pinned to one connection must still reach its sibling"
    );
}

/// Streaming is the path desktop and mobile actually drive, and it had no
/// fallback of any kind. A 429 on connect must reach the second connection.
#[tokio::test]
async fn the_streaming_connect_phase_also_fails_over() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let api = service(transport.clone());

    let _ = api
        .stream_with_opts(
            "shared-model",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some(4096),
            None,
        )
        .await;

    assert_eq!(
        transport.connections(),
        vec!["intl".to_string(), "cn".to_string()],
        "the stream connect phase must fail over too"
    );
}

/// A provider that never opted in must behave exactly as before: one connection,
/// no chain, and the retry ladder reached untouched.
#[tokio::test]
async fn a_single_connection_provider_does_not_fail_over() {
    let transport = ScriptedTransport::new(vec![429, 200]);
    let mut only = connection("intl", INTL, 0);
    only.profile_name = "solo".to_string();
    only.connection = ConnectionSpec::default();
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![only],
        })
        .expect("client"),
    );
    let api = ApiService::new(
        client,
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "0.0.0",
        None,
        None,
    );

    let _ = api
        .messages_create(messages_request("shared-model", None))
        .await;

    let hops = transport.connections();
    assert!(
        hops.iter().all(|c| c == "intl"),
        "with no connections configured nothing may be re-pointed; got {hops:?}"
    );
}

/// A sibling connection can expose the same model through a different wire.
#[tokio::test]
async fn cross_protocol_failover_adapts_cached_history_for_both_drivers() {
    for streaming in [false, true] {
        let transport = ScriptedTransport::new(vec![429, 200]);
        let first = connection("intl", INTL, 0);
        let mut second = connection("cn", CN, 1);
        second.protocol = ProtocolFamily::OpenAiChat;
        let client = Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![first, second],
            })
            .unwrap(),
        );
        let api = ApiService::new(
            client,
            transport.clone(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        );
        let mut request = llm_runtime::LlmRequest::new("shared-model").with_user_text("hello");
        request.input.max_tokens = Some(4096);
        request
            .input
            .prompt_cache
            .breakpoints
            .push(lingxi_llm_client::protocol::CacheBreakpoint {
                position: lingxi_llm_client::protocol::CachePosition::Message {
                    index: 0,
                    block: 0,
                },
                scope: None,
                ttl: lingxi_llm_client::protocol::CacheTtl::FiveMinutes,
            });
        if streaming {
            let error = api
                .stream_request(request)
                .await
                .err()
                .expect("second connection sentinel");
            assert!(
                matches!(error, LlmError::InvalidRequest {message} if message == "reached-the-good-connection")
            );
        } else {
            let response = api
                .execute_side_query_request(request)
                .await
                .expect("adapted fallback request");
            assert_eq!(response.model, "shared-model");
        }
        assert_eq!(transport.connections(), vec!["intl", "cn"]);
    }
}

#[tokio::test(start_paused = true)]
async fn cross_protocol_model_fallback_rebuilds_history_cache_policy() {
    let transport = ScriptedTransport::new(vec![529, 529, 529, 200]);
    let mut primary = connection("intl", INTL, 0);
    primary.connection = Default::default();
    primary.provider_id = ProviderId::Custom {
        name: "primary".into(),
    };
    primary.models[0].display_model = "claude-opus-4-6".into();
    primary.models[0].request_model = "claude-opus-4-6".into();
    let mut fallback = connection("cn", CN, 1);
    fallback.connection = Default::default();
    fallback.provider_id = ProviderId::Custom {
        name: "secondary".into(),
    };
    fallback.protocol = ProtocolFamily::OpenAiChat;
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![primary, fallback],
        })
        .unwrap(),
    );
    let api = ApiService::new(
        client,
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    api.messages_create({
        let mut request = messages_request("claude-opus-4-6", None);
        request.system = Some(llm_runtime::SystemPromptInput::source_vector(
            vec!["cached system".into()],
            None,
            None,
        ));
        request.opts.fallback = match Some("shared-model") {
            Some(models) => llm_runtime::FallbackPolicy::from_models_csv(models),
            None => llm_runtime::FallbackPolicy::Configured,
        };
        request
    })
    .await
    .expect("cross-protocol model fallback must succeed");
    assert_eq!(transport.connections(), vec!["intl", "intl", "intl", "cn"]);
}

// These cases exercise the real ApiService preparation, single dispatch,
// response classification and next-call selection paths, not just a pool model.
#[derive(Debug, Default)]
struct AccountTestClock(std::sync::atomic::AtomicU64);
impl llm_runtime::accounts::AccountClock for AccountTestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
fn account_service(
    transport: Arc<dyn llm_runtime::Transport>,
    clock: Arc<AccountTestClock>,
) -> ApiService {
    account_service_with_billing(transport, clock, None)
}
fn account_service_with_billing(
    transport: Arc<dyn llm_runtime::Transport>,
    clock: Arc<AccountTestClock>,
    policy: Option<bool>,
) -> ApiService {
    ApiService::new(
        Arc::new(account_runtime(clock, policy).0),
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
}
fn account_runtime(
    clock: Arc<AccountTestClock>,
    policy: Option<bool>,
) -> (ModelRuntime, Arc<llm_runtime::accounts::AccountPool>) {
    use lingxi_core::host::provider_accounts::{AccountAuthKind, AccountMetadata, CredentialRef};
    let accounts: Vec<_> = ["a", "b"]
        .into_iter()
        .enumerate()
        .map(|(priority, id)| AccountMetadata {
            account_id: id.into(),
            provider_id: "grouped".into(),
            display_name: id.into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "grouped".into(),
            credential_ref: CredentialRef {
                store_scope: "test".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: priority as u32,
            config_revision: 1,
            identity_generation: 1,
        })
        .collect();
    let mut a = connection("a", INTL, 0);
    a.profile_name = "account.a".into();
    let mut b = connection("b", CN, 1);
    b.profile_name = "account.b".into();
    for profile in [&mut a, &mut b] {
        let mut model = profile.models[0].clone();
        model.display_model = "second-model".into();
        model.request_model = "second-model".into();
        model.billing_model = "second-model".into();
        profile.models.push(model);
    }
    let pool = Arc::new(
        llm_runtime::accounts::AccountPool::new(
            accounts,
            Arc::new(llm_runtime::accounts::MemoryAccountPoolStore::default()),
        )
        .with_clock(clock),
    );
    if let Some(allow) = policy {
        a.pricing.billing_mode = Some(lingxi_core::host::ModelBillingMode::Subscription);
        b.pricing.billing_mode = Some(lingxi_core::host::ModelBillingMode::PerToken);
        let mut snapshot = pool.snapshot();
        snapshot.config_revision += 1;
        snapshot.provider_policies.insert(
            "grouped".into(),
            lingxi_core::host::provider_accounts::ProviderAccountPolicy {
                allow_subscription_to_metered: allow,
            },
        );
        pool.refresh(snapshot).unwrap();
    }
    let runtime = ModelRuntime::from_config(ClientConfig {
        providers: vec![a, b],
    })
    .unwrap()
    .with_account_pool(pool.clone());
    (runtime, pool)
}

#[test]
fn shared_model_availability_reuses_account_billing_and_expiring_entitlements() {
    use lingxi_core::host::provider_accounts::{
        AccountModelEntitlement, AccountModelEntitlementStatus,
    };
    let (runtime, pool) = account_runtime(Arc::new(AccountTestClock::default()), Some(true));
    let mut config = runtime.configuration();
    // A visible logical binding is separate from its hidden execution accounts.
    for profile in &mut config.providers {
        profile.connection.hidden = true;
    }
    let mut logical = config.providers[0].clone();
    logical.profile_name = "grouped".into();
    logical.connection.hidden = false;
    config.providers.push(logical);
    let mut snapshot = pool.snapshot();
    snapshot.model_entitlements = vec![
        AccountModelEntitlement {
            account_id: "a".into(),
            identity_generation: 1,
            model_id: "shared-model".into(),
            status: AccountModelEntitlementStatus::Forbidden,
            observed_at_ms: 0,
            retry_at_ms: Some(300_000),
            reason: Some("model_permission_denied".into()),
        },
        AccountModelEntitlement {
            account_id: "b".into(),
            identity_generation: 1,
            model_id: "shared-model".into(),
            status: AccountModelEntitlementStatus::Confirmed,
            observed_at_ms: 1,
            retry_at_ms: None,
            reason: None,
        },
    ];
    let rows = ModelRuntime::project_account_model_availability(&config, &snapshot, 2).unwrap();
    let model = rows
        .iter()
        .find(|row| row.model_id == "shared-model")
        .unwrap();
    assert_eq!(model.forbidden_account_ids, ["a"]);
    assert_eq!(model.eligible_account_ids, ["b"]);
    assert_eq!(model.confirmed_account_ids, ["b"]);
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 300_001).unwrap();
    let model = rows
        .iter()
        .find(|row| row.model_id == "shared-model")
        .unwrap();
    assert_eq!(model.unverified_account_ids, ["a"]);
    snapshot
        .provider_policies
        .get_mut("grouped")
        .unwrap()
        .allow_subscription_to_metered = false;
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 300_001).unwrap();
    assert_eq!(
        rows.iter()
            .find(|row| row.model_id == "shared-model")
            .unwrap()
            .eligible_account_ids,
        ["a"]
    );
}

#[tokio::test]
async fn grouped_model_uses_credentialed_sibling_when_head_connection_has_no_account() {
    let transport = ScriptedTransport::new(vec![200]);
    let (runtime, pool) = account_runtime(Arc::new(AccountTestClock::default()), None);
    let mut snapshot = pool.snapshot();
    snapshot.config_revision += 1;
    snapshot
        .accounts
        .retain(|account| account.account_id == "b");
    snapshot.accounts[0].execution_binding_id = "grouped:tail".into();
    snapshot.accounts[0].config_revision += 1;
    pool.refresh(snapshot).unwrap();
    let mut config = ClientConfig {
        providers: vec![connection("head", INTL, 0), connection("tail", CN, 1)],
    };
    config.providers.extend(runtime.configuration().providers);
    let runtime = ModelRuntime::from_config(config)
        .unwrap()
        .with_account_pool(pool);
    let service = ApiService::new(
        Arc::new(runtime),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    service
        .messages_create(messages_request("shared-model", Some("grouped")))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["cn"]);
}

#[test]
fn shared_model_availability_honors_model_and_endpoint_cooldowns_without_forbidding_entitlement() {
    use lingxi_core::host::provider_accounts::{AccountHealthScopeKind, AccountScopedHealth};
    let (runtime, pool) = account_runtime(Arc::new(AccountTestClock::default()), None);
    let mut config = runtime.configuration();
    for profile in &mut config.providers {
        profile.connection.hidden = true;
    }
    let mut logical = config.providers[0].clone();
    logical.profile_name = "grouped".into();
    logical.connection.hidden = false;
    config.providers.push(logical);
    let mut snapshot = pool.snapshot();
    snapshot.scoped_health.push(AccountScopedHealth {
        account_id: "a".into(),
        identity_generation: 1,
        scope_kind: AccountHealthScopeKind::Model,
        scope_id: "shared-model".into(),
        unavailable_until_ms: Some(60_000),
        reason: "rate_limited".into(),
    });
    let rows = ModelRuntime::project_account_model_availability(&config, &snapshot, 1).unwrap();
    assert_eq!(
        rows.iter()
            .find(|row| row.model_id == "shared-model")
            .unwrap()
            .eligible_account_ids,
        ["b"]
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.model_id == "second-model")
            .unwrap()
            .eligible_account_ids,
        ["a", "b"]
    );
    assert!(
        rows.iter().all(|row| row.forbidden_account_ids.is_empty()),
        "rate limits are readiness, not authorization denial"
    );
    snapshot.scoped_health.push(AccountScopedHealth {
        account_id: "a".into(),
        identity_generation: 1,
        scope_kind: AccountHealthScopeKind::Endpoint,
        scope_id: lingxi_llm_client::files::provider_file_endpoint_fingerprint(INTL),
        unavailable_until_ms: Some(90_000),
        reason: "endpoint_unavailable".into(),
    });
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 60_001).unwrap();
    assert!(
        rows.iter().all(|row| row.eligible_account_ids == ["b"]),
        "endpoint cooldown spans models on only its account and endpoint"
    );
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 90_001).unwrap();
    assert!(rows
        .iter()
        .all(|row| row.eligible_account_ids == ["a", "b"]));
    snapshot.scoped_health[1].identity_generation = 0;
    snapshot.scoped_health[1].unavailable_until_ms = None;
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 90_001).unwrap();
    assert!(
        rows.iter()
            .all(|row| row.eligible_account_ids == ["a", "b"]),
        "old identity faults cannot affect the current account"
    );
    snapshot
        .health
        .push(lingxi_core::host::provider_accounts::AccountHealth {
            account_id: "a".into(),
            identity_generation: 1,
            status: lingxi_core::host::provider_accounts::AccountHealthStatus::QuotaExhausted,
            reason: Some("quota_exhausted".into()),
            cooldown_until_ms: Some(120_000),
            health_revision: 1,
        });
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 90_001).unwrap();
    assert!(rows.iter().all(|row| row.eligible_account_ids == ["b"]));
    let rows =
        ModelRuntime::project_account_model_availability(&config, &snapshot, 120_001).unwrap();
    assert!(
        rows.iter()
            .all(|row| row.eligible_account_ids == ["a", "b"]),
        "quota deadline permits one real business recovery probe"
    );
}
#[tokio::test]
async fn account_cooldown_skips_network_then_restores_original_priority() {
    let transport = ScriptedTransport::new(vec![429, 200, 200, 200]);
    let clock = Arc::new(AccountTestClock::default());
    let service = account_service(transport.clone(), clock.clone());
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "cn", "cn"]);
    clock.0.store(30_001, std::sync::atomic::Ordering::SeqCst);
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "cn", "cn", "intl"]);
}
#[tokio::test]
async fn exhausted_account_pool_is_aggregated_and_next_call_sends_zero_requests() {
    let transport = ScriptedTransport::new(vec![429, 429]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    for _ in 0..2 {
        let error = service
            .messages_create(messages_request("shared-model", None))
            .await
            .unwrap_err();
        match error {
            LlmError::AllAccountsUnavailable {
                accounts,
                earliest_recovery_ms,
            } => {
                assert_eq!(accounts.len(), 2);
                assert!(
                    earliest_recovery_ms.is_some()
                        || accounts.iter().all(|a| a.reason == "rate_limited")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(transport.connections(), vec!["intl", "cn"]);
}
#[tokio::test]
async fn invalid_request_does_not_rotate_or_poison_accounts() {
    let transport = ScriptedTransport::new(vec![400, 200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    let error = service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error.classification_source(),
            LlmError::InvalidRequest { .. }
        ),
        "{error:?}"
    );
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "intl"]);
}
#[tokio::test]
async fn account_authentication_failure_is_skipped_on_following_calls() {
    let transport = ScriptedTransport::new(vec![401, 200, 200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "cn", "cn"]);
}

#[tokio::test]
async fn metered_fallback_requires_explicit_subscription_policy() {
    for allow in [false, true] {
        let transport = ScriptedTransport::new(vec![429, 200]);
        let service = account_service_with_billing(
            transport.clone(),
            Arc::new(AccountTestClock::default()),
            Some(allow),
        );
        let result = service
            .messages_create(messages_request("shared-model", Some("account.a")))
            .await;
        if allow {
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(transport.connections(), vec!["intl", "cn"]);
        } else {
            assert!(matches!(
                result,
                Err(LlmError::AllAccountsUnavailable { .. })
            ));
            assert_eq!(transport.connections(), vec!["intl"]);
        }
    }
}
#[tokio::test]
async fn pooled_stream_connect_uses_same_health_and_skips_cooling_account() {
    let transport = ScriptedTransport::new(vec![429, 200, 200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    for _ in 0..2 {
        let result = service
            .stream_with_opts(
                "shared-model",
                None,
                None,
                Vec::new(),
                Vec::new(),
                None,
                Some(4096),
                None,
            )
            .await;
        let error = result.err().expect("good connection sentinel");
        assert!(
            matches!(&error, LlmError::InvalidRequest { message } if message == "reached-the-good-connection"),
            "{error:?}"
        );
    }
    assert_eq!(transport.connections(), vec!["intl", "cn", "cn"]);
}

#[derive(Default)]
struct RecordingAccountReporter(
    Mutex<Vec<lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent>>,
);
#[async_trait::async_trait]
impl llm_runtime::accounts::AccountRoutingReporter for RecordingAccountReporter {
    async fn report(
        &self,
        event: lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent,
    ) {
        self.0.lock().unwrap().push(event);
    }
}
#[tokio::test]
async fn structured_routing_reports_actual_selection_and_aggregate_in_order() {
    let transport = ScriptedTransport::new(vec![429, 429]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    let reporter = Arc::new(RecordingAccountReporter::default());
    service.set_account_routing_reporter(reporter.clone());
    let error = service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::AllAccountsUnavailable { .. }));
    let events = reporter.0.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].selected_account_id.as_deref(), Some("a"));
    assert_eq!(events[1].selected_account_id.as_deref(), Some("b"));
    assert_eq!(events[1].failed_accounts[0].reason_code, "rate_limited");
    assert_eq!(events[2].status, "all_unavailable");
    assert_eq!(events[2].failed_accounts.len(), 2);
    assert_eq!(events[2].earliest_recovery_ms, Some(30_000));
    assert!(events
        .iter()
        .all(|event| event.logical_call_id == events[0].logical_call_id));
    let json = serde_json::to_string(&*events).unwrap();
    assert!(!json.contains("access_token"));
    assert!(!json.contains("api_key"));
}

#[tokio::test]
async fn resource_owner_stays_on_b_when_higher_priority_a_recovers() {
    let transport = ScriptedTransport::new(vec![429, 200, 200, 200]);
    let clock = Arc::new(AccountTestClock::default());
    let service = account_service(transport.clone(), clock.clone());
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    clock.0.store(30_001, std::sync::atomic::Ordering::SeqCst);
    let mut request = llm_runtime::LlmRequest::new("shared-model");
    request.profile = Some("account.b".into());
    request.input.max_tokens = Some(4096);
    request.execution.file_account_scope = Some(
        serde_json::to_string(&llm_runtime::accounts::AccountResourceScope {
            store_scope: "test".into(),
            account_id: "b".into(),
            identity_generation: 1,
        })
        .unwrap(),
    );
    service.execute_side_query_request(request).await.unwrap();
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "cn", "cn", "intl"]);
}
#[tokio::test]
async fn explicit_account_b_is_selected_before_global_priority_a() {
    let transport = ScriptedTransport::new(vec![200, 200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    service
        .messages_create(messages_request("shared-model", Some("account.b")))
        .await
        .unwrap();
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["cn", "intl"]);
}
#[tokio::test]
async fn managed_provider_with_no_remaining_accounts_never_uses_base_route() {
    use lingxi_core::host::provider_accounts::AccountSnapshot;
    let transport = ScriptedTransport::new(vec![200]);
    let policies = BTreeMap::new();
    let directory = Arc::new(std::sync::RwLock::new(AccountSnapshot {
        store_scope: "test".into(),
        config_revision: 2,
        accounts: vec![],
        health: vec![],
        model_entitlements: vec![],
        scoped_health: vec![],
        model_availability: vec![],
        provider_policies: policies,
    }));
    let pool = Arc::new(llm_runtime::accounts::AccountPool::from_directory(
        directory,
        Arc::new(llm_runtime::accounts::MemoryAccountPoolStore::default()),
    ));
    let runtime = ModelRuntime::from_config(ClientConfig {
        providers: vec![connection("intl", INTL, 0), connection("cn", CN, 1)],
    })
    .unwrap()
    .with_account_pool(pool);
    let service = ApiService::new(
        Arc::new(runtime),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    assert!(matches!(
        service
            .messages_create(messages_request("shared-model", None))
            .await,
        Err(LlmError::AllAccountsUnavailable { .. })
    ));
    assert!(transport.connections().is_empty());
}

#[tokio::test]
async fn resource_from_replaced_identity_is_rejected_before_any_dispatch() {
    let transport = ScriptedTransport::new(vec![200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    let mut request = llm_runtime::LlmRequest::new("shared-model").with_profile("account.b");
    request.input.max_tokens = Some(4096);
    let old = llm_runtime::accounts::AccountResourceScope {
        store_scope: "test".into(),
        account_id: "b".into(),
        identity_generation: 0,
    };
    request.execution.file_account_scope = Some(serde_json::to_string(&old).unwrap());
    assert!(matches!(
        service.execute_side_query_request(request).await,
        Err(LlmError::InvalidRequest { .. })
    ));
    assert!(transport.connections().is_empty());
}

#[tokio::test]
async fn malformed_resource_scope_is_rejected_before_any_dispatch() {
    let transport = ScriptedTransport::new(vec![200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    let mut request = llm_runtime::LlmRequest::new("shared-model").with_profile("account.b");
    request.input.max_tokens = Some(4096);
    request.execution.file_account_scope = Some("resource-owned-by-b".into());
    assert!(matches!(
        service.execute_side_query_request(request).await,
        Err(LlmError::InvalidRequest { .. })
    ));
    assert!(transport.connections().is_empty());
}

#[tokio::test]
async fn model_denial_preserves_other_models_and_reprobes_after_bounded_ttl() {
    let transport = ScriptedTransport::new(vec![403, 200, 200, 200, 200]);
    let clock = Arc::new(AccountTestClock::default());
    let service = account_service(transport.clone(), clock.clone());
    // A cannot use M1, B can. M2 on A is independent of that entitlement.
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    service
        .messages_create(messages_request("second-model", None))
        .await
        .unwrap();
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "cn", "intl", "cn"]);
    clock.0.store(300_001, std::sync::atomic::Ordering::SeqCst);
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(
        transport.connections(),
        vec!["intl", "cn", "intl", "cn", "intl"]
    );
}

#[tokio::test]
async fn all_model_denials_are_scoped_and_do_not_repeat_network_before_ttl() {
    let transport = ScriptedTransport::new(vec![403, 403, 200]);
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    for _ in 0..2 {
        match service
            .messages_create(messages_request("shared-model", None))
            .await
        {
            Err(LlmError::AllAccountsUnavailable {
                accounts,
                earliest_recovery_ms,
            }) => {
                assert_eq!(earliest_recovery_ms, Some(300_000));
                assert_eq!(accounts.len(), 2);
                assert!(accounts.iter().all(|account| matches!(&account.scope, llm_runtime::accounts::FailureScope::Model(model) if model == "shared-model")));
            }
            other => panic!("expected model aggregate, got {other:?}"),
        }
    }
    assert_eq!(transport.connections(), vec!["intl", "cn"]);
    service
        .messages_create(messages_request("second-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), vec!["intl", "cn", "intl"]);
}

struct TailFrames(std::collections::VecDeque<serde_json::Value>);
impl llm_runtime::test_support::FrameStream for TailFrames {
    fn next_frame(
        &mut self,
    ) -> BoxFuture<'_, Result<Option<llm_runtime::RawStreamFrame>, LlmError>> {
        Box::pin(async move {
            Ok(self
                .0
                .pop_front()
                .map(|frame| llm_runtime::RawStreamFrame::new(serde_json::to_vec(&frame).unwrap())))
        })
    }
}
struct QuotaTransport {
    inner: Arc<ScriptedTransport>,
    retry_after_seconds: Option<u64>,
}
impl llm_runtime::test_support::FixtureTransport for QuotaTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            let mut response =
                llm_runtime::test_support::FixtureTransport::execute(self.inner.as_ref(), request)
                    .await?;
            if response.status == 429 {
                response.body_json = lingxi_llm_client::protocol::JsonPayload::from_generated_value(&serde_json::json!({"error":{"message":"quota exhausted","type":"insufficient_quota","code":"insufficient_quota"}})).unwrap();
                if let Some(seconds) = self.retry_after_seconds {
                    response
                        .headers
                        .insert("retry-after".into(), seconds.to_string());
                }
            }
            Ok(response)
        })
    }
    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        llm_runtime::test_support::FixtureTransport::open_stream(self.inner.as_ref(), request)
    }
}
llm_runtime::impl_fixture_transport!(QuotaTransport);

#[derive(Debug)]
struct VersionedCredentials {
    revision: Arc<std::sync::atomic::AtomicU64>,
    refreshes: std::sync::atomic::AtomicUsize,
}
impl llm_runtime::CredentialProvider for VersionedCredentials {
    fn load<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        Box::pin(async move {
            Ok(llm_runtime::Credential::BearerToken(format!(
                "token{}",
                self.revision.load(std::sync::atomic::Ordering::SeqCst)
            )))
        })
    }
    fn load_versioned<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<
        'a,
        Result<llm_runtime::VersionedCredential, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            let revision = self.revision.load(std::sync::atomic::Ordering::SeqCst);
            Ok(llm_runtime::VersionedCredential {
                credential: llm_runtime::Credential::BearerToken(format!("token{revision}")),
                credential_revision: Some(revision),
            })
        })
    }
    fn refresh_versioned<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
        _: &'a llm_runtime::Credential,
    ) -> BoxFuture<
        'a,
        Result<Option<llm_runtime::VersionedCredential>, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            self.refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.revision.store(2, std::sync::atomic::Ordering::SeqCst);
            self.load_versioned(scope).await.map(Some)
        })
    }
    fn current_credential_revision<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async move {
            Ok(Some(
                self.revision.load(std::sync::atomic::Ordering::SeqCst),
            ))
        })
    }
}
struct RotatingAuthTransport {
    credentials: Arc<VersionedCredentials>,
    calls: Mutex<Vec<String>>,
    late_stream: bool,
}
#[derive(Debug)]
struct DerivedCredentials {
    token: std::sync::atomic::AtomicU64,
    refreshes: std::sync::atomic::AtomicUsize,
}
impl llm_runtime::CredentialProvider for DerivedCredentials {
    fn load<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        Box::pin(async move {
            Ok(llm_runtime::Credential::BearerToken(format!(
                "derived-{}",
                self.token.load(std::sync::atomic::Ordering::SeqCst)
            )))
        })
    }
    fn load_versioned<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<
        'a,
        Result<llm_runtime::VersionedCredential, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            Ok(llm_runtime::VersionedCredential {
                credential: self.load(scope).await?,
                credential_revision: Some(1),
            })
        })
    }
    fn refresh_versioned<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
        _: &'a llm_runtime::Credential,
    ) -> BoxFuture<
        'a,
        Result<Option<llm_runtime::VersionedCredential>, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            self.refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.token.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.load_versioned(scope).await.map(Some)
        })
    }
    fn current_credential_revision<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async { Ok(Some(1)) })
    }
    fn rejected_credential_is_current<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
        rejected: &'a llm_runtime::Credential,
    ) -> BoxFuture<'a, Result<Option<bool>, LlmError>> {
        Box::pin(async move {
            self.load(scope)
                .await
                .map(|current| Some(&current == rejected))
        })
    }
}
#[tokio::test]
async fn derived_copilot_bearer_rejection_cools_temporarily_without_marking_raw_grant_needs_auth() {
    let credentials = Arc::new(DerivedCredentials {
        token: std::sync::atomic::AtomicU64::new(1),
        refreshes: Default::default(),
    });
    let transport = ScriptedTransport::new(vec![401, 401, 200, 200, 200]);
    let clock = Arc::new(AccountTestClock::default());
    let (runtime, _) = account_runtime(clock.clone(), None);
    let mut config = runtime.configuration();
    for profile in &mut config.providers {
        profile.protocol = ProtocolFamily::OpenAiChat;
        profile.auth = AuthStrategy::CopilotBearer;
        profile.credential = CredentialConfig::HostManaged {
            id: "synthetic".into(),
        };
    }
    let service = ApiService::new(
        Arc::new(
            runtime
                .reconfigured(config)
                .unwrap()
                .with_credential_provider(credentials.clone()),
        ),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(
        credentials
            .refreshes
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(transport.connections(), ["intl", "intl", "cn", "cn"]);
    clock.0.store(5_001, std::sync::atomic::Ordering::SeqCst);
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert_eq!(
        transport.connections(),
        ["intl", "intl", "cn", "cn", "intl"]
    );
}
#[derive(Debug)]
struct FailedGrantCredentials {
    revision: std::sync::atomic::AtomicU64,
    fail: std::sync::atomic::AtomicBool,
    rotate_during_failure: bool,
}
impl llm_runtime::CredentialProvider for FailedGrantCredentials {
    fn load<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        Box::pin(async { Ok(llm_runtime::Credential::BearerToken("synthetic".into())) })
    }
    fn load_versioned<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<
        'a,
        Result<llm_runtime::VersionedCredential, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            let revision = self.revision.load(std::sync::atomic::Ordering::SeqCst);
            if scope.profile_name == "account.a"
                && self.fail.load(std::sync::atomic::Ordering::SeqCst)
            {
                if self.rotate_during_failure {
                    self.revision.store(3, std::sync::atomic::Ordering::SeqCst);
                }
                return Err(llm_runtime::VersionedCredentialError {
                    error: LlmError::OAuthRefreshDead,
                    credential_revision: Some(revision),
                });
            }
            Ok(llm_runtime::VersionedCredential {
                credential: llm_runtime::Credential::BearerToken(format!("token{revision}")),
                credential_revision: Some(revision),
            })
        })
    }
    fn current_credential_revision<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async move {
            Ok(Some(
                self.revision.load(std::sync::atomic::Ordering::SeqCst),
            ))
        })
    }
}
#[tokio::test]
async fn failed_proactive_grant_carries_its_own_revision_without_poisoning_newer_grant() {
    for rotate_during_failure in [false, true] {
        let credentials = Arc::new(FailedGrantCredentials {
            revision: std::sync::atomic::AtomicU64::new(2),
            fail: std::sync::atomic::AtomicBool::new(true),
            rotate_during_failure,
        });
        let transport = ScriptedTransport::new(vec![200]);
        let (runtime, _) = account_runtime(Arc::new(AccountTestClock::default()), None);
        let mut config = runtime.configuration();
        for profile in &mut config.providers {
            profile.auth = AuthStrategy::Bearer;
            profile.credential = CredentialConfig::HostManaged {
                id: "synthetic".into(),
            };
        }
        let runtime = runtime
            .reconfigured(config)
            .unwrap()
            .with_credential_provider(credentials.clone());
        let service = ApiService::new(
            Arc::new(runtime),
            transport.clone(),
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        );
        let first = service
            .messages_create(messages_request("shared-model", None))
            .await;
        if rotate_during_failure {
            assert!(matches!(first, Err(LlmError::OAuthRefreshDead)));
            assert!(
                transport.connections().is_empty(),
                "old failed grant must not authorize replay on another account"
            );
            credentials
                .fail
                .store(false, std::sync::atomic::Ordering::SeqCst);
            service
                .messages_create(messages_request("shared-model", None))
                .await
                .unwrap();
            assert_eq!(transport.connections(), ["intl"]);
        } else {
            first.unwrap();
            service
                .messages_create(messages_request("shared-model", None))
                .await
                .unwrap();
            assert_eq!(
                transport.connections(),
                ["cn", "cn"],
                "genuinely dead current grant must remain needs_auth"
            );
        }
    }
}
impl llm_runtime::test_support::FixtureTransport for RotatingAuthTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(request.url.clone());
            calls.len()
        };
        Box::pin(async move {
            let status = if !self.late_stream && call <= 2 {
                401
            } else {
                200
            };
            if call == 2 && !self.late_stream {
                self.credentials
                    .revision
                    .store(3, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(ProviderResponse {status,headers:Default::default(),request_id:None,body_json:lingxi_llm_client::protocol::JsonPayload::from_generated_value(&if status==401 {serde_json::json!({"error":{"message":"rejected token","type":"authentication_error"}})} else {ok_body()}).unwrap()})
        })
    }
    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.calls.lock().unwrap().push(request.url.clone());
        Box::pin(async move {
            Ok(StreamingResponse {status:200,headers:Default::default(),frames:Box::new(RotatingAuthFrames {
            revision:self.credentials.revision.clone(),frames:[
                serde_json::json!({"type":"message_start","message":{"id":"msg_auth","type":"message","role":"assistant","model":"shared-model","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"visible"}}),
                serde_json::json!({"type":"error","error":{"type":"authentication_error","message":"rejected old token"}}),
            ].into(),
        })})
        })
    }
}
struct RotatingAuthFrames {
    revision: Arc<std::sync::atomic::AtomicU64>,
    frames: std::collections::VecDeque<serde_json::Value>,
}
impl llm_runtime::test_support::FrameStream for RotatingAuthFrames {
    fn next_frame(
        &mut self,
    ) -> BoxFuture<'_, Result<Option<llm_runtime::RawStreamFrame>, LlmError>> {
        Box::pin(async move {
            let frame = self.frames.pop_front();
            if frame.as_ref().is_some_and(|frame| frame["type"] == "error") {
                self.revision.store(3, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(frame
                .map(|frame| llm_runtime::RawStreamFrame::new(serde_json::to_vec(&frame).unwrap())))
        })
    }
}
llm_runtime::impl_fixture_transport!(RotatingAuthTransport);
fn versioned_auth_service(transport: Arc<RotatingAuthTransport>) -> ApiService {
    let (runtime, _) = account_runtime(Arc::new(AccountTestClock::default()), None);
    let mut config = runtime.configuration();
    for profile in &mut config.providers {
        profile.auth = AuthStrategy::Bearer;
        profile.credential = CredentialConfig::HostManaged {
            id: format!(
                "acct.{}",
                profile.profile_name.trim_start_matches("account.")
            ),
        };
    }
    let runtime = runtime
        .reconfigured(config)
        .unwrap()
        .with_credential_provider(transport.credentials.clone());
    ApiService::new(
        Arc::new(runtime),
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
}
#[tokio::test]
async fn exhausted_renewal_late_old_401_cannot_poison_new_credential_or_replay() {
    let credentials = Arc::new(VersionedCredentials {
        revision: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        refreshes: Default::default(),
    });
    let transport = Arc::new(RotatingAuthTransport {
        credentials: credentials.clone(),
        calls: Default::default(),
        late_stream: false,
    });
    let service = versioned_auth_service(transport.clone());
    let error = service
        .messages_create(messages_request("shared-model", Some("account.a")))
        .await
        .unwrap_err();
    assert!(matches!(
        error.classification_source(),
        LlmError::Authentication { .. }
    ));
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        2,
        "spent renewal cannot refresh or replay stale rejection"
    );
    assert_eq!(
        credentials
            .refreshes
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(
        calls.iter().all(|url| url.starts_with(INTL)),
        "rev3 remains healthy and retains original priority"
    );
}
#[tokio::test]
async fn late_stream_old_401_cannot_poison_new_credential_or_replay_output() {
    use futures::StreamExt;
    let credentials = Arc::new(VersionedCredentials {
        revision: Arc::new(std::sync::atomic::AtomicU64::new(2)),
        refreshes: Default::default(),
    });
    let transport = Arc::new(RotatingAuthTransport {
        credentials: credentials.clone(),
        calls: Default::default(),
        late_stream: true,
    });
    let service = versioned_auth_service(transport.clone());
    let mut request = llm_runtime::LlmRequest::new("shared-model");
    request.input.max_tokens = Some(4096);
    let mut stream = service.stream_request(request).await.unwrap();
    let mut output = false;
    let mut rejected = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(llm_runtime::HistoryEvent::ContentBlockDelta { .. }) => output = true,
            Err(error) => {
                assert!(matches!(
                    error.classification_source(),
                    LlmError::Authentication { .. }
                ));
                rejected = true
            }
            _ => {}
        }
    }
    assert!(output && rejected);
    assert_eq!(
        credentials
            .refreshes
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(transport.calls.lock().unwrap().len(), 1);
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].starts_with(INTL));
}

#[tokio::test]
async fn quota_topup_recovery_uses_bounded_business_probe_and_real_reset_when_known() {
    for reset in [None, Some(600u64)] {
        let inner = ScriptedTransport::new(vec![429, 200, 200, 200, 200]);
        let transport = Arc::new(QuotaTransport {
            inner: inner.clone(),
            retry_after_seconds: reset,
        });
        let clock = Arc::new(AccountTestClock::default());
        let (runtime, _) = account_runtime(clock.clone(), None);
        let mut config = runtime.configuration();
        for profile in &mut config.providers {
            profile.protocol = ProtocolFamily::OpenAiChat;
        }
        let service = ApiService::new(
            Arc::new(runtime.reconfigured(config).unwrap()),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "test",
            None,
            None,
        );
        service
            .messages_create(messages_request("shared-model", None))
            .await
            .unwrap();
        let reset_ms = reset.unwrap_or(300) * 1000;
        clock
            .0
            .store(reset_ms - 1, std::sync::atomic::Ordering::SeqCst);
        service
            .messages_create(messages_request("shared-model", None))
            .await
            .unwrap();
        assert_eq!(inner.connections(), ["intl", "cn", "cn"]);
        clock
            .0
            .store(reset_ms + 1, std::sync::atomic::Ordering::SeqCst);
        service
            .messages_create(messages_request("shared-model", None))
            .await
            .unwrap();
        service
            .messages_create(messages_request("shared-model", None))
            .await
            .unwrap();
        assert_eq!(
            inner.connections(),
            ["intl", "cn", "cn", "intl", "intl"],
            "successful business probe restores original priority"
        );
    }
}
struct LateStreamFailure(Mutex<Vec<String>>, bool);
impl llm_runtime::test_support::FixtureTransport for LateStreamFailure {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let first = {
            let mut calls = self.0.lock().unwrap();
            calls.push(request.url.clone());
            calls.len() == 1
        };
        let status = if self.1 && first { 429 } else { 200 };
        Box::pin(async move {
            Ok(ProviderResponse {
                status,
                headers: BTreeMap::new(),
                body_json: lingxi_llm_client::protocol::JsonPayload::from_generated_value(
                    &ok_body(),
                )
                .unwrap(),
                request_id: None,
            })
        })
    }
    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.0.lock().unwrap().push(request.url.clone());
        Box::pin(async move {
            let mut frames = vec![
                serde_json::json!({"type":"message_start","message":{"id":"msg_stream","type":"message","role":"assistant","model":"shared-model","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"visible"}}),
            ];
            if self.1 {
                frames.extend([
                    serde_json::json!({"type":"content_block_stop","index":0}),
                    serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                    serde_json::json!({"type":"message_stop"}),
                ]);
            } else {
                frames.push(serde_json::json!({"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}));
            }
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(TailFrames(frames.into())),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(LateStreamFailure);

#[tokio::test]
async fn recovery_probe_stays_owned_until_true_stream_terminal() {
    use futures::StreamExt;
    let transport = Arc::new(LateStreamFailure(Mutex::new(Vec::new()), true));
    let clock = Arc::new(AccountTestClock::default());
    let service = account_service(transport.clone(), clock.clone());
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    clock.0.store(30_001, std::sync::atomic::Ordering::SeqCst);
    let mut request = llm_runtime::LlmRequest::new("shared-model");
    request.input.max_tokens = Some(4096);
    let mut stream = service.stream_request(request).await.unwrap();
    // Opening and yielding visible content must not release A's recovery lease.
    while let Some(event) = stream.next().await {
        if matches!(
            event.unwrap(),
            llm_runtime::HistoryEvent::ContentBlockDelta { .. }
        ) {
            break;
        }
    }
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    assert!(transport.0.lock().unwrap().last().unwrap().starts_with(CN));
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    let calls = transport.0.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert!(
        calls.last().unwrap().starts_with(INTL),
        "terminal success releases A's recovery lease"
    );
}

#[tokio::test]
async fn late_stream_error_records_health_and_never_replays_after_output() {
    use futures::StreamExt;
    let transport = Arc::new(LateStreamFailure(Mutex::new(Vec::new()), false));
    let service = account_service(transport.clone(), Arc::new(AccountTestClock::default()));
    let retry_scope = llm_runtime::model::retry_scope::ModelCallRetryScope::default();
    retry_scope
        .run(async {
            let mut request = llm_runtime::LlmRequest::new("shared-model");
            request.input.max_tokens = Some(4096);
            let mut stream = service.stream_request(request.clone()).await.unwrap();
            let mut output = false;
            let mut error = false;
            while let Some(event) = stream.next().await {
                match event {
                    Ok(llm_runtime::HistoryEvent::ContentBlockDelta { .. }) => output = true,
                    Err(_) => error = true,
                    _ => {}
                }
            }
            assert!(output && error);
            assert!(
                service.stream_request(request.clone()).await.is_err(),
                "same logical call cannot reopen after output"
            );
            assert!(
                service.execute_side_query_request(request).await.is_err(),
                "same logical call cannot fall back to nonstream after output"
            );
        })
        .await;
    assert_eq!(
        transport.0.lock().unwrap().len(),
        1,
        "visible output cannot trigger replay"
    );
    service
        .messages_create(messages_request("shared-model", None))
        .await
        .unwrap();
    let calls = transport.0.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].starts_with(CN),
        "late structured failure must cool A down"
    );
}
