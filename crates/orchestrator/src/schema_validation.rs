//! JSON-Schema input-validation for tool dispatch — the Rust analogue of
//! claude-code's `inputSchema.safeParse` (`toolExecution.ts:615`).
//!
//! Boon validates exported JSON Schema; tools additionally retain native Zod
//! refinement issues at the tool boundary. Diagnostics use 2.1.263 `zue`.

use boon::{Compiler, Schemas, ValidationError};
use tool_api::native_schema::{
    format_issues, js_json, js_keys, js_number, js_value_eq, quoted_options, value_type,
    NativeSchemaError as ToolSchemaError,
};

/// Recursively flatten a boon [`ValidationError`] into concise leaf messages.
///
/// boon's `Display` is multi-line and prefixed (`jsonschema validation failed
/// with …`); we want a single compact line suitable for a `<tool_use_error>`
/// body, so we walk to the leaf causes and render `at '<loc>': <kind>` for each.
fn flatten(err: &ValidationError, out: &mut Vec<String>) {
    if err.causes.is_empty() {
        let loc = err.instance_location.to_string();
        let loc = if loc.is_empty() {
            "(root)".to_string()
        } else {
            loc
        };
        out.push(format!("at '{loc}': {}", err.kind));
    } else {
        for cause in &err.causes {
            flatten(cause, out);
        }
    }
}

/// Validate a tool input against its JSON Schema (claude-code's
/// `inputSchema.safeParse`). Returns a concise human-readable error on failure.
/// BEHAVIORAL parity only — the message bytes intentionally differ from
/// claude-code's Zod `formatZodValidationError` output (unportable).
///
/// If the schema itself fails to COMPILE (malformed schema), this returns `Ok`
/// (PASS) and logs a `tracing::warn!`: claude-code's tools always carry valid
/// Zod schemas, so a compile failure is a `LingXi` schema bug, not a model-input
/// error — we must not block a tool call because its own schema is broken.
///
/// PERF: this compiles the schema on every call. Tools are few and schemas are
/// small, so the cost is negligible; if a hot path emerges, memoize the
/// compiled `boon::SchemaIndex` per tool (keyed by the schema's identity).
pub(crate) fn validate_tool_input_schema(
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<(), String> {
    // boon addresses schemas by URL; an in-memory pseudo-URL is sufficient for a
    // self-contained, dependency-free tool schema (no `$ref` to external docs).
    const URL: &str = "mem://tool-input-schema";
    let mut schemas = Schemas::new();
    let mut compiler = Compiler::new();

    if let Err(e) = compiler.add_resource(URL, schema.clone()) {
        tracing::warn!(
            error = %e,
            "tool input_schema failed to load (treating as PASS — `LingXi` schema bug, not model input error)"
        );
        return Ok(());
    }
    let sch = match compiler.compile(URL, &mut schemas) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "tool input_schema failed to compile (treating as PASS — `LingXi` schema bug, not model input error)"
            );
            return Ok(());
        }
    };

    if let Err(err) = schemas.validate(input, sch) {
        let mut out = Vec::new();
        flatten(&err, &mut out);
        // Defensive: a root error with no causes and an empty-rendering `kind`
        // would otherwise yield an empty message; fall back to the kind string.
        if out.is_empty() {
            out.push(err.kind.to_string());
        }
        return Err(out.join("; "));
    }
    Ok(())
}

/// Zod v4's issue objects survive at the tool boundary for constraints that
/// JSON Schema cannot express. `zue` groups structural errors, falling back to
/// the two-space JSON representation of *all* issues only when none group.
pub(crate) fn validate_tool_schema(
    tool: &dyn tool_api::Tool,
    input: &serde_json::Value,
) -> Result<(), String> {
    validate_tool_schema_detailed(tool, input).map_err(|error| error.display)
}

pub(crate) fn validate_tool_schema_detailed(
    tool: &dyn tool_api::Tool,
    input: &serde_json::Value,
) -> Result<(), ToolSchemaError> {
    if let Some(parsed) = tool.parse_native_input(input) {
        return parsed.map(|_| ());
    }
    validate_tool_schema_values(tool, tool.input_validation_schema(), input)
}

/// Validate exact inputs and schemas in one reversible consumer namespace.
pub(crate) fn validate_tool_schema_projected(
    tool: &dyn tool_api::Tool,
    input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<(), ToolSchemaError> {
    use lingxi_core::types::utf16_json::{Utf16JsonConsumer, Utf16JsonProjection};
    let schema = tool
        .input_schema_projection()
        .filter(|schema| &schema.value == tool.input_validation_schema())
        .unwrap_or_else(|| Utf16JsonProjection::plain(tool.input_validation_schema().clone()));
    if schema.strings.is_empty()
        && schema.keys.is_empty()
        && input.strings.is_empty()
        && input.keys.is_empty()
    {
        return validate_tool_schema_detailed(tool, &input.value);
    }
    let error = |message: String| ToolSchemaError {
        raw: message.clone(),
        display: message,
    };
    // Rust regex engines cannot represent JavaScript's isolated surrogate
    // character classes. Do not turn a failed regex compilation into PASS.
    let mut pending = vec![&schema.value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::Object(object) => {
                if object.contains_key("pattern") || object.contains_key("patternProperties") {
                    return Err(error(
                        "Exact UTF-16 schema regex validation is unavailable".into(),
                    ));
                }
                pending.extend(object.values());
            }
            serde_json::Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    let consumer =
        Utf16JsonConsumer::new(&[&schema, input]).map_err(|cause| error(cause.to_string()))?;
    validate_tool_schema_values(tool, &consumer.values()[0], &consumer.values()[1])
}

fn validate_tool_schema_values(
    tool: &dyn tool_api::Tool,
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<(), ToolSchemaError> {
    let mut issues = Vec::new();
    let fallback = validate_tool_input_schema(schema, input).err();
    if !collect_issues(schema, Some(input), &[], &mut issues) {
        return fallback.map_or(Ok(()), |display| {
            Err(ToolSchemaError {
                raw: display.clone(),
                display,
            })
        });
    }
    let structural_count = issues.len();
    for native in tool.input_validation_issues(input) {
        if let Some(existing) = issues[..structural_count]
            .iter_mut()
            .find(|issue| issue["code"] == native["code"] && issue["path"] == native["path"])
        {
            *existing = native;
        } else {
            issues.push(native);
        }
    }
    if issues.is_empty() {
        return Ok(());
    }
    issues.sort_by_key(|issue| {
        declaration_order(
            schema,
            issue["path"].as_array().map_or(&[][..], Vec::as_slice),
        )
    });
    Err(ToolSchemaError {
        display: format_issues(tool.name(), &issues),
        raw: js_json(&serde_json::Value::Array(issues), true),
    })
}

#[cfg(test)]
pub(crate) fn validate_named_tool_input_schema(
    tool_name: &str,
    schema: &serde_json::Value,
    input: &serde_json::Value,
) -> Result<(), String> {
    let mut issues = Vec::new();
    if collect_issues(schema, Some(input), &[], &mut issues) {
        if issues.is_empty() {
            Ok(())
        } else {
            Err(format_issues(tool_name, &issues))
        }
    } else {
        validate_tool_input_schema(schema, input)
    }
}

fn declaration_order(schema: &serde_json::Value, path: &[serde_json::Value]) -> Vec<usize> {
    let mut schema = schema;
    let mut order = Vec::new();
    for part in path {
        if let Some(key) = part.as_str() {
            let properties = schema
                .get("properties")
                .and_then(serde_json::Value::as_object);
            order.push(
                properties
                    .and_then(|props| js_keys(props).into_iter().position(|name| name == key))
                    .unwrap_or(usize::MAX),
            );
            schema = properties
                .and_then(|props| props.get(key))
                .unwrap_or(&serde_json::Value::Null);
        } else {
            order.push(
                part.as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(usize::MAX),
            );
            schema = schema.get("items").unwrap_or(&serde_json::Value::Null);
        }
    }
    // Object refinements run after child schemas (a parent path is last).
    order.push(usize::MAX);
    order
}

fn collect_issues(
    schema: &serde_json::Value,
    input: Option<&serde_json::Value>,
    path: &[serde_json::Value],
    issues: &mut Vec<serde_json::Value>,
) -> bool {
    use serde_json::{json, Value};
    if path.is_empty() {
        if let Some(flat) =
            input.and_then(|input| tool_api::native_schema::collect_flat_issues(schema, input))
        {
            issues.extend(flat);
            return true;
        }
    }
    let Some(object) = schema.as_object() else {
        return schema == &json!(true);
    };
    // Unexported Zod metadata comes from Tool::input_validation_issues. Never
    // manufacture errors for unknown JSON Schema vocabulary.
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "title"
                | "description"
                | "$schema"
                | "default"
                | "enum"
                | "const"
                | "anyOf"
                | "oneOf"
                | "pattern"
                | "format"
                | "minLength"
                | "maxLength"
                | "minItems"
                | "maxItems"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
        )
    }) {
        return false;
    }
    if let Some(branches) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    {
        let mut errors = Vec::new();
        let mut dirty = None;
        for branch in branches {
            let mut branch_issues = Vec::new();
            if !collect_issues(branch, input, &[], &mut branch_issues) {
                return false;
            }
            if branch_issues.is_empty() {
                return true;
            }
            if dirty.is_none()
                && branch_issues.iter().all(|issue| {
                    !matches!(
                        issue["code"].as_str(),
                        Some("invalid_type" | "invalid_value" | "invalid_union")
                    )
                })
            {
                dirty = Some(branch_issues.clone());
            }
            errors.push(json!(branch_issues));
        }
        if let Some(mut dirty) = dirty {
            for issue in &mut dirty {
                let mut full = path.to_vec();
                full.extend(issue["path"].as_array().into_iter().flatten().cloned());
                issue["path"] = json!(full);
            }
            issues.extend(dirty);
        } else {
            issues.push(json!({"code":"invalid_union","errors":errors,"path":path,"message":"Invalid input"}));
        }
        return true;
    }
    let values = schema
        .get("enum")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| schema.get("const").map(|value| vec![value.clone()]));
    if let Some(values) = values {
        if !values
            .iter()
            .any(|value| input.is_some_and(|input| js_value_eq(input, value)))
        {
            let message = if values.len() == 1 {
                format!("Invalid input: expected {}", js_json(&values[0], false))
            } else {
                format!(
                    "Invalid option: expected one of {}",
                    quoted_options(&values, "|")
                )
            };
            issues.push(
                json!({"code":"invalid_value","values":values,"path":path,"message":message}),
            );
        }
        return true;
    }
    let received = value_type(input);
    let expected = schema.get("type").and_then(Value::as_str);
    if schema.get("type").is_some() && expected.is_none() {
        return false;
    }
    if let Some(expected) = expected {
        if !matches!(
            expected,
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        ) {
            return false;
        }
        let matches = expected == received
            || (expected == "integer"
                && input.is_some_and(|v| v.as_f64().is_some_and(|n| n.fract() == 0.0)));
        if !matches {
            if expected == "integer" && received == "number" {
                issues.push(json!({"expected":"int","format":"safeint","code":"invalid_type","path":path,"message":"Invalid input: expected int, received number"}));
            } else {
                issues.push(json!({"expected":expected,"code":"invalid_type","path":path,"message":format!("Invalid input: expected {expected}, received {received}")}));
            }
            return true;
        }
    }

    let Some(input) = input else {
        return false;
    };
    if let Some(input) = input.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        let required = schema.get("required").and_then(Value::as_array);
        if required.is_some_and(|keys| {
            keys.iter().any(|key| {
                !properties
                    .is_some_and(|props| key.as_str().is_some_and(|key| props.contains_key(key)))
            })
        }) {
            return false;
        }
        if let Some(properties) = properties {
            for key in js_keys(properties) {
                let child = &properties[key];
                let value = input.get(key);
                if value.is_none()
                    && !required.is_some_and(|keys| keys.iter().any(|v| v.as_str() == Some(key)))
                {
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(json!(key));
                if !collect_issues(child, value, &child_path, issues) {
                    return false;
                }
            }
        }
        if schema.get("additionalProperties") == Some(&json!(false)) {
            let keys: Vec<_> = js_keys(input)
                .into_iter()
                // Native Zod intentionally ignores __proto__ while walking
                // object keys, even for strict objects.
                .filter(|key| {
                    key.as_str() != "__proto__" && !properties.is_some_and(|p| p.contains_key(*key))
                })
                .cloned()
                .collect();
            if !keys.is_empty() {
                issues.push(json!({"code":"unrecognized_keys","keys":keys,"path":path,"message":format!("Unrecognized key{}: {}", if keys.len() == 1 { "" } else { "s" }, keys.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "))}));
            }
        } else if let Some(extra_schema) =
            schema.get("additionalProperties").filter(|v| v.is_object())
        {
            for (key, value) in input {
                if properties.is_some_and(|props| props.contains_key(key)) {
                    continue;
                }
                let mut child_path = path.to_vec();
                child_path.push(json!(key));
                if !collect_issues(extra_schema, Some(value), &child_path, issues) {
                    return false;
                }
            }
        }
    }
    if let (Some(value), Some(pattern)) = (
        input.as_str(),
        schema.get("pattern").and_then(Value::as_str),
    ) {
        let Ok(regex) = regex::Regex::new(pattern) else {
            return false;
        };
        if !regex.is_match(value) {
            issues.push(json!({"origin":"string","code":"invalid_format","format":"regex","pattern":format!("/{pattern}/"),"path":path,"message":format!("Invalid string: must match pattern /{pattern}/")}));
        }
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str) {
        if format != "uri" {
            return false;
        }
        if input
            .as_str()
            .is_some_and(|value| url::Url::parse(value).is_err())
        {
            issues.push(
                json!({"code":"invalid_format","format":"url","path":path,"message":"Invalid URL"}),
            );
        }
    }
    if let (Some(array), Some(items)) = (input.as_array(), schema.get("items")) {
        for (index, value) in array.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(json!(index));
            if !collect_issues(items, Some(value), &child_path, issues) {
                return false;
            }
        }
    }
    // Zod's string length checks count UTF-16 code units, as JavaScript does.
    let measure = input
        .as_str()
        .map(|s| ("string", s.encode_utf16().count() as f64))
        .or_else(|| input.as_array().map(|v| ("array", v.len() as f64)))
        .or_else(|| input.as_f64().map(|n| ("number", n)));
    if let Some((kind, value)) = measure {
        for (key, small, inclusive) in [
            ("minLength", true, true),
            ("maxLength", false, true),
            ("minItems", true, true),
            ("maxItems", false, true),
            ("minimum", true, true),
            ("maximum", false, true),
            ("exclusiveMinimum", true, false),
            ("exclusiveMaximum", false, false),
        ] {
            let Some(limit) = schema.get(key) else {
                continue;
            };
            let Some(bound) = limit.as_f64() else {
                return false;
            };
            let fails = if small {
                value < bound || (!inclusive && value == bound)
            } else {
                value > bound || (!inclusive && value == bound)
            };
            if !fails {
                continue;
            }
            let display_limit = limit
                .as_number()
                .map_or_else(|| limit.to_string(), js_number);
            let relation = if small {
                if inclusive {
                    ">="
                } else {
                    ">"
                }
            } else if inclusive {
                "<="
            } else {
                "<"
            };
            let size = if small { "small" } else { "big" };
            let message = match kind {
                "string" => {
                    format!(
                        "Too {size}: expected string to have {relation}{display_limit} characters"
                    )
                }
                "array" => {
                    format!("Too {size}: expected array to have {relation}{display_limit} items")
                }
                _ => format!("Too {size}: expected number to be {relation}{display_limit}"),
            };
            let mut issue = serde_json::Map::new();
            issue.insert("origin".into(), json!(kind));
            issue.insert(
                "code".into(),
                json!(if small { "too_small" } else { "too_big" }),
            );
            issue.insert(
                if small { "minimum" } else { "maximum" }.into(),
                limit.clone(),
            );
            issue.insert("inclusive".into(), json!(inclusive));
            issue.insert("path".into(), json!(path));
            issue.insert("message".into(), json!(message));
            issues.push(Value::Object(issue));
        }
    }

    true
}

/// Validate a `PostToolUse`-hook `updatedToolOutput` replacement against the
/// tool's declared output JSON Schema (claude-code's `outputSchema.safeParse`,
/// BIN off 202169384: `e.outputSchema?.safeParse(D.updatedToolOutput)
/// ?.success!==!1`). Returns the concise error detail on a schema MISMATCH so
/// the caller can surface the `... does not match <tool>'s output shape ...`
/// meta message (BIN off 202465455) and keep the original output.
///
/// Identical compile/validate logic to [`validate_tool_input_schema`]: a
/// schema that fails to COMPILE is treated as PASS (a `LingXi` schema bug must
/// not discard a hook's valid replacement), matching the `?.` short-circuit in
/// the binary (`success!==!1` is also satisfied when `safeParse` is `undefined`
/// because `outputSchema` is absent — handled by the caller's `None` branch).
pub(crate) fn validate_tool_output_schema(
    schema: &serde_json::Value,
    output: &serde_json::Value,
) -> Result<(), String> {
    tool_api::output_schema::validate(schema, output)
}

#[cfg(test)]
mod tests {
    use super::{
        validate_named_tool_input_schema, validate_tool_input_schema, validate_tool_output_schema,
    };
    use serde_json::json;

    struct NativeCronSchema {
        projection: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        name: String,
        schema: serde_json::Value,
    }
    #[async_trait::async_trait]
    impl tool_api::Tool for NativeCronSchema {
        fn input_schema_projection(
            &self,
        ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
            self.projection.clone()
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn max_result_size_chars(&self) -> usize {
            1000
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            false
        }
        fn input_schema(&self) -> &serde_json::Value {
            &self.schema
        }
        fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
            true
        }
        async fn description(
            &self,
            _: &serde_json::Value,
            _: &tool_api::tool_trait::DescriptionOptions,
        ) -> String {
            unreachable!()
        }
        async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> String {
            unreachable!()
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &tool_api::context::ToolUseContext,
        ) -> permission::PermissionResult {
            unreachable!()
        }
        async fn call(
            &self,
            _: serde_json::Value,
            _: tool_api::context::ToolUseContext,
            _: tool_api::progress::ToolProgressSender,
        ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
            unreachable!()
        }
    }

    #[test]
    fn projected_schema_matches_exact_keys_required_enums_and_utf16_lengths() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection as Projection;
        let projection = Projection::parse(r#"{"type":"object","properties":{"\ud800":{"type":"string","enum":["\ud801"],"minLength":1,"maxLength":1},"\ue000":{"type":"string"}},"required":["\ud800"],"additionalProperties":false}"#).unwrap();
        let tool = NativeCronSchema {
            name: "Exact".into(),
            schema: projection.value.clone(),
            projection: Some(projection),
        };
        for raw in [
            r#"{"\ud800":"\ud801"}"#,
            r#"{"\ue000":"plain","\ud800":"\ud801"}"#,
        ] {
            assert!(
                super::validate_tool_schema_projected(&tool, &Projection::parse(raw).unwrap())
                    .is_ok(),
                "{raw}"
            );
        }
        for raw in [
            r#"{"\ud801":"\ud801"}"#,
            r#"{"\ud800":"\ud800"}"#,
            r#"{"\ue000":"\ud801"}"#,
            r#"{"\ud800":"\ud801","\ud802":"extra"}"#,
        ] {
            assert!(
                super::validate_tool_schema_projected(&tool, &Projection::parse(raw).unwrap())
                    .is_err(),
                "{raw}"
            );
        }
        let projection = Projection::parse(
            r#"{"type":"object","properties":{"value":{"type":"string","pattern":"\\uE000"}}}"#,
        )
        .unwrap();
        let tool = NativeCronSchema {
            name: "Exact".into(),
            schema: projection.value.clone(),
            projection: Some(projection),
        };
        let error = super::validate_tool_schema_projected(
            &tool,
            &Projection::parse(r#"{"value":"\ud800"}"#).unwrap(),
        )
        .unwrap_err();
        assert!(error.display.contains("regex validation is unavailable"));
    }

    #[test]
    fn cron_outer_diagnostics_match_executed_native_2_1_270_zod() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/cron/tests/fixtures/malformed_input_2_1_270.json"
        ))
        .unwrap();
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            let name = case["tool"].as_str().unwrap();
            let tool = NativeCronSchema {
                projection: None,
                name: name.into(),
                schema: oracle["schemas"][name].clone(),
            };
            // The companion tool-cron test locks the real Tool::coerce_input
            // output to these actual native preprocessors before this gate.
            let result = super::validate_tool_schema_detailed(&tool, &case["coerced"]);
            if case["success"] == true {
                assert!(result.is_ok(), "case {index}: {case}: {result:?}");
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    error.display,
                    case["display"].as_str().unwrap(),
                    "display case {index}"
                );
                assert_eq!(error.raw, case["raw"].as_str().unwrap(), "raw case {index}");
            }
        }
    }

    #[tokio::test]
    async fn cron_main_dispatch_keeps_native_raw_and_model_diagnostics_distinct() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/cron/tests/fixtures/malformed_input_2_1_270.json"
        ))
        .unwrap();
        let mut registry = tool_api::registry::ToolRegistry::new();
        for name in ["ScheduleWakeup", "CronCreate", "CronDelete", "CronList"] {
            registry.register_builtin(Arc::new(NativeCronSchema {
                projection: None,
                name: name.into(),
                schema: oracle["schemas"][name].clone(),
            }));
        }
        let orch = crate::conversation::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::path::PathBuf::from("/tmp"),
        );
        for (index, case) in oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, case)| case["success"] == false)
        {
            let id = lingxi_core::types::ToolUseId::new();
            let uses = vec![(
                id.clone(),
                case["tool"].as_str().unwrap().into(),
                case["coerced"].clone(),
                None,
            )];
            let (blocks, _, _, _) =
                crate::turn_loop::dispatch_tool_uses_tracked(&orch, &uses, None)
                    .await
                    .unwrap();
            let lingxi_core::types::ContentBlock::ToolResult {
                content, is_error, ..
            } = &blocks[0]
            else {
                panic!("expected tool result")
            };
            assert!(is_error.unwrap_or(false));
            assert_eq!(
                content,
                &format!(
                    "<tool_use_error>InputValidationError: {}</tool_use_error>",
                    case["display"].as_str().unwrap()
                ),
                "model {index}"
            );
            assert_eq!(
                orch.transcript
                    .tool_use_results
                    .lock()
                    .await
                    .get(&id.to_string()),
                Some(&json!(format!(
                    "InputValidationError: {}",
                    case["raw"].as_str().unwrap()
                ))),
                "persisted {index}"
            );
        }
    }

    #[tokio::test]
    async fn monitor_main_dispatch_matches_native_270_schema_corpus() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::sync::Arc;
        telemetry::test_set_flag("tengu_amber_sentinel", true);
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/task/tests/fixtures/monitor_schema_2_1_270.json"
        ))
        .unwrap();
        let context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });

        let tool: Arc<dyn tool_api::Tool> = Arc::new(tool_task::monitor::MonitorTool::new(context));
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(tool.clone());
        let orch = crate::conversation::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::path::PathBuf::from("/tmp"),
        );
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            telemetry::test_set_flag("tengu_breezy_crescent", case["mode"] == "bounded");
            if case["success"] == true {
                assert_eq!(
                    tool.parse_native_input(&case["input"]).unwrap().unwrap(),
                    case["normalized"],
                    "parsed {index}"
                );
                assert!(
                    super::validate_tool_schema_detailed(tool.as_ref(), &case["input"]).is_ok()
                );
                continue;
            }
            let id = lingxi_core::types::ToolUseId::new();
            let uses = vec![(id.clone(), "Monitor".into(), case["input"].clone(), None)];
            let (blocks, _, _, _) =
                crate::turn_loop::dispatch_tool_uses_tracked(&orch, &uses, None)
                    .await
                    .unwrap();
            let lingxi_core::types::ContentBlock::ToolResult {
                content, is_error, ..
            } = &blocks[0]
            else {
                panic!("expected tool result")
            };
            assert!(is_error.unwrap_or(false));
            assert_eq!(
                content,
                &format!(
                    "<tool_use_error>InputValidationError: {}</tool_use_error>",
                    case["display"].as_str().unwrap()
                ),
                "model {index}"
            );
            assert_eq!(
                orch.transcript
                    .tool_use_results
                    .lock()
                    .await
                    .get(&id.to_string()),
                Some(&json!(format!(
                    "InputValidationError: {}",
                    case["raw"].as_str().unwrap()
                ))),
                "raw {index}"
            );
        }
        telemetry::test_clear_flag("tengu_amber_sentinel");
        telemetry::test_clear_flag("tengu_breezy_crescent");
    }

    #[test]
    fn issue_json_uses_javascript_numbers_and_integer_key_order() {
        assert_eq!(
            super::js_json(&json!([1.0, -0.0, 1e-7, 1e-6, 1e20, 1e21]), false),
            "[1,0,1e-7,0.000001,100000000000000000000,1e+21]"
        );
        assert_eq!(validate_named_tool_input_schema("Tool", &json!({"type":"object","properties":{},"additionalProperties":false}), &json!({"10":1,"2":1,"01":1})).unwrap_err(), "Tool failed due to the following issues:\nAn unexpected parameter `2` was provided\nAn unexpected parameter `10` was provided\nAn unexpected parameter `01` was provided");
        assert!(validate_named_tool_input_schema("Tool", &json!({"const":1}), &json!(1.0)).is_ok());
    }

    #[test]
    fn diagnostics_match_executed_2_1_263_zod_4_4_3_oracle() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/schema-validation-2.1.263.json"))
                .unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let actual = validate_named_tool_input_schema("Tool", &case["schema"], &case["input"]);
            let expected = case["expected"]
                .as_str()
                .map_or(Ok(()), |s| Err(s.to_owned()));
            assert_eq!(actual, expected, "oracle case {}", case["name"]);
        }
    }

    #[test]
    fn named_error_matches_oracle_grouping_and_bytes() {
        let schema = json!({"type":"object", "properties": {
            "a": {"type":"number"}, "path": {"type":"string"}
        }, "required":["path"], "additionalProperties":false});
        assert_eq!(
            validate_named_tool_input_schema("Read", &schema, &json!({"a":true,"extra":1}))
                .unwrap_err(),
            "Read failed due to the following issues:\nThe required parameter `path` is missing\nAn unexpected parameter `extra` was provided\nThe parameter `a` type is expected as `number` but provided as `boolean`"
        );
    }

    #[test]
    fn named_error_uses_zod_array_path_and_singular_prefix() {
        let schema = json!({"type":"object", "properties":{"rows":{"type":"array", "items":{"type":"object", "properties":{"name":{"type":"string"}}, "required":["name"]}}}});
        assert_eq!(
            validate_named_tool_input_schema("Tool", &schema, &json!({"rows":[{}]})).unwrap_err(),
            "Tool failed due to the following issue:\nThe required parameter `rows[0].name` is missing"
        );
    }

    #[test]
    fn enum_error_preserves_zod_issue_bytes() {
        let schema = json!({"type":"string", "enum":["a","b"]});
        let expected = json!([{"code":"invalid_value","values":["a","b"],"path":[],"message":"Invalid option: expected one of \"a\"|\"b\""}]);
        assert_eq!(
            validate_named_tool_input_schema("Tool", &schema, &json!("c")).unwrap_err(),
            serde_json::to_string_pretty(&expected).unwrap()
        );
    }

    #[test]
    fn string_constraints_count_utf16_code_units() {
        assert!(validate_named_tool_input_schema(
            "Tool",
            &json!({"type":"string","minLength":2}),
            &json!("😀")
        )
        .is_ok());
        let error = validate_named_tool_input_schema(
            "Tool",
            &json!({"type":"string","maxLength":1}),
            &json!("😀"),
        )
        .unwrap_err();
        let issues: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(issues[0]["code"], "too_big");
    }

    #[test]
    fn structural_errors_win_over_enum_issues_and_keep_declaration_order() {
        let schema = json!({"type":"object", "properties": {
            "z": {"type":"string"}, "a":{"type":"string"}, "mode":{"type":"string", "enum":["a"]}
        }, "required":["z","a","mode"]});
        assert_eq!(validate_named_tool_input_schema("Tool", &schema, &json!({"mode":"bad"})).unwrap_err(),
            "Tool failed due to the following issues:\nThe required parameter `z` is missing\nThe required parameter `a` is missing");
    }

    #[test]
    fn union_returns_nested_zod_errors_and_prefers_dirty_branch() {
        let schema = json!({"anyOf":[{"type":"string","minLength":5},{"type":"number"}]});
        let error = validate_named_tool_input_schema("Tool", &schema, &json!("a")).unwrap_err();
        let issues: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(issues[0]["code"], "too_small");
        let error = validate_named_tool_input_schema("Tool", &schema, &json!(true)).unwrap_err();
        let issues: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(issues[0]["code"], "invalid_union");
        assert_eq!(issues[0]["errors"][0][0]["code"], "invalid_type");
    }

    #[test]
    fn native_custom_issues_and_type_message_overrides_follow_zue() {
        let issue = json!({"code":"custom","path":[],"message":"Duplicate questions"});
        assert_eq!(
            super::format_issues("Tool", std::slice::from_ref(&issue)),
            serde_json::to_string_pretty(&vec![issue]).unwrap()
        );
        let issue = json!({"code":"invalid_type","expected":"string","received":"number","path":["field"],"message":"custom message"});
        assert_eq!(super::format_issues("Tool", &[issue]), "Tool failed due to the following issue:\nThe parameter `field` type is expected as `string` but provided as `unknown`");
    }

    #[test]
    fn missing_required_field_fails() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        let err = validate_tool_input_schema(&schema, &json!({})).unwrap_err();
        assert!(err.contains("path"), "message should name the field: {err}");
    }

    #[test]
    fn type_mismatch_fails() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        let err = validate_tool_input_schema(&schema, &json!({ "path": 123 })).unwrap_err();
        assert!(
            err.contains("/path"),
            "message should locate the field: {err}"
        );
    }

    #[test]
    fn valid_input_passes() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        assert!(validate_tool_input_schema(&schema, &json!({ "path": "/x" })).is_ok());
    }

    #[test]
    fn malformed_schema_passes_through() {
        // A JSON scalar is not a valid Draft-07 schema (a schema must be an
        // object or boolean), so boon fails to compile it and we take the
        // warn-and-PASS branch. A schema bug must never block the model's call.
        let schema = json!("not a schema object at all");
        assert!(validate_tool_input_schema(&schema, &json!({ "anything": true })).is_ok());
    }

    #[test]
    fn invalid_type_keyword_schema_passes_through() {
        // `type` must be a string or array of strings; `123` makes the schema
        // itself invalid, forcing boon's compile-error branch → warn + PASS.
        let schema = json!({ "type": 123 });
        assert!(validate_tool_input_schema(&schema, &json!({ "x": 1 })).is_ok());
    }

    #[test]
    fn empty_object_schema_accepts_anything() {
        let schema = json!({ "type": "object" });
        assert!(validate_tool_input_schema(&schema, &json!({ "a": 1, "b": [2] })).is_ok());
    }

    #[test]
    fn output_schema_mismatch_returns_detail() {
        // #38: a hook `updatedToolOutput` that does not match the tool's output
        // schema must FAIL so the caller keeps the original output and emits the
        // `... does not match <tool>'s output shape ...` meta message.
        let schema = json!({
            "type": "object",
            "properties": { "result": { "type": "string" } },
            "required": ["result"],
        });
        let err = validate_tool_output_schema(&schema, &json!({})).unwrap_err();
        assert!(
            err.contains("result"),
            "message should name the field: {err}"
        );
    }

    #[test]
    fn output_schema_match_passes() {
        let schema = json!({
            "type": "object",
            "properties": { "result": { "type": "string" } },
            "required": ["result"],
        });
        assert!(validate_tool_output_schema(&schema, &json!({ "result": "ok" })).is_ok());
    }

    #[test]
    fn output_schema_malformed_passes_through() {
        // A schema bug must not discard a hook's valid replacement.
        let schema = json!("not a schema");
        assert!(validate_tool_output_schema(&schema, &json!({ "x": 1 })).is_ok());
    }
}
