//! Flat, lossless JSON transport for the persistent StructuredOutput worker.
//!
//! This is an IPC DTO, not a JSON projection: strings and keys travel as exact
//! UTF-16 code units, numbers as IEEE-754 bit strings, and the node graph is
//! flat so its serialization never recursively walks user data.

use lingxi_core::types::utf16_json::{
    JsonValueView, Utf16JsonProjection, Utf16JsonProjectionError,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(super) struct SourceValueDto {
    root: usize,
    nodes: Vec<FlatNode>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FlatNode {
    Null,
    Bool {
        value: bool,
    },
    Number {
        #[serde(rename = "bitsHex")]
        bits_hex: String,
    },
    String {
        #[serde(rename = "codeUnits")]
        code_units: Vec<u16>,
    },
    Array {
        items: Vec<usize>,
    },
    Object {
        entries: Vec<FlatProperty>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlatProperty {
    #[serde(rename = "keyUnits")]
    key_units: Vec<u16>,
    value: usize,
}

enum Visit<'a> {
    Value(JsonValueView<'a>),
    FinishArray(usize),
    FinishObject(Vec<Vec<u16>>),
}

impl SourceValueDto {
    pub(super) fn from_projection(
        projection: &Utf16JsonProjection,
    ) -> Result<Self, Utf16JsonProjectionError> {
        projection.validate()?;
        let root = projection
            .at_pointer("")
            .ok_or(Utf16JsonProjectionError::InvalidPointer)?;
        let mut visits = vec![Visit::Value(root)];
        let mut nodes = Vec::new();
        let mut completed = Vec::new();

        while let Some(visit) = visits.pop() {
            match visit {
                Visit::Value(value) if value.is_null() => {
                    completed.push(push(&mut nodes, FlatNode::Null));
                }
                Visit::Value(value) if value.as_bool().is_some() => {
                    completed.push(push(
                        &mut nodes,
                        FlatNode::Bool {
                            value: value.as_bool().expect("checked above"),
                        },
                    ));
                }
                Visit::Value(value) if value.as_number().is_some() => {
                    let bits = value.as_number().expect("checked above").to_bits();
                    completed.push(push(
                        &mut nodes,
                        FlatNode::Number {
                            bits_hex: format!("{bits:016x}"),
                        },
                    ));
                }
                Visit::Value(value) if value.code_units().is_some() => {
                    completed.push(push(
                        &mut nodes,
                        FlatNode::String {
                            code_units: value.code_units().expect("checked above").to_vec(),
                        },
                    ));
                }
                Visit::Value(value) if value.array_len().is_some() => {
                    let length = value.array_len().expect("checked above");
                    let mut children = Vec::with_capacity(length);
                    for index in 0..length {
                        children.push(value.get_index(index).ok_or(
                            Utf16JsonProjectionError::InvalidProjection(
                                "array view lost an indexed child",
                            ),
                        )?);
                    }
                    visits.push(Visit::FinishArray(length));
                    visits.extend(children.into_iter().rev().map(Visit::Value));
                }
                Visit::Value(value) => {
                    let entries = value.object_entries().ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "flat node is not a JSON scalar or container",
                        ),
                    )?;
                    let mut keys = Vec::with_capacity(entries.len());
                    let mut children = Vec::with_capacity(entries.len());
                    for entry in entries {
                        keys.push(entry.code_units.to_vec());
                        children.push(entry.value);
                    }
                    visits.push(Visit::FinishObject(keys));
                    visits.extend(children.into_iter().rev().map(Visit::Value));
                }
                Visit::FinishArray(length) => {
                    let start = completed.len().checked_sub(length).ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "flat array child stack underflow",
                        ),
                    )?;
                    let items = completed.split_off(start);
                    completed.push(push(&mut nodes, FlatNode::Array { items }));
                }
                Visit::FinishObject(keys) => {
                    let start = completed.len().checked_sub(keys.len()).ok_or(
                        Utf16JsonProjectionError::InvalidProjection(
                            "flat object child stack underflow",
                        ),
                    )?;
                    let children = completed.split_off(start);
                    let entries = keys
                        .into_iter()
                        .zip(children)
                        .map(|(key_units, value)| FlatProperty { key_units, value })
                        .collect();
                    completed.push(push(&mut nodes, FlatNode::Object { entries }));
                }
            }
        }

        if completed.len() != 1 {
            return Err(Utf16JsonProjectionError::InvalidProjection(
                "flat source encoder did not produce one root",
            ));
        }
        Ok(Self {
            root: completed[0],
            nodes,
        })
    }
}

fn push(nodes: &mut Vec<FlatNode>, node: FlatNode) -> usize {
    let index = nodes.len();
    nodes.push(node);
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_keeps_lone_surrogates_and_binary64_bits() {
        let projection =
            Utf16JsonProjection::parse(r#"{"\ud800":"\udfff","overflow":1e400,"negativeZero":-0}"#)
                .expect("valid projection");
        let dto = SourceValueDto::from_projection(&projection).expect("flat dto");
        let encoded = serde_json::to_value(&dto).expect("control DTO JSON");
        let nodes = encoded["nodes"].as_array().expect("flat nodes");

        let object = nodes
            .iter()
            .find(|node| node["kind"] == "object")
            .expect("root object");
        assert_eq!(
            object["entries"][0]["keyUnits"],
            serde_json::json!([0xd800])
        );

        let text = nodes
            .iter()
            .find(|node| node["kind"] == "string")
            .expect("string node");
        assert_eq!(text["codeUnits"], serde_json::json!([0xdfff]));

        let numbers: Vec<_> = nodes
            .iter()
            .filter(|node| node["kind"] == "number")
            .map(|node| node["bitsHex"].as_str().unwrap())
            .collect();
        assert!(numbers.contains(&"7ff0000000000000"));
        assert!(numbers.contains(&"8000000000000000"));
    }

    #[test]
    fn dto_preserves_source_order_while_projection_serializes_native_key_order() {
        let mut source = String::new();
        for _ in 0..2048 {
            source.push('[');
        }
        source.push_str(r#"{"z":1,"2":2,"a":3}"#);
        for _ in 0..2048 {
            source.push(']');
        }
        let projection = Utf16JsonProjection::parse(&source).expect("deep projection");
        let dto = SourceValueDto::from_projection(&projection).expect("flat dto");
        let json = serde_json::to_string(&dto).expect("flat DTO serialization");
        assert!(json.len() < 200_000);

        let input = projection
            .at_pointer(&format!("{}", "/0".repeat(2048)))
            .expect("nested object");
        let keys: Vec<Vec<u16>> = input
            .object_entries()
            .expect("object entries")
            .into_iter()
            .map(|entry| entry.code_units.to_vec())
            .collect();
        let source_order: Vec<Vec<u16>> = ["z", "2", "a"]
            .into_iter()
            .map(|key| key.encode_utf16().collect())
            .collect();
        assert_eq!(keys, source_order);

        let encoded_projection = projection
            .to_json_string()
            .expect("serialize projection with JavaScript property order");
        assert!(encoded_projection.contains(r#"{"2":2,"z":1,"a":3}"#));
    }
}
