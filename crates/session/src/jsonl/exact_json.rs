//! Pure exact JavaScript JSON codec shared with the core model boundary.

pub use lingxi_core::types::exact_json::{
    parse_exact_json, to_vec_with_overrides, ExactJsonError, ExactJsonValue, Utf16Overrides,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::fmt::Display;

/// Reserved in-memory carrier. It is regenerated from native string escapes
/// on read and omitted by the `JsonlMessage` native serializer.
pub(crate) const PRIVATE_UTF16_KEY: &str = "lingxi_exact_json_utf16";

/// Reserved in-memory carrier for the original outer JSONL property order.
/// It stores only exact UTF-16 key strings, never a source row or its values.
pub(crate) const PRIVATE_SOURCE_KEY_ORDER_KEY: &str = "lingxi_jsonl_source_key_order";

/// Recover the source row's exact ordered top-level key units.
#[must_use]
pub(crate) fn message_source_key_order(
    message: &super::schema::JsonlMessage,
) -> Option<Vec<Vec<u16>>> {
    let value = message.extra.get_property(PRIVATE_SOURCE_KEY_ORDER_KEY)?;
    let length = value.array_len()?;
    let mut order = Vec::with_capacity(length);
    for index in 0..length {
        order.push(value.get_index(index)?.code_units()?.to_vec());
    }
    Some(order)
}

/// Attach reader-generated key order without retaining any row payload.
pub(crate) fn set_message_source_key_order(
    message: &mut super::schema::JsonlMessage,
    order: Vec<Vec<u16>>,
) {
    let keys = order
        .into_iter()
        .map(|code_units| {
            let display = String::from_utf16_lossy(&code_units);
            Utf16JsonProjection::root_string(display, code_units)
                .expect("source property key display matches its UTF-16 units")
        })
        .collect();
    let order = Utf16JsonProjection::array(keys).expect("source key order is a JSON array");
    message
        .extra
        .set_field(PRIVATE_SOURCE_KEY_ORDER_KEY, order)
        .expect("extra is a JSON object");
}

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
    let output = native_message_projection(message)?
        .to_json_string()
        .map_err(projection_error)?;
    Ok(output.into_bytes())
}

/// Serialize a typed transcript message to one flat source row, preserving
/// its exact string units without a recursive `Value` or whole-row shadow.
pub fn native_message_projection(
    message: &super::schema::JsonlMessage,
) -> Result<Utf16JsonProjection, ExactJsonError> {
    let encoded = serde_json::to_vec(message)?;
    let json = std::str::from_utf8(&encoded).expect("serde_json emits UTF-8");
    let mut projection = Utf16JsonProjection::parse(json).map_err(projection_error)?;
    for (pointer, code_units) in message_utf16_overrides(message) {
        projection
            .set_string_units(&pointer, &code_units)
            .map_err(projection_error)?;
    }
    Ok(projection)
}

/// Remove the private delivery identity before comparing a durable source row
/// with its existing JSONL copy.
pub(crate) fn payload_without_delivery_id(
    mut payload: Utf16JsonProjection,
) -> Result<Utf16JsonProjection, ExactJsonError> {
    payload
        .remove_field("deliveryId")
        .map_err(projection_error)?;
    Ok(payload)
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
            let origin = message.origin_projection.as_ref().unwrap();
            assert_eq!(origin.string_at("/kind"), Some("peer"));
            assert_eq!(origin.string_units("/body"), Some(units.clone()));
            if String::from_utf16(&units).is_err() {
                assert_eq!(overrides["/origin/body"], units);
                assert_eq!(loaded.utf16_by_uuid[&message.uuid], overrides);
            } else {
                assert!(overrides.is_empty());
            }
            let serialized = native_message_projection(message).unwrap();
            assert!(!serialized.contains_property(PRIVATE_UTF16_KEY));
            assert!(!serialized.contains_property("deliveryId"));
            assert_eq!(
                serialized.to_json_string().unwrap().as_bytes(),
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
