//! Shared output-schema gate for main and nested tool dispatch.

use boon::{Compiler, Schemas, ValidationError};
use serde_json::Value;

fn flatten(error: &ValidationError, out: &mut Vec<String>) {
    if error.causes.is_empty() {
        let location = error.instance_location.to_string();
        let location = if location.is_empty() {
            "(root)".to_string()
        } else {
            location
        };
        out.push(format!("at '{location}': {}", error.kind));
    } else {
        for cause in &error.causes {
            flatten(cause, out);
        }
    }
}

/// Validate a hook-supplied result against a tool's exported output schema.
/// A broken tool schema is a host bug, so it does not reject the hook result.
pub fn validate(schema: &Value, output: &Value) -> Result<(), String> {
    const URL: &str = "mem://tool-output-schema";
    let mut schemas = Schemas::new();
    let mut compiler = Compiler::new();
    if let Err(error) = compiler.add_resource(URL, schema.clone()) {
        tracing::warn!(%error, "tool output_schema failed to load");
        return Ok(());
    }
    let index = match compiler.compile(URL, &mut schemas) {
        Ok(index) => index,
        Err(error) => {
            tracing::warn!(%error, "tool output_schema failed to compile");
            return Ok(());
        }
    };
    if let Err(error) = schemas.validate(output, index) {
        let mut issues = Vec::new();
        flatten(&error, &mut issues);
        if issues.is_empty() {
            issues.push(error.kind.to_string());
        }
        return Err(issues.join("; "));
    }
    Ok(())
}
