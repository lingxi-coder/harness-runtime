//! Regression tests for provider services.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use bytes::Bytes;
use futures::{stream, StreamExt};
use llm_runtime::{
    services::{
        providers::{
            anthropic::{skills, types::AnthropicSkillScope},
            openai::audio,
            AnthropicClient, OpenAiClient,
        },
        sdk, ProviderServices,
    },
    BoxFuture, LlmError, ProviderRequest, ProviderResponse, StreamingResponse,
};
use serde_json::json;

struct Sent {
    url: String,
    headers: Vec<(String, String)>,
    body: Bytes,
    chunks: usize,
    content_length: Option<u64>,
}

#[derive(Default)]
struct HostTransport {
    sent: Mutex<Vec<Sent>>,
}

impl HostTransport {
    fn respond(&self, sent: Sent) -> sdk::StreamResponse {
        let body = if sent.url.ends_with("/audio/transcriptions") {
            Bytes::from_static(br#"{"text":"transcribed","language":"en"}"#)
        } else if sent.url.ends_with("/audio/speech") {
            Bytes::from_static(&[0, 255, 13, 0, 128])
        } else if sent.url.contains("/v1/skills?") {
            Bytes::from_static(br#"{"data":[],"next_page":null}"#)
        } else if sent.url.contains("/v1/files?") {
            Bytes::from_static(
                br#"{"data":[{"id":"file-1","filename":"report.pdf","mime_type":"application/pdf"}],"next_page":null}"#,
            )
        } else {
            panic!("unexpected service URL: {}", sent.url);
        };
        self.sent.lock().unwrap().push(sent);
        sdk::StreamResponse {
            status: 200,
            headers: vec![("x-request-id".into(), "service-request".into())],
            body: stream::once(async move { Ok(body) }).boxed(),
        }
    }
}

impl llm_runtime::test_support::FixtureTransport for HostTransport {
    fn execute<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async { panic!("independent services must preserve raw response bytes") })
    }

    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async { panic!("independent services must use the raw transport") })
    }

    fn send_raw(
        &self,
        request: sdk::HttpRequest,
    ) -> BoxFuture<'_, Result<sdk::StreamResponse, sdk::protocol::LlmError>> {
        Box::pin(async move {
            Ok(self.respond(Sent {
                url: request.url,
                headers: request.headers,
                body: request.body,
                chunks: 1,
                content_length: None,
            }))
        })
    }

    fn send_stream_raw(
        &self,
        request: sdk::HttpStreamRequest,
    ) -> BoxFuture<'_, Result<sdk::StreamResponse, sdk::protocol::LlmError>> {
        Box::pin(async move {
            let mut input = request.body;
            let mut bytes = Vec::new();
            let mut chunks = 0;
            while let Some(chunk) = input.next().await {
                chunks += 1;
                bytes.extend_from_slice(&chunk?);
            }
            Ok(self.respond(Sent {
                url: request.url,
                headers: request.headers,
                body: bytes.into(),
                chunks,
                content_length: Some(request.content_length),
            }))
        })
    }
}
llm_runtime::impl_fixture_transport!(HostTransport);

fn profiles() -> Vec<sdk::protocol::ProviderProfile> {
    serde_json::from_value(json!([
        {
            "provider_id":"openai", "profile_name":"speech", "auth":"api_key",
            "base_url":"https://api.openai.com/v1", "protocol":"open_ai_responses",
            "regions":["international"],
            "audio":{"mode":"enabled","value":{
                "transcriptions_endpoint":"https://api.openai.com/v1/audio/transcriptions",
                "translations_endpoint":"https://api.openai.com/v1/audio/translations",
                "speech_endpoint":"https://api.openai.com/v1/audio/speech",
                "auth":{"type":"bearer"}
            }}
        },
        {
            "provider_id":"anthropic", "profile_name":"resources", "auth":"api_key",
            "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages",
            "regions":["international"]
        }
    ]))
    .unwrap()
}

fn options(key: &str, account: &str) -> sdk::RequestOptions {
    sdk::RequestOptions {
        credential: Some(sdk::protocol::Secret::new(key.into())),
        account_scope: Some(account.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn shared_services_preserve_audio_streams_and_request_scoped_credentials() {
    let transport = Arc::new(HostTransport::default());
    let services = ProviderServices::with_transport(
        &profiles(),
        sdk::protocol::Region::International,
        transport.clone(),
    )
    .unwrap();
    let input = audio::AudioInput {
        filename: "voice.wav".into(),
        media_type: "audio/wav".into(),
        size_bytes: 4,
        body: stream::iter([
            Ok(Bytes::from_static(&[0, 255])),
            Ok(Bytes::from_static(&[1, 2])),
        ])
        .boxed(),
    };
    let openai = services
        .client()
        .provider::<OpenAiClient>("speech")
        .unwrap();
    let transcript = openai
        .audio()
        .transcribe(
            input,
            &audio::TranscriptionRequest::new(audio::TranscriptionModel::Whisper1),
            &options("first-key", "account-one"),
        )
        .await
        .unwrap();
    assert_eq!(transcript.text, "transcribed");

    let clone = services.clone();
    let openai_clone = clone.client().provider::<OpenAiClient>("speech").unwrap();
    let mut speech = openai_clone
        .audio()
        .synthesize(
            &audio::SpeechRequest {
                model: audio::SpeechModel::Tts1,
                input: "hello".into(),
                voice: audio::SpeechVoice::Alloy,
                format: audio::SpeechFormat::Wav,
                instructions: None,
                speed: None,
            },
            &options("second-key", "account-two"),
        )
        .await
        .unwrap();
    assert_eq!(
        speech.next_chunk().await.unwrap().unwrap().as_ref(),
        &[0, 255, 13, 0, 128]
    );
    assert!(speech.next_chunk().await.unwrap().is_none());
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[0].chunks >= 4,
        "multipart prefix, input chunks and suffix stay streamed"
    );
    assert_eq!(sent[0].content_length, Some(sent[0].body.len() as u64));
    assert!(sent[0].body.windows(4).any(|bytes| bytes == [0, 255, 1, 2]));
    for (request, key) in sent.iter().zip(["first-key", "second-key"]) {
        assert!(request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == &format!("Bearer {key}")
        }));
    }
}

#[tokio::test]
async fn file_references_keep_account_scope_and_reject_cross_account_reuse() {
    let transport = Arc::new(HostTransport::default());
    let services = ProviderServices::with_transport(
        &profiles(),
        sdk::protocol::Region::International,
        transport.clone(),
    )
    .unwrap();
    let snapshot = services.snapshot();
    let first = sdk::RequestOptions {
        file_account_scope: Some("files-account".into()),
        ..options("first-key", "conversation-account")
    };
    let anthropic = snapshot.provider::<AnthropicClient>("resources").unwrap();
    let files = anthropic.files();
    let page = files.list(None, &first).await.unwrap();
    let reference = &page.files[0].file;
    assert_eq!(reference.account_scope.as_deref(), Some("files-account"));
    let second = options("second-key", "another-account");
    assert!(files.get(reference, &second).await.is_err());
    assert_eq!(transport.sent.lock().unwrap().len(), 1);
    assert!(files
        .list(None, &sdk::RequestOptions::default())
        .await
        .is_err());
}

#[test]
fn file_services_enforce_selected_region_before_io() {
    let transport = Arc::new(HostTransport::default());
    let services = ProviderServices::with_transport(
        &profiles(),
        sdk::protocol::Region::ChinaMainland,
        transport.clone(),
    )
    .unwrap();
    assert!(services
        .snapshot()
        .provider::<AnthropicClient>("resources")
        .is_err());
    assert!(transport.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn remote_skills_reuse_host_transport_with_explicit_workspace_scope() {
    let transport = Arc::new(HostTransport::default());
    let services = ProviderServices::with_transport(
        &profiles(),
        sdk::protocol::Region::International,
        transport.clone(),
    )
    .unwrap();
    let scope = AnthropicSkillScope::new(
        "resources",
        "https://api.anthropic.com",
        "account-workspace",
    )
    .unwrap()
    .with_workspace_id("wrkspc_test")
    .unwrap();
    let anthropic = services
        .client()
        .provider::<AnthropicClient>("resources")
        .unwrap();
    let skills = anthropic.skills(scope).unwrap();
    let page = skills
        .list(
            &skills::AnthropicSkillListOptions::default(),
            &options("skills-key", "account-workspace"),
        )
        .await
        .unwrap();
    assert!(page.skills.is_empty());
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("anthropic-workspace-id") && value == "wrkspc_test"
    }));
}

#[tokio::test]
async fn file_service_honors_request_options_total_timeout() {
    struct HangingTransport(Mutex<Option<std::time::Duration>>);

    #[async_trait::async_trait]
    impl sdk::Transport for HangingTransport {
        async fn send(
            &self,
            request: sdk::HttpRequest,
        ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
            *self.0.lock().unwrap() = request.timeout;
            std::future::pending().await
        }
    }

    let transport = Arc::new(HangingTransport(Mutex::new(None)));
    let services = ProviderServices::with_transport(
        &profiles(),
        sdk::protocol::Region::International,
        transport.clone(),
    )
    .unwrap();
    let snapshot = services.snapshot();
    let timeout = std::time::Duration::from_millis(10);
    let options = sdk::RequestOptions {
        total_timeout: Some(timeout),
        ..options("key", "account")
    };
    let anthropic = snapshot.provider::<AnthropicClient>("resources").unwrap();
    let files = anthropic.files();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        files.list(None, &options),
    )
    .await;
    assert!(matches!(
        result,
        Ok(Err(sdk::protocol::LlmError::TransportTimeout { .. }))
    ));
    assert!(transport
        .0
        .lock()
        .unwrap()
        .is_some_and(|wire_timeout| wire_timeout <= timeout));
}

#[derive(Debug, Default)]
struct HostCredentials {
    loads: AtomicUsize,
}

impl llm_runtime::CredentialProvider for HostCredentials {
    fn load<'a>(
        &'a self,
        _: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(llm_runtime::Credential::ApiKey("host-key".into())) })
    }
}

fn host_configured_services(
    auth: llm_runtime::AuthStrategy,
    transport: Arc<HostTransport>,
    credentials: Arc<HostCredentials>,
) -> ProviderServices {
    let mut profile: llm_runtime::ProviderProfile = serde_json::from_value(json!({
        "provider_id": llm_runtime::ProviderId::AnthropicFirstParty,
        "profile_name": "resources",
        "base_url": "https://api.anthropic.com",
        "protocol": "anthropic_messages",
        "auth": auth,
        "credential": {"type": "host_managed", "id": "host-account"},
        "models": [{"display_model":"test-model", "request_model":"test-model", "billing_model":"test-model"}]
    }))
    .unwrap();
    profile.wire_profile = Some(profiles().remove(1));
    llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: vec![profile],
    })
    .unwrap()
    .with_credential_provider(credentials)
    .provider_services(sdk::protocol::Region::International, transport)
    .unwrap()
}

fn header<'a>(request: &'a Sent, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
}

#[tokio::test]
async fn host_configured_files_honor_explicit_credentials_and_matching_account_scopes() {
    use llm_runtime::AuthStrategy;

    for auth in [
        AuthStrategy::ApiKey,
        AuthStrategy::Bearer,
        AuthStrategy::OAuthBearer,
        AuthStrategy::GcpToken,
    ] {
        let transport = Arc::new(HostTransport::default());
        let credentials = Arc::new(HostCredentials::default());
        let services = host_configured_services(auth, transport.clone(), credentials.clone());
        let anthropic = services
            .client()
            .provider::<AnthropicClient>("resources")
            .unwrap();
        let files = anthropic.files();

        for (key, account) in [
            ("request-key-b", "files-account-b"),
            ("request-key-c", "files-account-c"),
        ] {
            let request_options = sdk::RequestOptions {
                file_account_scope: Some(account.into()),
                ..options(key, "conversation-account")
            };
            let page = files.list(None, &request_options).await.unwrap();
            assert_eq!(page.files[0].file.account_scope.as_deref(), Some(account));
        }
        assert_eq!(
            credentials.loads.load(Ordering::SeqCst),
            0,
            "explicit credentials must not access the host store"
        );

        let page = files
            .list(
                None,
                &sdk::RequestOptions {
                    account_scope: Some("host-account".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.files[0].file.account_scope.as_deref(),
            Some("host-account")
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 1);

        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        for (request, key) in sent
            .iter()
            .zip(["request-key-b", "request-key-c", "host-key"])
        {
            if auth == AuthStrategy::ApiKey {
                assert_eq!(header(request, "x-api-key"), Some(key));
                assert_eq!(header(request, "authorization"), None);
            } else {
                assert_eq!(
                    header(request, "authorization"),
                    Some(format!("Bearer {key}").as_str())
                );
                assert_eq!(header(request, "x-api-key"), None);
            }
            if auth == AuthStrategy::OAuthBearer {
                let betas = header(request, "anthropic-beta").unwrap();
                assert!(betas
                    .split(',')
                    .any(|beta| beta.trim() == "oauth-2025-04-20"));
                assert!(
                    !betas.contains("files-api-"),
                    "the GA Files API needs no retired beta"
                );
            }
        }
    }
}

#[tokio::test]
async fn host_configured_files_reject_unsupported_explicit_credential_overrides_before_io() {
    use llm_runtime::AuthStrategy;

    for auth in [
        AuthStrategy::CopilotBearer,
        AuthStrategy::ChatGptOAuth,
        AuthStrategy::AwsSigV4,
        AuthStrategy::AzureToken,
    ] {
        let transport = Arc::new(HostTransport::default());
        let credentials = Arc::new(HostCredentials::default());
        let services = host_configured_services(auth, transport.clone(), credentials.clone());
        let anthropic = services
            .client()
            .provider::<AnthropicClient>("resources")
            .unwrap();
        let error = anthropic
            .files()
            .list(None, &options("explicit-other-account", "other-account"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, sdk::protocol::LlmError::UnsupportedCapability { message }
            if message.contains("explicit service credentials are unsupported"))
        );
        assert_eq!(credentials.loads.load(Ordering::SeqCst), 0);
        assert!(transport.sent.lock().unwrap().is_empty());
    }
}

#[derive(Debug, Default)]
struct ConfiguredCredentials(Mutex<Vec<llm_runtime::CredentialScope>>);
impl llm_runtime::CredentialProvider for ConfiguredCredentials {
    fn load<'a>(
        &'a self,
        scope: &'a llm_runtime::CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::Credential, LlmError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(scope.clone());
            match scope.credential_id.as_deref() {
                Some("original-static") => Ok(llm_runtime::Credential::ApiKey("static-key".into())),
                Some("original-managed") => {
                    Ok(llm_runtime::Credential::BearerToken("managed-token".into()))
                }
                _ => Err(LlmError::Authentication {
                    message: "wrong configured scope".into(),
                }),
            }
        })
    }
}
fn credential_profile(
    name: &str,
    credential: llm_runtime::CredentialConfig,
) -> llm_runtime::ProviderProfile {
    let mut profile = llm_runtime::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "openai")
        .unwrap();
    profile.profile_name = name.into();
    profile.credential = credential;
    profile
}
#[tokio::test]
async fn bound_services_capture_original_static_and_managed_credential_scopes() {
    let credentials = Arc::new(ConfiguredCredentials::default());
    let mut managed = credential_profile(
        "effective-managed",
        llm_runtime::CredentialConfig::HostManaged {
            id: "original-managed".into(),
        },
    );
    managed.auth = llm_runtime::AuthStrategy::Bearer;
    let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: vec![
            credential_profile(
                "effective-static",
                llm_runtime::CredentialConfig::Static {
                    id: "original-static".into(),
                },
            ),
            managed,
        ],
    })
    .unwrap()
    .with_credential_provider(credentials.clone());
    let transport = Arc::new(HostTransport::default());
    let services = client
        .provider_services(sdk::protocol::Region::International, transport.clone())
        .unwrap();
    assert_eq!(
        services
            .service_credential("effective-static")
            .await
            .unwrap()
            .expose_secret(),
        "static-key"
    );
    assert_eq!(
        services
            .service_credential("effective-managed")
            .await
            .unwrap()
            .expose_secret(),
        "managed-token"
    );
    assert!(matches!(
        services.service_credential("other-profile").await,
        Err(LlmError::ModelUnavailable)
    ));
    let scopes = credentials.0.lock().unwrap().clone();
    assert_eq!(
        scopes
            .into_iter()
            .map(|scope| (scope.profile_name, scope.credential_id))
            .collect::<Vec<_>>(),
        vec![
            ("effective-static".into(), Some("original-static".into())),
            ("effective-managed".into(), Some("original-managed".into())),
        ]
    );
    assert!(transport.sent.lock().unwrap().is_empty());
}
#[test]
fn configured_service_credentials_load_original_env_in_child() {
    const MARKER: &str = "LINGXI_SERVICE_CREDENTIAL_TEST_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "configured_service_credentials_load_original_env_in_child",
            ])
            .env(MARKER, "1")
            .env("LINGXI_EXACT_AUDIO_CREDENTIAL_SOURCE", "configured-only")
            .env("OPENAI_API_KEY", "wrong-unconfigured-default")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated credential test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
            providers: vec![credential_profile(
                "effective-env",
                llm_runtime::CredentialConfig::Env {
                    var: "LINGXI_EXACT_AUDIO_CREDENTIAL_SOURCE".into(),
                },
            )],
        })
        .unwrap();
        let transport = Arc::new(HostTransport::default());
        let services = client
            .provider_services(sdk::protocol::Region::International, transport.clone())
            .unwrap();
        assert_eq!(
            services
                .service_credential("effective-env")
                .await
                .unwrap()
                .expose_secret(),
            "configured-only"
        );
        assert!(transport.sent.lock().unwrap().is_empty());
    });
}
#[tokio::test]
async fn service_tokens_reject_account_specific_auth_and_unconfigured_sdk_sources() {
    let mut profile = credential_profile(
        "account-auth",
        llm_runtime::CredentialConfig::HostManaged {
            id: "account".into(),
        },
    );
    profile.auth = llm_runtime::AuthStrategy::ChatGptPlan;
    let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: vec![profile],
    })
    .unwrap();
    assert!(matches!(
        client.service_credential("account-auth").await,
        Err(LlmError::UnsupportedCapability { .. })
    ));
    let services = ProviderServices::with_transport(
        &profiles(),
        sdk::protocol::Region::International,
        Arc::new(HostTransport::default()),
    )
    .unwrap();
    assert!(matches!(
        services.service_credential("speech").await,
        Err(LlmError::UnsupportedCapability { .. })
    ));
}
