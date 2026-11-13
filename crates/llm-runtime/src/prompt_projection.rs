//! Pure prompt-content projection shared by generation and token accounting.
//! Credential loading, cache observations and generation controls stay at egress.

use crate::LlmRequest;
use lingxi_llm_client::{protocol as wire, providers::anthropic::system_prompt as prompt};
use std::borrow::Cow;

pub(crate) fn apply_system(
    request: &mut LlmRequest,
    profile: Option<&wire::ProviderProfile>,
    protocol: wire::ProtocolFamily,
    policy: prompt::CachePolicy,
    cache_markers: bool,
) {
    let Some(projection) = project_system(request, profile, protocol, policy) else {
        return;
    };
    request.input.system = projection.iter().map(|item| item.block.clone()).collect();
    request
        .input
        .prompt_cache
        .breakpoints
        .retain(|point| !matches!(point.position, wire::CachePosition::System { .. }));
    if cache_markers {
        request.input.prompt_cache.breakpoints.extend(
            projection
                .into_iter()
                .filter_map(|item| item.cache_breakpoint),
        );
    }
}

fn project_system(
    request: &LlmRequest,
    profile: Option<&wire::ProviderProfile>,
    protocol: wire::ProtocolFamily,
    policy: prompt::CachePolicy,
) -> Option<Vec<prompt::EncodedSystemBlock>> {
    let Some(context) = request.execution.prompt_cache.as_ref() else {
        return None;
    };
    let projection = if request.execution.classifier_system_blocks {
        prompt::project_classifier_system_blocks(&request.input.system, profile, protocol, policy)
    } else {
        use lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind;
        let include_native_identity = request.execution.anthropic_request_kind
            == AnthropicRequestKind::Main
            || (request.execution.anthropic_request_kind == AnthropicRequestKind::SideQuery
                && request
                    .execution
                    .include_native_system_identity_for_side_query
                && request.execution.query_source.as_deref() == Some("insights"));
        let prefixed = context
            .native_system_prefix
            .as_ref()
            .and_then(|(prefix, attribution)| {
                profile.map(|profile| {
                    if include_native_identity {
                        prompt::prepend_native_system_prefix(
                            context.system.as_ref(),
                            prefix,
                            attribution,
                            profile,
                        )
                    } else {
                        prompt::prepend_native_attribution(
                            context.system.as_ref(),
                            prefix,
                            attribution,
                            profile,
                        )
                    }
                })
            });
        prefixed
            .as_ref()
            .or(context.system.as_ref())
            .map_or_else(Vec::new, |system| {
                prompt::project_system_prompt(
                    system,
                    profile,
                    &request.input.model,
                    protocol,
                    policy,
                )
            })
    };
    Some(projection)
}

/// Hashing/approximation needs only projected system text. Keep opaque message
/// receipts borrowed instead of cloning the full request on every cache hit.
pub(crate) fn system_for_route<'a>(
    request: &'a LlmRequest,
    profile: Option<&wire::ProviderProfile>,
    protocol: wire::ProtocolFamily,
) -> Cow<'a, [wire::SystemBlock]> {
    let Some(context) = request.execution.prompt_cache.as_ref() else {
        return Cow::Borrowed(&request.input.system);
    };
    Cow::Owned(
        project_system(request, profile, protocol, context.policy.clone())
            .expect("prompt-cache context is present")
            .into_iter()
            .map(|item| item.block)
            .collect(),
    )
}

pub(crate) fn system_for_estimate(request: &LlmRequest) -> Cow<'_, [wire::SystemBlock]> {
    system_for_route(
        request,
        request.execution.prompt_projection_profile.as_ref(),
        request
            .execution
            .context_usage_protocol
            .or(request.execution.input_protocol)
            .unwrap_or(wire::ProtocolFamily::AnthropicMessages),
    )
}

/// The pinned codec omits output_config in CountTokens mode. Restore its
/// canonical input-affecting format before the captured Main/SideQuery merge.
pub(crate) fn apply_count_output_format(
    request: &LlmRequest,
    body: &mut wire::JsonPayload,
) -> Result<(), crate::LlmError> {
    let wire::OutputFormat::JsonSchema { schema, .. } = &request.input.output_format else {
        return Ok(());
    };
    let format = wire::JsonPayload::parse(r#"{"type":"json_schema"}"#)
        .expect("static format object")
        .with_object_field("schema", schema)
        .map_err(crate::upstream::error)?;
    let config = wire::JsonPayload::parse("{}")
        .expect("empty object")
        .with_object_field("format", &format)
        .map_err(crate::upstream::error)?;
    *body = body
        .with_object_field("output_config", &config)
        .map_err(crate::upstream::error)?;
    Ok(())
}

/// Project only model-visible content. Counting must not run generation's
/// credential/account/effort or retry policies merely to recover deferred text.
pub(crate) fn for_count<'a>(
    request: &'a LlmRequest,
    profile: Option<&wire::ProviderProfile>,
    protocol: wire::ProtocolFamily,
) -> Cow<'a, LlmRequest> {
    let Some(context) = &request.execution.prompt_cache else {
        return Cow::Borrowed(request);
    };
    let policy = context.policy.clone();
    let mut projected = request.clone();
    apply_system(&mut projected, profile, protocol, policy, false);
    Cow::Owned(projected)
}
