//! `--json-schema` structured-output support.
//!
//! In print mode the model calls a
//! `StructuredOutput` tool whose `input_schema` is the user-supplied schema.
//! Returned arguments are validated at the canonical tool call boundary.
//! The existing query driver owns continuation and its retry budget. This
//! module contains configuration parsing and the shared validation entrypoint.
//!
//! Validation is shared with `orchestrator::structured_output::StructuredOutputTool`.
//! Its strict Draft 7 Boon evaluator and native Ajv diagnostics cover references,
//! combinators, bounds, lengths, patterns and object/array constraints. Schema
//! load and compilation failures are errors; this module never silently passes
//! constraints that its own ad hoc walker does not recognize.

use serde_json::Value;

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

/// Validate with the same strict schema evaluator and error renderer used by
/// the StructuredOutput tool. The complete native diagnostic remains one item
/// so its ordered errors, paths, punctuation, and wording are preserved.
#[must_use]
pub fn validate(value: &Value, schema: &Value) -> Vec<String> {
    orchestrator::structured_output::validate_output(schema, value)
        .err()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "desktop")]
    #[tokio::test]
    async fn configured_schema_capture_is_not_shadowed_by_the_real_ui_catalog() {
        use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
        let schema = Utf16JsonProjection::plain(
            json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}}}),
        );
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
                json!({"answer":"captured"}),
                ToolUseContext::model_seed("schema-owner".into(), None),
                tx,
            )
            .await
            .unwrap();
        assert!(selected.result_ends_turn(&result));
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().value,
            json!({"answer":"captured"})
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

    #[test]
    fn valid_object_passes() {
        let schema = json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": { "name": { "type": "string" }, "age": { "type": "integer" } }
        });
        assert!(validate(&json!({"name": "Ada", "age": 36}), &schema).is_empty());
    }

    #[test]
    fn missing_required_property_fails() {
        let schema = json!({ "type": "object", "required": ["name"] });
        let errs = validate(&json!({}), &schema);
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("must have required property 'name'"),
            "{errs:?}"
        );
    }

    #[test]
    fn wrong_property_type_fails() {
        let schema = json!({
            "type": "object",
            "properties": { "age": { "type": "integer" } }
        });
        let errs = validate(&json!({"age": "old"}), &schema);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("/age: must be integer"), "{errs:?}");
    }

    #[test]
    fn integer_accepts_integral_float_rejects_fraction() {
        let schema = json!({ "type": "integer" });
        assert!(validate(&json!(5.0), &schema).is_empty());
        assert!(!validate(&json!(5.5), &schema).is_empty());
    }

    #[test]
    fn enum_constraint() {
        let schema = json!({ "enum": ["a", "b"] });
        assert!(validate(&json!("a"), &schema).is_empty());
        assert!(!validate(&json!("c"), &schema).is_empty());
    }

    #[test]
    fn nested_array_items_validated() {
        let schema = json!({
            "type": "array",
            "items": { "type": "object", "required": ["id"], "properties": { "id": { "type": "integer" } } }
        });
        assert!(validate(&json!([{"id": 1}, {"id": 2}]), &schema).is_empty());
        let errs = validate(&json!([{"id": 1}, {"name": "x"}]), &schema);
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("/1: must have required property 'id'"),
            "{errs:?}"
        );
    }

    #[test]
    fn type_array_accepts_either() {
        let schema = json!({ "type": ["string", "null"] });
        assert!(validate(&json!("x"), &schema).is_empty());
        assert!(validate(&json!(null), &schema).is_empty());
        assert!(!validate(&json!(42), &schema).is_empty());
    }

    #[test]
    fn additional_properties_false_rejects_extras() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "additionalProperties": false
        });
        assert!(validate(&json!({"a": "x"}), &schema).is_empty());
        let errs = validate(&json!({"a": "x", "b": 1}), &schema);
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].contains("must NOT have additional properties ('b' is not allowed)"),
            "{errs:?}"
        );
    }

    #[test]
    fn constraints_previously_ignored_are_enforced() {
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
                validate(&valid, &schema).is_empty(),
                "valid instance rejected: {schema}"
            );
            assert!(
                !validate(&invalid, &schema).is_empty(),
                "constraint silently passed: {schema}"
            );
        }
    }

    #[test]
    fn false_schema_and_unresolvable_reference_are_not_accepted() {
        assert!(!validate(&json!({}), &json!(false)).is_empty());
        assert!(!validate(&json!({}), &json!({"$ref":"#/definitions/missing"})).is_empty());
        assert!(!validate(&json!({}), &json!({"type":"not-a-json-schema-type"})).is_empty());
    }

    #[test]
    fn shared_native_diagnostics_are_retained_verbatim() {
        let schema = json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}}});
        assert_eq!(
            validate(&json!({"answer":42}), &schema),
            vec!["Output does not match required schema: /answer: must be string"]
        );
        assert_eq!(
            validate(&json!({}), &schema),
            vec![
                "Output does not match required schema: root: must have required property 'answer'"
            ]
        );
        assert_eq!(
            validate(&json!({"answer":42}), &schema),
            orchestrator::structured_output::validate_output(&schema, &json!({"answer":42}))
                .err()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }
}
