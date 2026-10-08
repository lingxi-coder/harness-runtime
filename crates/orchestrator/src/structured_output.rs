//! The `StructuredOutput` tool for `--json-schema` structured output.
//!
//! The model sees the user-supplied JSON Schema, while the native predispatch
//! parser accepts a passthrough object. The dynamic tool validates that object
//! inside `call`, matching Claude Code's Ajv-backed Qe/Ze path, and captures it
//! only after successful validation. Invalid output remains a tool-call error
//! so the model can correct it within the turn.
//!
//! Both desktop and CLI composition roots register this tool. The CLI's print
//! path reads the accepted result from the shared slot and owns its outer
//! retry budget when no structured result was produced.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::Value;
use tool_api::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolProgressSender,
    ToolStaticContext, ToolUseContext,
};

/// Canonical name of the synthetic structured-output tool.
pub const STRUCTURED_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// Shared slot the [`StructuredOutputTool`] writes the model's structured result
/// into after validation. The print path reads it after the turn to emit.
pub type StructuredOutputSlot = Arc<Mutex<Option<Value>>>;

/// The forced `StructuredOutput` tool. Its `input_schema` IS the user's schema;
/// `call` validates the model's arguments, captures accepted output into the
/// shared slot, and returns the canonical acknowledgement.
pub struct StructuredOutputTool {
    /// The user-supplied JSON schema, returned verbatim as `input_schema`.
    schema: Value,
    /// Where `call` deposits the model's structured arguments.
    captured: StructuredOutputSlot,
}

impl StructuredOutputTool {
    /// Build the tool for `schema`, capturing the model's call into `slot`.
    #[must_use]
    pub fn new(schema: Value, slot: StructuredOutputSlot) -> Self {
        Self {
            schema,
            captured: slot,
        }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    fn name(&self) -> &str {
        STRUCTURED_OUTPUT_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("return the final response as structured JSON")
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn input_validation_schema(&self) -> &Value {
        static BASE: OnceLock<Value> = OnceLock::new();
        BASE.get_or_init(|| serde_json::json!({"type":"object"}))
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // claude sets `maxResultSizeChars:1e5` on the StructuredOutput tool (the
        // shared default). The result is a trivial ack ("{\"ok\":true}") so the
        // cap is never approached, but match the binary value for parity.
        100_000
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // Captures into an in-memory slot; no workspace side effect.
        true
    }

    fn result_ends_turn(&self, _result: &ToolCallResult) -> bool {
        // Claude Code's StructuredOutput result carries `endsTurn: true`.
        true
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        // Synthetic output tool — always allowed (the model is forced to call it).
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "structured output".to_string(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        "Return structured output in the requested format".to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        "Use this tool to return your final response in the requested structured format. You MUST call this tool exactly once at the end of your response to provide the structured output.".to_string()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        validate_output(&self.schema, &input).map_err(ToolError::InvalidInput)?;
        // Capture only values accepted by the dynamic call-stage validator.
        // Lock is held only for the store (no await across it).
        if let Ok(mut slot) = self.captured.lock() {
            *slot = Some(input);
        }
        Ok(ToolCallResult {
            data: Value::String("Structured output provided successfully".to_string()),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// The oracle's Qe uses Ajv({allErrors:true, validateFormats:false}). Boon
// performs the JSON-Schema evaluation, with validation-only ordered groups to
// avoid its short-circuiting type/const/enum checks. The advertised schema stays
// exactly as supplied. Each local reference retains a separate resource so
// moving assertions into groups cannot break JSON-pointer targets.
struct ValidationSchemas<'a> {
    root: &'a Value,
    refs: std::collections::HashMap<String, String>,
    pending: Vec<(String, Value)>,
}

impl ValidationSchemas<'_> {
    fn normalize(&mut self, schema: &Value) -> Value {
        let Some(object) = schema.as_object() else {
            return schema.clone();
        };
        let mut groups = Vec::new();
        // Ajv RULES order, extracted from 2.1.263's bundled Ajv.
        for key in [
            "$ref",
            "type",
            "const",
            "enum",
            "not",
            "anyOf",
            "oneOf",
            "allOf",
            "if",
            "maximum",
            "minimum",
            "exclusiveMaximum",
            "exclusiveMinimum",
            "multipleOf",
            "maxLength",
            "minLength",
            "pattern",
            "maxItems",
            "minItems",
            "additionalItems",
            "items",
            "contains",
            "uniqueItems",
            "maxProperties",
            "minProperties",
            "required",
            "propertyNames",
            "additionalProperties",
            "dependencies",
            "properties",
            "patternProperties",
        ] {
            let Some(value) = object.get(key) else {
                continue;
            };
            let mut group = serde_json::Map::new();
            let normalized = match key {
                "$ref" => {
                    if let Some(pointer) = value.as_str().and_then(|s| s.strip_prefix('#')) {
                        if let Some(target) = self.root.pointer(pointer) {
                            let url = if let Some(url) = self.refs.get(pointer) {
                                url.clone()
                            } else {
                                let url = format!("mem://structured-output/ref{}", self.refs.len());
                                self.refs.insert(pointer.to_string(), url.clone());
                                self.pending.push((url.clone(), target.clone()));
                                url
                            };
                            Value::String(url)
                        } else {
                            value.clone()
                        }
                    } else {
                        value.clone()
                    }
                }
                "type" if object.get("nullable") == Some(&Value::Bool(true)) => {
                    let mut types = value
                        .as_array()
                        .cloned()
                        .unwrap_or_else(|| vec![value.clone()]);
                    if !types.iter().any(|v| v == "null") {
                        types.push(Value::String("null".into()));
                    }
                    Value::Array(types)
                }
                "not" | "if" | "contains" | "propertyNames" | "additionalProperties" => {
                    self.normalize(value)
                }
                "anyOf" | "oneOf" | "allOf" => Value::Array(
                    value
                        .as_array()
                        .map(|a| a.iter().map(|v| self.normalize(v)).collect())
                        .unwrap_or_default(),
                ),
                "items" => {
                    if let Some(items) = value.as_array() {
                        Value::Array(items.iter().map(|v| self.normalize(v)).collect())
                    } else {
                        self.normalize(value)
                    }
                }
                "properties" | "patternProperties" | "dependencies" => Value::Object(
                    value
                        .as_object()
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| {
                                    (
                                        k.clone(),
                                        if key == "dependencies" && v.is_array() {
                                            v.clone()
                                        } else {
                                            self.normalize(v)
                                        },
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
                _ => value.clone(),
            };
            group.insert(key.into(), normalized);
            if key == "type" {
                group.insert("x-original-type".into(), value.clone());
            }
            if key == "if" {
                for branch in ["then", "else"] {
                    if let Some(value) = object.get(branch) {
                        group.insert(branch.into(), self.normalize(value));
                    }
                }
            }
            if key == "additionalItems" {
                // Tuple length is part of the additional-items constraint.
                if let Some(items) = object.get("items").and_then(Value::as_array) {
                    group.insert(
                        "items".into(),
                        Value::Array(vec![Value::Bool(true); items.len()]),
                    );
                }
            }
            if key == "additionalProperties" {
                for recognized in ["properties", "patternProperties"] {
                    if let Some(properties) = object.get(recognized).and_then(Value::as_object) {
                        group.insert(
                            recognized.into(),
                            Value::Object(
                                properties
                                    .keys()
                                    .map(|k| (k.clone(), Value::Bool(true)))
                                    .collect(),
                            ),
                        );
                    }
                }
            }
            groups.push(Value::Object(group));
        }
        serde_json::json!({"allOf": groups})
    }
}

fn validate_output(schema: &Value, input: &Value) -> Result<(), String> {
    const URL: &str = "mem://structured-output/root";
    let mut normalized = ValidationSchemas {
        root: schema,
        refs: [(String::new(), URL.to_string())].into_iter().collect(),
        pending: Vec::new(),
    };
    let root = normalized.normalize(schema);
    let mut resources = std::collections::HashMap::new();
    resources.insert(URL.to_string(), root.clone());
    let mut compiler = boon::Compiler::new();
    compiler.set_default_draft(boon::Draft::V7);
    let mut schemas = boon::Schemas::new();
    compiler
        .add_resource(URL, root)
        .map_err(|e| format!("Invalid structured output schema: {e}"))?;
    while let Some((url, value)) = normalized.pending.pop() {
        let value = normalized.normalize(&value);
        resources.insert(url.clone(), value.clone());
        compiler
            .add_resource(&url, value)
            .map_err(|e| format!("Invalid structured output schema: {e}"))?;
    }
    let compiled = compiler
        .compile(URL, &mut schemas)
        .map_err(|e| format!("Invalid structured output schema: {e}"))?;
    if let Err(error) = schemas.validate(input, compiled) {
        let mut errors = Vec::new();
        ajv_errors(&error, input, &resources, None, &mut errors);
        return Err(format!(
            "Output does not match required schema: {}",
            errors.join(", ")
        ));
    }
    Ok(())
}

fn abbreviated(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    abbreviate(&text, 300)
}

fn abbreviate(text: &str, limit: usize) -> String {
    if text.encode_utf16().count() <= limit {
        return text.to_string();
    }
    let mut units = 0;
    let prefix: String = text
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= limit
        })
        .collect();
    format!("{prefix}…")
}

fn schema_node<'a>(
    resources: &'a std::collections::HashMap<String, Value>,
    url: &str,
) -> Option<&'a Value> {
    let (resource, pointer) = url.split_once('#').unwrap_or((url, ""));
    resources.get(resource)?.pointer(pointer)
}

fn ajv_errors(
    error: &boon::ValidationError<'_, '_>,
    input: &Value,
    resources: &std::collections::HashMap<String, Value>,
    parent_url: Option<&str>,
    out: &mut Vec<String>,
) {
    use boon::ErrorKind as E;
    let path = error.instance_location.to_string();
    let path = if path.is_empty() { "root" } else { &path };
    let mut emit = |message: String| out.push(format!("{path}: {message}"));
    match &error.kind {
        E::Group | E::Schema { .. } | E::Reference { .. } | E::AllOf => {
            for cause in &error.causes {
                ajv_errors(cause, input, resources, Some(error.schema_url), out);
            }
        }
        E::AnyOf | E::OneOf(None) => {
            for cause in &error.causes {
                ajv_errors(cause, input, resources, Some(error.schema_url), out);
            }
            out.push(format!(
                "{path}: must match {} schema in {}",
                if matches!(&error.kind, E::AnyOf) {
                    "a"
                } else {
                    "exactly one"
                },
                if matches!(&error.kind, E::AnyOf) {
                    "anyOf"
                } else {
                    "oneOf"
                }
            ));
        }
        E::OneOf(Some(_)) => emit("must match exactly one schema in oneOf".into()),
        E::FalseSchema => emit("boolean schema is false".into()),
        E::Type { want, .. } => {
            let types =
                schema_node(resources, error.schema_url).and_then(|n| n.get("x-original-type"));
            let types = match types {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(t)) => t
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
                _ => want
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            };
            emit(format!("must be {types}"));
        }
        E::Const { want } => emit(format!("must be equal to constant: {}", abbreviated(want))),
        E::Enum { want } => emit(format!(
            "must be equal to one of the allowed values: {}",
            abbreviated(&Value::Array((*want).clone()))
        )),
        E::Required { want } => {
            for property in want {
                emit(format!("must have required property '{property}'"));
            }
        }
        E::AdditionalProperties { got } => {
            for property in got {
                emit(format!(
                    "must NOT have additional properties ('{}' is not allowed)",
                    abbreviate(property, 80)
                ));
            }
        }
        E::MinProperties { got, want } => emit(format!(
            "must NOT have fewer than {want} properties (got {got})"
        )),
        E::MaxProperties { got, want } => emit(format!(
            "must NOT have more than {want} properties (got {got})"
        )),
        E::MinItems { got, want } => {
            emit(format!("must NOT have fewer than {want} items (got {got})"))
        }
        E::MaxItems { got, want } => {
            emit(format!("must NOT have more than {want} items (got {got})"))
        }
        E::MinLength { got, want } => emit(format!(
            "must NOT have fewer than {want} characters (got {got})"
        )),
        E::MaxLength { got, want } => emit(format!(
            "must NOT have more than {want} characters (got {got})"
        )),
        E::Pattern { want, .. } => emit(format!("must match pattern \"{want}\"")),
        E::Minimum { want, .. } => emit(format!("must be >= {want}")),
        E::Maximum { want, .. } => emit(format!("must be <= {want}")),
        E::ExclusiveMinimum { want, .. } => emit(format!("must be > {want}")),
        E::ExclusiveMaximum { want, .. } => emit(format!("must be < {want}")),
        E::MultipleOf { want, .. } => emit(format!("must be multiple of {want}")),
        E::Not => emit("must NOT be valid".into()),
        E::Contains | E::MinContains { .. } | E::MaxContains { .. } => {
            for cause in &error.causes {
                ajv_errors(cause, input, resources, Some(error.schema_url), out);
            }
            out.push(format!("{path}: must contain at least 1 valid item(s)"));
        }
        E::UniqueItems { got } => emit(format!(
            "must NOT have duplicate items (items ## {} and {} are identical)",
            got[1], got[0]
        )),
        E::AdditionalItems { got } => {
            let len = input
                .pointer(&error.instance_location.to_string())
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            emit(format!(
                "must NOT have more than {} items",
                len.saturating_sub(*got)
            ));
        }
        E::Dependency { prop, missing } | E::DependentRequired { prop, missing } => {
            let dependencies = schema_node(resources, error.schema_url)
                .and_then(|node| node.get("dependencies"))
                .and_then(|d| d.get(*prop))
                .and_then(Value::as_array);
            let names = dependencies
                .map(|d| d.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_else(|| missing.to_vec());
            for _ in missing {
                emit(format!(
                    "must have {} {} when property {prop} is present",
                    if names.len() == 1 {
                        "property"
                    } else {
                        "properties"
                    },
                    names.join(", ")
                ));
            }
        }
        E::PropertyName { prop } => {
            // Boon replaces a single leaf's kind with PropertyName; recover its
            // diagnostic from the property-name schema before adding Ajv's wrapper.
            if let Some(schema) =
                schema_node(resources, error.schema_url).and_then(|n| n.get("propertyNames"))
            {
                if let Err(message) = validate_output(schema, &Value::String(prop.clone())) {
                    if let Some(message) =
                        message.strip_prefix("Output does not match required schema: ")
                    {
                        out.push(message.replace("root:", &format!("{path}:")));
                    }
                }
            } else {
                for cause in &error.causes {
                    ajv_errors(cause, input, resources, Some(error.schema_url), out);
                }
            }
            out.push(format!("{path}: property name must be valid"));
        }
        _ => emit(error.kind.to_string()),
    }
    // Boon omits conditional wrappers and collapses a singleton union's
    // wrapper. Emit each only when leaving that branch (never once per leaf).
    let url = error.schema_url;
    let mut boundaries = Vec::new();
    for keyword in ["then", "else", "anyOf", "oneOf"] {
        let needle = format!("/{keyword}/");
        let marker = if keyword == "then" || keyword == "else" {
            url.find(&needle)
                .or_else(|| url.strip_suffix(&format!("/{keyword}")).map(str::len))
        } else {
            url.find(&needle)
        };
        if let Some(index) = marker {
            let container = &url[..index];
            let branch_prefix = format!("{container}/{keyword}");
            if parent_url.is_some_and(|parent| parent.starts_with(&branch_prefix)) {
                continue;
            }
            let singleton = schema_node(resources, container)
                .and_then(|n| n.get(keyword))
                .and_then(Value::as_array)
                .is_some_and(|a| a.len() == 1);
            if keyword == "anyOf" || keyword == "oneOf" {
                if !singleton {
                    continue;
                }
            } else if schema_node(resources, container)
                .and_then(|n| n.get("if"))
                .is_none()
            {
                continue;
            }
            let tail = &url[branch_prefix.len()..];
            let levels = tail
                .split('/')
                .filter(|part| matches!(*part, "properties" | "patternProperties" | "items"))
                .count();
            let location = error.instance_location.to_string();
            let mut parts: Vec<_> = location.split('/').collect();
            parts.truncate(parts.len().saturating_sub(levels));
            let location = parts.join("/");
            let location = if location.is_empty() {
                "root"
            } else {
                &location
            };
            let message = match keyword {
                "then" | "else" => format!("{location}: must match \"{keyword}\" schema"),
                "anyOf" => format!("{location}: must match a schema in anyOf"),
                _ => format!("{location}: must match exactly one schema in oneOf"),
            };
            boundaries.push((index, message));
        }
    }
    boundaries.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    out.extend(boundaries.into_iter().map(|(_, message)| message));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dynamic_schema_only_runs_at_call_stage() {
        let tool = StructuredOutputTool::new(
            json!({"type":"object", "required":["answer"]}),
            Arc::new(Mutex::new(None)),
        );
        assert!(crate::schema_validation::validate_tool_schema(&tool, &json!({})).is_ok());
        assert!(crate::schema_validation::validate_tool_schema(&tool, &json!([])).is_err());
    }

    #[tokio::test]
    async fn invalid_structured_output_does_not_replace_the_captured_value() {
        let slot = Arc::new(Mutex::new(Some(json!({"answer":42}))));
        let tool = StructuredOutputTool::new(
            json!({"type":"object", "required":["answer"]}),
            slot.clone(),
        );
        let (tx, _) = tool_api::progress::progress_channel();
        let error = tool
            .call(
                json!({}),
                ToolUseContext::model_seed("test".into(), None),
                tx,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.model_facing_message(),
            "Output does not match required schema: root: must have required property 'answer'"
        );
        assert_eq!(*slot.lock().unwrap(), Some(json!({"answer":42})));
    }

    #[test]
    fn all_of_and_local_refs_match_bundled_ajv_diagnostics() {
        // Qe/Ze + bundled Kkt Ajv, Claude Code 2.1.263.
        let schema = json!({"type":"object", "definitions":{"number":{"type":"integer","minimum":5}},
            "allOf":[{"required":["x"]},{"properties":{"a":{"$ref":"#/definitions/number"}}}],
            "additionalProperties":false});
        assert_eq!(validate_output(&schema, &json!({"a":2,"b":4})).unwrap_err(),
            "Output does not match required schema: root: must have required property 'x', /a: must be >= 5, root: must NOT have additional properties ('a' is not allowed), root: must NOT have additional properties ('b' is not allowed)");
        assert!(validate_output(
            &json!({"properties":{"x":{"type":"integer"},"y":{"$ref":"#/properties/x"}}}),
            &json!({"x":1,"y":2})
        )
        .is_ok());
        assert_eq!(
            validate_output(
                &json!({"properties":{"x":{"type":"integer"},"y":{"$ref":"#/properties/x"}}}),
                &json!({"x":1,"y":"bad"})
            )
            .unwrap_err(),
            "Output does not match required schema: /y: must be integer"
        );
    }

    #[test]
    fn ze_enriches_lengths_enums_constants_and_all_errors_in_ajv_order() {
        let schema = json!({"type":"object", "maxProperties":1, "minProperties":5,
            "required":["x"], "properties":{"a":{"type":"integer"}}, "additionalProperties":false});
        assert_eq!(validate_output(&schema, &json!({"a":"bad","b":2})).unwrap_err(),
            "Output does not match required schema: root: must NOT have more than 1 properties (got 2), root: must NOT have fewer than 5 properties (got 2), root: must have required property 'x', root: must NOT have additional properties ('b' is not allowed), /a: must be integer");
        assert_eq!(validate_output(&json!({"properties":{"text":{"minLength":3}, "tags":{"maxItems":1}}}), &json!({"text":"😀","tags":[1,2]})).unwrap_err(),
            "Output does not match required schema: /text: must NOT have fewer than 3 characters (got 1), /tags: must NOT have more than 1 items (got 2)");
        assert_eq!(validate_output(&json!({"properties":{"value":{"const":1,"enum":[2,3]}}}), &json!({"value":0})).unwrap_err(),
            "Output does not match required schema: /value: must be equal to constant: 1, /value: must be equal to one of the allowed values: [2,3]");
        assert!(validate_output(
            &json!({"properties":{"date":{"type":"string","format":"date-time"}}}),
            &json!({"date":"not a date"})
        )
        .is_ok());
    }

    #[test]
    fn union_order_dependencies_and_condition_wrappers_match_ajv() {
        assert_eq!(
            validate_output(
                &json!({"properties":{"x":{"type":["string","number"]}}}),
                &json!({"x":false})
            )
            .unwrap_err(),
            "Output does not match required schema: /x: must be string,number"
        );
        assert_eq!(validate_output(&json!({"type":"object", "dependencies":{"a":["b","c"]}, "propertyNames":{"pattern":"^[a-z]+$"}}), &json!({"a":1,"BAD":2})).unwrap_err(),
            "Output does not match required schema: root: must match pattern \"^[a-z]+$\", root: property name must be valid, root: must have properties b, c when property a is present, root: must have properties b, c when property a is present");
        assert_eq!(validate_output(&json!({"if":{"required":["a"]}, "then":{"required":["b"]}}), &json!({"a":1})).unwrap_err(),
            "Output does not match required schema: root: must have required property 'b', root: must match \"then\" schema");
        assert_eq!(validate_output(&json!({"anyOf":[{"required":["x"]}]}), &json!({})).unwrap_err(),
            "Output does not match required schema: root: must have required property 'x', root: must match a schema in anyOf");
    }

    #[tokio::test]
    async fn tool_prompt_matches_2_1_263_oracle() {
        let tool = StructuredOutputTool::new(json!({"type":"object"}), Arc::new(Mutex::new(None)));
        assert_eq!(
            tool.description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: true
                }
            )
            .await,
            "Return structured output in the requested format"
        );
        assert_eq!(tool.prompt(&PromptOptions::default()).await,
            "Use this tool to return your final response in the requested structured format. You MUST call this tool exactly once at the end of your response to provide the structured output.");
    }

    #[test]
    fn tool_exposes_the_user_schema_as_input_schema() {
        let schema = json!({ "type": "object", "required": ["x"] });
        let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
        let tool = StructuredOutputTool::new(schema.clone(), slot);
        assert_eq!(tool.name(), "StructuredOutput");
        assert_eq!(tool.input_schema(), &schema);
    }

    #[tokio::test]
    async fn call_captures_the_models_arguments_into_the_slot() {
        let slot: StructuredOutputSlot = Arc::new(Mutex::new(None));
        let tool = StructuredOutputTool::new(json!({"type": "object"}), slot.clone());
        let (tx, _rx) = tool_api::progress::progress_channel();
        let result = tool
            .call(
                json!({"answer": 42}),
                ToolUseContext::model_seed("test".into(), None),
                tx,
            )
            .await
            .expect("call ok");
        assert_eq!(
            result.data,
            Value::String("Structured output provided successfully".to_string())
        );
        assert!(
            tool.result_ends_turn(&result),
            "StructuredOutput mirrors the oracle's endsTurn:true result"
        );
        assert_eq!(
            slot.lock().unwrap().as_ref(),
            Some(&json!({"answer": 42})),
            "the model's arguments must be captured for validation"
        );
    }
}
