//! Native offline Draft-7 validation for app tasks. External schema references
//! fail before model execution. Formats match the Desktop app contract.
use lingxi_core::{
    host::json_schema_validator::{CompiledJsonSchema, JsonSchemaValidation, SchemaWorkerError},
    types::utf16_json::Utf16JsonProjection,
};
use serde_json::Value;
use std::sync::Arc;
pub(super) struct NativeSchema(jsonschema::Validator);
fn local_references(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().all(|(key, value)| {
            if ["$ref", "$dynamicRef", "$recursiveRef"].contains(&key.as_str()) {
                value.as_str().is_some_and(|s| s.starts_with('#'))
            } else {
                local_references(value)
            }
        }),
        Value::Array(values) => values.iter().all(local_references),
        _ => true,
    }
}
impl NativeSchema {
    pub(super) fn compile(schema: &Value) -> Result<Arc<Self>, String> {
        if !schema.is_object() || schema.to_string().len() > 256 * 1024 || !local_references(schema)
        {
            return Err("invalid_output_schema".into());
        }
        if schema.get("$schema").is_some_and(|v| {
            !matches!(
                v.as_str(),
                Some(
                    "http://json-schema.org/draft-07/schema#"
                        | "http://json-schema.org/draft-07/schema"
                )
            )
        }) {
            return Err("unsupported_schema_dialect".into());
        }
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .should_validate_formats(false)
            .build(schema)
            .map(|validator| Arc::new(Self(validator)))
            .map_err(|_| "invalid_output_schema".into())
    }
}
#[async_trait::async_trait]
impl CompiledJsonSchema for NativeSchema {
    async fn validate(
        &self,
        input: &Utf16JsonProjection,
    ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
        let text = input
            .to_json_string()
            .map_err(|_| SchemaWorkerError::Protocol("invalid result JSON".into()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| SchemaWorkerError::Protocol("result is not portable JSON".into()))?;
        Ok(match self.0.validate(&value) {
            Ok(()) => JsonSchemaValidation::Valid,
            Err(_) => JsonSchemaValidation::Invalid {
                detail: "The result does not satisfy the requested JSON Schema.".into(),
                keywords: vec!["schema".into()],
            },
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn accepts_only_valid_results() {
        let schema=NativeSchema::compile(&serde_json::json!({"type":"object","properties":{"grams":{"type":"number","minimum":0}},"required":["grams"],"additionalProperties":false})).unwrap();
        assert_eq!(
            schema
                .validate(&Utf16JsonProjection::parse("{\"grams\":120}").unwrap())
                .await
                .unwrap(),
            JsonSchemaValidation::Valid
        );
        assert!(matches!(
            schema
                .validate(&Utf16JsonProjection::parse("{\"grams\":-1}").unwrap())
                .await
                .unwrap(),
            JsonSchemaValidation::Invalid { .. }
        ));
    }
    #[tokio::test]
    async fn schema_patterns_use_ecmascript_whitespace() {
        for (pattern, whitespace_valid) in [(r"^\s+$", true), (r"^\S+$", false)] {
            let schema = NativeSchema::compile(&serde_json::json!({
                "type": "string",
                "pattern": pattern
            }))
            .unwrap();
            for (character, expected) in [
                ('\u{2000}', whitespace_valid),
                ('\u{2028}', whitespace_valid),
                ('a', !whitespace_valid),
                ('\u{200b}', !whitespace_valid),
            ] {
                let input = serde_json::to_string(&character.to_string()).unwrap();
                let result = schema
                    .validate(&Utf16JsonProjection::parse(&input).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    matches!(result, JsonSchemaValidation::Valid),
                    expected,
                    "pattern {pattern}, U+{:04X}",
                    character as u32
                );
            }
        }
    }
    #[test]
    fn invalid_and_external_schemas_fail_before_execution() {
        for schema in [
            serde_json::json!({"$ref":"https://example.com/private"}),
            serde_json::json!({"type":"unknown"}),
            serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema"}),
        ] {
            assert!(NativeSchema::compile(&schema).is_err());
        }
        assert!(NativeSchema::compile(&serde_json::json!({"type":"object","definitions":{"food":{"type":"string"}},"properties":{"food":{"$ref":"#/definitions/food"}}})).is_ok());
    }
}
