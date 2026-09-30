use crate::upstream::codec_fixtures::HistoryFixture;
use crate::upstream::codec_fixtures::{FixtureCodec, GeminiCodec};
// Task 6: Gemini File API upload flow — pure builders/parsers + client driver.
//
// No vendored reference exists; the wire shapes follow the documented Google
// resumable-upload protocol and every header byte is pinned here.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::gemini_files_fixture::{
    file_status_request, parse_file_status, parse_start_response, parse_upload_response,
    start_upload_request, upload_finalize_request, GeminiFile,
};
use llm_runtime::client::ModelRuntime;

use llm_runtime::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, ContentBlock, CredentialConfig, LlmError,
    Message, ModelProfile, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile,
    ProviderRequest, ProviderResponse, StreamingResponse,
};

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

// ── start_upload_request ──────────────────────────────────────────────────────

#[test]
fn start_upload_request_pins_url_headers_and_body_for_v1beta_base() {
    let request = start_upload_request(GEMINI_BASE, 3, "image/png", "my file");

    assert_eq!(request.method, "POST");
    assert_eq!(
        request.url,
        "https://generativelanguage.googleapis.com/upload/v1beta/files"
    );

    let mut expected_headers = BTreeMap::new();
    expected_headers.insert(
        "x-goog-upload-protocol".to_string(),
        "resumable".to_string(),
    );
    expected_headers.insert("x-goog-upload-command".to_string(), "start".to_string());
    expected_headers.insert(
        "x-goog-upload-header-content-length".to_string(),
        "3".to_string(),
    );
    expected_headers.insert(
        "x-goog-upload-header-content-type".to_string(),
        "image/png".to_string(),
    );
    expected_headers.insert("content-type".to_string(), "application/json".to_string());
    assert_eq!(request.headers, expected_headers);

    assert_eq!(
        request.body_json,
        serde_json::json!({"file": {"display_name": "my file"}})
    );
    assert_eq!(request.body_bytes, None);
}

#[test]
fn start_upload_request_strips_trailing_slash_then_version_segment() {
    let request = start_upload_request(
        "https://generativelanguage.googleapis.com/v1beta/",
        1,
        "image/png",
        "f",
    );
    assert_eq!(
        request.url,
        "https://generativelanguage.googleapis.com/upload/v1beta/files"
    );
}

#[test]
fn start_upload_request_strips_trailing_v1_segment() {
    let request = start_upload_request(
        "https://generativelanguage.googleapis.com/v1",
        1,
        "image/png",
        "f",
    );
    assert_eq!(
        request.url,
        "https://generativelanguage.googleapis.com/upload/v1beta/files"
    );
}

/// Pinned edge: a base URL without a trailing `/v1beta` or `/v1` version
/// segment is used as-is — the upload path is appended to the full base.
#[test]
fn start_upload_request_uses_non_versioned_base_as_is() {
    let request = start_upload_request("https://example.com/proxy", 1, "image/png", "f");
    assert_eq!(request.url, "https://example.com/proxy/upload/v1beta/files");
}

// ── parse_start_response ──────────────────────────────────────────────────────

#[test]
fn parse_start_response_extracts_upload_url() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "x-goog-upload-url".to_string(),
        "https://generativelanguage.googleapis.com/upload/v1beta/files?upload_id=abc".to_string(),
    );
    let url = parse_start_response(&headers).expect("upload url");
    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/upload/v1beta/files?upload_id=abc"
    );
}

#[test]
fn parse_start_response_matches_header_name_case_insensitively() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "X-Goog-Upload-URL".to_string(),
        "https://upload.example/u?id=1".to_string(),
    );
    let url = parse_start_response(&headers).expect("upload url");
    assert_eq!(url, "https://upload.example/u?id=1");
}

#[test]
fn parse_start_response_missing_header_errors_without_echoing_values() {
    let mut headers = BTreeMap::new();
    headers.insert("x-goog-api-key".to_string(), "SECRET-VALUE-123".to_string());
    headers.insert("content-type".to_string(), "application/json".to_string());

    let error = parse_start_response(&headers).expect_err("must error");
    let LlmError::InvalidRequest { message } = error else {
        panic!("expected InvalidRequest, got {error:?}");
    };
    assert_eq!(
        message,
        "Gemini upload start response missing x-goog-upload-url header"
    );
    // No-secret rule: the error must not echo any header values.
    assert!(!message.contains("SECRET-VALUE-123"));
    assert!(!message.contains("application/json"));
}

// ── upload_finalize_request ───────────────────────────────────────────────────

#[test]
fn upload_finalize_request_pins_headers_and_raw_body() {
    let request = upload_finalize_request("https://upload.example/u?id=1", vec![1, 2, 3]);

    assert_eq!(request.method, "POST");
    assert_eq!(request.url, "https://upload.example/u?id=1");

    let mut expected_headers = BTreeMap::new();
    expected_headers.insert(
        "x-goog-upload-command".to_string(),
        "upload, finalize".to_string(),
    );
    expected_headers.insert("x-goog-upload-offset".to_string(), "0".to_string());
    assert_eq!(request.headers, expected_headers);

    assert_eq!(request.body_bytes, Some(vec![1, 2, 3]));
    assert_eq!(request.body_json, serde_json::Value::Null);
}

// ── parse_upload_response ─────────────────────────────────────────────────────

#[test]
fn parse_upload_response_reads_camel_case_file_object() {
    let body = serde_json::json!({
        "file": {
            "name": "files/abc",
            "uri": "https://generativelanguage.googleapis.com/v1beta/files/abc",
            "mimeType": "image/png",
            "state": "ACTIVE"
        }
    });
    let file = parse_upload_response(&body).expect("file");
    assert_eq!(
        file,
        GeminiFile {
            name: "files/abc".to_string(),
            uri: "https://generativelanguage.googleapis.com/v1beta/files/abc".to_string(),
            mime_type: "image/png".to_string(),
            state: "ACTIVE".to_string(),
        }
    );
}

#[test]
fn parse_upload_response_missing_uri_errors_without_echoing_values() {
    let body = serde_json::json!({
        "file": {"name": "files/abc", "mimeType": "image/png", "state": "ACTIVE"}
    });
    let error = parse_upload_response(&body).expect_err("must error");
    let LlmError::InvalidRequest { message } = error else {
        panic!("expected InvalidRequest, got {error:?}");
    };
    assert_eq!(message, "Gemini upload response missing file.uri");
    assert!(!message.contains("files/abc"));
}

#[test]
fn parse_upload_response_passes_failed_state_through() {
    let body = serde_json::json!({
        "file": {
            "name": "files/bad",
            "uri": "https://generativelanguage.googleapis.com/v1beta/files/bad",
            "mimeType": "video/mp4",
            "state": "FAILED"
        }
    });
    let file = parse_upload_response(&body).expect("FAILED is data, not an error");
    assert_eq!(file.state, "FAILED");
}

// ── file_status_request / parse_file_status ───────────────────────────────────

#[test]
fn file_status_request_is_a_get_on_the_file_resource() {
    let request = file_status_request(GEMINI_BASE, "files/abc");
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.url,
        "https://generativelanguage.googleapis.com/v1beta/files/abc"
    );
    assert!(request.headers.is_empty());
    assert_eq!(request.body_json, serde_json::Value::Null);
    assert_eq!(request.body_bytes, None);
}

/// files.get returns the File resource at the TOP level — not nested under
/// `"file"` like the upload response.
#[test]
fn parse_file_status_reads_top_level_file_resource() {
    let body = serde_json::json!({
        "name": "files/abc",
        "uri": "https://generativelanguage.googleapis.com/v1beta/files/abc",
        "mimeType": "video/mp4",
        "state": "PROCESSING"
    });
    let file = parse_file_status(&body).expect("file");
    assert_eq!(file.name, "files/abc");
    assert_eq!(file.state, "PROCESSING");
}

// ── BONUS (T5 review): legacy ProviderRequest JSON without body_bytes ─────────

#[test]
fn provider_request_legacy_json_without_body_bytes_deserializes_to_none() {
    let request: ProviderRequest = serde_json::from_str(
        r#"{"method":"POST","url":"https://example.com","body_json":{"a":1}}"#,
    )
    .expect("legacy JSON must deserialize");
    assert_eq!(request.body_bytes, None);
}

// ── Driver: ModelRuntime::upload_file with a scripted mock transport ──────

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

impl llm_runtime::test_support::FixtureTransport for ScriptedTransport {
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
llm_runtime::impl_fixture_transport!(ScriptedTransport);

fn gemini_client() -> ModelRuntime {
    std::env::set_var("LLM_CLIENT_GEMINI_FILES_TEST_KEY", "gemini-files-key");
    ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::Gemini,
            profile_name: "gemini".to_string(),
            base_url: GEMINI_BASE.to_string(),
            protocol: ProtocolFamily::GeminiGenerateContent,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "LLM_CLIENT_GEMINI_FILES_TEST_KEY".to_string(),
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

fn anthropic_client() -> ModelRuntime {
    std::env::set_var("LLM_CLIENT_GEMINI_FILES_TEST_KEY", "gemini-files-key");
    ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "LLM_CLIENT_GEMINI_FILES_TEST_KEY".to_string(),
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

fn start_ok_response(upload_url: &str) -> ProviderResponse {
    let mut response = ProviderResponse::json(200, serde_json::Value::Null);
    response
        .headers
        .insert("x-goog-upload-url".to_string(), upload_url.to_string());
    response
}

fn upload_ok_response(state: &str) -> ProviderResponse {
    ProviderResponse::json(
        200,
        serde_json::json!({
            "file": {
                "name": "files/abc",
                "uri": "https://generativelanguage.googleapis.com/v1beta/files/abc",
                "mimeType": "image/png",
                "state": state
            }
        }),
    )
}

#[tokio::test]
async fn upload_file_runs_the_two_step_authenticated_flow() {
    let upload_url = "https://generativelanguage.googleapis.com/upload/v1beta/files?upload_id=abc";
    let transport = Arc::new(ScriptedTransport::returning(vec![
        start_ok_response(upload_url),
        upload_ok_response("ACTIVE"),
    ]));
    let client = gemini_client();

    let file = client
        .upload_file(
            "gemini",
            vec![9, 8, 7],
            "image/png",
            "shot.png",
            transport.clone(),
        )
        .await
        .expect("uploaded file");

    assert_eq!(file.name, "files/abc");
    assert_eq!(
        file.uri,
        "https://generativelanguage.googleapis.com/v1beta/files/abc"
    );
    assert_eq!(file.mime_type, "image/png");
    assert_eq!(file.state, "ACTIVE");

    let seen = transport.seen();
    assert_eq!(seen.len(), 2, "exactly two transport calls (start, upload)");

    // Leg 1: start — authenticated with the gemini api-key header.
    let start = &seen[0];
    assert_eq!(
        start.url,
        "https://generativelanguage.googleapis.com/upload/v1beta/files"
    );
    assert_eq!(
        start.headers.get("x-goog-api-key").map(String::as_str),
        Some("gemini-files-key")
    );
    assert_eq!(
        start
            .headers
            .get("x-goog-upload-command")
            .map(String::as_str),
        Some("start")
    );
    assert_eq!(
        start
            .headers
            .get("x-goog-upload-header-content-length")
            .map(String::as_str),
        Some("3")
    );
    assert_eq!(
        start.body_json,
        serde_json::json!({"file": {"display_name": "shot.png"}})
    );

    // Leg 2: upload+finalize — to the returned URL, with the exact bytes.
    let upload = &seen[1];
    assert_eq!(upload.url, upload_url);
    assert_eq!(
        upload.headers.get("x-goog-api-key").map(String::as_str),
        None
    );
    assert_eq!(
        upload
            .headers
            .get("x-goog-upload-command")
            .map(String::as_str),
        Some("upload, finalize")
    );
    assert_eq!(
        upload
            .headers
            .get("x-goog-upload-offset")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(upload.body_bytes, Some(vec![9, 8, 7]));
    assert_eq!(upload.body_json, serde_json::Value::Null);
}

#[tokio::test]
async fn upload_file_rejects_non_gemini_profiles() {
    let transport = Arc::new(ScriptedTransport::returning(vec![]));
    let client = anthropic_client();

    let error = client
        .upload_file("claude", vec![1], "image/png", "f", transport.clone())
        .await
        .expect_err("must reject");
    assert!(matches!(
        error,
        LlmError::InvalidRequest { ref message }
            if message == "file upload requires a gemini provider profile"
    ));
    assert!(transport.seen().is_empty(), "no request may be sent");
}

#[tokio::test]
async fn upload_file_errors_when_upload_url_header_is_missing() {
    // Start response with NO x-goog-upload-url header.
    let transport = Arc::new(ScriptedTransport::returning(vec![ProviderResponse::json(
        200,
        serde_json::Value::Null,
    )]));
    let client = gemini_client();

    let error = client
        .upload_file("gemini", vec![1], "image/png", "f", transport.clone())
        .await
        .expect_err("must error");
    assert!(matches!(
        error,
        LlmError::InvalidRequest { ref message }
            if message == "Gemini upload start response missing x-goog-upload-url header"
    ));
    assert_eq!(transport.seen().len(), 1, "upload leg must not run");
}

#[tokio::test]
async fn upload_file_passes_failed_state_through_without_error() {
    let transport = Arc::new(ScriptedTransport::returning(vec![
        start_ok_response("https://generativelanguage.googleapis.com/upload/u?id=1"),
        upload_ok_response("FAILED"),
    ]));
    let client = gemini_client();

    let file = client
        .upload_file("gemini", vec![1], "image/png", "f", transport.clone())
        .await
        .expect("driver does not error on state");
    assert_eq!(file.state, "FAILED");
}

#[tokio::test]
async fn upload_file_maps_error_statuses_through_the_gemini_taxonomy() {
    let error_response = ProviderResponse::json(
        403,
        serde_json::json!({
            "error": {"code": 403, "message": "no", "status": "PERMISSION_DENIED"}
        }),
    );
    let transport = Arc::new(ScriptedTransport::returning(vec![error_response]));
    let client = gemini_client();

    let error = client
        .upload_file("gemini", vec![1], "image/png", "f", transport.clone())
        .await
        .expect_err("must map status");
    assert!(matches!(error, LlmError::PermissionDenied { .. }));
}

// ── Integration shape: uploaded uri plugs into the batch-2 ImageUrl encoding ──

#[test]
fn uploaded_file_uri_round_trips_into_gemini_file_data_encoding() {
    let body = serde_json::json!({
        "file": {
            "name": "files/abc",
            "uri": "https://generativelanguage.googleapis.com/v1beta/files/abc",
            "mimeType": "image/png",
            "state": "ACTIVE"
        }
    });
    let file = parse_upload_response(&body).expect("file");

    let codec = GeminiCodec::new(GEMINI_BASE);
    let mut request = HistoryFixture::new("gemini-2.0-flash");
    request.messages.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::ImageUrl {
            url: file.uri.clone(),
        }],
    });
    let provider_request = codec.encode_request(&request).expect("encoded");
    let part = &provider_request.body_json["contents"][0]["parts"][0];
    assert_eq!(
        part["fileData"]["fileUri"],
        "https://generativelanguage.googleapis.com/v1beta/files/abc"
    );
}
