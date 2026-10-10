//! `--json-schema` structured-output support.
//!
//! In print mode the model calls a
//! `StructuredOutput` tool whose `input_schema` is the user-supplied schema.
//! Returned arguments are compiled and validated by the host-owned JSON Schema
//! service at the canonical tool boundary. The orchestrator owns continuation
//! and retries; this module parses the CLI retry budget.

/// `MAX_STRUCTURED_OUTPUT_RETRIES` default (claude-code reads the env of the
/// same name, defaulting to `5`).
pub const DEFAULT_MAX_STRUCTURED_OUTPUT_RETRIES: i64 = 5;

/// Resolve the structured-output retry budget from `MAX_STRUCTURED_OUTPUT_RETRIES`
/// Native 2.1.293 accepts signed parseInt prefixes and defaults an unparseable
/// value to five. Zero and negative limits are retained in terminal diagnostics.
#[must_use]
pub fn resolve_max_retries(env_value: Option<&str>) -> i64 {
    let Some(raw) = env_value else {
        return DEFAULT_MAX_STRUCTURED_OUTPUT_RETRIES;
    };
    let raw = lingxi_core::host::instruction_memory_sanitize::js_trim(raw);
    let (negative, digits) = if let Some(rest) = raw.strip_prefix('-') {
        (true, rest)
    } else {
        (false, raw.strip_prefix('+').unwrap_or(raw))
    };
    let (radix, digits) = if let Some(rest) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (16, rest)
    } else {
        (10, digits)
    };
    let mut magnitude = 0u64;
    let mut parsed = false;
    for c in digits.chars() {
        let Some(digit) = c.to_digit(radix).filter(|_| c.is_ascii()) else {
            break;
        };
        parsed = true;
        magnitude = magnitude
            .saturating_mul(u64::from(radix))
            .saturating_add(u64::from(digit));
    }
    if !parsed {
        return DEFAULT_MAX_STRUCTURED_OUTPUT_RETRIES;
    }
    if negative {
        if magnitude >= (i64::MAX as u64) + 1 {
            i64::MIN
        } else {
            -(magnitude as i64)
        }
    } else {
        magnitude.min(i64::MAX as u64) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::json_schema_validator::{
        CompiledJsonSchema, JsonSchemaValidation, JsonSchemaValidator, JsonSchemaValidatorSession,
        SchemaProfile,
    };
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde_json::{Value, json};

    type TestValidator = crate::json_schema_validator::BunJsonSchemaValidator;

    fn test_validator() -> std::sync::Arc<TestValidator> {
        TestValidator::new_from_host_environment()
    }

    async fn validate(validator: &TestValidator, value: &Value, schema: &Value) -> Vec<String> {
        let schema = Utf16JsonProjection::from_serde_value(schema)
            .expect("test JSON schema converts to a source projection");
        let value = Utf16JsonProjection::from_serde_value(value)
            .expect("test output converts to a source projection");
        let session = validator.new_session(SchemaProfile::StructuredOutputDraft7);
        let compiled = match session.compile(&schema).await {
            Ok(compiled) => compiled,
            Err(error) => {
                return vec![format!("Output does not match required schema: {error}")];
            }
        };
        match compiled.validate(&value).await {
            Ok(JsonSchemaValidation::Valid) => Vec::new(),
            Ok(JsonSchemaValidation::Invalid { detail, .. }) => vec![format!(
                "Output does not match required schema: {}",
                detail.string_at("").unwrap_or("<invalid validator detail>")
            )],
            Err(error) => vec![format!("Output does not match required schema: {error}")],
        }
    }

    #[cfg(feature = "desktop")]
    #[tokio::test]
    async fn configured_schema_capture_is_not_shadowed_by_the_real_ui_catalog() {
        use std::sync::{Arc, Mutex};
        use tool_api::{Tool, ToolStaticContext, ToolUseContext};
        let mut registry = tool_api::ToolRegistry::new();
        let ctx = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        tool_ui::register_all_without_ask_user_question(&mut registry, ctx);
        let schema = Utf16JsonProjection::from_serde_value(
            &json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}}}),
        )
        .unwrap();
        let slot: orchestrator::structured_output::StructuredOutputSlot =
            Arc::new(Mutex::new(None));
        let dynamic: Arc<dyn Tool> =
            Arc::new(orchestrator::structured_output::StructuredOutputTool::new(
                schema.clone(),
                slot.clone(),
            ));
        registry.register_builtin(dynamic.clone());
        assert_eq!(
            registry
                .available_tools(&ToolStaticContext::default())
                .iter()
                .filter(|tool| tool.name() == "StructuredOutput")
                .count(),
            1
        );
        let selected = registry.find_by_name("StructuredOutput").unwrap();
        assert!(
            Arc::ptr_eq(&selected, &dynamic),
            "the dispatcher must select the configured capture owner"
        );
        assert!(Arc::ptr_eq(
            &registry.find_registered("StructuredOutput").unwrap(),
            &dynamic
        ));
        assert_eq!(selected.input_schema_projection(), Some(schema));
        let (tx, _rx) = tool_api::progress::progress_channel();
        let result = selected
            .call(
                Utf16JsonProjection::from_serde_value(&json!({"answer":"captured"})).unwrap(),
                ToolUseContext::model_seed("schema-owner".into(), None),
                tx,
            )
            .await
            .unwrap();
        assert!(selected.result_ends_turn(&result));
        assert_eq!(
            slot.lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"answer":"captured"}"#
        );
    }

    #[test]
    fn retry_budget_defaults_and_overrides() {
        assert_eq!(resolve_max_retries(None), 5);
        assert_eq!(resolve_max_retries(Some("3")), 3);
        assert_eq!(resolve_max_retries(Some("0")), 0);
        assert_eq!(resolve_max_retries(Some("-1")), -1);
        assert_eq!(resolve_max_retries(Some("2tail")), 2);
        assert_eq!(resolve_max_retries(Some("nope")), 5); // unparseable → default
        assert_eq!(resolve_max_retries(Some("  7 ")), 7); // trimmed
    }

    #[tokio::test]
    async fn valid_object_passes() {
        let validator = test_validator();
        let schema = json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": { "name": { "type": "string" }, "age": { "type": "integer" } }
        });
        assert!(
            validate(&validator, &json!({"name": "Ada", "age": 36}), &schema)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn missing_required_property_fails() {
        let validator = test_validator();
        let schema = json!({ "type": "object", "required": ["name"] });
        let errs = validate(&validator, &json!({}), &schema).await;
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("must have required property 'name'"),
            "{errs:?}"
        );
    }

    #[tokio::test]
    async fn wrong_property_type_fails() {
        let validator = test_validator();
        let schema = json!({
            "type": "object",
            "properties": { "age": { "type": "integer" } }
        });
        let errs = validate(&validator, &json!({"age": "old"}), &schema).await;
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("/age: must be integer"), "{errs:?}");
    }

    #[tokio::test]
    async fn integer_accepts_integral_float_rejects_fraction() {
        let validator = test_validator();
        let schema = json!({ "type": "integer" });
        assert!(validate(&validator, &json!(5.0), &schema).await.is_empty());
        assert!(!validate(&validator, &json!(5.5), &schema).await.is_empty());
    }

    #[tokio::test]
    async fn enum_constraint() {
        let validator = test_validator();
        let schema = json!({ "enum": ["a", "b"] });
        assert!(validate(&validator, &json!("a"), &schema).await.is_empty());
        assert!(!validate(&validator, &json!("c"), &schema).await.is_empty());
    }

    #[tokio::test]
    async fn nested_array_items_validated() {
        let validator = test_validator();
        let schema = json!({
            "type": "array",
            "items": { "type": "object", "required": ["id"], "properties": { "id": { "type": "integer" } } }
        });
        assert!(
            validate(&validator, &json!([{"id": 1}, {"id": 2}]), &schema)
                .await
                .is_empty()
        );
        let errs = validate(&validator, &json!([{"id": 1}, {"name": "x"}]), &schema).await;
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("/1: must have required property 'id'"),
            "{errs:?}"
        );
    }

    #[tokio::test]
    async fn type_array_accepts_either() {
        let validator = test_validator();
        let schema = json!({ "type": ["string", "null"] });
        assert!(validate(&validator, &json!("x"), &schema).await.is_empty());
        assert!(validate(&validator, &json!(null), &schema).await.is_empty());
        assert!(!validate(&validator, &json!(42), &schema).await.is_empty());
    }

    #[tokio::test]
    async fn additional_properties_false_rejects_extras() {
        let validator = test_validator();
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "additionalProperties": false
        });
        assert!(
            validate(&validator, &json!({"a": "x"}), &schema)
                .await
                .is_empty()
        );
        let errs = validate(&validator, &json!({"a": "x", "b": 1}), &schema).await;
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("must NOT have additional properties ('b' is not allowed)"),
            "{errs:?}"
        );
    }

    #[tokio::test]
    async fn constraints_previously_ignored_are_enforced() {
        let validator = test_validator();
        for (schema, valid, invalid) in [
            (
                json!({"$ref":"#/definitions/positive", "definitions":{"positive":{"type":"number","minimum":10}}}),
                json!(10),
                json!(1),
            ),
            (
                json!({"allOf":[{"type":"number"},{"minimum":10}]}),
                json!(12),
                json!(2),
            ),
            (
                json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
                json!("x"),
                json!(2),
            ),
            (
                json!({"oneOf":[{"type":"number"},{"type":"integer"}]}),
                json!(2.5),
                json!(2),
            ),
            (
                json!({"type":"string","pattern":"^[a-z]+$"}),
                json!("abc"),
                json!("123"),
            ),
            (
                json!({"type":"number","maximum":10,"multipleOf":2}),
                json!(8),
                json!(9),
            ),
            (
                json!({"type":"string","minLength":2,"maxLength":3}),
                json!("abc"),
                json!("a"),
            ),
            (
                json!({"type":"array","minItems":2,"maxItems":3,"uniqueItems":true}),
                json!([1, 2]),
                json!([1, 1]),
            ),
            (
                json!({"if":{"required":["a"]},"then":{"required":["b"]}}),
                json!({"a":1,"b":2}),
                json!({"a":1}),
            ),
            (
                json!({"type":"object","patternProperties":{"^x":{"type":"number"}},"additionalProperties":false}),
                json!({"x1":2}),
                json!({"x1":"bad"}),
            ),
        ] {
            assert!(
                validate(&validator, &valid, &schema).await.is_empty(),
                "valid instance rejected: {schema}"
            );
            assert!(
                !validate(&validator, &invalid, &schema).await.is_empty(),
                "constraint silently passed: {schema}"
            );
        }
    }

    #[tokio::test]
    async fn false_schema_and_unresolvable_reference_are_not_accepted() {
        let validator = test_validator();
        assert!(
            !validate(&validator, &json!({}), &json!(false))
                .await
                .is_empty()
        );
        assert!(
            !validate(
                &validator,
                &json!({}),
                &json!({"$ref":"#/definitions/missing"})
            )
            .await
            .is_empty()
        );
        assert!(
            !validate(
                &validator,
                &json!({}),
                &json!({"type":"not-a-json-schema-type"})
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn shared_native_diagnostics_are_retained_verbatim() {
        let validator = test_validator();
        let schema = json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}}});
        assert_eq!(
            validate(&validator, &json!({"answer":42}), &schema).await,
            vec!["Output does not match required schema: /answer: must be string"]
        );
        assert_eq!(
            validate(&validator, &json!({}), &schema).await,
            vec![
                "Output does not match required schema: root: must have required property 'answer'"
            ]
        );
        assert!(
            validate(&validator, &json!({"answer":"ok"}), &schema)
                .await
                .is_empty()
        );
    }
}
