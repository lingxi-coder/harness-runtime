use super::*;

pub(super) fn model_ref(model: &llm_runtime::PricingModelRef) -> cost::ModelRef {
    use llm_runtime::ProviderId as P;
    let provider = match &model.pricing_provider_id {
        P::AnthropicFirstParty | P::FoundryClaude => cost::ProviderId::Anthropic,
        P::OpenAI | P::AzureOpenAI => cost::ProviderId::OpenAI,
        P::Gemini | P::VertexGemini | P::VertexClaude => cost::ProviderId::GoogleGemini,
        P::BedrockClaude => cost::ProviderId::AmazonBedrock,
        P::OpenAICompatible { name } | P::Custom { name } => {
            cost::ProviderId::OpenAICompatible { name: name.clone() }
        }
    };
    cost::ModelRef {
        provider,
        model: model.billing_model.clone(),
    }
}

pub(super) fn captured_prices(
    catalog: &cost::PricingCatalog,
    model: &cost::ModelRef,
    route: &llm_runtime::ResolvedRoute,
    configured: &llm_runtime::PricingConfig,
) -> Result<(cost::ModelPricing, Option<cost::ModelPricing>), String> {
    let declared = configured.overrides.iter().find(|(name, _)| {
        name == &route.display_model
            || name == &route.request_model
            || name == &route.pricing_model.billing_model
    });
    let explicit = declared.is_some();
    if configured.billing_mode == Some(lingxi_core::host::ModelBillingMode::Subscription)
        && !explicit
    {
        return Err("subscription requires explicit attempt pricing".into());
    }
    let (mut price, resolution) = catalog
        .resolve(model)
        .map_err(|_| "attempt route has no price")?;
    let canonical = match resolution {
        cost::PricingResolution::ExactModel { model_ref } => model_ref.model,
        _ => return Err("attempt route requires an exact model price".into()),
    };
    price.model_ref = model.clone();
    if let Some((_, declared)) = declared {
        validate_override(&price, declared)?;
    }
    let reference =
        !explicit && matches!(price.source, cost::PricingSource::BuiltInReference { .. });
    if reference
        && !price
            .token_rates
            .contains_key(&cost::TokenClass::ReasoningOutput)
        && matches!(
            model.provider,
            cost::ProviderId::Anthropic
                | cost::ProviderId::OpenAI
                | cost::ProviderId::GoogleGemini
                | cost::ProviderId::AmazonBedrock
        )
    {
        // Documented first-party reference policy shared with cost_translate.
        // Actual profile overrides were checked above; explicit zero survives.
        if let Some(rate) = price.token_rates.get(&cost::TokenClass::Output).cloned() {
            price
                .token_rates
                .insert(cost::TokenClass::ReasoningOutput, rate);
        }
    }
    let mut fast = if explicit {
        Some(price.clone())
    } else if reference {
        match canonical.as_str() {
            "claude-opus-4-6" | "claude-opus-4-7" => {
                Some(cost::PricingCatalog::opus_4_6_fast_pricing(model))
            }
            "claude-opus-4-8" => Some(cost::PricingCatalog::opus_4_8_fast_pricing(model)),
            _ => None,
        }
    } else {
        None
    };
    if !explicit {
        if let Some(fast) = &mut fast {
            let rate = price
                .token_rates
                .get(&cost::TokenClass::ReasoningOutput)
                .filter(|rate| rate.nano_usd_per_token == 0)
                .cloned()
                .or_else(|| fast.token_rates.get(&cost::TokenClass::Output).cloned());
            if let Some(rate) = rate {
                fast.token_rates
                    .insert(cost::TokenClass::ReasoningOutput, rate);
            }
        }
    }
    Ok((price, fast))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn integer_rate(value: f64) -> Result<u64, String> {
    let scaled = (value * 1000.0).round();
    if !value.is_finite()
        || value < 0.0
        || !scaled.is_finite()
        || scaled >= 18_446_744_073_709_551_616.0
    {
        return Err("invalid profile price override".into());
    }
    Ok(scaled as u64)
}

fn validate_override(
    price: &cost::ModelPricing,
    declared: &llm_runtime::PricingOverride,
) -> Result<(), String> {
    use cost::TokenClass as T;
    for (class, value) in [
        (T::Input, declared.input_per_million),
        (T::Output, declared.output_per_million),
        (T::CacheRead, declared.cache_read_per_million),
        (T::CacheWrite, declared.cache_write_per_million),
    ] {
        let expected = integer_rate(value)?;
        match price.token_rates.get(&class) {
            Some(actual) if actual.nano_usd_per_token == expected => {}
            None if class == T::CacheWrite && expected == 0 => {}
            _ => return Err("profile override and captured catalog disagree".into()),
        }
    }
    let explicit_reasoning = integer_rate(declared.reasoning_per_million)?;
    let actual = price
        .token_rates
        .get(&T::ReasoningOutput)
        .ok_or("missing override reasoning price")?
        .nano_usd_per_token;
    // Parsing materializes the output fallback; an explicit zero stays zero.
    if actual != explicit_reasoning {
        return Err("profile reasoning override and captured catalog disagree".into());
    }
    Ok(())
}

pub(super) fn with_rate_bounds(
    pricing: &cost::ModelPricing,
    rates: &llm_runtime::AttemptTokenRates,
) -> Result<cost::ModelPricing, String> {
    use cost::TokenClass as T;
    let mut bound = pricing.clone();
    bound.token_rates.clear();
    for (class, rate) in [
        (T::Input, rates.input_per_million),
        (T::Output, rates.output_per_million),
        (T::CacheRead, rates.cache_read_per_million),
        (T::CacheWrite, rates.cache_write_per_million),
        (T::CacheWrite1h, rates.cache_write_1h_per_million),
        (T::ReasoningOutput, rates.reasoning_per_million),
    ] {
        if let Some(rate) = rate {
            let nano = (rate * 1000.0).ceil();
            if !nano.is_finite() || nano < 0.0 || nano >= u64::MAX as f64 {
                return Err("invalid attempt price ceiling".into());
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            bound.token_rates.insert(
                class,
                cost::MoneyPerToken {
                    nano_usd_per_token: nano as u64,
                },
            );
        }
    }
    Ok(bound)
}

pub(super) fn token_quote(
    quote: &llm_runtime::CostEstimate,
    model: &cost::ModelRef,
) -> Option<u64> {
    if !quote.estimated || model_ref(&quote.pricing_model) != *model {
        return None;
    }
    let amount = quote.total_cost_usd? * 1_000_000_000.0;
    if !amount.is_finite() || amount < 0.0 || amount >= u64::MAX as f64 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(amount.round() as u64)
}

pub(super) fn quote(
    route: &PinnedRoute,
    prepared: &llm_runtime::PreparedLlmCall,
) -> Result<
    (
        cost::ModelPricing,
        cost::AttemptUsageContract,
        u64,
        u64,
        u64,
    ),
    String,
> {
    quote_body(route, prepared.route.protocol, &prepared.provider_request)
}

pub(super) fn quote_body(
    route: &PinnedRoute,
    protocol: llm_runtime::ProtocolFamily,
    request: &llm_runtime::ProviderRequest,
) -> Result<
    (
        cost::ModelPricing,
        cost::AttemptUsageContract,
        u64,
        u64,
        u64,
    ),
    String,
> {
    use cost::TokenClass as T;
    use llm_runtime::ProtocolFamily as P;
    let contract = match protocol {
        P::OpenAiChat
        | P::OpenAiResponses
        | P::GeminiGenerateContent
        | P::GeminiInteractions
        | P::VertexGemini
        | P::AzureOpenAi => cost::AttemptUsageContract::StandardDisjointTokensV1,
        P::AnthropicMessages | P::VertexClaude | P::BedrockClaude | P::FoundryClaude => {
            cost::AttemptUsageContract::AnthropicCacheTtlV1
        }
    };
    if request.body_bytes.is_some()
        || request.stream_transport != llm_runtime::ProviderStreamTransport::Http
    {
        return Err("registered attempts require canonical JSON HTTP transport".into());
    }
    let body = &request.body_json;
    if !body.is_object() || body.get("web_search_options").is_some() {
        return Err("unsupported provider-native request configuration".into());
    }
    if let Some(tools) = body.get("tools") {
        let tools = tools.as_array().ok_or("invalid tools body")?;
        for tool in tools {
            let ordinary = tool.get("type").and_then(serde_json::Value::as_str) == Some("function")
                || (tool.get("type").is_none()
                    && tool.get("name").is_some()
                    && tool.get("input_schema").is_some())
                || (tool.as_object().is_some_and(|value| value.len() == 1)
                    && tool.get("functionDeclarations").is_some());
            if !ordinary {
                return Err("unbounded provider-native billable tools are unsupported".into());
            }
        }
    }
    let limits = [
        body.get("max_tokens"),
        body.get("max_completion_tokens"),
        body.get("max_output_tokens"),
        body.pointer("/generationConfig/maxOutputTokens"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if limits.len() != 1 {
        return Err("one unambiguous output limit required".into());
    }
    let output = limits[0]
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or("finite output limit required")?;
    if output > u64::from(route.output_cap) {
        return Err("prepared output exceeds captured route limit".into());
    }
    let bytes = request
        .wire_body_bytes()
        .map_err(|_| "invalid canonical body overrides")?;
    let input = llm_runtime::model::count_tokens::approximate_tokens_for_bytes(
        u64::try_from(bytes.len()).map_err(|_| "body length overflow")?,
    );
    let input_cap = route
        .limits
        .input_cap(u32::try_from(output).map_err(|_| "output limit overflow")?)
        .ok_or("finite input limit required")?;
    let input_cap = route
        .input_cap
        .map_or(input_cap, |configured| input_cap.min(configured));
    if input > input_cap {
        return Err("canonical request exceeds captured input limit".into());
    }
    let speed = body
        .get("speed")
        .or_else(|| body.get("service_tier"))
        .map(|speed| speed.as_str().ok_or("invalid effective speed"))
        .transpose()?;
    let price = match speed {
        None if route.default_fast => route.fast.clone().ok_or("missing default fast tier")?,
        None | Some("standard" | "default") => route.pricing.clone(),
        Some("fast" | "priority") => route.fast.clone().ok_or("missing pinned fast tier")?,
        Some(_) => return Err("unknown effective speed".into()),
    };
    let input = if route.token_pricing_requires_quote {
        input_cap
    } else {
        input
    };
    let mut input_classes = vec![T::Input, T::CacheRead];
    if contract == cost::AttemptUsageContract::AnthropicCacheTtlV1 {
        input_classes.extend([T::CacheWrite, T::CacheWrite1h]);
    }
    let rate = |class| {
        price
            .token_rates
            .get(&class)
            .map(|rate| rate.nano_usd_per_token)
            .ok_or("missing reachable token price")
    };
    let input_rate = input_classes
        .into_iter()
        .map(rate)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let output_rate = rate(T::Output)?.max(rate(T::ReasoningOutput)?);
    let money = input
        .checked_mul(input_rate)
        .and_then(|value| {
            output
                .checked_mul(output_rate)
                .and_then(|other| value.checked_add(other))
        })
        .ok_or("attempt quote overflow")?;
    Ok((price, contract, input, output, money))
}
