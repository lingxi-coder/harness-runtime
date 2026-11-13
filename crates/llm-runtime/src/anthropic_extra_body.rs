//! One logical operation's explicit Anthropic body overrides. Generation,
//! counting and measured-prefix identity must observe the same snapshot.

use crate::{LlmError, LlmRequest, ProtocolFamily};
use lingxi_llm_client::{
    protocol::JsonPayload, providers::anthropic::request_policy::AnthropicRequestKind,
};
use std::borrow::Cow;

/// Host-owned capture; `None` inside a capture is an authoritative empty
/// environment, including across credential awaits and physical retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedAnthropicExtraBody {
    body: Option<JsonPayload>,
    count: Option<JsonPayload>,
    prompt: Option<JsonPayload>,
    failure: Option<LlmError>,
}

pub(crate) fn capture(request: &mut LlmRequest) -> Result<(), LlmError> {
    if request.execution.anthropic_extra_body.is_none() {
        let body = if request.execution.read_token_count {
            // Read counts one scalar source, independently of generation.
            Ok(None)
        } else {
            crate::service::extra_body_object()
        };
        // Preserve a failure too, but only an actual Anthropic-family route
        // may surface it. A Responses primary can still fail over later using
        // the original capture without observing an intervening env change.
        request.execution.anthropic_extra_body = Some(
            match body.and_then(|body| {
                CapturedAnthropicExtraBody::new(body, request.execution.anthropic_request_kind)
            }) {
                Ok(capture) => capture,
                Err(error) => CapturedAnthropicExtraBody {
                    body: None,
                    count: None,
                    prompt: None,
                    failure: Some(error),
                },
            },
        );
    }
    Ok(())
}

pub(crate) fn captured(request: &LlmRequest) -> Result<Cow<'_, LlmRequest>, LlmError> {
    if request.execution.anthropic_extra_body.is_some() {
        return Ok(Cow::Borrowed(request));
    }
    let mut request = request.clone();
    capture(&mut request)?;
    Ok(Cow::Owned(request))
}

pub(crate) fn is_anthropic(protocol: ProtocolFamily) -> bool {
    matches!(
        protocol,
        ProtocolFamily::AnthropicMessages
            | ProtocolFamily::BedrockClaude
            | ProtocolFamily::VertexClaude
            | ProtocolFamily::FoundryClaude
    )
}

pub(crate) fn validate(request: &LlmRequest, protocol: ProtocolFamily) -> Result<(), LlmError> {
    if is_anthropic(protocol) {
        if let Some(failure) = request
            .execution
            .anthropic_extra_body
            .as_ref()
            .and_then(|body| body.failure.as_ref())
        {
            return Err(failure.clone());
        }
    }
    Ok(())
}

pub(crate) fn body(request: &LlmRequest, protocol: ProtocolFamily) -> Option<&JsonPayload> {
    is_anthropic(protocol).then_some(())?;
    request
        .execution
        .anthropic_extra_body
        .as_ref()?
        .body
        .as_ref()
}

pub(crate) fn count(request: &LlmRequest, protocol: ProtocolFamily) -> Option<&JsonPayload> {
    is_anthropic(protocol).then_some(())?;
    request
        .execution
        .anthropic_extra_body
        .as_ref()?
        .count
        .as_ref()
}

pub(crate) fn prompt(request: &LlmRequest) -> Option<&JsonPayload> {
    let protocol = request
        .execution
        .context_usage_protocol
        .or(request.execution.input_protocol)
        .unwrap_or(ProtocolFamily::AnthropicMessages);
    is_anthropic(protocol).then_some(())?;
    request
        .execution
        .anthropic_extra_body
        .as_ref()?
        .prompt
        .as_ref()
}

pub(crate) fn replaces(request: &LlmRequest, field: &str) -> bool {
    prompt(request).is_some_and(|extra| {
        lingxi_llm_client::providers::anthropic::request_policy::has_field_path(extra, &[field])
    })
}

impl CapturedAnthropicExtraBody {
    pub(crate) fn new(
        body: Option<JsonPayload>,
        kind: AnthropicRequestKind,
    ) -> Result<Self, LlmError> {
        let Some(extra) = body.as_ref() else {
            return Ok(Self {
                body: None,
                count: None,
                prompt: None,
                failure: None,
            });
        };
        let mut count = JsonPayload::parse("{}").expect("empty object");
        let mut prompt = count.clone();
        let encoding = lingxi_llm_client::exact_json::JsonEncoding::JavaScript;
        // These fields add or replace model-visible input. Output ceilings,
        // metadata, betas and sampling controls do not invalidate a measurement
        // and are not CountTokens parameters.
        for key in [
            "model",
            "system",
            "messages",
            "tools",
            "tool_choice",
            "output_format",
        ] {
            let Some(value) = extra.payload_at(&format!("/{key}")) else {
                continue;
            };
            if key != "output_format" {
                count = count
                    .with_object_field(key, &value)
                    .map_err(crate::upstream::error)?;
            }
            let clean = match key {
                "system" | "tools" => {
                    crate::prompt_cache::block_array_without_cache_controls(&value, encoding)
                }
                "messages" => {
                    if let Some(length) = value.array_len("") {
                        let messages = (0..length)
                            .map(|index| {
                                crate::prompt_cache::message_without_cache_controls(
                                    &value.array_item(index).expect("array member"),
                                    encoding,
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(crate::upstream::error)?;
                        JsonPayload::array_from_items(&messages, encoding)
                    } else {
                        Ok(value.clone())
                    }
                }
                _ => Ok(value.clone()),
            }
            .map_err(crate::upstream::error)?;
            prompt = prompt
                .with_object_field(key, &clean)
                .map_err(crate::upstream::error)?;
        }
        if let Some(config) = extra.payload_at("/output_config") {
            let mut format = JsonPayload::parse("{}").expect("empty object");
            if let Some(value) = config.payload_at("/format") {
                format = format
                    .with_object_field("format", &value)
                    .map_err(crate::upstream::error)?;
            }
            // Main merges config fields; an effort-only override leaves the
            // computed format intact. Side queries replace the whole slot.
            if kind != AnthropicRequestKind::Main || config.payload_at("/format").is_some() {
                prompt = prompt
                    .with_object_field("output_config", &format)
                    .map_err(crate::upstream::error)?;
            }
        }
        if let Some(thinking) = extra.payload_at("/thinking") {
            count = count
                .with_object_field("thinking", &thinking)
                .map_err(crate::upstream::error)?;
        }
        Ok(Self {
            body,
            count: (count.object_len("") != Some(0)).then_some(count),
            prompt: (prompt.object_len("") != Some(0)).then_some(prompt),
            failure: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_failure_is_deferred_until_an_anthropic_physical_route() {
        let error = LlmError::InvalidRequest {
            message: "captured source failure".into(),
        };
        let mut request = LlmRequest::new("m");
        request.execution.anthropic_extra_body = Some(CapturedAnthropicExtraBody {
            body: None,
            count: None,
            prompt: None,
            failure: Some(error.clone()),
        });
        assert!(validate(&request, ProtocolFamily::OpenAiResponses).is_ok());
        let retry = request.clone();
        assert_eq!(
            validate(&retry, ProtocolFamily::AnthropicMessages),
            Err(error)
        );
    }

    #[test]
    fn count_projection_excludes_generation_only_controls_and_read_remains_isolated() {
        let snapshot = CapturedAnthropicExtraBody::new(Some(JsonPayload::parse(
            r#"{"system":"text","thinking":{"type":"disabled"},"metadata":{"user_id":"fixture"},"max_tokens":8,"stream":true,"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}"#,
        ).unwrap()), AnthropicRequestKind::Main).unwrap();
        let count = snapshot.count.unwrap();
        assert_eq!(count.string_at("/system").as_deref(), Some("text"));
        assert!(count.payload_at("/thinking").is_some());
        for field in ["/metadata", "/max_tokens", "/stream", "/output_config"] {
            assert!(count.payload_at(field).is_none(), "{field}");
        }
        assert!(snapshot
            .prompt
            .unwrap()
            .payload_at("/output_config/format")
            .is_some());
        let mut read = LlmRequest::new("m");
        read.execution.read_token_count = true;
        capture(&mut read).unwrap();
        assert!(body(&read, ProtocolFamily::AnthropicMessages).is_none());
    }
}
