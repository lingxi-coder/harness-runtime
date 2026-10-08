//! Real SDK draft, session and dispatch regressions for request-owned credentials.
use super::*;
use crate::{Capabilities, ModelProfile, PricingConfig, ProviderProfile, SigningConfig};
use futures::StreamExt;
use lingxi_llm_client::{self as sdk, protocol as wire};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const WAIT: Duration = Duration::from_secs(3);
const TOKEN_A: &str = "synthetic-snapshot-account-A";
const TOKEN_B: &str = "synthetic-snapshot-account-B";

fn cache_control_ttl(control: &Value) -> wire::CacheTtl {
    assert_eq!(
        control["type"], "ephemeral",
        "cache marker must remain present"
    );
    match control.get("ttl") {
        None => wire::CacheTtl::FiveMinutes,
        Some(ttl) => serde_json::from_value(ttl.clone()).expect("explicit cache TTL must be valid"),
    }
}

#[derive(Debug)]
struct SnapshotStore {
    credential: Mutex<Credential>,
    loads: AtomicUsize,
}

impl SnapshotStore {
    fn new(credential: Credential) -> Self {
        Self {
            credential: Mutex::new(credential),
            loads: AtomicUsize::new(0),
        }
    }

    fn set(&self, credential: Credential) {
        *self.credential.lock().unwrap() = credential;
    }
}

impl CredentialProvider for SnapshotStore {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> crate::BoxFuture<'a, Result<Credential, LlmError>> {
        assert_eq!(scope.profile_name, "snapshot");
        assert_eq!(scope.credential_id.as_deref(), Some("snapshot-key"));
        Box::pin(async move {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.credential.lock().unwrap().clone())
        })
    }
}

#[derive(Debug)]
struct DelayedSnapshotStore {
    credential: Credential,
    loads: AtomicUsize,
    load_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    resume_load: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl CredentialProvider for DelayedSnapshotStore {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> crate::BoxFuture<'a, Result<Credential, LlmError>> {
        assert_eq!(scope.profile_name, "snapshot");
        assert_eq!(scope.credential_id.as_deref(), Some("snapshot-key"));
        self.loads.fetch_add(1, Ordering::SeqCst);
        let credential = self.credential.clone();
        let load_started = self.load_started.lock().unwrap().take();
        let resume_load = self.resume_load.lock().unwrap().take();
        Box::pin(async move {
            if let Some(load_started) = load_started {
                let _ = load_started.send(());
            }
            if let Some(resume_load) = resume_load {
                resume_load
                    .await
                    .expect("the account-change test releases the credential load");
            }
            Ok(credential)
        })
    }
}

fn profile(protocol: ProtocolFamily, auth: AuthStrategy, websockets: bool) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: wire::Region::all(),
        provider_id: if auth == AuthStrategy::AwsSigV4 {
            ProviderId::BedrockClaude
        } else {
            ProviderId::OpenAI
        },
        profile_name: "snapshot".into(),
        base_url: if auth == AuthStrategy::AwsSigV4 {
            "https://bedrock-runtime.us-east-1.amazonaws.com".into()
        } else {
            "https://api.openai.com/v1".into()
        },
        protocol,
        auth,
        credential: CredentialConfig::HostManaged {
            id: "snapshot-key".into(),
        },
        models: vec![ModelProfile {
            display_model: "snapshot-model".into(),
            request_model: "snapshot-model".into(),
            billing_model: "snapshot-model".into(),
            aliases: vec![],
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                ..Default::default()
            },
        }],
        pricing: PricingConfig::default(),
        signing: (auth == AuthStrategy::AwsSigV4).then(|| SigningConfig {
            region: "us-east-1".into(),
            service: "bedrock".into(),
        }),
        azure: None,
        supports_websockets: websockets,
        supports_websocket_compression: false,
        websocket_connect_timeout_ms: Some(2_000),
        vision_delegate: None,
        connection: Default::default(),
    }
}

fn runtime(profile: ProviderProfile, store: Arc<dyn CredentialProvider>) -> ModelRuntime {
    ModelRuntime::from_config(ClientConfig {
        providers: vec![profile],
    })
    .unwrap()
    .with_credential_provider(store)
}

#[tokio::test]
async fn prompt_cache_subscriber_uses_the_request_credential_snapshot_scopes() {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        CachePolicy, PromptCacheQuerySource, PromptCacheTtlInputs, PromptCacheTtlSettings,
        PromptText, SystemPromptInput,
    };

    let mut first_party = profile(
        ProtocolFamily::AnthropicMessages,
        AuthStrategy::OAuthBearer,
        false,
    );
    first_party.provider_id = ProviderId::AnthropicFirstParty;
    first_party.base_url = "https://api.anthropic.com".into();
    let store = Arc::new(SnapshotStore::new(Credential::AnthropicOAuth {
        access_token: TOKEN_A.into(),
        scopes: vec!["user:inference".into()],
    }));
    let current_epoch = Arc::new(AtomicU64::new(7));
    let overage = Arc::new(AtomicBool::new(false));
    let client = runtime(first_party, store.clone());
    let mut request = crate::LlmRequest::new("snapshot-model").with_profile("snapshot");
    request.execution.prompt_cache = Some(crate::PromptCacheRequestContext {
        system: Some(SystemPromptInput::source_vector(
            vec![PromptText::from_string("system text")],
            None,
            Some(PromptText::from_string("LingXi")),
        )),
        policy: CachePolicy::from_process(
            false,
            Some(lingxi_llm_client::providers::anthropic::system_prompt::AgentPromptCacheTtlOverride::OneHour),
            false,
            PromptCacheQuerySource::Named("repl_main_thread"),
            PromptCacheTtlSettings::default(),
            PromptCacheTtlInputs::default(),
        ),
        current_account_epoch: Arc::new({
            let current_epoch = current_epoch.clone();
            move || current_epoch.load(Ordering::SeqCst)
        }),
        native_bare_mode: false,
        native_unix_socket: false,
        overage_for_scope: Arc::new({
            let overage = overage.clone();
            move |scope, epoch| {
                overage.load(Ordering::SeqCst)
                    && epoch == 7
                && scope.profile_name == "snapshot"
                && scope.credential_id.as_deref() == Some("snapshot-key")
            }
        }),
        pending_overage: Arc::new(Mutex::new(None)),
    });

    let prepared = client.prepare(&request).await.unwrap();
    let prompt_cache = prepared.prompt_cache.as_ref().unwrap();
    assert!(prompt_cache.is_subscriber);
    assert_eq!(prompt_cache.account_epoch, 7);
    assert!(!prompt_cache.account_epoch_stale);
    assert_eq!(prompt_cache.scope.profile_name, "snapshot");
    assert_eq!(
        prompt_cache.scope.credential_id.as_deref(),
        Some("snapshot-key")
    );
    assert_eq!(
        prepared.provider_request.body_json["system"][0]["cache_control"]["ttl"], "1h",
        "the scoped subscriber and explicit one-hour override reach final SDK egress"
    );
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);

    overage.store(true, Ordering::SeqCst);
    let overage_prepared = client.prepare(&request).await.unwrap();
    assert_eq!(
        cache_control_ttl(
            &overage_prepared.provider_request.body_json["system"][0]["cache_control"]
        ),
        wire::CacheTtl::FiveMinutes,
        "subscriber overage suppresses an explicit one-hour prompt TTL at egress"
    );
    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn account_change_after_request_build_binds_the_current_credential_generation() {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        AgentPromptCacheTtlOverride, CachePolicy, PromptCacheQuerySource, PromptCacheTtlInputs,
        PromptCacheTtlSettings, PromptText, SystemPromptInput,
    };

    let mut first_party = profile(
        ProtocolFamily::AnthropicMessages,
        AuthStrategy::OAuthBearer,
        false,
    );
    first_party.provider_id = ProviderId::AnthropicFirstParty;
    first_party.base_url = "https://api.anthropic.com".into();
    let store = Arc::new(SnapshotStore::new(Credential::AnthropicOAuth {
        access_token: TOKEN_A.into(),
        scopes: vec!["user:inference".into()],
    }));
    let client = runtime(first_party, store.clone());
    let current_epoch = Arc::new(AtomicU64::new(7));
    let overage = Arc::new(AtomicBool::new(false));
    let mut request = crate::LlmRequest::new("snapshot-model").with_profile("snapshot");
    request.execution.prompt_cache = Some(crate::PromptCacheRequestContext {
        system: Some(SystemPromptInput::source_vector(
            vec![PromptText::from_string("system text")],
            None,
            Some(PromptText::from_string("LingXi")),
        )),
        policy: CachePolicy::from_process(
            false,
            Some(AgentPromptCacheTtlOverride::OneHour),
            false,
            PromptCacheQuerySource::Named("repl_main_thread"),
            PromptCacheTtlSettings::default(),
            PromptCacheTtlInputs::default(),
        ),
        current_account_epoch: Arc::new({
            let current_epoch = current_epoch.clone();
            move || current_epoch.load(Ordering::SeqCst)
        }),
        native_bare_mode: false,
        native_unix_socket: false,
        overage_for_scope: Arc::new({
            let current_epoch = current_epoch.clone();
            let overage = overage.clone();
            move |scope, request_epoch| {
                request_epoch == current_epoch.load(Ordering::SeqCst)
                    && overage.load(Ordering::SeqCst)
                    && scope.profile_name == "snapshot"
                    && scope.credential_id.as_deref() == Some("snapshot-key")
            }
        }),
        pending_overage: Arc::new(Mutex::new(None)),
    });

    // The context is assembled under account A. Model the completed account
    // change before credential selection, including B's newly observed
    // overage state.
    current_epoch.store(8, Ordering::SeqCst);
    overage.store(true, Ordering::SeqCst);
    store.set(Credential::AnthropicOAuth {
        access_token: TOKEN_B.into(),
        scopes: vec!["user:inference".into()],
    });

    let prepared = client.prepare(&request).await.unwrap();
    let prompt_cache = prepared.prompt_cache.as_ref().unwrap();
    assert!(prompt_cache.is_subscriber);
    assert_eq!(prompt_cache.account_epoch, 8);
    assert!(!prompt_cache.account_epoch_stale);
    assert_eq!(
        cache_control_ttl(&prepared.provider_request.body_json["system"][0]["cache_control"]),
        wire::CacheTtl::FiveMinutes,
        "the current account's overage state is used after a pre-prepare switch"
    );
    let credential = prepared.authenticator.captured_credential().await.unwrap();
    assert!(
        matches!(credential, Some(Credential::AnthropicOAuth { ref access_token, .. }) if access_token == TOKEN_B)
    );
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn account_change_during_credential_load_keeps_the_request_generation_separate() {
    use lingxi_llm_client::providers::anthropic::system_prompt::{
        AgentPromptCacheTtlOverride, CachePolicy, PromptCacheQuerySource, PromptCacheTtlInputs,
        PromptCacheTtlSettings, PromptText, SystemPromptInput,
    };

    let mut first_party = profile(
        ProtocolFamily::AnthropicMessages,
        AuthStrategy::OAuthBearer,
        false,
    );
    first_party.provider_id = ProviderId::AnthropicFirstParty;
    first_party.base_url = "https://api.anthropic.com".into();
    let (load_started_tx, load_started_rx) = tokio::sync::oneshot::channel();
    let (resume_load_tx, resume_load_rx) = tokio::sync::oneshot::channel();
    let store = Arc::new(DelayedSnapshotStore {
        credential: Credential::AnthropicOAuth {
            access_token: TOKEN_A.into(),
            scopes: vec!["user:inference".into()],
        },
        loads: AtomicUsize::new(0),
        load_started: Mutex::new(Some(load_started_tx)),
        resume_load: Mutex::new(Some(resume_load_rx)),
    });
    let client = runtime(first_party, store.clone());
    let current_epoch = Arc::new(AtomicU64::new(7));
    let overage = Arc::new(AtomicBool::new(true));
    let mut request = crate::LlmRequest::new("snapshot-model").with_profile("snapshot");
    request.execution.prompt_cache = Some(crate::PromptCacheRequestContext {
        system: Some(SystemPromptInput::source_vector(
            vec![PromptText::from_string("system text")],
            None,
            Some(PromptText::from_string("LingXi")),
        )),
        policy: CachePolicy::from_process(
            false,
            Some(AgentPromptCacheTtlOverride::OneHour),
            false,
            PromptCacheQuerySource::Named("repl_main_thread"),
            PromptCacheTtlSettings::default(),
            PromptCacheTtlInputs::default(),
        ),
        current_account_epoch: Arc::new({
            let current_epoch = current_epoch.clone();
            move || current_epoch.load(Ordering::SeqCst)
        }),
        native_bare_mode: false,
        native_unix_socket: false,
        overage_for_scope: Arc::new({
            let current_epoch = current_epoch.clone();
            let overage = overage.clone();
            move |scope, request_epoch| {
                request_epoch == current_epoch.load(Ordering::SeqCst)
                    && overage.load(Ordering::SeqCst)
                    && scope.profile_name == "snapshot"
                    && scope.credential_id.as_deref() == Some("snapshot-key")
            }
        }),
        pending_overage: Arc::new(Mutex::new(None)),
    });

    // The provider snapshots account A before suspending its load. Advancing
    // the Host generation and clearing overage while this one load is pending
    // must leave the call bound to A's earlier generation.
    let prepared_task = tokio::spawn(async move { client.prepare(&request).await });
    load_started_rx.await.unwrap();
    current_epoch.store(8, Ordering::SeqCst);
    overage.store(false, Ordering::SeqCst);
    resume_load_tx.send(()).unwrap();

    let prepared = prepared_task.await.unwrap().unwrap();
    let prompt_cache = prepared.prompt_cache.as_ref().unwrap();
    assert!(prompt_cache.is_subscriber);
    assert_eq!(prompt_cache.account_epoch, 7);
    assert!(prompt_cache.account_epoch_stale);
    assert_eq!(
        prepared.provider_request.body_json["system"][0]["cache_control"]["ttl"], "1h",
        "the reset account state prevents stale overage from suppressing the new request"
    );
    let credential = prepared.authenticator.captured_credential().await.unwrap();
    assert!(
        matches!(credential, Some(Credential::AnthropicOAuth { ref access_token, .. }) if access_token == TOKEN_A)
    );
    assert_eq!(
        store.loads.load(Ordering::SeqCst),
        1,
        "credential projection and final authentication share the same request snapshot"
    );
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .unwrap()
        .1
        .as_str()
}

fn sealed_auth(prepared: &PreparedLlmCall) -> &str {
    prepared
        .provider_request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .unwrap()
        .1
        .as_str()
}

fn response_frames(id: &str, sse: bool) -> sdk::StreamResponse {
    let frames = [
        json!({"type":"response.created","response":{"id":id,"model":"snapshot-model","output":[]}}),
        json!({"type":"response.completed","response":{"id":id,"model":"snapshot-model","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":3,"total_tokens":7}}}),
    ].into_iter().map(move |frame| {
        let bytes = if sse { format!("data: {frame}\n\n").into_bytes() } else { serde_json::to_vec(&frame).unwrap() };
        Ok(bytes::Bytes::from(bytes))
    }).collect::<Vec<Result<bytes::Bytes, wire::LlmError>>>();
    sdk::StreamResponse {
        status: if sse { 200 } else { 101 },
        headers: vec![],
        body: futures::stream::iter(frames).boxed(),
    }
}

#[derive(Clone)]
struct SocketSend {
    authorization: String,
    body: Value,
}

#[derive(Default)]
struct RaceObservations {
    handshakes: Mutex<Vec<sdk::HttpRequest>>,
    socket_sends: Mutex<Vec<SocketSend>>,
    http_sends: Mutex<Vec<sdk::HttpRequest>>,
    closes: AtomicUsize,
    dispatches: AtomicUsize,
    response_ordinal: AtomicUsize,
}

struct RaceTransport {
    store: Arc<SnapshotStore>,
    observations: Arc<RaceObservations>,
    first_upgrade_required: bool,
}

#[async_trait::async_trait]
impl sdk::Transport for RaceTransport {
    async fn send(&self, request: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
        self.observations.http_sends.lock().unwrap().push(request);
        let ordinal = self
            .observations
            .response_ordinal
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        assert_eq!(self.observations.dispatches.load(Ordering::SeqCst), ordinal);
        Ok(response_frames(&format!("response-{ordinal}"), true))
    }

    async fn connect_websocket(
        &self,
        handshake: sdk::HttpRequest,
    ) -> Result<Box<dyn sdk::transport::WebSocketConnection>, wire::LlmError> {
        assert_eq!(handshake.method, "GET");
        assert!(handshake.body.is_empty());
        let authorization = header(&handshake.headers, "authorization").to_owned();
        let first = {
            let mut handshakes = self.observations.handshakes.lock().unwrap();
            handshakes.push(handshake);
            handshakes.len() == 1
        };
        if first {
            assert_eq!(authorization, format!("Bearer {TOKEN_A}"));
            // The store changes strictly after the authenticated handshake was
            // built and strictly before session.prepare returns to draft.seal.
            self.store.set(Credential::BearerToken(TOKEN_B.into()));
            if self.first_upgrade_required {
                return Err(wire::LlmError::Transport {
                    message: "HTTP 426 upgrade required".into(),
                });
            }
        }
        Ok(Box::new(RaceSocket {
            authorization,
            observations: self.observations.clone(),
        }))
    }
}

struct RaceSocket {
    authorization: String,
    observations: Arc<RaceObservations>,
}

#[async_trait::async_trait]
impl sdk::transport::WebSocketConnection for RaceSocket {
    async fn send(&mut self, payload: bytes::Bytes) -> Result<sdk::StreamResponse, wire::LlmError> {
        let body: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(body["type"], "response.create");
        self.observations
            .socket_sends
            .lock()
            .unwrap()
            .push(SocketSend {
                authorization: self.authorization.clone(),
                body,
            });
        let ordinal = self
            .observations
            .response_ordinal
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        assert_eq!(self.observations.dispatches.load(Ordering::SeqCst), ordinal);
        Ok(response_frames(&format!("response-{ordinal}"), false))
    }

    async fn close(&mut self) -> Result<(), wire::LlmError> {
        self.observations.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn consume(received: sdk::ReceivedCall) {
    let mut stream = received
        .into_stream()
        .unwrap_or_else(|_| panic!("successful streaming response required"));
    tokio::time::timeout(WAIT, async {
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(stream.usage_is_complete());
    let usage = stream.observed_usage().unwrap();
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.output_tokens, 3);
}

async fn send_draft(
    client: &ModelRuntime,
    transport: Arc<dyn Transport>,
    session: &mut ResponsesSession,
    request: &LlmRequest,
    observations: &Arc<RaceObservations>,
    expected_authorization: &str,
) {
    let prepared = tokio::time::timeout(WAIT, client.prepare_on(request, transport))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        prepared.provider_request.stream_transport,
        ProviderStreamTransport::ResponsesWebSocket
    );
    let prepared = tokio::time::timeout(WAIT, client.prepare_shared_stream(prepared, session))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sealed_auth(&prepared), expected_authorization);
    let mut mark = || {
        observations.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };
    let (_, received) = tokio::time::timeout(
        WAIT,
        client.open_shared_stream(prepared, session, &mut mark),
    )
    .await
    .unwrap()
    .unwrap();
    consume(received).await;
}

#[tokio::test]
async fn websocket_handshake_and_seal_share_credentials_and_next_draft_rebinds() {
    let store = Arc::new(SnapshotStore::new(Credential::BearerToken(TOKEN_A.into())));
    let observations = Arc::new(RaceObservations::default());
    let transport: Arc<dyn Transport> = Arc::new(RaceTransport {
        store: store.clone(),
        observations: observations.clone(),
        first_upgrade_required: false,
    });
    let client = runtime(
        profile(ProtocolFamily::OpenAiResponses, AuthStrategy::Bearer, true),
        store.clone(),
    );
    let mut session = ResponsesSession::new();
    let mut request = LlmRequest::new("snapshot-model").with_user_text("original");
    request.stream = true;
    request.execution.account_scope = Some("trusted-snapshot-scope".into());
    for (ordinal, token) in [(1, TOKEN_A), (2, TOKEN_B), (3, TOKEN_B)] {
        if ordinal == 3 {
            request
                .input
                .messages
                .push(wire::ConversationMessage::user_text("new B input"));
        }
        send_draft(
            &client,
            transport.clone(),
            &mut session,
            &request,
            &observations,
            &format!("Bearer {token}"),
        )
        .await;
        assert_eq!(
            store.loads.load(Ordering::SeqCst),
            ordinal,
            "one credential load per fresh draft"
        );
    }
    let handshakes = observations.handshakes.lock().unwrap();
    assert_eq!(handshakes.len(), 2);
    assert_eq!(
        header(&handshakes[0].headers, "authorization"),
        format!("Bearer {TOKEN_A}")
    );
    assert_eq!(
        header(&handshakes[1].headers, "authorization"),
        format!("Bearer {TOKEN_B}")
    );
    let sends = observations.socket_sends.lock().unwrap();
    assert_eq!(sends.len(), 3);
    assert_eq!(sends[0].authorization, format!("Bearer {TOKEN_A}"));
    assert_eq!(sends[1].authorization, format!("Bearer {TOKEN_B}"));
    assert_eq!(sends[2].authorization, format!("Bearer {TOKEN_B}"));
    assert!(
        sends[1].body.get("previous_response_id").is_none(),
        "B must not inherit A continuation"
    );
    assert_eq!(sends[2].body["previous_response_id"], "response-2");
    assert_eq!(
        sends[2].body["input"].as_array().unwrap().len(),
        1,
        "B reuse sends only the newly appended input"
    );
    assert!(sends[2].body["input"].to_string().contains("new B input"));
    assert!(observations.http_sends.lock().unwrap().is_empty());
    assert_eq!(observations.closes.load(Ordering::SeqCst), 1);
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 3);
    drop(sends);
    drop(handshakes);
    tokio::time::timeout(WAIT, session.close())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn upgrade_required_http_fallback_retains_draft_credentials_then_refreshes() {
    let store = Arc::new(SnapshotStore::new(Credential::BearerToken(TOKEN_A.into())));
    let observations = Arc::new(RaceObservations::default());
    let transport: Arc<dyn Transport> = Arc::new(RaceTransport {
        store: store.clone(),
        observations: observations.clone(),
        first_upgrade_required: true,
    });
    let client = runtime(
        profile(ProtocolFamily::OpenAiResponses, AuthStrategy::Bearer, true),
        store.clone(),
    );
    let mut session = ResponsesSession::new();
    let mut request = LlmRequest::new("snapshot-model").with_user_text("original");
    request.stream = true;
    send_draft(
        &client,
        transport.clone(),
        &mut session,
        &request,
        &observations,
        &format!("Bearer {TOKEN_A}"),
    )
    .await;
    assert!(session.fallback_to_http());
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    assert_eq!(observations.http_sends.lock().unwrap().len(), 1);
    assert_eq!(
        header(
            &observations.http_sends.lock().unwrap()[0].headers,
            "authorization"
        ),
        format!("Bearer {TOKEN_A}")
    );
    send_draft(
        &client,
        transport,
        &mut session,
        &request,
        &observations,
        &format!("Bearer {TOKEN_B}"),
    )
    .await;
    assert!(
        !session.fallback_to_http(),
        "new credentials clear the old account's cached HTTP fallback"
    );
    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
    assert_eq!(observations.handshakes.lock().unwrap().len(), 2);
    let sends = observations.socket_sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].authorization, format!("Bearer {TOKEN_B}"));
    assert!(sends[0].body.get("previous_response_id").is_none());
    assert_eq!(observations.http_sends.lock().unwrap().len(), 1);
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(observations.closes.load(Ordering::SeqCst), 0);
    drop(sends);
    tokio::time::timeout(WAIT, session.close())
        .await
        .unwrap()
        .unwrap();
}

#[derive(Default)]
struct SignedHttpCapture {
    requests: Mutex<Vec<sdk::HttpRequest>>,
}

#[async_trait::async_trait]
impl sdk::Transport for SignedHttpCapture {
    async fn send(&self, request: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
        self.requests.lock().unwrap().push(request);
        let body = serde_json::to_vec(&json!({"id":"signed-response","model":"snapshot-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":3}})).unwrap();
        Ok(sdk::StreamResponse {
            status: 200,
            headers: vec![],
            body: futures::stream::once(async move { Ok(bytes::Bytes::from(body)) }).boxed(),
        })
    }
}

#[tokio::test]
async fn request_snapshot_signs_final_http_policy_body_and_headers() {
    const ACCESS: &str = "SYNTHETIC_ACCESS_FOR_SNAPSHOT";
    const SECRET: &str = "synthetic-signing-secret-for-test-only";
    let store = Arc::new(SnapshotStore::new(Credential::AwsSigV4 {
        access_key_id: ACCESS.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
    }));
    let client = runtime(
        profile(
            ProtocolFamily::AnthropicMessages,
            AuthStrategy::AwsSigV4,
            false,
        ),
        store.clone(),
    );
    let transport = Arc::new(SignedHttpCapture::default());
    let request = LlmRequest::new("snapshot-model").with_user_text("final signed request");
    let mut prepared = tokio::time::timeout(WAIT, client.prepare_on(&request, transport.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.loads.load(Ordering::SeqCst),
        0,
        "draft assembly does not acquire credentials early"
    );
    prepared.provider_request.body_json["max_tokens"] = json!(17);
    prepared
        .provider_request
        .headers
        .insert("X-Snapshot-Policy".into(), "final-policy".into());
    tokio::time::timeout(WAIT, client.seal_prepared(&mut prepared))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    let sealed_authorization = sealed_auth(&prepared).to_owned();
    let call = prepared.wire_call.take().unwrap();
    let dispatches = AtomicUsize::new(0);
    let received = tokio::time::timeout(
        WAIT,
        call.dispatch_once_with(|| {
            dispatches.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let collected = tokio::time::timeout(WAIT, received.collect())
        .await
        .unwrap()
        .unwrap();
    collected.decode().unwrap();
    tokio::time::timeout(WAIT, collected.finish())
        .await
        .unwrap();
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(
        serde_json::from_slice::<Value>(&sent.body).unwrap()["max_tokens"],
        17
    );
    assert_eq!(header(&sent.headers, "x-snapshot-policy"), "final-policy");
    assert_eq!(header(&sent.headers, "authorization"), sealed_authorization);
    let unsigned: BTreeMap<String, String> = sent
        .headers
        .iter()
        .filter(|(name, _)| {
            !["authorization", "x-amz-date", "x-amz-content-sha256"]
                .iter()
                .any(|excluded| name.eq_ignore_ascii_case(excluded))
        })
        .cloned()
        .collect();
    let expected = sdk::auth::sigv4::sign_request(
        &sent.method,
        &sent.url,
        &unsigned,
        &sent.body,
        ACCESS,
        SECRET,
        None,
        "us-east-1",
        "bedrock",
        header(&sent.headers, "x-amz-date"),
    )
    .unwrap();
    assert_eq!(
        sealed_authorization, expected.authorization,
        "signature must cover the final SDK wire image"
    );
    assert_eq!(
        store.loads.load(Ordering::SeqCst),
        1,
        "dispatch must not reload credentials"
    );
}

async fn assert_public_prepare_prewarm_account_snapshot(explicit_clock: bool) {
    const SOURCE: &str = "public-prepare-snapshot";
    let store = Arc::new(SnapshotStore::new(Credential::BearerToken(TOKEN_A.into())));
    let observations = Arc::new(RaceObservations::default());
    let transport: Arc<dyn Transport> = Arc::new(RaceTransport {
        store: store.clone(),
        observations: observations.clone(),
        first_upgrade_required: false,
    });
    let client = runtime(
        profile(ProtocolFamily::OpenAiResponses, AuthStrategy::Bearer, true),
        store.clone(),
    );
    let mut request = LlmRequest::new("snapshot-model").with_user_text("public prepared input");
    request.stream = true;
    request.execution.account_scope = Some("trusted-public-snapshot-scope".into());
    request.execution.query_source = Some(SOURCE.into());
    let mut session = ResponsesSession::new();

    for (ordinal, token) in [(1, TOKEN_A), (2, TOKEN_B)] {
        let mut prepared = tokio::time::timeout(WAIT, async {
            if explicit_clock {
                client
                    .prepare_at(
                        &request,
                        std::time::UNIX_EPOCH + Duration::from_secs(1_440_938_160),
                    )
                    .await
            } else {
                client.prepare(&request).await
            }
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            store.loads.load(Ordering::SeqCst),
            ordinal,
            "each public preparation acquires its own credential snapshot"
        );
        assert_eq!(sealed_auth(&prepared), format!("Bearer {token}"));
        assert_eq!(prepared.route.resolved_route.profile_name, "snapshot");
        assert_eq!(
            prepared.route.resolved_route.display_model,
            "snapshot-model"
        );
        assert_eq!(prepared.route.protocol, ProtocolFamily::OpenAiResponses);
        assert_eq!(
            prepared.provider_request.stream_transport,
            ProviderStreamTransport::ResponsesWebSocket
        );
        let draft = prepared.wire_draft.as_ref().unwrap();
        assert_eq!(draft.profile().profile_name, "snapshot");
        assert_eq!(draft.model().display_model, "snapshot-model");

        prepared
            .provider_request
            .headers
            .insert("X-Snapshot-Source".into(), SOURCE.into());
        prepared
            .provider_request
            .headers
            .insert("X-Snapshot-Policy".into(), "final-public-policy".into());
        prepared
            .provider_request
            .headers
            .insert("X-Request-Id".into(), format!("public-prepare-{ordinal}"));
        if ordinal == 1 {
            // Public preparation has already authenticated A. Rotation before
            // the first SDK handshake must not move this draft onto account B.
            store.set(Credential::BearerToken(TOKEN_B.into()));
            assert_eq!(sealed_auth(&prepared), format!("Bearer {TOKEN_A}"));
        }
        observations.dispatches.fetch_add(1, Ordering::SeqCst);
        tokio::time::timeout(
            WAIT,
            client.prewarm_prepared_websocket(prepared, transport.clone(), &mut session),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            store.loads.load(Ordering::SeqCst),
            ordinal,
            "SDK handshake and final seal must reuse the public draft's first load"
        );
        assert!(session.last_response_from_prewarm());
        assert_eq!(
            session.last_response_id().as_deref(),
            Some(format!("response-{ordinal}").as_str())
        );
    }

    let handshakes = observations.handshakes.lock().unwrap();
    assert_eq!(handshakes.len(), 2, "B must rebind the A connection");
    for (index, token) in [TOKEN_A, TOKEN_B].into_iter().enumerate() {
        assert_eq!(
            header(&handshakes[index].headers, "authorization"),
            format!("Bearer {token}")
        );
        assert_eq!(
            header(&handshakes[index].headers, "x-snapshot-source"),
            SOURCE
        );
        assert_eq!(
            header(&handshakes[index].headers, "x-snapshot-policy"),
            "final-public-policy"
        );
        assert_eq!(
            header(&handshakes[index].headers, "x-request-id"),
            format!("public-prepare-{}", index + 1)
        );
    }
    let sends = observations.socket_sends.lock().unwrap();
    assert_eq!(sends.len(), 2);
    for (send, token) in sends.iter().zip([TOKEN_A, TOKEN_B]) {
        assert_eq!(send.authorization, format!("Bearer {token}"));
        assert_eq!(send.body["model"], "snapshot-model");
        assert_eq!(send.body["generate"], false);
        assert!(
            send.body["input"]
                .to_string()
                .contains("public prepared input")
        );
        assert!(send.body.get("query_source").is_none());
        assert!(send.body.get("account_scope").is_none());
        assert!(send.body.get("previous_response_id").is_none());
    }
    assert!(observations.http_sends.lock().unwrap().is_empty());
    assert_eq!(observations.closes.load(Ordering::SeqCst), 1);
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
    drop(sends);
    drop(handshakes);
    tokio::time::timeout(WAIT, session.close())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn public_prepare_keeps_first_account_through_prewarm_and_rebinds_next_draft() {
    assert_public_prepare_prewarm_account_snapshot(false).await;
}

#[tokio::test]
async fn public_prepare_at_keeps_first_account_through_prewarm_and_rebinds_next_draft() {
    assert_public_prepare_prewarm_account_snapshot(true).await;
}

#[tokio::test]
async fn public_prepare_at_reuses_first_credentials_to_sign_final_http_policy() {
    const ACCESS_A: &str = "SYNTHETIC_PUBLIC_SNAPSHOT_A";
    const SECRET_A: &str = "synthetic-public-signing-secret-A";
    const ACCESS_B: &str = "SYNTHETIC_PUBLIC_SNAPSHOT_B";
    const SOURCE: &str = "public-signed-snapshot";
    let store = Arc::new(SnapshotStore::new(Credential::AwsSigV4 {
        access_key_id: ACCESS_A.into(),
        secret_access_key: SECRET_A.into(),
        session_token: None,
    }));
    let client = runtime(
        profile(
            ProtocolFamily::AnthropicMessages,
            AuthStrategy::AwsSigV4,
            false,
        ),
        store.clone(),
    );
    let transport = Arc::new(SignedHttpCapture::default());
    let mut request = LlmRequest::new("snapshot-model").with_user_text("public signed request");
    request.execution.query_source = Some(SOURCE.into());
    let inspection_time = std::time::UNIX_EPOCH + Duration::from_secs(1_440_938_160);
    let mut prepared = tokio::time::timeout(WAIT, client.prepare_at(&request, inspection_time))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    let inspection_authorization = sealed_auth(&prepared).to_owned();
    assert!(inspection_authorization.contains(&format!("Credential={ACCESS_A}/")));
    assert_eq!(
        prepared.provider_request.headers["x-amz-date"],
        "20150830T123600Z"
    );
    store.set(Credential::AwsSigV4 {
        access_key_id: ACCESS_B.into(),
        secret_access_key: "synthetic-public-signing-secret-B".into(),
        session_token: None,
    });
    prepared.provider_request.body_json["max_tokens"] = json!(23);
    // Replacing the body policy invalidates the public inspection's retained
    // byte image; seal must serialize and authenticate this final JSON body.
    prepared.provider_request.body_bytes = None;
    prepared
        .provider_request
        .headers
        .insert("X-Snapshot-Policy".into(), "public-final-policy".into());
    prepared
        .provider_request
        .headers
        .insert("X-Snapshot-Source".into(), SOURCE.into());
    tokio::time::timeout(WAIT, client.seal_prepared(&mut prepared))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    let final_authorization = sealed_auth(&prepared).to_owned();
    assert_ne!(final_authorization, inspection_authorization);
    assert!(final_authorization.contains(&format!("Credential={ACCESS_A}/")));
    assert_ne!(
        prepared.provider_request.headers["x-amz-date"], "20150830T123600Z",
        "the inspection clock must not freeze final dispatch signing"
    );
    let call = prepared.wire_call.take().unwrap();
    assert_eq!(call.profile().profile_name, "snapshot");
    assert_eq!(call.model().display_model, "snapshot-model");
    let dispatches = AtomicUsize::new(0);
    let received = tokio::time::timeout(
        WAIT,
        call.dispatch_once_using(transport.as_ref(), || {
            dispatches.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let collected = tokio::time::timeout(WAIT, received.collect())
        .await
        .unwrap()
        .unwrap();
    collected.decode().unwrap();
    tokio::time::timeout(WAIT, collected.finish())
        .await
        .unwrap();
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["max_tokens"], 23);
    assert!(body.get("query_source").is_none());
    assert_eq!(header(&sent.headers, "x-snapshot-source"), SOURCE);
    assert_eq!(
        header(&sent.headers, "x-snapshot-policy"),
        "public-final-policy"
    );
    assert_eq!(header(&sent.headers, "authorization"), final_authorization);
    let unsigned: BTreeMap<String, String> = sent
        .headers
        .iter()
        .filter(|(name, _)| {
            !["authorization", "x-amz-date", "x-amz-content-sha256"]
                .iter()
                .any(|excluded| name.eq_ignore_ascii_case(excluded))
        })
        .cloned()
        .collect();
    let expected = sdk::auth::sigv4::sign_request(
        &sent.method,
        &sent.url,
        &unsigned,
        &sent.body,
        ACCESS_A,
        SECRET_A,
        None,
        "us-east-1",
        "bedrock",
        header(&sent.headers, "x-amz-date"),
    )
    .unwrap();
    assert_eq!(final_authorization, expected.authorization);
    assert_eq!(
        header(&sent.headers, "x-amz-content-sha256"),
        expected.x_amz_content_sha256
    );
    assert_eq!(
        store.loads.load(Ordering::SeqCst),
        1,
        "final policy signing and actual HTTP dispatch must use the first public snapshot"
    );
    drop(requests);

    let next = tokio::time::timeout(WAIT, client.prepare_at(&request, inspection_time))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
    assert!(sealed_auth(&next).contains(&format!("Credential={ACCESS_B}/")));
}

#[tokio::test]
async fn chatgpt_host_seal_keeps_exact_text_and_applies_final_body_policy() {
    let store = Arc::new(SnapshotStore::new(Credential::ChatGptOAuth {
        access_token: "synthetic-chatgpt-access-token".into(),
        account_id: Some("synthetic-chatgpt-account".into()),
        fedramp: false,
    }));
    let client = runtime(
        profile(
            ProtocolFamily::OpenAiResponses,
            AuthStrategy::ChatGptOAuth,
            false,
        ),
        store.clone(),
    );
    let mut request = LlmRequest::new("snapshot-model").with_user_text("�");
    request.input.max_tokens = Some(96);
    request.input.temperature = Some(0.4);
    request
        .execution
        .message_json_string_overrides
        .insert("/messages/0/content/0/text".into(), vec![0xd800]);

    let mut prepared = tokio::time::timeout(
        WAIT,
        client.prepare_at(&request, std::time::SystemTime::UNIX_EPOCH),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    assert_eq!(prepared.provider_request.body_json["store"], false);
    assert_eq!(prepared.provider_request.body_json["instructions"], "");

    // Late host policy can add fields after the draft's first policy pass;
    // final sealing must reapply OAuth policy through semantic JSON.
    prepared.provider_request.body_json["instructions"] = json!("�");
    prepared.provider_request.body_json["metadata"] = json!({"annotation":"�"});
    prepared.provider_request.body_json["max_output_tokens"] = json!("discarded exact field");
    prepared.provider_request.body_json["temperature"] = json!(0.4);
    prepared.provider_request.body_json["top_p"] =
        json!({"annotation":"discarded exact descendant"});
    prepared
        .provider_request
        .json_string_overrides
        .insert("/instructions".into(), vec![0xd801]);
    prepared
        .provider_request
        .json_string_overrides
        .insert("/metadata/annotation".into(), vec![0xd802]);
    prepared
        .provider_request
        .json_string_overrides
        .insert("/max_output_tokens".into(), vec![0xd803]);
    prepared
        .provider_request
        .json_string_overrides
        .insert("/top_p/annotation".into(), vec![0xd804]);
    prepared.provider_request.body_bytes =
        Some(prepared.provider_request.wire_body_bytes().unwrap());
    tokio::time::timeout(WAIT, client.seal_prepared(&mut prepared))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
    assert_eq!(
        sealed_auth(&prepared),
        "Bearer synthetic-chatgpt-access-token"
    );
    assert!(
        prepared
            .provider_request
            .headers
            .iter()
            .any(
                |(name, value)| name.eq_ignore_ascii_case("chatgpt-account-id")
                    && value == "synthetic-chatgpt-account"
            )
    );
    let call = prepared.wire_call.as_ref().unwrap();
    let wire = std::str::from_utf8(&call.request().body).unwrap();
    assert!(wire.contains("\\ud800"), "{wire}");
    assert!(wire.contains("\"store\":false"), "{wire}");
    assert!(wire.contains("\"instructions\":\"\\ud801\""), "{wire}");
    assert!(wire.contains("\\ud802"), "{wire}");
    assert!(!wire.contains("\\ud803"), "{wire}");
    assert!(!wire.contains("\\ud804"), "{wire}");
    for removed in ["max_output_tokens", "temperature", "top_p"] {
        assert!(!wire.contains(removed), "{wire}");
    }
    assert!(
        serde_json::from_slice::<Value>(&call.request().body).is_err(),
        "the isolated surrogate must remain an exact UTF-16 escape, not be normalized through UTF-8 JSON"
    );
}
