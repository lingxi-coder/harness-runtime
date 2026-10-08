//! Current Native 2.1.293 tool-result persistence source/file contract.
//!
//! The companion Native fixture returns the same fixed source through a real
//! registered Mod tool. This test calls the production persistence and wrapper
//! entry points, keeping source preview units, final file bytes, and the model
//! envelope as separate assertions.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_llm_client::protocol::{
    ContentBlock as WireContentBlock, ConversationMessage as WireMessage, MessageRole,
};
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, LlmRequest, ModelRuntime, SubscriberState, Transport};
use orchestrator::tool_result_persistence::{
    persist, resolve_threshold, wrap_utf16, MAX_PERSIST_UTF16_UNITS, PREVIEW_CHARS,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

const PREFIX: &str = "begin";
const PADDING: char = 'z';
const PADDING_UNITS: usize = 50_001;
const LONE_UNIT: u16 = 0xd800;
const TOOL_USE_ID: &str = "toolu_native_tool_result_fixture";
const BOUNDARY_TOOL_USE_ID: &str = "toolu_native_tool_result_boundary_fixture";

fn native_result_units() -> Vec<u16> {
    let mut units = PREFIX.encode_utf16().collect::<Vec<_>>();
    units.push(LONE_UNIT);
    units.extend(std::iter::repeat_n(PADDING as u16, PADDING_UNITS));
    units
}

fn utf16_contains(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn expected_native_file_bytes() -> Vec<u8> {
    let mut bytes = PREFIX.as_bytes().to_vec();
    // Native writes JS strings as UTF-8; an isolated surrogate becomes U+FFFD
    // at this OS-file boundary. The original U+D800 remains in the source
    // preview and JSONL evidence.
    bytes.extend_from_slice(&[0xef, 0xbf, 0xbd]);
    bytes.extend(std::iter::repeat_n(PADDING as u8, PADDING_UNITS));
    bytes
}

fn native_unicode_boundary_result_units() -> Vec<u16> {
    let mut units = vec![u16::from(b'a'); 1_999];
    units.extend("😀".encode_utf16());
    units.extend("TAIL".encode_utf16());
    units.extend("😀".encode_utf16());
    units.extend(std::iter::repeat_n(u16::from(b'z'), 48_000));
    units
}

struct CapturingRequestTransport {
    request_body: Mutex<Option<Vec<u8>>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Transport for CapturingRequestTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.request_body.lock().expect("request capture lock") = Some(request.body.to_vec());
        Ok(lingxi_llm_client::HttpResponse {
            status: 400,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"error":{"type":"invalid_request_error","message":"capture fixture"}}"#
                .to_vec()
                .into(),
        }
        .into())
    }
}

#[tokio::test]
async fn native_tool_result_lone_preview_and_utf8_file_boundary() {
    let source_units = native_result_units();
    assert_eq!(source_units.len(), 50_007);
    assert_eq!(source_units[5], LONE_UNIT);
    let source = Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&source_units),
        source_units.clone(),
    )
    .expect("source string projection");

    // Registered-tool selection normally uses a 50,000 UTF-16-unit default.
    // This test enters `persist` directly to isolate its file/preview contract;
    // it does not test threshold dispatch or the separate JSON-string coercion
    // helper in `tool_result_text`.
    assert_eq!(resolve_threshold(MAX_PERSIST_UTF16_UNITS, None), 50_000);
    assert!(source_units.len() > resolve_threshold(MAX_PERSIST_UTF16_UNITS, None));

    let config_home = tempfile::tempdir().expect("isolated config home");
    let tool_results_dir = config_home
        .path()
        .join("projects")
        .join("current-alignment")
        .join("session")
        .join("tool-results");
    let persisted = persist(
        config_home.path(),
        &tool_results_dir,
        TOOL_USE_ID,
        &source,
        false,
        MAX_PERSIST_UTF16_UNITS,
    )
    .await
    .expect("persist Native-sized result through production entry point");

    assert_eq!(persisted.original_size, 50_007);
    assert_eq!(persisted.preview_utf16.len(), PREVIEW_CHARS);
    assert_eq!(persisted.preview_utf16[5], LONE_UNIT);
    assert!(persisted.has_more);
    assert_eq!(
        persisted.filepath,
        tool_results_dir.join(format!("{TOOL_USE_ID}.txt"))
    );

    let file_bytes = tokio::fs::read(&persisted.filepath)
        .await
        .expect("read exact persisted bytes");
    assert_eq!(file_bytes, expected_native_file_bytes());
    assert_eq!(file_bytes.len(), 50_009);
    assert_eq!(
        file_bytes
            .windows(3)
            .filter(|w| *w == [0xef, 0xbf, 0xbd])
            .count(),
        1
    );

    let envelope = wrap_utf16(
        persisted.original_size,
        &persisted.filepath.to_string_lossy(),
        &persisted.preview_utf16,
        persisted.has_more,
        persisted.truncated_at,
    );
    let preview_marker = "Preview (first 2KB):\n".encode_utf16().collect::<Vec<_>>();
    let preview_start = utf16_contains(&envelope, &preview_marker)
        .expect("wrapped model content has preview marker")
        + preview_marker.len();
    assert_eq!(
        &envelope[preview_start..preview_start + PREVIEW_CHARS],
        persisted.preview_utf16,
        "the production wrapper carries source preview code units without normalization"
    );
    assert_eq!(envelope[preview_start + 5], LONE_UNIT);

    println!(
        "CURRENT_ALIGNMENT_TOOL_RESULT_PERSIST={}",
        serde_json::json!({
            "source_utf16_units": source_units.len(),
            "lone_unit_index": 5,
            "source_preview_unit_at_5": format!("U+{:04X}", persisted.preview_utf16[5]),
            "persisted_file_bytes": file_bytes.len(),
            "persisted_file_replacement_count": file_bytes.windows(3).filter(|w| *w == [0xef, 0xbf, 0xbd]).count(),
            "wrapper_preview_unit_at_5": format!("U+{:04X}", envelope[preview_start + 5]),
            "filepath_is_under_config_home": persisted.filepath.starts_with(config_home.path()),
        })
    );
}

#[tokio::test]
async fn native_tool_result_unicode_preview_boundary_reaches_sdk_request_and_file() {
    let source_units = native_unicode_boundary_result_units();
    assert_eq!(source_units.len(), 50_007);
    assert_eq!(source_units[1_999..2_001], [0xd83d, 0xde00]);
    assert_eq!(source_units[2_005..2_007], [0xd83d, 0xde00]);
    let source = Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&source_units),
        source_units.clone(),
    )
    .expect("source string projection");

    let config_home = tempfile::tempdir().expect("isolated config home");
    let tool_results_dir = config_home
        .path()
        .join("projects")
        .join("current-alignment")
        .join("session")
        .join("tool-results");
    let persisted = persist(
        config_home.path(),
        &tool_results_dir,
        BOUNDARY_TOOL_USE_ID,
        &source,
        false,
        MAX_PERSIST_UTF16_UNITS,
    )
    .await
    .expect("persist Native Unicode-boundary result through production entry point");

    assert_eq!(persisted.original_size, 50_007);
    assert_eq!(persisted.preview_utf16.len(), PREVIEW_CHARS);
    assert_eq!(
        persisted.preview_utf16,
        source_units[..PREVIEW_CHARS],
        "the 2KB preview is a JavaScript UTF-16 slice and splits the pair"
    );
    assert_eq!(persisted.preview_utf16[PREVIEW_CHARS - 1], 0xd83d);
    assert!(persisted.has_more);
    assert_eq!(
        persisted.filepath,
        tool_results_dir.join(format!("{BOUNDARY_TOOL_USE_ID}.txt"))
    );

    let file_bytes = tokio::fs::read(&persisted.filepath)
        .await
        .expect("read exact persisted bytes");
    let expected_file_bytes = String::from_utf16(&source_units)
        .expect("only complete surrogate pairs are persisted")
        .into_bytes();
    assert_eq!(file_bytes, expected_file_bytes);
    assert_eq!(file_bytes.len(), 50_011);
    assert_eq!(
        file_bytes
            .windows(3)
            .filter(|w| *w == [0xef, 0xbf, 0xbd])
            .count(),
        0,
        "complete source pairs persist as their original UTF-8 emoji bytes"
    );

    let envelope = wrap_utf16(
        persisted.original_size,
        &persisted.filepath.to_string_lossy(),
        &persisted.preview_utf16,
        persisted.has_more,
        persisted.truncated_at,
    );
    let preview_marker = "Preview (first 2KB):\n".encode_utf16().collect::<Vec<_>>();
    let preview_start = utf16_contains(&envelope, &preview_marker)
        .expect("wrapped model content has preview marker")
        + preview_marker.len();
    assert_eq!(
        &envelope[preview_start..preview_start + PREVIEW_CHARS],
        persisted.preview_utf16,
        "the production wrapper carries the split high surrogate exactly"
    );
    let ordinary_ellipsis = "\n...\n</persisted-output>"
        .encode_utf16()
        .collect::<Vec<_>>();
    assert!(
        envelope.ends_with(&ordinary_ellipsis),
        "the persistence preview uses the ordinary ellipsis without an omitted-count string"
    );

    // Feed the production wrapper through ApiService and the actual SDK
    // Anthropic request codec/dispatch path. The fixed transport returns a
    // local 400 after capturing the outgoing request; it never reaches a
    // provider or uses credentials.
    let transport = Arc::new(CapturingRequestTransport {
        request_body: Mutex::new(None),
        calls: AtomicUsize::new(0),
    });
    let config: ClientConfig = serde_json::from_value(serde_json::json!({
        "providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"direct",
            "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages",
            "auth":"none", "credential":{"type":"none"},
            "models":[{
                "display_model":"claude-sonnet-4-6", "request_model":"claude-sonnet-4-6",
                "billing_model":"claude-sonnet-4-6",
                "capabilities":{"streaming":true,"tools":true,"vision":false,
                    "documents":false,"reasoning":false,"structured_output":false}
            }]
        }]
    }))
    .expect("fixed Anthropic route config");
    let api = ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "current-alignment-fixture",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
    .with_thinking(ThinkingConfig::Disabled);
    let mut request = LlmRequest::new("claude-sonnet-4-6");
    request.input.messages.push(WireMessage {
        role: MessageRole::User,
        content: vec![WireContentBlock::TextJsUtf16 {
            text: String::from_utf16_lossy(&envelope),
            utf16_code_units: envelope,
            thought_signature: None,
            citations: None,
        }],
        native_options: Vec::new(),
    });
    let error = api
        .execute_side_query_request(request)
        .await
        .expect_err("fixed transport terminates after request capture");
    assert_eq!(error.http_status(), Some(400));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

    let request_body = transport
        .request_body
        .lock()
        .expect("request capture lock")
        .clone()
        .expect("SDK emitted one final request");
    let request_projection = Utf16JsonProjection::parse(
        std::str::from_utf8(&request_body).expect("final SDK body is valid UTF-8 JSON"),
    )
    .expect("parse final SDK request body without reserializing it");
    let model_text = request_projection
        .string_units("/messages/0/content/0/text")
        .expect("Anthropic message carries the persisted-output envelope");
    let wire_preview_start = utf16_contains(&model_text, &preview_marker)
        .expect("SDK request retained the preview marker")
        + preview_marker.len();
    let mut expected_wire_preview = source_units[..PREVIEW_CHARS].to_vec();
    expected_wire_preview[PREVIEW_CHARS - 1] = 0xfffd;
    assert_eq!(
        &model_text[wire_preview_start..wire_preview_start + PREVIEW_CHARS],
        expected_wire_preview,
        "SDK's final Claude request projection replaces the split high surrogate"
    );

    println!(
        "CURRENT_ALIGNMENT_TOOL_RESULT_PERSIST_BOUNDARY={}",
        serde_json::json!({
            "source_utf16_units": source_units.len(),
            "source_pair_at_1999": [format!("U+{:04X}", source_units[1999]), format!("U+{:04X}", source_units[2000])],
            "tail_pair_at_2005": [format!("U+{:04X}", source_units[2005]), format!("U+{:04X}", source_units[2006])],
            "source_preview_units": persisted.preview_utf16.len(),
            "source_preview_last_unit": format!("U+{:04X}", persisted.preview_utf16[PREVIEW_CHARS - 1]),
            "sdk_request_preview_last_unit": format!("U+{:04X}", model_text[wire_preview_start + PREVIEW_CHARS - 1]),
            "persisted_file_bytes": file_bytes.len(),
            "persisted_file_replacement_count": 0,
            "request_status_after_capture": error.http_status(),
            "filepath_is_under_config_home": persisted.filepath.starts_with(config_home.path()),
        })
    );
}
