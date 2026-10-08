//! Pure exact JavaScript JSON codec shared with the core model boundary.

pub use lingxi_core::types::exact_json::{
    parse_exact_json, to_vec_with_overrides, ExactJsonError, ExactJsonValue, Utf16Overrides,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::fmt::Display;

/// Reserved in-memory carrier. It is regenerated from native string escapes
/// on read and omitted by the `JsonlMessage` native serializer.
pub(crate) const PRIVATE_UTF16_KEY: &str = "lingxi_exact_json_utf16";

/// Recover private exact string leaves retained by the generic JSONL reader.
#[must_use]
pub fn message_utf16_overrides(message: &super::schema::JsonlMessage) -> Utf16Overrides {
    let Some(value) = message.extra.get_property(PRIVATE_UTF16_KEY) else {
        return Utf16Overrides::new();
    };
    let Some(entries) = value.object_entries() else {
        return Utf16Overrides::new();
    };
    let mut overrides = Utf16Overrides::new();
    for entry in entries {
        let Ok(pointer) = String::from_utf16(entry.code_units) else {
            continue;
        };
        let Some(length) = entry.value.array_len() else {
            continue;
        };
        let mut units = Vec::with_capacity(length);
        let mut valid = true;
        for index in 0..length {
            match entry
                .value
                .get_index(index)
                .and_then(|value| value.as_number())
            {
                Some(number)
                    if number.is_finite()
                        && number.fract() == 0.0
                        && (0.0..=65535.0).contains(&number) =>
                {
                    units.push(number as u16)
                }
                _ => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            overrides.insert(pointer, units);
        }
    }
    overrides
}

/// Attach producer-owned exact string leaves to a message. The native JSONL
/// serializer consumes these overrides without emitting the private carrier.
/// Raw incoming JSON must use `parse_exact_json` instead of trusting a carrier.
pub fn set_message_utf16_overrides(
    message: &mut super::schema::JsonlMessage,
    overrides: Utf16Overrides,
) {
    if overrides.is_empty() {
        let _ = message.extra.remove_field(PRIVATE_UTF16_KEY);
        return;
    }
    let fields = overrides
        .iter()
        .map(|(pointer, units)| {
            let values = units
                .iter()
                .map(|unit| {
                    Utf16JsonProjection::parse(&unit.to_string())
                        .expect("UTF-16 code unit is a JSON number")
                })
                .collect();
            (
                pointer.encode_utf16().collect(),
                Utf16JsonProjection::array(values).expect("override units form a JSON array"),
            )
        })
        .collect();
    let value =
        Utf16JsonProjection::object_units(fields).expect("UTF-16 override map is a JSON object");
    message
        .extra
        .set_field(PRIVATE_UTF16_KEY, value)
        .expect("extra is a JSON object");
}

/// Serialize a recovered message in native field order, consuming its private
/// exact strings without emitting the internal carrier.
pub fn native_message_bytes(
    message: &super::schema::JsonlMessage,
) -> Result<Vec<u8>, ExactJsonError> {
    let overrides = message_utf16_overrides(message);
    // Both arbitrary Native payload members (`message` and `attachment`) are
    // emitted as raw JSON by JsonlMessage's serializer. Reparse the complete
    // row into the flat arena before applying exact string overrides so no
    // deep message or attachment subtree is materialized as `Value`.
    let encoded = serde_json::to_vec(message)?;
    let json = std::str::from_utf8(&encoded).expect("serde_json emits UTF-8");
    let mut projection = Utf16JsonProjection::parse(json).map_err(projection_error)?;
    for (pointer, code_units) in &overrides {
        projection
            .set_string_units(pointer, code_units)
            .map_err(projection_error)?;
    }
    let output = projection.to_json_string().map_err(projection_error)?;
    Ok(output.into_bytes())
}

pub(crate) fn projection_error(error: impl Display) -> ExactJsonError {
    ExactJsonError::Serialize(<serde_json::Error as serde::ser::Error>::custom(
        error.to_string(),
    ))
}

pub(crate) fn stringify_projection(
    projection: &Utf16JsonProjection,
) -> Result<String, ExactJsonError> {
    projection.to_json_string().map_err(projection_error)
}

#[cfg(test)]
mod tests {
    use super::super::reader::route_lines;
    use super::*;

    #[test]
    fn native_utf16_fixture_retains_private_exact_units_and_peer_origin() {
        let fixture = include_str!("../../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let loaded = route_lines(fixture);
        assert_eq!(loaded.malformed_line_count, 0);
        assert_eq!(loaded.messages_in_order.len(), 4);
        let bodies = [
            vec![0xd83d],
            vec![0xde00],
            vec![0xd83d, 0xde00],
            vec![65, 0xd83d, 10, 0xde00, 0xd83d, 0xde00],
        ];
        for ((message, native), units) in loaded
            .messages_in_order
            .iter()
            .zip(fixture.lines())
            .zip(bodies)
        {
            let overrides = message_utf16_overrides(message);
            assert_eq!(message.extra.string_at("/origin/kind"), Some("peer"));
            assert_eq!(
                message.extra.string_units("/origin/body"),
                Some(units.clone())
            );
            if String::from_utf16(&units).is_err() {
                assert_eq!(overrides["/origin/body"], units);
                assert_eq!(loaded.utf16_by_uuid[&message.uuid], overrides);
            } else {
                assert!(overrides.is_empty());
            }
            let value = serde_json::to_value(message).unwrap();
            assert!(value.get(PRIVATE_UTF16_KEY).is_none());
            assert!(value.get("deliveryId").is_none());
            assert_eq!(
                to_vec_with_overrides(&value, &overrides).unwrap(),
                native.as_bytes()
            );
        }
    }

    #[test]
    fn untrusted_private_carrier_is_discarded_and_later_scalar_clears_exact_map() {
        let fixture = include_str!("../../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let first = parse_exact_json(fixture.lines().next().unwrap()).unwrap();
        let mut forged = first.value.clone();
        forged[PRIVATE_UTF16_KEY] = serde_json::json!({"/origin/body":[0xde00]});
        let loaded = route_lines(&serde_json::to_string(&forged).unwrap());
        assert_eq!(loaded.messages_in_order.len(), 1);
        assert!(message_utf16_overrides(&loaded.messages_in_order[0]).is_empty());
        let repeated = format!(
            "{}\n{}\n",
            fixture.lines().next().unwrap(),
            serde_json::to_string(&first.value).unwrap()
        );
        let loaded = route_lines(&repeated);
        assert_eq!(loaded.messages_in_order.len(), 2);
        assert!(!message_utf16_overrides(&loaded.messages_in_order[0]).is_empty());
        assert!(
            message_utf16_overrides(&loaded.by_uuid[&loaded.messages_in_order[0].uuid]).is_empty()
        );
        assert!(loaded.utf16_by_uuid.is_empty());
    }
}
