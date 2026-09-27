//! Strict JSON-schema conversion — a byte-faithful port of claude-code 2.1.206's
//! `N8n`/`M8n` converter (structured-output strict mode).
//!
//! `N8n` walks a JSON schema and produces the strict variant the Anthropic
//! Messages API accepts for `strict: true` tools: an `object` root, every
//! object closed with `additionalProperties: false`, and only a conservative
//! keyword/type subset allowed. A schema that cannot be represented strictly is
//! rejected with a machine reason (surfaced in the `sending non-strict`
//! warning). Pure and provider-agnostic; the request path applies it behind the
//! Claude-model + `tengu_structured_output_strict` gates.

use serde_json::{Map, Value};

/// Recursion / size budget — claude `M8n(e, 32, {remaining: 1e5})`.
const MAX_DEPTH: i32 = 32;
const MAX_NODES: i64 = 100_000;

/// Keywords allowed on any node — claude `Xzh`.
const ALLOWED_KEYWORDS: &[&str] = &[
    "$schema",
    "type",
    "description",
    "title",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
];

/// Keywords allowed alongside `anyOf` — claude `Qzh`.
const ANYOF_SIBLINGS: &[&str] = &["$schema", "description", "title"];

/// Types allowed — claude `Gzc`.
const ALLOWED_TYPES: &[&str] = &[
    "object", "array", "string", "integer", "number", "boolean", "null",
];

/// Convert `schema` to its strict form (claude `N8n`). `Ok(strict_schema)` on
/// success; `Err(reason)` with claude's exact machine reason otherwise. The
/// root must resolve to an `object`.
pub fn to_strict_schema(schema: &Value) -> Result<Value, &'static str> {
    let mut remaining = MAX_NODES;
    let node = convert(schema, MAX_DEPTH, &mut remaining)?;
    if node.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root_not_object");
    }
    // claude `{...t.node, type:"object"}` — the type is already "object" here.
    Ok(node)
}

/// claude `zzc` — a JSON primitive usable in `const`/`enum` (null, string,
/// finite number, boolean). JSON numbers are always finite.
fn is_primitive(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::String(_) | Value::Number(_) | Value::Bool(_)
    )
}

/// Whether `arr` has a duplicate element (claude `new Set(x).size !== x.length`).
fn has_duplicates(arr: &[Value]) -> bool {
    for (i, a) in arr.iter().enumerate() {
        if arr[i + 1..].iter().any(|b| b == a) {
            return true;
        }
    }
    false
}

/// claude `M8n` — the recursive strict-schema node converter.
fn convert(e: &Value, depth: i32, remaining: &mut i64) -> Result<Value, &'static str> {
    if depth <= 0 {
        return Err("max_depth");
    }
    *remaining -= 1;
    if *remaining < 0 {
        return Err("max_nodes");
    }
    let Value::Object(map) = e else {
        return Err("not_object");
    };
    for key in map.keys() {
        if !ALLOWED_KEYWORDS.contains(&key.as_str()) {
            return Err("unsupported_keyword");
        }
    }
    let mut n = Map::new();
    if let Some(d) = map.get("description") {
        if !d.is_string() {
            return Err("unsupported_keyword");
        }
        n.insert("description".to_string(), d.clone());
    }
    if let Some(t) = map.get("title") {
        if !t.is_string() {
            return Err("unsupported_keyword");
        }
        n.insert("title".to_string(), t.clone());
    }
    if let Some(any_of) = map.get("anyOf") {
        for key in map.keys() {
            if key != "anyOf" && !ANYOF_SIBLINGS.contains(&key.as_str()) {
                return Err("unsupported_keyword");
            }
        }
        let Value::Array(arr) = any_of else {
            return Err("unsupported_keyword");
        };
        if arr.is_empty() {
            return Err("unsupported_keyword");
        }
        let mut out = Vec::with_capacity(arr.len());
        for s in arr {
            out.push(convert(s, depth - 1, remaining)?);
        }
        n.insert("anyOf".to_string(), Value::Array(out));
        return Ok(Value::Object(n));
    }
    if let Some(c) = map.get("const") {
        if !is_primitive(c) {
            return Err("unsupported_const");
        }
        n.insert("const".to_string(), c.clone());
    }
    if let Some(en) = map.get("enum") {
        let Value::Array(arr) = en else {
            return Err("unsupported_enum");
        };
        if arr.is_empty() || !arr.iter().all(is_primitive) || has_duplicates(arr) {
            return Err("unsupported_enum");
        }
        n.insert("enum".to_string(), Value::Array(arr.clone()));
    }
    // Type validation (claude's `o` handling).
    let o = map.get("type");
    if let Some(ty) = o {
        match ty {
            Value::String(s) => {
                if !ALLOWED_TYPES.contains(&s.as_str()) {
                    return Err("unsupported_type");
                }
                n.insert("type".to_string(), ty.clone());
            }
            Value::Array(arr) => {
                let ok = !arr.is_empty()
                    && arr.iter().all(|i| {
                        i.as_str().is_some_and(|s| {
                            ALLOWED_TYPES.contains(&s) && s != "object" && s != "array"
                        })
                    })
                    && !has_duplicates(arr);
                if !ok {
                    return Err("unsupported_type");
                }
                n.insert("type".to_string(), ty.clone());
            }
            _ => return Err("unsupported_type"),
        }
    }
    // `o` as a plain string ("object"/"array"/…), or None (incl. a type-array).
    let o_str = o.and_then(Value::as_str);
    if o_str != Some("object")
        && (map.contains_key("properties")
            || map.contains_key("required")
            || map.contains_key("additionalProperties"))
    {
        return Err("mismatched_keywords");
    }
    if o_str != Some("array") && map.contains_key("items") {
        return Err("mismatched_keywords");
    }
    if o_str == Some("object") {
        let Some(Value::Object(props)) = map.get("properties") else {
            return Err("no_properties");
        };
        if let Some(ap) = map.get("additionalProperties") {
            if ap != &Value::Bool(false) {
                return Err("additional_properties");
            }
        }
        if let Some(req) = map.get("required") {
            let Value::Array(ra) = req else {
                return Err("invalid_required");
            };
            let ok = ra
                .iter()
                .all(|a| a.as_str().is_some_and(|s| props.contains_key(s)))
                && !has_duplicates(ra);
            if !ok {
                return Err("invalid_required");
            }
            n.insert("required".to_string(), req.clone());
        }
        let mut strict_props = Map::new();
        for (k, v) in props {
            strict_props.insert(k.clone(), convert(v, depth - 1, remaining)?);
        }
        n.insert("properties".to_string(), Value::Object(strict_props));
        n.insert("additionalProperties".to_string(), Value::Bool(false));
    } else if o_str == Some("array") {
        match map.get("items") {
            None | Some(Value::Array(_)) => return Err("unsupported_items"),
            Some(items) => {
                n.insert("items".to_string(), convert(items, depth - 1, remaining)?);
            }
        }
    } else if o.is_none() && !n.contains_key("enum") && !n.contains_key("const") {
        return Err("missing_type");
    }
    Ok(Value::Object(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn simple_object_gets_additional_properties_false() {
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let strict = to_strict_schema(&schema).unwrap();
        assert_eq!(
            strict,
            json!({
                "type": "object",
                "required": ["name"],
                "properties": { "name": { "type": "string" } },
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn nested_objects_and_arrays_are_closed_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "id": { "type": "integer" } } }
                }
            }
        });
        let strict = to_strict_schema(&schema).unwrap();
        // Every object node closed with additionalProperties:false.
        assert_eq!(strict["additionalProperties"], json!(false));
        assert_eq!(
            strict["properties"]["items"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn strips_schema_keyword_but_permits_it() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {}
        });
        let strict = to_strict_schema(&schema).unwrap();
        assert!(strict.get("$schema").is_none());
    }

    #[test]
    fn root_must_be_object() {
        assert_eq!(
            to_strict_schema(&json!({ "type": "string" })).unwrap_err(),
            "root_not_object"
        );
    }

    #[test]
    fn rejects_unsupported_keyword_and_type() {
        assert_eq!(
            to_strict_schema(&json!({ "type": "object", "properties": {}, "minProperties": 1 }))
                .unwrap_err(),
            "unsupported_keyword"
        );
        assert_eq!(
            to_strict_schema(&json!({ "type": "tuple" })).unwrap_err(),
            "unsupported_type"
        );
    }

    #[test]
    fn rejects_open_additional_properties() {
        assert_eq!(
            to_strict_schema(
                &json!({ "type": "object", "properties": {}, "additionalProperties": true })
            )
            .unwrap_err(),
            "additional_properties"
        );
    }

    #[test]
    fn rejects_mismatched_keywords() {
        // items on a non-array.
        assert_eq!(
            to_strict_schema(&json!({ "type": "object", "properties": {}, "items": {} }))
                .unwrap_err(),
            "mismatched_keywords"
        );
    }

    #[test]
    fn rejects_bad_required_and_missing_properties() {
        assert_eq!(
            to_strict_schema(&json!({ "type": "object", "required": ["x"], "properties": {} }))
                .unwrap_err(),
            "invalid_required"
        );
        assert_eq!(
            to_strict_schema(&json!({ "type": "object" })).unwrap_err(),
            "no_properties"
        );
    }

    #[test]
    fn accepts_enum_const_anyof() {
        // anyOf root is a valid node (though not an object root → root_not_object).
        assert_eq!(
            to_strict_schema(&json!({ "anyOf": [ { "type": "string" } ] })).unwrap_err(),
            "root_not_object"
        );
        // enum-only property node.
        let schema = json!({
            "type": "object",
            "properties": { "color": { "enum": ["r", "g", "b"] } }
        });
        let strict = to_strict_schema(&schema).unwrap();
        assert_eq!(
            strict["properties"]["color"]["enum"],
            json!(["r", "g", "b"])
        );
    }

    #[test]
    fn rejects_enum_with_duplicates_or_non_primitive() {
        assert_eq!(
            to_strict_schema(
                &json!({ "type": "object", "properties": { "x": { "enum": [1, 1] } } })
            )
            .unwrap_err(),
            "unsupported_enum"
        );
        assert_eq!(
            to_strict_schema(
                &json!({ "type": "object", "properties": { "x": { "enum": [{ "a": 1 }] } } })
            )
            .unwrap_err(),
            "unsupported_enum"
        );
    }

    #[test]
    fn rejects_multi_type_with_object_or_array() {
        // A type array containing "object" is disallowed.
        assert_eq!(
            to_strict_schema(&json!({ "type": ["object", "string"] })).unwrap_err(),
            "unsupported_type"
        );
        // A scalar multi-type is fine as a node (but not as the object root).
        assert_eq!(
            to_strict_schema(&json!({ "type": ["string", "integer"] })).unwrap_err(),
            "root_not_object"
        );
    }

    #[test]
    fn enforces_depth_budget() {
        // Build 40 nested single-property objects → exceeds MAX_DEPTH (32).
        let mut schema =
            json!({ "type": "object", "properties": { "leaf": { "type": "string" } } });
        for _ in 0..40 {
            schema = json!({ "type": "object", "properties": { "child": schema } });
        }
        assert_eq!(to_strict_schema(&schema).unwrap_err(), "max_depth");
    }
}
