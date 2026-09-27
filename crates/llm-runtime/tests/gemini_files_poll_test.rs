//! Task 7: `wait_for_file_active` — polling convenience over the Gemini File
//! API status endpoint (`files.get`).
//!
//! There is NO claude-code/codex ground truth for polling cadence; the
//! defaults (2s interval, 300s budget) are this crate's own choice. These
//! tests pin the loop semantics, not any vendored behavior. All tests run
//! with `start_paused` so tokio sleeps auto-advance and the suite is instant.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use llm_runtime::client::DefaultLlmClient;
use llm_runtime::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig, FileActivationPoll,
    LlmError, ModelProfile, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
    ProviderRequest, ProviderResponse, StreamingResponse, Transport,
};

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

// ── Scripted transport (same fixture style as gemini_files_test.rs) ───────────

#[derive(Debug)]
struct ScriptedTransport {
    responses: Mutex<VecDeque<ProviderResponse>>,
    seen: Mutex<Vec<ProviderRequest>>,
}

impl ScriptedTransport {
    fn returning(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<ProviderRequest> {
        self.seen.lock().expect("seen lock").clone()
    }
}

impl Transport for ScriptedTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.seen.lock().expect("seen lock").push(request.clone());
        let response = self.responses.lock().expect("responses lock").pop_front();
        Box::pin(async move {
            response.ok_or(LlmError::Transport {
                message: "no scripted response left".to_string(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "open_stream not scripted".to_string(),
            })
        })
    }
}

fn gemini_client() -> DefaultLlmClient {
    std::env::set_var("LLM_CLIENT_GEMINI_POLL_TEST_KEY", "gemini-poll-key");
    DefaultLlmClient::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::Gemini,
            profile_name: "gemini".to_string(),
            base_url: GEMINI_BASE.to_string(),
            protocol: ProtocolFamily::GeminiGenerateContent,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "LLM_CLIENT_GEMINI_POLL_TEST_KEY".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "Gemini Flash".to_string(),
                request_model: "gemini-2.0-flash".to_string(),
                billing_model: "gemini-2.0-flash".to_string(),
                aliases: vec!["gemini".to_string()],
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
            connection: Default::default(),
        }],
    })
    .expect("client")
}

fn anthropic_client() -> DefaultLlmClient {
    std::env::set_var("LLM_CLIENT_GEMINI_POLL_TEST_KEY", "gemini-poll-key");
    DefaultLlmClient::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "LLM_CLIENT_GEMINI_POLL_TEST_KEY".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "Claude".to_string(),
                request_model: "claude-sonnet-4-20250514".to_string(),
                billing_model: "claude-sonnet-4".to_string(),
                aliases: vec!["claude".to_string()],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities::default(),
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
}

/// files.get returns the File resource at the TOP level (not nested under
/// `"file"` like the upload response).
fn status_response(state: &str) -> ProviderResponse {
    ProviderResponse::json(
        200,
        serde_json::json!({
            "name": "files/abc",
            "uri": "https://generativelanguage.googleapis.com/v1beta/files/abc",
            "mimeType": "video/mp4",
            "state": state
        }),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn wait_for_file_active_polls_until_active() {
    let transport = ScriptedTransport::returning(vec![
        status_response("PROCESSING"),
        status_response("PROCESSING"),
        status_response("ACTIVE"),
    ]);
    let client = gemini_client();

    let file = client
        .wait_for_file_active(
            "gemini",
            "files/abc",
            &transport,
            FileActivationPoll::default(),
        )
        .await
        .expect("file becomes ACTIVE");

    assert_eq!(file.name, "files/abc");
    assert_eq!(
        file.uri,
        "https://generativelanguage.googleapis.com/v1beta/files/abc"
    );
    assert_eq!(file.state, "ACTIVE");

    let seen = transport.seen();
    assert_eq!(seen.len(), 3, "exactly one request per scripted status");
    for request in &seen {
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.url,
            "https://generativelanguage.googleapis.com/v1beta/files/abc"
        );
        // Authenticated through the same path as upload_file.
        assert_eq!(
            request.headers.get("x-goog-api-key").map(String::as_str),
            Some("gemini-poll-key")
        );
        assert_eq!(request.body_json, serde_json::Value::Null);
        assert_eq!(request.body_bytes, None);
    }
}

#[tokio::test(start_paused = true)]
async fn wait_for_file_active_fails_fast_on_failed_state() {
    let transport = ScriptedTransport::returning(vec![
        status_response("PROCESSING"),
        status_response("FAILED"),
    ]);
    let client = gemini_client();

    let error = client
        .wait_for_file_active(
            "gemini",
            "files/abc",
            &transport,
            FileActivationPoll::default(),
        )
        .await
        .expect_err("FAILED must error");
    assert!(matches!(
        error,
        LlmError::InvalidRequest { ref message }
            if message == "gemini file processing failed: files/abc"
    ));
    assert_eq!(transport.seen().len(), 2, "stops on the FAILED response");
}

/// Loop semantics under {interval: 2s, max_wait: 10s}: requests fire at
/// t = 0, 2, 4, 6, 8, 10 (the poll at exactly the deadline is still within
/// budget — the loop only gives up when `now + interval` would EXCEED the
/// deadline), then the budget check fails at t = 10 → 6 requests total.
#[tokio::test(start_paused = true)]
async fn wait_for_file_active_times_out() {
    let transport =
        ScriptedTransport::returning((0..10).map(|_| status_response("PROCESSING")).collect());
    let client = gemini_client();
    let poll = FileActivationPoll {
        interval: Duration::from_secs(2),
        max_wait: Duration::from_secs(10),
    };

    let error = client
        .wait_for_file_active("gemini", "files/abc", &transport, poll)
        .await
        .expect_err("budget exhaustion must error");
    assert!(matches!(
        error,
        LlmError::Transport { ref message }
            if message == "gemini file did not become ACTIVE within 10s"
    ));
    assert_eq!(transport.seen().len(), 6, "polls at t=0,2,4,6,8,10");
}

#[tokio::test(start_paused = true)]
async fn wait_for_file_active_rejects_non_gemini_family() {
    let transport = ScriptedTransport::returning(vec![]);
    let client = anthropic_client();

    let error = client
        .wait_for_file_active(
            "claude",
            "files/abc",
            &transport,
            FileActivationPoll::default(),
        )
        .await
        .expect_err("must reject");
    // Same family-guard shape as upload_file.
    assert!(matches!(
        error,
        LlmError::InvalidRequest { ref message }
            if message == "file upload requires a gemini provider profile"
    ));
    assert!(transport.seen().is_empty(), "no request may be sent");
}

/// Tolerant-decoder convention: an unknown state string is neither success
/// nor failure — the loop keeps polling until the budget runs out.
#[tokio::test(start_paused = true)]
async fn wait_for_file_active_unknown_state_keeps_polling() {
    let transport = ScriptedTransport::returning(vec![
        status_response("PROCESSING"),
        status_response("SOMETHING_NEW"),
        status_response("ACTIVE"),
    ]);
    let client = gemini_client();

    let file = client
        .wait_for_file_active(
            "gemini",
            "files/abc",
            &transport,
            FileActivationPoll::default(),
        )
        .await
        .expect("unknown state keeps polling");
    assert_eq!(file.state, "ACTIVE");
    assert_eq!(transport.seen().len(), 3);
}
