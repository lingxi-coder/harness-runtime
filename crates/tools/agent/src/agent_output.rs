use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};
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
        .ok_or_else(|| invalid("Agent output must be an object"))?;
    let mut builder = Utf16JsonBuilder::new();
    let root = import_object_fields(&mut builder, root, fields)?;
    builder
        .finish(root)
        .map_err(|_| invalid("Agent output object could not be parsed"))
}

fn import_object_fields(
    builder: &mut Utf16JsonBuilder,
    root: JsonValueView<'_>,
    fields: &[&str],
) -> Result<usize, ToolError> {
    if !object(root) {
        return Err(invalid("Agent output must be an object"));
    }
    let mut parsed = Vec::new();
    for field in fields {
        if let Some(value) = root.get_property(field) {
            let value = builder
                .import_view(value)
                .map_err(|_| invalid("Agent output object could not be parsed"))?;
            parsed.push(((*field).to_owned(), value));
        }
    }
    builder
        .push_object(parsed)
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

fn parse_text_blocks(output: JsonValueView<'_>) -> Result<Utf16JsonProjection, ToolError> {
    let count = output
        .array_len()
        .ok_or_else(|| invalid("Completed Agent output content must be an array"))?;
    let mut builder = Utf16JsonBuilder::new();
    let mut parsed = Vec::with_capacity(count);
    for index in 0..count {
        let root = output
            .get_index(index)
            .filter(|value| object(*value))
            .ok_or_else(|| invalid("Completed Agent output has an invalid content block"))?;
        if root.get_property("type").and_then(JsonValueView::as_str) != Some("text")
            || !root.get_property("text").is_some_and(string)
        {
            return Err(invalid("Completed Agent output requires text blocks"));
        }
        parsed.push(import_object_fields(&mut builder, root, &["type", "text"])?);
    }
    builder
        .push_array(parsed)
        .and_then(|root| builder.finish(root))
        .map_err(|_| invalid("Completed Agent output content could not be parsed"))
}

fn parse_strings(output: JsonValueView<'_>) -> Result<Utf16JsonProjection, ToolError> {
    let count = output
        .array_len()
        .ok_or_else(|| invalid("Agent output model list must be an array"))?;
    let mut builder = Utf16JsonBuilder::new();
    let mut parsed = Vec::with_capacity(count);
    for index in 0..count {
        let item = output
            .get_index(index)
            .filter(|item| !item.is_undefined())
            .ok_or_else(|| invalid("Agent output model list is invalid"))?;
        if !string(item) {
            return Err(invalid("Agent output model list must contain strings"));
        }
        parsed.push(
            builder
                .import_view(item)
                .map_err(|_| invalid("Agent output model list is invalid"))?,
        );
    }
    builder
        .push_array(parsed)
        .and_then(|root| builder.finish(root))
        .map_err(|_| invalid("Agent output model list could not be parsed"))
}

fn optional_string(output: &Utf16JsonProjection, field: &str) -> bool {
    optional(output, field, string)
}

fn optional_number(output: &Utf16JsonProjection, field: &str) -> bool {
    optional(output, field, number)
}

fn parse_task_calls(output: JsonValueView<'_>) -> Result<Utf16JsonProjection, ToolError> {
    let count = output
        .array_len()
        .ok_or_else(|| invalid("Agent output task calls must be an array"))?;
    let mut builder = Utf16JsonBuilder::new();
    let mut parsed = Vec::with_capacity(count);
    for index in 0..count {
        let row = output
            .get_index(index)
            .ok_or_else(|| invalid("Agent output has an invalid task call"))?;
        if !matches!(
            row.get_property("name").and_then(JsonValueView::as_str),
            Some("TaskCreate" | "TaskUpdate")
        ) || !row.get_property("result").is_some_and(string)
        {
            return Err(invalid("Agent output has an invalid task call"));
        }
        parsed.push(import_object_fields(
            &mut builder,
            row,
            &["name", "input", "result"],
        )?);
    }
    builder
        .push_array(parsed)
        .and_then(|root| builder.finish(root))
        .map_err(|_| invalid("Agent output task calls could not be parsed"))
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
    if !optional(output, "canContinueAgent", |value| {
        value.as_bool().is_some()
    }) {
        return Err(invalid(
            "Completed Agent output has an invalid continuation flag",
        ));
    }

    let content = output
        .get_property("content")
        .ok_or_else(|| invalid("Completed Agent output requires content"))?;
    let content = parse_text_blocks(content)?;
    let usage = output
        .subprojection("/usage")
        .map_err(|_| invalid("Completed Agent output requires usage"))?;
    let usage = parse_usage(&usage)?;
    let models = output
        .get_property("modelsUsed")
        .map(parse_strings)
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
        "canContinueAgent",
        "harnessNoteCount",
        "harnessTailCount",
        "harnessSectionHash",
    ] {
        if let Ok(value) = output.subprojection(&format!("/{field}")) {
            parsed.push((field.into(), value));
        }
    }
    if let Some(calls) = output.get_property("taskCalls") {
        parsed.push(("taskCalls".into(), parse_task_calls(calls)?));
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
        || !optional(output, "isAsync", |value| value.as_bool() == Some(true))
        || !optional(output, "canReadOutputFile", |value| {
            value.as_bool().is_some()
        })
        || !optional(output, "canContinueAgent", |value| {
            value.as_bool().is_some()
        })
        || !optional(output, "sharesCwd", |value| value.as_bool().is_some())
    {
        return Err(invalid("Async Agent output has an invalid optional field"));
    }
    let models = output
        .get_property("modelsUsed")
        .map(parse_strings)
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
    for field in [
        "prompt",
        "outputFile",
        "canReadOutputFile",
        "canContinueAgent",
        "sharesCwd",
    ] {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn error(result: Result<Utf16JsonProjection, ToolError>) -> String {
        let ToolError::InvalidInput(message) = result.unwrap_err() else {
            panic!("expected Agent output input error");
        };
        message.string_at("").unwrap().to_owned()
    }

    #[test]
    fn borrowed_array_parsers_keep_native_validation_errors() {
        for (source, expected) in [
            ("null", "Completed Agent output content must be an array"),
            (
                "[null]",
                "Completed Agent output has an invalid content block",
            ),
            (
                r#"[{"type":"image","text":"x"}]"#,
                "Completed Agent output requires text blocks",
            ),
            (
                r#"[{"type":"text","text":null}]"#,
                "Completed Agent output requires text blocks",
            ),
        ] {
            let input = Utf16JsonProjection::parse(source).unwrap();
            assert_eq!(
                error(parse_text_blocks(input.at_pointer("").unwrap())),
                expected
            );
        }
        for (source, expected) in [
            ("null", "Agent output task calls must be an array"),
            ("[null]", "Agent output has an invalid task call"),
            (
                r#"[{"name":"TaskDelete","result":"x"}]"#,
                "Agent output has an invalid task call",
            ),
            (
                r#"[{"name":"TaskCreate","result":null}]"#,
                "Agent output has an invalid task call",
            ),
        ] {
            let input = Utf16JsonProjection::parse(source).unwrap();
            assert_eq!(
                error(parse_task_calls(input.at_pointer("").unwrap())),
                expected
            );
        }
        let sparse = Utf16JsonProjection::sparse_array(1, Vec::new()).unwrap();
        let root = sparse.at_pointer("").unwrap();
        assert_eq!(
            error(parse_text_blocks(root)),
            "Completed Agent output has an invalid content block"
        );
        assert_eq!(
            error(parse_task_calls(root)),
            "Agent output has an invalid task call"
        );
        assert_eq!(
            error(parse_strings(root)),
            "Agent output model list is invalid"
        );
        let explicit_null = Utf16JsonProjection::parse("[null]").unwrap();
        assert_eq!(
            error(parse_strings(explicit_null.at_pointer("").unwrap())),
            "Agent output model list must contain strings"
        );

        let named_row = Utf16JsonProjection::sparse_array_with_properties(
            0,
            Vec::new(),
            vec![
                ("name".encode_utf16().collect(), "TaskCreate".into()),
                ("result".encode_utf16().collect(), "ok".into()),
            ],
        )
        .unwrap();
        let rows = Utf16JsonProjection::array(vec![named_row]).unwrap();
        assert_eq!(
            error(parse_task_calls(rows.at_pointer("").unwrap())),
            "Agent output must be an object"
        );
    }

    #[test]
    fn borrowed_task_call_input_keeps_sparse_named_properties_and_exact_keys() {
        let input = Utf16JsonProjection::sparse_array_with_properties(
            u32::MAX,
            vec![(
                9,
                Utf16JsonProjection::parse(r#"{"\ud800":"\udc00"}"#).unwrap(),
            )],
            vec![(
                vec![0xdc00],
                Utf16JsonProjection::parse(r#""\ud800""#).unwrap(),
            )],
        )
        .unwrap();
        let row = Utf16JsonProjection::object(vec![
            ("result".into(), "ok".into()),
            ("ignored".into(), "extra".into()),
            ("input".into(), input.clone()),
            ("name".into(), "TaskUpdate".into()),
        ])
        .unwrap();
        let rows = Utf16JsonProjection::array(vec![row]).unwrap();
        let parsed = parse_task_calls(rows.at_pointer("").unwrap()).unwrap();
        assert_eq!(parsed.subprojection("/0/input").unwrap(), input);
        assert!(parsed.at_pointer("/0/ignored").is_none());
        let row = parsed.at_pointer("/0").unwrap().object_entries().unwrap();
        assert_eq!(
            row.iter()
                .map(|entry| entry.display_key)
                .collect::<Vec<_>>(),
            ["name", "input", "result"]
        );
    }

    #[test]
    fn borrowed_completed_arrays_preserve_large_shape_exact_bytes_and_stripping() {
        let block = r#"{"text":"\ud800","ignored":{"nested":[1,2,3]},"type":"text"}"#;
        let call =
            r#"{"result":"\udc00","ignored":true,"input":{"\ud800":"\udc00"},"name":"TaskCreate"}"#;
        for count in [0, 1, 256, 4_096] {
            let blocks = std::iter::repeat_n(block, count)
                .collect::<Vec<_>>()
                .join(",");
            let calls = std::iter::repeat_n(call, count)
                .collect::<Vec<_>>()
                .join(",");
            let models = std::iter::repeat_n(r#""\ud800""#, count)
                .collect::<Vec<_>>()
                .join(",");
            let output = Utf16JsonProjection::parse(&format!(r#"{{"status":"completed","agentId":"a","prompt":"p","content":[{blocks}],"modelsUsed":[{models}],"totalToolUseCount":1,"totalDurationMs":2,"totalTokens":3,"usage":{{"input_tokens":4,"output_tokens":5}},"taskCalls":[{calls}],"ignored":true}}"#)).unwrap();
            let parsed = parse(&output, false).unwrap();
            let clean_blocks = std::iter::repeat_n(r#"{"type":"text","text":"\ud800"}"#, count)
                .collect::<Vec<_>>()
                .join(",");
            let clean_calls = std::iter::repeat_n(
                r#"{"name":"TaskCreate","input":{"\ud800":"\udc00"},"result":"\udc00"}"#,
                count,
            )
            .collect::<Vec<_>>()
            .join(",");
            assert_eq!(
                parsed.to_json_string().unwrap(),
                format!(
                    r#"{{"status":"completed","agentId":"a","prompt":"p","content":[{clean_blocks}],"modelsUsed":[{models}],"totalToolUseCount":1,"totalDurationMs":2,"totalTokens":3,"usage":{{"input_tokens":4,"output_tokens":5}},"taskCalls":[{clean_calls}]}}"#
                )
            );
        }
    }

    #[test]
    fn borrowed_completed_arrays_preserve_cross_field_validation_order() {
        let output = Utf16JsonProjection::parse(r#"{"status":"completed","agentId":"a","prompt":"p","content":[null],"totalToolUseCount":1,"totalDurationMs":2,"totalTokens":3,"usage":null,"modelsUsed":[null],"taskCalls":[null]}"#).unwrap();
        assert_eq!(
            error(parse(&output, false)),
            "Completed Agent output has an invalid content block"
        );
        let mut output = output;
        output
            .set_field("content", Utf16JsonProjection::parse("[]").unwrap())
            .unwrap();
        assert_eq!(
            error(parse(&output, false)),
            "Agent output usage must be an object"
        );
        output
            .set_field(
                "usage",
                Utf16JsonProjection::parse(r#"{"input_tokens":1,"output_tokens":2}"#).unwrap(),
            )
            .unwrap();
        assert_eq!(
            error(parse(&output, false)),
            "Agent output model list must contain strings"
        );
        output
            .set_field("modelsUsed", Utf16JsonProjection::parse("[]").unwrap())
            .unwrap();
        assert_eq!(
            error(parse(&output, false)),
            "Agent output has an invalid task call"
        );
    }
}
