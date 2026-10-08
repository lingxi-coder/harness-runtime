//! Claude Code 2.1.292's JSON-Schema branch of `bro`, expressed over exact
//! source projections. Both the tool input and its schema stay in the flat
//! UTF-16 carrier; only schema metadata is inspected and no input `Value` is
//! materialized.

use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::collections::{HashMap, HashSet};

/// Counts input fields that the schema branch changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JsonSchemaCoercionStats {
    /// Top-level string fields parsed and replaced with JSON values.
    pub coerced_fields: usize,
}

/// Apply the JSON-Schema half of Claude Code 2.1.292 `bro`.
///
/// `input_json_schema` is the Source-owned schema carried as a projection.
/// `exempt_top_level_keys` mirrors the optional
/// `streamedInputRepairExemptKeys()` Set: `None` means the source supplied no
/// exemption callback and therefore exempts no keys. A caller that cannot
/// establish the metadata's provenance must track that separately and must
/// not call this as though an absent set were confirmed.
///
/// This deliberately does not implement the separate Zod `inputSchema`
/// branch. Tool input and schema data are never converted through
/// `serde_json::Value` here.
pub fn coerce_json_schema_string_fields(
    input: &Utf16JsonProjection,
    input_json_schema: Option<&Utf16JsonProjection>,
    exempt_top_level_keys: Option<&HashSet<Vec<u16>>>,
) -> Result<
    (Utf16JsonProjection, JsonSchemaCoercionStats),
    lingxi_core::types::utf16_json::Utf16JsonProjectionError,
> {
    input.validate()?;
    let mut stats = JsonSchemaCoercionStats::default();
    let Some(schema) = input_json_schema else {
        return Ok((input.clone(), stats));
    };
    schema.validate()?;

    let Some(mut input_fields) = source_top_level_fields(input)? else {
        return Ok((input.clone(), stats));
    };
    let Some(schema_root) = schema.at_pointer("") else {
        return Ok((input.clone(), stats));
    };
    let Some(properties) = member(schema_root, "properties") else {
        return Ok((input.clone(), stats));
    };
    let Some(property_schemas) = object_entries_like_javascript(properties) else {
        return Ok((input.clone(), stats));
    };

    let definitions = schema_definitions(schema_root);
    let field_indices: HashMap<Vec<u16>, usize> = input_fields
        .iter()
        .enumerate()
        .map(|(index, (key, _))| (key.clone(), index))
        .collect();

    for (key_units, property_schema) in property_schemas {
        if exempt_top_level_keys.is_some_and(|keys| keys.contains(&key_units)) {
            continue;
        }
        let Some(field_index) = field_indices.get(&key_units).copied() else {
            continue;
        };
        let Some(original_units) = input_fields[field_index].1.string_units("") else {
            continue;
        };

        let kind = match json_schema_type(property_schema, definitions, &mut Vec::new()) {
            Some(type_name) => coercion_kind(&type_name),
            None if is_annotation_only_schema(property_schema) => Some(CoercionKind::Any),
            None => None,
        };
        let Some(kind) = kind else {
            continue;
        };

        let Ok(parsed) = Utf16JsonProjection::parse_utf16_json(&original_units) else {
            continue;
        };
        if accepts(kind, &parsed, &original_units)? {
            input_fields[field_index].1 = parsed;
            stats.coerced_fields += 1;
        }
    }

    if stats.coerced_fields == 0 {
        return Ok((input.clone(), stats));
    }
    // Native `bro` shallow-copies changed inputs with `{...input}`. This keeps
    // object inputs as objects and turns an array into its enumerable indexed
    // properties, exactly as JavaScript object spread does.
    Ok((Utf16JsonProjection::object_units(input_fields)?, stats))
}

#[derive(Clone, Copy)]
enum CoercionKind {
    Array,
    Object,
    Boolean,
    Integer,
    Number,
    Any,
}

fn coercion_kind(type_name: &[u16]) -> Option<CoercionKind> {
    if ascii_eq(type_name, "array") {
        Some(CoercionKind::Array)
    } else if ascii_eq(type_name, "object") {
        Some(CoercionKind::Object)
    } else if ascii_eq(type_name, "boolean") {
        Some(CoercionKind::Boolean)
    } else if ascii_eq(type_name, "integer") {
        Some(CoercionKind::Integer)
    } else if ascii_eq(type_name, "number") {
        Some(CoercionKind::Number)
    } else {
        None
    }
}

fn accepts(
    kind: CoercionKind,
    candidate: &Utf16JsonProjection,
    original_units: &[u16],
) -> Result<bool, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    let Some(root) = candidate.at_pointer("") else {
        return Ok(false);
    };
    let is_array = root.array_len().is_some();
    let is_object = root.object_entries().is_some();
    let is_boolean = root.as_bool().is_some();
    let canonical_finite_number = if let Some(number) = root.as_number() {
        if number.is_finite() {
            candidate
                .to_json_string()?
                .encode_utf16()
                .eq(original_units.iter().copied())
        } else {
            false
        }
    } else {
        false
    };

    Ok(match kind {
        CoercionKind::Array => is_array,
        CoercionKind::Object => is_object,
        CoercionKind::Boolean => is_boolean,
        CoercionKind::Integer => {
            root.as_number()
                .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
                && canonical_finite_number
        }
        CoercionKind::Number => canonical_finite_number,
        CoercionKind::Any => is_boolean || is_array || is_object || canonical_finite_number,
    })
}

/// Source `qDe`: infer a JSON Schema type from `type`, local `$ref`, `anyOf`,
/// or `oneOf`. The shared reference set intentionally persists across union
/// branches, matching its JavaScript Set argument.
fn json_schema_type<'a>(
    schema: JsonValueView<'a>,
    definitions: Option<JsonValueView<'a>>,
    visited: &mut Vec<JsonValueView<'a>>,
) -> Option<Vec<u16>> {
    // Native tracks object identity and the total visited-set size, including
    // sibling union branches. It does not use recursion depth as that limit.
    if visited.len() > 64
        || visited.iter().any(|node| node.same_node_as(schema))
        || (schema.object_entries().is_none() && schema.array_len().is_none())
    {
        return None;
    }
    visited.push(schema);
    if let Some(value) = member(schema, "type") {
        if let Some(units) = value.code_units() {
            return Some(units.to_vec());
        }
        if let Some(length) = value.array_len() {
            let types = (0..length).filter_map(|index| {
                value
                    .get_index(index)
                    .and_then(JsonValueView::code_units)
                    .map(<[u16]>::to_vec)
            });
            if let Some(selected) = choose_union_type(types) {
                return Some(selected);
            }
        }
    }
    if let Some(reference) = member(schema, "$ref").and_then(JsonValueView::code_units) {
        if let Some(definitions) = definitions.filter(|value| js_truthy(*value)) {
            if let Some(name) = local_definition_name(reference) {
                // A matching local ref returns its result immediately, even
                // when the target is absent or cannot infer a type.
                return resolve_definition(definitions, &name)
                    .and_then(|target| json_schema_type(target, Some(definitions), visited));
            }
        }
    }
    for name in ["anyOf", "oneOf"] {
        let Some(union) = member(schema, name) else {
            continue;
        };
        let Some(length) = union.array_len() else {
            continue;
        };
        let inferred = (0..length).filter_map(|index| {
            union
                .get_index(index)
                .and_then(|branch| json_schema_type(branch, definitions, visited))
        });
        if let Some(selected) = choose_union_type(inferred) {
            return Some(selected);
        }
    }
    None
}

fn choose_union_type(types: impl IntoIterator<Item = Vec<u16>>) -> Option<Vec<u16>> {
    let mut compound = None;
    let mut saw_string = false;
    let mut other = None;
    for type_name in types {
        if ascii_eq(&type_name, "array") || ascii_eq(&type_name, "object") {
            compound.get_or_insert(type_name);
        } else if ascii_eq(&type_name, "string") {
            saw_string = true;
        } else if !ascii_eq(&type_name, "null") {
            other.get_or_insert(type_name);
        }
    }
    compound
        .or_else(|| saw_string.then(|| "string".encode_utf16().collect()))
        .or(other)
}

fn schema_definitions<'a>(schema: JsonValueView<'a>) -> Option<JsonValueView<'a>> {
    match member(schema, "$defs") {
        Some(definitions) if !definitions.is_null() => Some(definitions),
        _ => member(schema, "definitions"),
    }
}

fn local_definition_name(reference: &[u16]) -> Option<Vec<u16>> {
    let prefix_a = "#/$defs/".encode_utf16().collect::<Vec<_>>();
    let prefix_b = "#/definitions/".encode_utf16().collect::<Vec<_>>();
    let name = reference
        .strip_prefix(prefix_a.as_slice())
        .or_else(|| reference.strip_prefix(prefix_b.as_slice()))?;
    (!name.is_empty() && !name.contains(&u16::from(b'/'))).then(|| name.to_vec())
}

fn resolve_definition<'a>(
    definitions: JsonValueView<'a>,
    name: &[u16],
) -> Option<JsonValueView<'a>> {
    if let Some(value) = definitions.get_property_units(name) {
        return Some(value);
    }
    let index_text = String::from_utf16(name).ok()?;
    let index = index_text.parse::<usize>().ok()?;
    (index.to_string() == index_text)
        .then(|| definitions.get_index(index))
        .flatten()
}

fn is_annotation_only_schema(schema: JsonValueView<'_>) -> bool {
    const ALLOWED: [&str; 8] = [
        "$comment",
        "default",
        "deprecated",
        "description",
        "examples",
        "readOnly",
        "title",
        "writeOnly",
    ];
    let Some(entries) = schema.object_entries() else {
        return false;
    };
    entries.iter().all(|entry| {
        ALLOWED
            .iter()
            .any(|allowed| entry.code_units == allowed.encode_utf16().collect::<Vec<_>>())
    })
}

fn object_entries_like_javascript<'a>(
    value: JsonValueView<'a>,
) -> Option<Vec<(Vec<u16>, JsonValueView<'a>)>> {
    if let Some(entries) = value.object_entries() {
        return Some(
            entries
                .into_iter()
                .map(|entry| (entry.code_units.to_vec(), entry.value))
                .collect(),
        );
    }
    let length = value.array_len()?;
    Some(
        (0..length)
            .filter_map(|index| {
                let child = value.get_index(index)?;
                Some((index.to_string().encode_utf16().collect(), child))
            })
            .collect(),
    )
}

fn source_top_level_fields(
    input: &Utf16JsonProjection,
) -> Result<
    Option<Vec<(Vec<u16>, Utf16JsonProjection)>>,
    lingxi_core::types::utf16_json::Utf16JsonProjectionError,
> {
    if let Some(entries) = input.object_entries() {
        let mut fields = Vec::with_capacity(entries.len());
        for entry in entries {
            let key = entry.code_units.to_vec();
            fields.push((key.clone(), input.subprojection_property_units(&key)?));
        }
        return Ok(Some(fields));
    }
    let Some(length) = input.array_len("") else {
        return Ok(None);
    };
    let mut fields = Vec::with_capacity(length);
    for index in 0..length {
        let pointer = format!("/{index}");
        let value = input.subprojection(&pointer)?;
        fields.push((index.to_string().encode_utf16().collect(), value));
    }
    Ok(Some(fields))
}

fn member<'a>(value: JsonValueView<'a>, key: &str) -> Option<JsonValueView<'a>> {
    value.get_property_units(&key.encode_utf16().collect::<Vec<_>>())
}

fn js_truthy(value: JsonValueView<'_>) -> bool {
    if value.is_null() {
        return false;
    }
    if let Some(boolean) = value.as_bool() {
        return boolean;
    }
    if let Some(number) = value.as_number() {
        return number != 0.0 && !number.is_nan();
    }
    if let Some(text) = value.code_units() {
        return !text.is_empty();
    }
    // JavaScript arrays and objects are truthy, including empty ones.
    value.array_len().is_some() || value.object_entries().is_some()
}

fn ascii_eq(units: &[u16], expected: &str) -> bool {
    units.iter().copied().eq(expected.encode_utf16())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).unwrap()
    }

    fn run(input: &str, schema: &str) -> (Utf16JsonProjection, JsonSchemaCoercionStats) {
        coerce_json_schema_string_fields(&parse(input), Some(&parse(schema)), None).unwrap()
    }

    #[test]
    fn number_and_boolean_coercion_requires_native_canonical_number_spelling() {
        let (actual, stats) = run(
            r#"{"bool":"true","number":"2","integer":"3","negative_zero":"-0","decimal":"1.0","exponent":"1e0","overflow":"1e400","bad":"yes"}"#,
            r#"{"properties":{"bool":{"type":"boolean"},"number":{"type":"number"},"integer":{"type":"integer"},"negative_zero":{"type":"number"},"decimal":{"type":"number"},"exponent":{"type":"number"},"overflow":{"type":"number"},"bad":{"type":"number"}}}"#,
        );
        assert_eq!(
            actual.to_json_string().unwrap(),
            r#"{"bool":true,"number":2,"integer":3,"negative_zero":"-0","decimal":"1.0","exponent":"1e0","overflow":"1e400","bad":"yes"}"#
        );
        assert_eq!(stats.coerced_fields, 3);
    }

    #[test]
    fn array_object_and_null_candidates_follow_their_schema_root_kinds() {
        let (actual, stats) = run(
            r#"{"array":"[1,2]","object":"{\"x\":1}","array_mismatch":"{}","object_mismatch":"[]","null":"null"}"#,
            r#"{"properties":{"array":{"type":"array"},"object":{"type":"object"},"array_mismatch":{"type":"array"},"object_mismatch":{"type":"object"},"null":{"type":"object"}}}"#,
        );
        assert_eq!(
            actual.to_json_string().unwrap(),
            r#"{"array":[1,2],"object":{"x":1},"array_mismatch":"{}","object_mismatch":"[]","null":"null"}"#
        );
        assert_eq!(stats.coerced_fields, 2);
    }

    #[test]
    fn type_union_priority_matches_native_array_object_string_then_other_rule() {
        let (actual, stats) = run(
            r#"{"object":"{}","array":"[]","first_compound":"{}","nullable_boolean":"true","string_before_boolean":"true"}"#,
            r#"{"properties":{"object":{"type":["string","object"]},"array":{"anyOf":[{"type":"string"},{"type":"array"}]},"first_compound":{"type":["array","object"]},"nullable_boolean":{"type":["null","boolean"]},"string_before_boolean":{"type":["string","boolean"]}}}"#,
        );
        assert_eq!(
            actual.to_json_string().unwrap(),
            r#"{"object":{},"array":[],"first_compound":"{}","nullable_boolean":true,"string_before_boolean":"true"}"#
        );
        assert_eq!(stats.coerced_fields, 3);
    }

    #[test]
    fn local_refs_resolve_defs_and_definitions_without_falling_back_from_present_defs() {
        let (actual, stats) = run(
            r#"{"a":"[]","b":"{}","blocked":"{}","external":"[]"}"#,
            r##"{"$defs":{"array":{"type":"array"},"alias":{"$ref":"#/$defs/array"}},"definitions":{"object":{"type":"object"}},"properties":{"a":{"$ref":"#/$defs/array"},"b":{"$ref":"#/definitions/object"},"blocked":{"$ref":"#/definitions/object"},"external":{"$ref":"https://example.invalid/schema"}}}"##,
        );
        assert_eq!(
            actual.to_json_string().unwrap(),
            r#"{"a":[],"b":"{}","blocked":"{}","external":"[]"}"#
        );
        assert_eq!(stats.coerced_fields, 1);

        let (legacy, legacy_stats) = run(
            r#"{"value":"{}"}"#,
            r##"{"definitions":{"object":{"type":"object"}},"properties":{"value":{"$ref":"#/definitions/object"}}}"##,
        );
        assert_eq!(legacy.to_json_string().unwrap(), r#"{"value":{}}"#);
        assert_eq!(legacy_stats.coerced_fields, 1);
    }

    #[test]
    fn annotation_only_schemas_use_native_any_without_parsing_json_strings_or_null() {
        let (actual, stats) = run(
            r#"{"boolean":"false","compound":"{\"x\":1}","number":"2","string":"\"text\"","null":"null","bad_number":"01","unknown_key":"true"}"#,
            r#"{"properties":{"boolean":{"description":"optional"},"compound":{"title":"optional"},"number":{},"string":{"description":"optional"},"null":{"readOnly":true},"bad_number":{"deprecated":true},"unknown_key":{"x-custom":true}}}"#,
        );
        assert_eq!(
            actual.to_json_string().unwrap(),
            r#"{"boolean":false,"compound":{"x":1},"number":2,"string":"\"text\"","null":"null","bad_number":"01","unknown_key":"true"}"#
        );
        assert_eq!(stats.coerced_fields, 3);
    }

    #[test]
    fn exemptions_are_exact_utf16_top_level_keys_and_none_means_no_exemptions() {
        let input = parse(r#"{"coerce":"true","exempt":"[]"}"#);
        let schema =
            parse(r#"{"properties":{"coerce":{"type":"boolean"},"exempt":{"type":"array"}}}"#);
        let no_exemptions = coerce_json_schema_string_fields(&input, Some(&schema), None).unwrap();
        assert_eq!(
            no_exemptions.0.to_json_string().unwrap(),
            r#"{"coerce":true,"exempt":[]}"#
        );

        let empty = HashSet::new();
        let explicit_empty =
            coerce_json_schema_string_fields(&input, Some(&schema), Some(&empty)).unwrap();
        assert_eq!(explicit_empty.0, no_exemptions.0);

        let exempt = HashSet::from(["exempt".encode_utf16().collect()]);
        let selective =
            coerce_json_schema_string_fields(&input, Some(&schema), Some(&exempt)).unwrap();
        assert_eq!(
            selective.0.to_json_string().unwrap(),
            r#"{"coerce":true,"exempt":"[]"}"#
        );
    }

    #[test]
    fn schema_and_input_keys_with_unpaired_utf16_units_match_and_round_trip() {
        let input = parse(r#"{"\ud800":"true","ordinary":"false"}"#);
        let schema =
            parse(r#"{"properties":{"\ud800":{"type":"boolean"},"ordinary":{"type":"boolean"}}}"#);
        let (actual, stats) =
            coerce_json_schema_string_fields(&input, Some(&schema), None).unwrap();
        assert_eq!(
            actual.to_json_string().unwrap(),
            r#"{"\ud800":true,"ordinary":false}"#
        );
        assert_eq!(
            actual
                .subprojection_property_units(&[0xd800])
                .unwrap()
                .at_pointer("")
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(stats.coerced_fields, 2);
    }

    #[test]
    fn recursive_refs_terminate_and_nonobject_inputs_remain_unchanged() {
        let (actual, stats) = run(
            r#"{"flag":"true"}"#,
            r##"{"$defs":{"recursive":{"anyOf":[{"$ref":"#/$defs/recursive"},{"type":"boolean"}]}},"properties":{"flag":{"$ref":"#/$defs/recursive"}}}"##,
        );
        assert_eq!(actual.to_json_string().unwrap(), r#"{"flag":true}"#);
        assert_eq!(stats.coerced_fields, 1);

        let schema = parse(r#"{"properties":{"0":{"type":"boolean"}}}"#);
        let array = parse(r#"["true"]"#);
        let coerced_array = coerce_json_schema_string_fields(&array, Some(&schema), None).unwrap();
        assert_eq!(coerced_array.0.to_json_string().unwrap(), r#"{"0":true}"#);
        assert_eq!(coerced_array.1.coerced_fields, 1);

        for source in [r#""true""#, "null", "1"] {
            let input = parse(source);
            let output = coerce_json_schema_string_fields(&input, Some(&schema), None).unwrap();
            assert_eq!(output.0, input);
            assert_eq!(output.1.coerced_fields, 0);
        }
    }
    #[test]
    fn reference_names_retain_lone_utf16_and_missing_refs_do_not_fall_through() {
        let input = Utf16JsonProjection::parse(r#"{"flag":"true","missing":"true"}"#).unwrap();
        let schema=Utf16JsonProjection::parse(r##"{"$defs":{"\ud800":{"type":"boolean"}},"properties":{"flag":{"$ref":"#/$defs/\ud800"},"missing":{"$ref":"#/$defs/absent","anyOf":[{"type":"boolean"}]}}}"##).unwrap();
        let (out, stats) = coerce_json_schema_string_fields(&input, Some(&schema), None).unwrap();
        assert_eq!(out.get_property("flag").unwrap().as_bool(), Some(true));
        assert_eq!(out.string_at("/missing"), Some("true"));
        assert_eq!(stats.coerced_fields, 1);
    }

    #[test]
    fn visited_budget_counts_sibling_nodes_and_shared_ref_identity() {
        let branches = std::iter::repeat_n("{}", 65)
            .chain(std::iter::once(r#"{"type":"boolean"}"#))
            .collect::<Vec<_>>()
            .join(",");
        let schema = Utf16JsonProjection::parse(&format!(
            r#"{{"properties":{{"flag":{{"anyOf":[{branches}]}}}}}}"#
        ))
        .unwrap();
        let input = Utf16JsonProjection::parse(r#"{"flag":"true"}"#).unwrap();
        let (out, stats) = coerce_json_schema_string_fields(&input, Some(&schema), None).unwrap();
        assert_eq!(out, input);
        assert_eq!(stats.coerced_fields, 0);
    }
}
