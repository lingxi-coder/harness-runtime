use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use tool_api::tool_trait::ToolError;

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidInput(Utf16JsonProjection::from(message))
}

fn view<'a>(output: &'a Utf16JsonProjection, field: &str) -> Option<JsonValueView<'a>> {
    output.at_pointer(&format!("/{field}"))
}

fn string(value: JsonValueView<'_>) -> bool {
    value.code_units().is_some()
}

fn number(value: JsonValueView<'_>) -> bool {
    value.as_number().is_some_and(f64::is_finite)
}

fn object(value: JsonValueView<'_>) -> bool {
    value.object_entries().is_some()
}

fn optional(
    output: &Utf16JsonProjection,
    field: &str,
    check: fn(JsonValueView<'_>) -> bool,
) -> bool {
    view(output, field).is_none_or(check)
}

fn strip_object(
    output: &Utf16JsonProjection,
    fields: &[&str],
) -> Result<Utf16JsonProjection, ToolError> {
    let root = output
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Agent output must be an object"))?;
    let mut parsed = Vec::new();
    for field in fields {
        if root.get_property(field).is_some() {
            let value = output
                .subprojection(&format!("/{field}"))
                .map_err(|_| invalid("Agent output object could not be parsed"))?;
            parsed.push(((*field).to_owned(), value));
        }
    }
    Utf16JsonProjection::object(parsed)
        .map_err(|_| invalid("Agent output object could not be parsed"))
}

fn parse_usage(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    let root = output
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Agent output usage must be an object"))?;
    if !root.get_property("input_tokens").is_some_and(number)
        || !root.get_property("output_tokens").is_some_and(number)
        || !optional(output, "cache_creation_input_tokens", |value| {
            value.is_null() || number(value)
        })
        || !optional(output, "cache_read_input_tokens", |value| {
            value.is_null() || number(value)
        })
        || !optional(output, "server_tool_use", |value| {
            value.is_null() || object(value)
        })
        || !optional(output, "service_tier", |value| {
            value.is_null() || string(value)
        })
        || !optional(output, "cache_creation", |value| {
            value.is_null() || object(value)
        })
    {
        return Err(invalid("Agent output usage has an invalid field"));
    }
    strip_object(
        output,
        &[
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "server_tool_use",
            "service_tier",
            "cache_creation",
        ],
    )
}

fn parse_text_blocks(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    let count = output
        .at_pointer("")
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| invalid("Completed Agent output content must be an array"))?;
    let mut parsed = Vec::with_capacity(count);
    for index in 0..count {
        let block = output
            .subprojection(&format!("/{index}"))
            .map_err(|_| invalid("Completed Agent output has an invalid content block"))?;
        let root = block
            .at_pointer("")
            .filter(|value| object(*value))
            .ok_or_else(|| invalid("Completed Agent output has an invalid content block"))?;
        if root.get_property("type").and_then(JsonValueView::as_str) != Some("text")
            || !root.get_property("text").is_some_and(string)
        {
            return Err(invalid("Completed Agent output requires text blocks"));
        }
        parsed.push(strip_object(&block, &["type", "text"])?);
    }
    Utf16JsonProjection::array(parsed)
        .map_err(|_| invalid("Completed Agent output content could not be parsed"))
}

fn parse_strings(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    let count = output
        .at_pointer("")
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| invalid("Agent output model list must be an array"))?;
    let mut parsed = Vec::with_capacity(count);
    for index in 0..count {
        let item = output
            .subprojection(&format!("/{index}"))
            .map_err(|_| invalid("Agent output model list is invalid"))?;
        if !item.at_pointer("").is_some_and(string) {
            return Err(invalid("Agent output model list must contain strings"));
        }
        parsed.push(item);
    }
    Utf16JsonProjection::array(parsed)
        .map_err(|_| invalid("Agent output model list could not be parsed"))
}

fn optional_string(output: &Utf16JsonProjection, field: &str) -> bool {
    optional(output, field, string)
}

fn optional_number(output: &Utf16JsonProjection, field: &str) -> bool {
    optional(output, field, number)
}

fn parse_completed(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    for field in ["agentId", "prompt"] {
        if !view(output, field).is_some_and(string) {
            return Err(invalid("Completed Agent output is missing a string field"));
        }
    }
    for field in ["totalToolUseCount", "totalDurationMs", "totalTokens"] {
        if !view(output, field).is_some_and(number) {
            return Err(invalid("Completed Agent output is missing a numeric field"));
        }
    }
    for field in [
        "agentType",
        "resolvedModel",
        "worktreePath",
        "worktreeBranch",
        "harnessSectionHash",
    ] {
        if !optional_string(output, field) {
            return Err(invalid(
                "Completed Agent output has an invalid optional string field",
            ));
        }
    }
    for field in ["harnessNoteCount", "harnessTailCount"] {
        if !optional_number(output, field) {
            return Err(invalid(
                "Completed Agent output has an invalid optional number field",
            ));
        }
    }

    let content = output
        .subprojection("/content")
        .map_err(|_| invalid("Completed Agent output requires content"))?;
    let content = parse_text_blocks(&content)?;
    let usage = output
        .subprojection("/usage")
        .map_err(|_| invalid("Completed Agent output requires usage"))?;
    let usage = parse_usage(&usage)?;
    let models = output
        .subprojection("/modelsUsed")
        .ok()
        .map(|value| parse_strings(&value))
        .transpose()?;

    let mut parsed = Vec::new();
    for field in ["status", "agentId", "agentType", "prompt"] {
        if let Ok(value) = output.subprojection(&format!("/{field}")) {
            parsed.push((field.to_owned(), value));
        }
    }
    parsed.push(("content".into(), content));
    if let Ok(value) = output.subprojection("/resolvedModel") {
        parsed.push(("resolvedModel".into(), value));
    }
    if let Some(models) = models {
        parsed.push(("modelsUsed".into(), models));
    }
    for field in ["totalToolUseCount", "totalDurationMs", "totalTokens"] {
        parsed.push((
            field.into(),
            output
                .subprojection(&format!("/{field}"))
                .expect("required output field"),
        ));
    }
    parsed.push(("usage".into(), usage));
    for field in [
        "worktreePath",
        "worktreeBranch",
        "harnessNoteCount",
        "harnessTailCount",
        "harnessSectionHash",
    ] {
        if let Ok(value) = output.subprojection(&format!("/{field}")) {
            parsed.push((field.into(), value));
        }
    }
    Utf16JsonProjection::object(parsed)
        .map_err(|_| invalid("Completed Agent output could not be parsed"))
}

fn parse_async_launched(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    for field in ["agentId", "description", "prompt", "outputFile"] {
        if !view(output, field).is_some_and(string) {
            return Err(invalid("Async Agent output is missing a string field"));
        }
    }
    if !optional_string(output, "resolvedModel")
        || !optional(output, "isAsync", |value| value.as_bool().is_some())
        || !optional(output, "canReadOutputFile", |value| {
            value.as_bool().is_some()
        })
    {
        return Err(invalid("Async Agent output has an invalid optional field"));
    }
    let models = output
        .subprojection("/modelsUsed")
        .ok()
        .map(|value| parse_strings(&value))
        .transpose()?;
    let mut parsed = Vec::new();
    for field in [
        "status",
        "isAsync",
        "agentId",
        "description",
        "resolvedModel",
    ] {
        if let Ok(value) = output.subprojection(&format!("/{field}")) {
            parsed.push((field.into(), value));
        }
    }
    if let Some(models) = models {
        parsed.push(("modelsUsed".into(), models));
    }
    for field in ["prompt", "outputFile", "canReadOutputFile"] {
        if let Ok(value) = output.subprojection(&format!("/{field}")) {
            parsed.push((field.into(), value));
        }
    }
    Utf16JsonProjection::object(parsed)
        .map_err(|_| invalid("Async Agent output could not be parsed"))
}

fn parse_teammate_spawned(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    for field in [
        "prompt",
        "teammate_id",
        "agent_id",
        "agent_type",
        "model",
        "name",
        "color",
        "tmux_session_name",
        "tmux_window_name",
        "tmux_pane_id",
        "team_name",
    ] {
        if !view(output, field).is_some_and(string) {
            return Err(invalid("Teammate Agent output is missing a string field"));
        }
    }
    if !optional(output, "model_profile", |value| {
        value.is_null() || string(value)
    }) || !view(output, "is_splitpane").is_some_and(|value| value.as_bool().is_some())
        || !view(output, "plan_mode_required").is_some_and(|value| value.as_bool().is_some())
    {
        return Err(invalid("Teammate Agent output has an invalid field"));
    }
    strip_object(
        output,
        &[
            "status",
            "prompt",
            "teammate_id",
            "agent_id",
            "agent_type",
            "model",
            "model_profile",
            "name",
            "color",
            "tmux_session_name",
            "tmux_window_name",
            "tmux_pane_id",
            "team_name",
            "is_splitpane",
            "plan_mode_required",
        ],
    )
}

fn parse_fusion(output: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    let root = output
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Fusion Agent output must be an object"))?;
    if !root.get_property("runId").is_some_and(string)
        || !matches!(
            root.get_property("mode").and_then(JsonValueView::as_str),
            Some("analysis" | "implement")
        )
        || !matches!(
            root.get_property("status").and_then(JsonValueView::as_str),
            Some("analyzed" | "unanalyzed" | "failed")
        )
    {
        return Err(invalid("Fusion Agent output has invalid run metadata"));
    }
    if !view(output, "analysisFailure").is_some_and(|value| value.is_null() || string(value)) {
        return Err(invalid(
            "Fusion Agent output has an invalid analysis failure",
        ));
    }

    let panels = output
        .subprojection("/panels")
        .map_err(|_| invalid("Fusion Agent output requires panel outcomes"))?;
    let panel_count = panels
        .at_pointer("")
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| invalid("Fusion Agent panel outcomes must be an array"))?;
    let mut parsed_panels = Vec::with_capacity(panel_count);
    for index in 0..panel_count {
        let panel = panels
            .subprojection(&format!("/{index}"))
            .map_err(|_| invalid("Fusion Agent output has an invalid panel outcome"))?;
        let panel_root = panel
            .at_pointer("")
            .filter(|value| object(*value))
            .ok_or_else(|| invalid("Fusion Agent output has an invalid panel outcome"))?;
        if !panel_root.get_property("panel_id").is_some_and(string)
            || !matches!(
                panel_root
                    .get_property("status")
                    .and_then(JsonValueView::as_str),
                Some("completed" | "failed" | "timed_out" | "cancelled")
            )
            || !panel_root.get_property("duration_ms").is_some_and(number)
            || !optional(&panel, "error_category", |value| {
                value.is_null() || string(value)
            })
        {
            return Err(invalid("Fusion Agent output has an invalid panel outcome"));
        }
        parsed_panels.push(strip_object(
            &panel,
            &["panel_id", "status", "duration_ms", "error_category"],
        )?);
    }

    let usage = output
        .subprojection("/usage")
        .map_err(|_| invalid("Fusion Agent output requires usage"))?;
    let usage_root = usage
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Fusion Agent output usage must be an object"))?;
    let usage_fields = [
        "input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "realized_nano_usd",
        "reserved_max_nano_usd",
        "provider_requests",
    ];
    if usage_fields
        .iter()
        .any(|field| !usage_root.get_property(field).is_some_and(number))
        || !usage_root
            .get_property("estimated")
            .is_some_and(|value| value.as_bool().is_some())
    {
        return Err(invalid("Fusion Agent output usage has an invalid field"));
    }
    let usage = strip_object(
        &usage,
        &[
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "realized_nano_usd",
            "reserved_max_nano_usd",
            "estimated",
            "provider_requests",
        ],
    )?;

    let timing = output
        .subprojection("/timing")
        .map_err(|_| invalid("Fusion Agent output requires timing"))?;
    let timing_root = timing
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Fusion Agent output timing must be an object"))?;
    let timing_fields = ["total_ms", "panels_ms", "analyst_ms", "verification_ms"];
    if timing_fields
        .iter()
        .any(|field| !timing_root.get_property(field).is_some_and(number))
    {
        return Err(invalid("Fusion Agent output timing has an invalid field"));
    }
    let timing = strip_object(&timing, &timing_fields)?;

    let egress = output
        .subprojection("/egress")
        .map_err(|_| invalid("Fusion Agent output requires egress profiles"))?;
    let egress_count = egress
        .at_pointer("")
        .and_then(JsonValueView::array_len)
        .ok_or_else(|| invalid("Fusion Agent egress profiles must be an array"))?;
    for index in 0..egress_count {
        let item = egress
            .at_pointer(&format!("/{index}"))
            .ok_or_else(|| invalid("Fusion Agent output has an invalid egress profile"))?;
        if !string(item) {
            return Err(invalid("Fusion Agent output has an invalid egress profile"));
        }
    }

    let settlement = output
        .subprojection("/attemptSettlement")
        .ok()
        .map(|value| parse_fusion_settlement(&value))
        .transpose()?;
    if !optional(output, "computationStatus", |value| {
        matches!(value.as_str(), Some("analyzed" | "unanalyzed"))
    }) {
        return Err(invalid(
            "Fusion Agent output has an invalid computation status",
        ));
    }

    let mut parsed = vec![
        (
            "runId".into(),
            output.subprojection("/runId").expect("validated run id"),
        ),
        (
            "mode".into(),
            output.subprojection("/mode").expect("validated mode"),
        ),
        (
            "status".into(),
            output.subprojection("/status").expect("validated status"),
        ),
        (
            "analysisFailure".into(),
            output
                .subprojection("/analysisFailure")
                .expect("generated Fusion output includes analysisFailure"),
        ),
        (
            "panels".into(),
            Utf16JsonProjection::array(parsed_panels)
                .map_err(|_| invalid("Fusion Agent panel outcomes could not be parsed"))?,
        ),
        ("usage".into(), usage),
        ("timing".into(), timing),
        ("egress".into(), egress),
    ];
    if let Ok(value) = output.subprojection("/computationStatus") {
        parsed.push(("computationStatus".into(), value));
    }
    if let Some(settlement) = settlement {
        parsed.push(("attemptSettlement".into(), settlement));
    }
    Utf16JsonProjection::object(parsed)
        .map_err(|_| invalid("Fusion Agent output could not be parsed"))
}

fn parse_fusion_settlement(
    settlement: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ToolError> {
    let root = settlement
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Fusion Agent settlement must be an object"))?;
    match root.get_property("status").and_then(JsonValueView::as_str) {
        Some("pending" | "settled") => strip_object(settlement, &["status"]),
        Some("failed") if root.get_property("reason").is_some_and(string) => {
            strip_object(settlement, &["status", "reason"])
        }
        _ => Err(invalid("Fusion Agent settlement has an invalid status")),
    }
}

pub(super) fn parse(
    output: &Utf16JsonProjection,
    allow_fusion: bool,
) -> Result<Utf16JsonProjection, ToolError> {
    let root = output
        .at_pointer("")
        .filter(|value| object(*value))
        .ok_or_else(|| invalid("Agent output must be an object"))?;
    match root.get_property("status").and_then(JsonValueView::as_str) {
        Some("completed") => parse_completed(output),
        Some("async_launched") => parse_async_launched(output),
        Some("teammate_spawned") => parse_teammate_spawned(output),
        Some("analyzed" | "unanalyzed" | "failed") if allow_fusion => parse_fusion(output),
        _ => Err(invalid("Agent output status is invalid")),
    }
}
