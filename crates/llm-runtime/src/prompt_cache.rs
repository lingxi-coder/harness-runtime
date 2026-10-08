//! Request-local cache diagnostics over the SDK's finalized Anthropic body.
//!
//! This observes an already prepared request; it never encodes or changes one.
//! Only hashes, tool names, and character counts survive the observation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub type Fingerprint = [u8; 32];

/// Evidence for the main request that actually reached provider dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSnapshot {
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

/// One owner per main conversation, explicitly scoped around each API call.
/// Sharing an `ApiService` with side queries cannot overwrite this capture.
#[derive(Debug, Clone, Default)]
pub struct RequestCapture(Arc<Mutex<Option<RequestSnapshot>>>);

tokio::task_local! {
    static CAPTURE: RequestCapture;
}

impl RequestCapture {
    pub async fn scope<F: std::future::Future>(&self, future: F) -> F::Output {
        // A separate slot for the in-flight call also isolates nested scopes.
        let request = Self::default();
        let output = CAPTURE.scope(request.clone(), Box::pin(future)).await;
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = request.take();
        output
    }

    pub fn take(&self) -> Option<RequestSnapshot> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

pub(crate) fn snapshot_prepared(
    prepared: &crate::PreparedLlmCall,
) -> Option<RequestSnapshot> {
    matches!(
        prepared.route.protocol,
        crate::ProtocolFamily::AnthropicMessages
            | crate::ProtocolFamily::BedrockClaude
            | crate::ProtocolFamily::VertexClaude
            | crate::ProtocolFamily::FoundryClaude
    )
    .then(|| {
        RequestSnapshot::from_request(
            &prepared.provider_request,
            &prepared.route.resolved_route.request_model,
        )
    })
}

/// Record a request only after the SDK dispatch marker accepts the attempt.
pub(crate) fn observe_snapshot(mut snapshot: Option<RequestSnapshot>) {
    let _ = CAPTURE.try_with(|capture| {
        if let Some(snapshot) = snapshot.as_mut() {
            snapshot.at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        }
        *capture.0.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
    });
}

fn without_cache_control(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(fields) = value.as_object_mut() {
        fields.remove("cache_control");
    }
    value
}

fn message_block(value: &Value) -> Value {
    let mut value = without_cache_control(value);
    if value["type"] == "tool_result" {
        if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
            *content = content.iter().map(without_cache_control).collect();
        }
    }
    value
}

fn message_without_cache_controls(value: &Value) -> Value {
    let mut value = without_cache_control(value);
    if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
        *content = content.iter().map(message_block).collect();
    }
    value
}

fn block_array_without_cache_controls(value: &Value) -> Value {
    match value.as_array() {
        Some(blocks) => Value::Array(blocks.iter().map(without_cache_control).collect()),
        None => value.clone(),
    }
}

fn fingerprint(value: &impl serde::Serialize) -> Fingerprint {
    // All callers use JSON values or string collections, whose serialization
    // is infallible. Hash serialized bytes so 0.0 and -0.0 stay distinct.
    Sha256::digest(serde_json::to_vec(value).expect("JSON cache diagnostic value")).into()
}

fn part_hash(value: &Value, request: &crate::ProviderRequest, prefix: &str) -> Fingerprint {
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

fn cache_controls(value: &Value, controls: &mut BTreeSet<String>) {
    if let Some(control) = value.get("cache_control") {
        controls.insert(control.to_string());
    }
    // Traverse protocol content blocks only. A tool schema/input may itself
    // contain a property named cache_control; that is prompt data, not policy.
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for block in content {
            if let Some(control) = block.get("cache_control") {
                controls.insert(control.to_string());
            }
            if block["type"] == "tool_result" {
                cache_controls(block, controls);
            }
        }
    }
}

impl RequestSnapshot {
    fn from_request(request: &crate::ProviderRequest, resolved_model: &str) -> Self {
        let body = &request.body_json;
        let system = match body.get("system") {
            Some(Value::Array(blocks)) => Value::Array(
                blocks
                    .iter()
                    .filter(|block| {
                        !block["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("x-anthropic-billing-header:"))
                    })
                    .cloned()
                    .collect(),
            ),
            value => value.cloned().unwrap_or(Value::Null),
        };
        let system_chars = match &system {
            Value::String(text) => text.encode_utf16().count(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .map(|text| text.encode_utf16().count())
                .sum(),
            _ => 0,
        };
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        let mut controls = BTreeSet::new();
        cache_controls(body, &mut controls);
        for field in ["system", "tools", "messages"] {
            for value in body[field].as_array().into_iter().flatten() {
                cache_controls(value, &mut controls);
            }
        }
        let ttl_1h = controls.iter().any(|control| {
            serde_json::from_str::<Value>(control).is_ok_and(|value| value["ttl"] == "1h")
        });
        let mut betas: BTreeSet<String> = request
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .flat_map(|(_, value)| value.split(',').map(str::trim).map(str::to_owned))
            .collect();
        if let Some(values) = body["anthropic_beta"].as_array() {
            betas.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let mut extra = body.as_object().cloned().unwrap_or_default();
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
            extra.remove(field);
        }
        if let Some(Value::Object(output)) = extra.get_mut("output_config") {
            output.remove("effort");
            if output.is_empty() {
                extra.remove("output_config");
            }
        }
        let thinking_mode = match body["thinking"]["type"].as_str() {
            Some("disabled") => "off",
            Some(_) => "on",
            None => "",
        };
        Self {
            at_ms: u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0),
            system: part_hash(
                &block_array_without_cache_controls(&system),
                request,
                "/system",
            ),
            system_chars: i64::try_from(system_chars).unwrap_or(i64::MAX),
            tools: part_hash(
                &block_array_without_cache_controls(&Value::Array(tools.clone())),
                request,
                "/tools",
            ),
            tool_names: tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .map(str::to_owned)
                .collect(),
            model: fingerprint(&body["model"].as_str().unwrap_or(resolved_model)),
            cache_policy: fingerprint(&controls),
            ttl_1h,
            betas: fingerprint(&betas),
            effort: fingerprint(&body["output_config"]["effort"]),
            fast_mode: body["speed"] == "fast",
            thinking_mode,
            thinking_display: fingerprint(&body["thinking"]["display"]),
            extra_body: fingerprint(&extra),
            defer_loading: tools.iter().any(|tool| tool["defer_loading"] == true),
            messages: body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, message)| {
                    part_hash(
                        &message_without_cache_controls(message),
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
    use serde_json::json;

    #[test]
    fn append_and_moving_cache_breakpoints_preserve_existing_message_hashes() {
        let mut request = crate::ProviderRequest::post_json(
            "unused",
            json!({
                "system":[{"type":"text","text":"😀","cache_control":{"type":"ephemeral"}}],
                "messages":[{"role":"user","content":[{"type":"text","text":"one","cache_control":{"type":"ephemeral"}}]}]
            }),
        );
        let first = RequestSnapshot::from_request(&request, "model");
        request.body_json["messages"][0]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        request.body_json["messages"].as_array_mut().unwrap().push(json!({"role":"user","content":[{"type":"text","text":"two","cache_control":{"type":"ephemeral"}}]}));
        let next = RequestSnapshot::from_request(&request, "model");
        assert_eq!(first.messages, next.messages[..1]);
        assert_eq!(first.cache_policy, next.cache_policy);
        assert_eq!(first.system_chars, 2);
    }

    #[test]
    fn signed_zero_exact_utf16_and_real_ttl_are_observed() {
        let mut request = crate::ProviderRequest::post_json(
            "unused",
            json!({
                "system":[{"text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}],
                "messages":[{"role":"user","content":[{"type":"text","text":"�"}]}],
                "tools":[{"name":"t","input_schema":{"minimum":0.0}}]
            }),
        );
        let first = RequestSnapshot::from_request(&request, "model");
        assert!(first.ttl_1h);
        request.body_json["tools"][0]["input_schema"]["minimum"] = json!(-0.0);
        request
            .json_string_overrides
            .insert("/messages/0/content/0/text".into(), vec![0xd800]);
        let next = RequestSnapshot::from_request(&request, "model");
        assert_ne!(first.tools, next.tools);
        assert_ne!(first.messages, next.messages);
    }

    #[test]
    fn tool_input_named_cache_control_is_content_not_a_cache_annotation() {
        let mut request = crate::ProviderRequest::post_json(
            "unused",
            json!({
                "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"t","input":{"cache_control":{"ttl":"1h"}}}]}],
                "tools":[{"name":"t","input_schema":{"properties":{"cache_control":{"type":"string"}}}}]
            }),
        );
        let first = RequestSnapshot::from_request(&request, "model");
        assert!(!first.ttl_1h);
        request.body_json["messages"][0]["content"][0]["input"]["cache_control"]["ttl"] =
            json!("5m");
        request.body_json["tools"][0]["input_schema"]["properties"]["cache_control"]["type"] =
            json!("number");
        let next = RequestSnapshot::from_request(&request, "model");
        assert_ne!(first.messages, next.messages);
        assert_ne!(first.tools, next.tools);
        assert_eq!(first.cache_policy, next.cache_policy);
    }

    #[tokio::test]
    async fn nested_capture_does_not_replace_the_parent_request() {
        let parent = RequestCapture::default();
        let child = RequestCapture::default();
        let request = crate::ProviderRequest::post_json("unused", json!({"model":"parent"}));
        let snapshot = RequestSnapshot::from_request(&request, "parent");
        parent
            .scope(async {
                CAPTURE.with(|capture| *capture.0.lock().unwrap() = Some(snapshot.clone()));
                child.scope(async {}).await;
            })
            .await;
        assert_eq!(parent.take(), Some(snapshot));
        assert_eq!(child.take(), None);
    }
}
