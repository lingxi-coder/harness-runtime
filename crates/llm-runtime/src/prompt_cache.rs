//! Request-local cache diagnostics over the SDK's finalized Anthropic body.
//!
//! This observes an already prepared request; it never encodes or changes one.
//! Only hashes, tool names, and character counts survive the observation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lingxi_llm_client::protocol::JsonPayload;
use sha2::{Digest, Sha256};

pub type Fingerprint = [u8; 32];

/// Evidence for the main request that actually reached provider dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSnapshot {
    /// H's logical-request parameter-save time, captured when the caller
    /// enters one API operation. Physical SDK retries inside that operation
    /// share this anchor. It is deliberately distinct from `at_ms`, which is
    /// the accepted physical-dispatch time below.
    pub request_anchor_at_ms: Option<u64>,
    /// Provider dispatch time, before waiting for the response to finish.
    pub at_ms: u64,
    pub system: Fingerprint,
    pub system_chars: i64,
    pub tools: Fingerprint,
    pub tool_names: BTreeSet<String>,
    pub model: Fingerprint,
    pub cache_policy: Fingerprint,
    pub ttl_1h: bool,
    pub betas: Fingerprint,
    pub effort: Fingerprint,
    pub fast_mode: bool,
    /// Empty when absent, otherwise `on` or `off`, as in the oracle.
    pub thinking_mode: &'static str,
    pub thinking_display: Fingerprint,
    pub extra_body: Fingerprint,
    pub defer_loading: bool,
    pub messages: Vec<Fingerprint>,
}

#[derive(Debug, Clone, Default)]
struct CapturedRequest {
    snapshot: Option<RequestSnapshot>,
    request_anchor_at_ms: Option<u64>,
}

/// One owner per main conversation, explicitly scoped around each API call.
/// Sharing an `ApiService` with side queries cannot overwrite this capture.
#[derive(Debug, Clone, Default)]
pub struct RequestCapture(Arc<Mutex<CapturedRequest>>);

tokio::task_local! {
    static CAPTURE: RequestCapture;
}

impl RequestCapture {
    pub async fn scope<F: std::future::Future>(&self, future: F) -> F::Output {
        let anchor_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        self.scope_at(anchor_at_ms, future).await
    }

    async fn scope_at<F: std::future::Future>(&self, anchor_at_ms: u64, future: F) -> F::Output {
        // A separate slot for the in-flight call also isolates nested scopes.
        let request = Self(Arc::new(Mutex::new(CapturedRequest {
            snapshot: None,
            request_anchor_at_ms: Some(anchor_at_ms),
        })));
        let output = CAPTURE.scope(request.clone(), Box::pin(future)).await;
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) =
            request.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        output
    }

    pub fn take(&self) -> Option<RequestSnapshot> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .take()
    }

    fn request_anchor_at_ms(&self) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_anchor_at_ms
    }
}

pub(crate) fn snapshot_prepared(prepared: &crate::PreparedLlmCall) -> Option<RequestSnapshot> {
    matches!(
        prepared.route.protocol,
        crate::ProtocolFamily::AnthropicMessages
            | crate::ProtocolFamily::BedrockClaude
            | crate::ProtocolFamily::VertexClaude
            | crate::ProtocolFamily::FoundryClaude
    )
    .then(|| {
        inherit_request_anchor(RequestSnapshot::from_request(
            &prepared.provider_request,
            &prepared.route.resolved_route.request_model,
        ))
    })
}

fn inherit_request_anchor(mut snapshot: RequestSnapshot) -> RequestSnapshot {
    snapshot.request_anchor_at_ms = CAPTURE
        .try_with(RequestCapture::request_anchor_at_ms)
        .ok()
        .flatten();
    snapshot
}

/// Record a request only after the SDK dispatch marker accepts the attempt.
pub(crate) fn observe_snapshot(mut snapshot: Option<RequestSnapshot>) {
    let _ = CAPTURE.try_with(|capture| {
        if let Some(snapshot) = snapshot.as_mut() {
            snapshot.at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        }
        capture.0.lock().unwrap_or_else(|e| e.into_inner()).snapshot = snapshot;
    });
}

fn fingerprint(value: &impl serde::Serialize) -> Fingerprint {
    Sha256::digest(serde_json::to_vec(value).expect("cache diagnostic values serialize")).into()
}

fn part_hash(value: &JsonPayload, request: &crate::ProviderRequest, prefix: &str) -> Fingerprint {
    let overrides: BTreeMap<_, _> = request
        .json_string_overrides
        .iter()
        .filter_map(|(path, units)| {
            path.strip_prefix(prefix)
                .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                .map(|suffix| (suffix, units))
        })
        .collect();
    fingerprint(&(value, overrides))
}

fn without_cache_control(
    value: &JsonPayload,
) -> Result<JsonPayload, lingxi_llm_client::protocol::LlmError> {
    if lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
        value,
        &["cache_control"],
    ) {
        value.without_object_field("cache_control")
    } else {
        Ok(value.clone())
    }
}

fn block_array_without_cache_controls(
    value: &JsonPayload,
    encoding: lingxi_llm_client::exact_json::JsonEncoding,
) -> Result<JsonPayload, lingxi_llm_client::protocol::LlmError> {
    let Some(length) = value.array_len("") else {
        return Ok(value.clone());
    };
    let blocks = (0..length)
        .filter_map(|index| value.array_item(index))
        .map(|block| without_cache_control(&block))
        .collect::<Result<Vec<_>, _>>()?;
    JsonPayload::array_from_items(&blocks, encoding)
}

fn message_without_cache_controls(
    value: &JsonPayload,
    encoding: lingxi_llm_client::exact_json::JsonEncoding,
) -> Result<JsonPayload, lingxi_llm_client::protocol::LlmError> {
    let mut message = without_cache_control(value)?;
    let Some(content) = value.payload_at("/content") else {
        return Ok(message);
    };
    let Some(length) = content.array_len("") else {
        return Ok(message);
    };
    let mut blocks = Vec::with_capacity(length);
    for index in 0..length {
        let Some(block) = content.array_item(index) else {
            continue;
        };
        let mut clean = without_cache_control(&block)?;
        if block.string_at("/type").as_deref() == Some("tool_result") {
            if let Some(nested) = block.payload_at("/content") {
                if nested.array_len("").is_some() {
                    clean = clean.without_object_field("content")?;
                    let nested = block_array_without_cache_controls(&nested, encoding)?;
                    clean = clean.with_object_field("content", &nested)?;
                }
            }
        }
        blocks.push(clean);
    }
    let content = JsonPayload::array_from_items(&blocks, encoding)?;
    message = message.with_object_field("content", &content)?;
    Ok(message)
}

fn cache_controls(value: &JsonPayload, controls: &mut BTreeSet<String>) {
    if let Some(control) = value.raw_source_at("/cache_control") {
        controls.insert(control);
    }
    // Traverse protocol content blocks only. A tool schema/input may itself
    // contain a property named cache_control; that is prompt data, not policy.
    if let Some(length) = value.array_len("/content") {
        for index in 0..length {
            let Some(block) = value.array_item_at("/content", index) else {
                continue;
            };
            if let Some(control) = block.raw_source_at("/cache_control") {
                controls.insert(control);
            }
            if block.string_at("/type").as_deref() == Some("tool_result") {
                cache_controls(&block, controls);
            }
        }
    }
}

fn empty_json(source: &str) -> JsonPayload {
    JsonPayload::parse(source).expect("static JSON cache value is valid")
}

fn clean_extra_body(
    body: &JsonPayload,
) -> Result<JsonPayload, lingxi_llm_client::protocol::LlmError> {
    if !body.is_root_object() {
        return Ok(empty_json("{}"));
    }
    let mut extra = body.clone();
    for field in [
        "system",
        "tools",
        "messages",
        "model",
        "stream",
        "max_tokens",
        "metadata",
        "thinking",
        "speed",
        "anthropic_version",
        "anthropic_beta",
        "cache_control",
    ] {
        extra = extra.without_object_field(field)?;
    }
    if let Some(config) = extra.payload_at("/output_config") {
        if config.is_root_object() {
            let config = config.without_object_field("effort")?;
            if config.object_len("") == Some(0) {
                extra = extra.without_object_field("output_config")?;
            } else {
                extra = extra.with_object_field("output_config", &config)?;
            }
        }
    }
    Ok(extra)
}

impl RequestSnapshot {
    fn from_request(request: &crate::ProviderRequest, resolved_model: &str) -> Self {
        let body = &request.body_json;
        let system = body
            .payload_at("/system")
            .unwrap_or_else(|| empty_json("null"));
        let system = if let Some(length) = system.array_len("") {
            let blocks = (0..length)
                .filter_map(|index| system.array_item(index))
                .filter(|block| {
                    !block
                        .string_at("/text")
                        .is_some_and(|text| text.starts_with("x-anthropic-billing-header:"))
                })
                .collect::<Vec<_>>();
            JsonPayload::array_from_items(&blocks, request.json_encoding)
                .expect("system blocks remain valid JSON")
        } else {
            system
        };
        let system_chars = system.root_string_code_units().map_or_else(
            || {
                system.array_len("").map_or(0, |length| {
                    (0..length)
                        .filter_map(|index| system.array_item(index))
                        .filter_map(|block| block.string_at("/text"))
                        .map(|text| text.encode_utf16().count())
                        .sum()
                })
            },
            |units| units.len(),
        );
        let tools = body
            .payload_at("/tools")
            .filter(|tools| tools.array_len("").is_some())
            .unwrap_or_else(|| empty_json("[]"));
        let mut controls = BTreeSet::new();
        cache_controls(body, &mut controls);
        for field in ["system", "tools", "messages"] {
            let pointer = format!("/{field}");
            if let Some(length) = body.array_len(&pointer) {
                for index in 0..length {
                    if let Some(value) = body.array_item_at(&pointer, index) {
                        cache_controls(&value, &mut controls);
                    }
                }
            }
        }
        let ttl_1h = controls.iter().any(|control| {
            JsonPayload::parse(control)
                .ok()
                .and_then(|value| value.string_at("/ttl"))
                .as_deref()
                == Some("1h")
        });
        let mut betas: BTreeSet<String> = request
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .flat_map(|(_, value)| value.split(',').map(str::trim).map(str::to_owned))
            .collect();
        if let Some(values) = body.payload_at("/anthropic_beta") {
            if let Some(length) = values.array_len("") {
                betas.extend(
                    (0..length).filter_map(|index| values.array_item(index)?.string_at("")),
                );
            }
        }
        let extra = clean_extra_body(body).expect("prepared request object is valid JSON");
        let thinking_mode = match body.string_at("/thinking/type").as_deref() {
            Some("disabled") => "off",
            Some(_) => "on",
            None => "",
        };
        Self {
            request_anchor_at_ms: None,
            at_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            system: part_hash(
                &block_array_without_cache_controls(&system, request.json_encoding)
                    .expect("system cache-control projection remains valid JSON"),
                request,
                "/system",
            ),
            system_chars: i64::try_from(system_chars).unwrap_or(i64::MAX),
            tools: part_hash(
                &block_array_without_cache_controls(&tools, request.json_encoding)
                    .expect("tool cache-control projection remains valid JSON"),
                request,
                "/tools",
            ),
            tool_names: tools
                .array_len("")
                .into_iter()
                .flat_map(|length| (0..length).filter_map(|index| tools.array_item(index)))
                .filter_map(|tool| tool.string_at("/name"))
                .collect(),
            model: fingerprint(
                &body
                    .string_at("/model")
                    .unwrap_or_else(|| resolved_model.into()),
            ),
            cache_policy: fingerprint(&controls),
            ttl_1h,
            betas: fingerprint(&betas),
            effort: fingerprint(
                &body
                    .payload_at("/output_config/effort")
                    .unwrap_or_else(|| empty_json("null")),
            ),
            fast_mode: body.string_at("/speed").as_deref() == Some("fast"),
            thinking_mode,
            thinking_display: fingerprint(
                &body
                    .payload_at("/thinking/display")
                    .unwrap_or_else(|| empty_json("null")),
            ),
            extra_body: fingerprint(&extra),
            defer_loading: tools
                .array_len("")
                .into_iter()
                .flat_map(|length| (0..length).filter_map(|index| tools.array_item(index)))
                .any(|tool| tool.bool_at("/defer_loading") == Some(true)),
            messages: body
                .array_len("/messages")
                .into_iter()
                .flat_map(|length| {
                    (0..length).filter_map(|index| body.array_item_at("/messages", index))
                })
                .enumerate()
                .map(|(index, message)| {
                    part_hash(
                        &message_without_cache_controls(&message, request.json_encoding)
                            .expect("message cache-control projection remains valid JSON"),
                        request,
                        &format!("/messages/{index}"),
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(source: &str) -> JsonPayload {
        JsonPayload::parse(source).expect("test JSON is valid")
    }

    fn request(source: &str) -> crate::ProviderRequest {
        crate::ProviderRequest::post_json("unused", payload(source))
    }

    #[test]
    fn append_and_moving_cache_breakpoints_preserve_existing_message_hashes() {
        let first_request = request(
            r#"{"system":[{"type":"text","text":"😀","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"one","cache_control":{"type":"ephemeral"}}]}]}"#,
        );
        let next_request = request(
            r#"{"system":[{"type":"text","text":"😀","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"one"}]},{"role":"user","content":[{"type":"text","text":"two","cache_control":{"type":"ephemeral"}}]}]}"#,
        );
        let first = RequestSnapshot::from_request(&first_request, "model");
        let next = RequestSnapshot::from_request(&next_request, "model");
        assert_eq!(first.messages, next.messages[..1]);
        assert_eq!(first.cache_policy, next.cache_policy);
        assert_eq!(first.system_chars, 2);
    }

    #[test]
    fn signed_zero_exact_utf16_and_real_ttl_are_observed() {
        let mut request = request(
            r#"{"system":[{"text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"�"}]}],"tools":[{"name":"t","input_schema":{"minimum":0.0}}]}"#,
        );
        let first = RequestSnapshot::from_request(&request, "model");
        assert!(first.ttl_1h);
        request.body_json = payload(
            r#"{"system":[{"text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"�"}]}],"tools":[{"name":"t","input_schema":{"minimum":-0.0}}]}"#,
        );
        request
            .json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);
        let next = RequestSnapshot::from_request(&request, "model");
        assert_ne!(first.tools, next.tools);
        assert_ne!(first.messages, next.messages);
    }

    #[test]
    fn tool_input_named_cache_control_is_content_not_a_cache_annotation() {
        let first_request = request(
            r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"t","input":{"cache_control":{"ttl":"1h"}}}]}],"tools":[{"name":"t","input_schema":{"properties":{"cache_control":{"type":"string"}}}}]}"#,
        );
        let first = RequestSnapshot::from_request(&first_request, "model");
        assert!(!first.ttl_1h);
        let next_request = request(
            r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"t","input":{"cache_control":{"ttl":"5m"}}}]}],"tools":[{"name":"t","input_schema":{"properties":{"cache_control":{"type":"number"}}}}]}"#,
        );
        let next = RequestSnapshot::from_request(&next_request, "model");
        assert_ne!(first.messages, next.messages);
        assert_ne!(first.tools, next.tools);
        assert_eq!(first.cache_policy, next.cache_policy);
    }

    #[tokio::test]
    async fn nested_capture_does_not_replace_the_parent_request() {
        let parent = RequestCapture::default();
        let child = RequestCapture::default();
        let request = request(r#"{"model":"parent"}"#);
        let snapshot = RequestSnapshot::from_request(&request, "parent");
        parent
            .scope_at(10, async {
                CAPTURE.with(|capture| {
                    *capture.0.lock().unwrap().snapshot =
                        Some(inherit_request_anchor(snapshot.clone()))
                });
                child.scope(async {}).await;
            })
            .await;
        let captured = parent.take().expect("parent request is captured");
        assert_eq!(captured.request_anchor_at_ms, Some(10));
        assert_eq!(captured.model, snapshot.model);
        assert_eq!(child.take(), None);
    }

    #[tokio::test]
    async fn logical_request_anchor_is_shared_by_retries_but_refreshes_for_next_request() {
        let capture = RequestCapture::default();
        let request = request(r#"{"model":"main"}"#);
        let base = RequestSnapshot::from_request(&request, "main");
        let mut attempt_anchors = Vec::new();
        capture
            .scope_at(100, async {
                for _retry in 0..2 {
                    let snapshot = inherit_request_anchor(base.clone());
                    attempt_anchors.push(snapshot.request_anchor_at_ms);
                    observe_snapshot(Some(snapshot));
                }
            })
            .await;
        let first_request = capture.take().expect("first request snapshot");
        assert_eq!(attempt_anchors, [Some(100), Some(100)]);
        assert_eq!(first_request.request_anchor_at_ms, Some(100));

        capture
            .scope_at(250, async {
                let next = inherit_request_anchor(base);
                observe_snapshot(Some(next));
            })
            .await;
        let next_request = capture.take().expect("next logical request snapshot");
        assert_eq!(next_request.request_anchor_at_ms, Some(250));
        assert_eq!(next_request.ttl_1h, first_request.ttl_1h);
    }
}
