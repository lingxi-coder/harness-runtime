use super::*;
use crate::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, Credential, CredentialConfig,
    CredentialProvider, CredentialScope, ModelProfile, ModelRuntime, PricingConfig, ProtocolFamily,
    ProviderId, ProviderProfile, SigningConfig,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Debug)]
struct RotatingCredentials {
    current: Mutex<Credential>,
    loads: AtomicUsize,
}

impl RotatingCredentials {
    fn new(credential: Credential) -> Self {
        Self {
            current: Mutex::new(credential),
            loads: AtomicUsize::new(0),
        }
    }
    fn set(&self, credential: Credential) {
        *self.current.lock().unwrap() = credential;
    }
}

impl CredentialProvider for RotatingCredentials {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        assert_eq!(scope.profile_name, "p");
        self.loads.fetch_add(1, Ordering::SeqCst);
        let value = self.current.lock().unwrap().clone();
        Box::pin(async move { Ok(value) })
    }
}

fn runtime(auth: AuthStrategy, credential: CredentialConfig) -> ModelRuntime {
    ModelRuntime::from_config(runtime_config(auth, credential)).unwrap()
}
fn runtime_config(auth: AuthStrategy, credential: CredentialConfig) -> ClientConfig {
    let aws = auth == AuthStrategy::AwsSigV4;
    ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: wire::Region::all(),
            provider_id: if aws {
                ProviderId::BedrockClaude
            } else {
                ProviderId::OpenAI
            },
            profile_name: "p".into(),
            base_url: if aws {
                "https://bedrock-runtime.us-east-1.amazonaws.com"
            } else {
                "https://api.openai.com/v1"
            }
            .into(),
            protocol: if aws {
                ProtocolFamily::AnthropicMessages
            } else {
                ProtocolFamily::OpenAiResponses
            },
            auth,
            credential,
            models: vec![ModelProfile {
                display_model: "model".into(),
                request_model: "model".into(),
                billing_model: "model".into(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: aws.then(|| SigningConfig {
                region: "us-east-1".into(),
                service: "bedrock".into(),
            }),
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    }
}

fn snapshot(profile: &str) -> RequestCredentialSnapshot {
    RequestCredentialSnapshot {
        profile: profile.into(),
        credential: tokio::sync::OnceCell::new(),
    }
}

fn request(method: &str, body: &'static [u8]) -> sdk::HttpRequest {
    sdk::HttpRequest {
        http1_header_layout: None,
        method: method.into(),
        url: "https://api.openai.com/v1/responses".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: body.into(),
        timeout: None,
    }
}

fn header<'a>(request: &'a sdk::HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn draft_snapshot_keeps_identity_while_next_draft_and_live_operations_refresh() {
    let store = Arc::new(RotatingCredentials::new(Credential::BearerToken(
        "A".into(),
    )));
    let client = runtime(
        AuthStrategy::Bearer,
        CredentialConfig::HostManaged {
            id: "credential".into(),
        },
    )
    .with_credential_provider(store.clone());
    let first = snapshot("p");
    for token in ["A", "B"] {
        store.set(Credential::BearerToken(token.into()));
        let mut wire = request("POST", b"{}");
        client
            .authenticate_wire("p", &mut wire, UNIX_EPOCH, Some(&first))
            .await
            .unwrap();
        assert_eq!(header(&wire, "authorization"), Some("Bearer A"));
    }
    let mut next = request("POST", b"{}");
    client
        .authenticate_wire("p", &mut next, UNIX_EPOCH, Some(&snapshot("p")))
        .await
        .unwrap();
    assert_eq!(header(&next, "authorization"), Some("Bearer B"));
    for token in ["C", "D"] {
        store.set(Credential::BearerToken(token.into()));
        let mut wire = request("POST", b"{}");
        client
            .authenticate_wire("p", &mut wire, UNIX_EPOCH, None)
            .await
            .unwrap();
        assert_eq!(
            header(&wire, "authorization"),
            Some(format!("Bearer {token}").as_str())
        );
    }
    assert_eq!(store.loads.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn snapshot_rejects_another_profile_before_loading_material() {
    let store = Arc::new(RotatingCredentials::new(Credential::BearerToken(
        "A".into(),
    )));
    let client = runtime(
        AuthStrategy::Bearer,
        CredentialConfig::HostManaged {
            id: "credential".into(),
        },
    )
    .with_credential_provider(store.clone());
    let error = client
        .authenticate_wire(
            "p",
            &mut request("POST", b"{}"),
            UNIX_EPOCH,
            Some(&snapshot("other")),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Authentication { .. }));
    assert_eq!(store.loads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_material_does_not_become_a_global_negative_cache() {
    let anonymous = runtime(AuthStrategy::Bearer, CredentialConfig::None);
    let missing = snapshot("p");
    for _ in 0..2 {
        let mut wire = request("POST", b"{}");
        anonymous
            .authenticate_wire("p", &mut wire, UNIX_EPOCH, Some(&missing))
            .await
            .unwrap();
        assert_eq!(header(&wire, "authorization"), None);
    }
    let client = runtime(
        AuthStrategy::Bearer,
        CredentialConfig::HostManaged {
            id: "credential".into(),
        },
    )
    .with_credential_provider(Arc::new(RotatingCredentials::new(Credential::BearerToken(
        "B".into(),
    ))));
    let mut wire = request("POST", b"{}");
    client
        .authenticate_wire("p", &mut wire, UNIX_EPOCH, Some(&snapshot("p")))
        .await
        .unwrap();
    assert_eq!(header(&wire, "authorization"), Some("Bearer B"));
}

#[tokio::test]
async fn frozen_sigv4_material_signs_each_current_method_body_and_timestamp() {
    let store = Arc::new(RotatingCredentials::new(Credential::AwsSigV4 {
        access_key_id: "AKIDA".into(),
        secret_access_key: "secret-A".into(),
        session_token: None,
    }));
    let client = runtime(
        AuthStrategy::AwsSigV4,
        CredentialConfig::HostManaged {
            id: "credential".into(),
        },
    )
    .with_credential_provider(store.clone());
    let material = snapshot("p");
    let first_time = UNIX_EPOCH + Duration::from_secs(1_440_938_160);
    let mut handshake = request("GET", b"");
    client
        .authenticate_wire("p", &mut handshake, first_time, Some(&material))
        .await
        .unwrap();
    store.set(Credential::AwsSigV4 {
        access_key_id: "AKIDB".into(),
        secret_access_key: "secret-B".into(),
        session_token: None,
    });
    let mut final_request = request("POST", br#"{"model":"model","messages":[]}"#);
    client
        .authenticate_wire(
            "p",
            &mut final_request,
            first_time + Duration::from_secs(1),
            Some(&material),
        )
        .await
        .unwrap();
    let unsigned: std::collections::BTreeMap<String, String> = final_request
        .headers
        .iter()
        .filter(|(key, _)| {
            !["authorization", "x-amz-date", "x-amz-content-sha256"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect();
    let expected = sdk::auth::sigv4::sign_request(
        &final_request.method,
        &final_request.url,
        &unsigned,
        &final_request.body,
        "AKIDA",
        "secret-A",
        None,
        "us-east-1",
        "bedrock",
        header(&final_request, "x-amz-date").unwrap(),
    )
    .unwrap();
    assert_eq!(
        header(&final_request, "authorization"),
        Some(expected.authorization.as_str())
    );
    assert_eq!(
        header(&final_request, "x-amz-content-sha256"),
        Some(expected.x_amz_content_sha256.as_str())
    );
    assert_ne!(
        header(&handshake, "authorization"),
        header(&final_request, "authorization")
    );
    assert_ne!(
        header(&handshake, "x-amz-date"),
        header(&final_request, "x-amz-date")
    );
    assert_eq!(store.loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn logical_request_retains_oauth_across_retry_drafts_and_next_turn_reads_host_rotation() {
    use crate::auth::anthropic::environment::{
        EnvironmentLookup, EnvironmentOAuthCredentialProvider,
    };
    let token = Arc::new(Mutex::new("first".to_owned()));
    let read = token.clone();
    let provider = Arc::new(EnvironmentOAuthCredentialProvider::live(
        EnvironmentLookup::new(move |key| {
            (key == "CLAUDE_CODE_OAUTH_TOKEN").then(|| read.lock().unwrap().clone())
        }),
        None,
    ));
    let mut config = runtime_config(
        AuthStrategy::Bearer,
        CredentialConfig::HostManaged {
            id: "anthropic-oauth".into(),
        },
    );
    config.providers[0].provider_id = ProviderId::AnthropicFirstParty;
    config.providers[0].protocol = ProtocolFamily::AnthropicMessages;
    config.providers[0].auth = AuthStrategy::OAuthBearer;
    let client = ModelRuntime::from_config(config)
        .unwrap()
        .with_credential_provider(provider);
    let logical = crate::RequestCredentials::default();
    let draft = || {
        HostAuthenticator::for_request(client.clone(), "p".into(), None, Arc::new(Mutex::new(None)))
            .with_request_credentials(Some(&logical))
    };
    assert!(
        matches!(draft().captured_credential().await.unwrap(),Some(Credential::AnthropicOAuth {access_token,..}) if access_token=="first")
    );
    *token.lock().unwrap() = "second".into();
    assert!(
        matches!(draft().captured_credential().await.unwrap(),Some(Credential::AnthropicOAuth {access_token,..}) if access_token=="first")
    );
    let next = HostAuthenticator::for_request(client, "p".into(), None, Arc::new(Mutex::new(None)))
        .with_request_credentials(Some(&crate::RequestCredentials::default()));
    assert!(
        matches!(next.captured_credential().await.unwrap(),Some(Credential::AnthropicOAuth {access_token,..}) if access_token=="second")
    );
}
