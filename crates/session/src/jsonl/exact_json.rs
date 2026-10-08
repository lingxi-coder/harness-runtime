//! Pure exact JavaScript JSON codec shared with the core model boundary.

pub use lingxi_core::types::exact_json::{
    parse_exact_json, to_vec_with_overrides, ExactJsonError, ExactJsonValue, Utf16Overrides,
};

/// Reserved in-memory carrier. It is regenerated from native string escapes
/// on read and omitted by the `JsonlMessage` native serializer.
pub(crate) const PRIVATE_UTF16_KEY: &str = "lingxi_exact_json_utf16";

/// Recover private exact string leaves retained by the generic JSONL reader.
#[must_use]
pub fn message_utf16_overrides(message: &super::schema::JsonlMessage) -> Utf16Overrides {
    message
        .extra
        .get(PRIVATE_UTF16_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

/// Attach producer-owned exact string leaves to a message. The native JSONL
/// serializer consumes these overrides without emitting the private carrier.
/// Raw incoming JSON must use `parse_exact_json` instead of trusting a carrier.
pub fn set_message_utf16_overrides(
    message: &mut super::schema::JsonlMessage,
    overrides: Utf16Overrides,
) {
    if overrides.is_empty() {
        message.extra.remove(PRIVATE_UTF16_KEY);
    } else {
        message.extra.insert(
            PRIVATE_UTF16_KEY.to_owned(),
            serde_json::to_value(overrides).expect("UTF-16 overrides are JSON"),
        );
    }
}

/// Serialize a recovered message in native field order, consuming its private
/// exact strings without emitting the internal carrier.
pub fn native_message_bytes(
    message: &super::schema::JsonlMessage,
) -> Result<Vec<u8>, ExactJsonError> {
    let overrides = message_utf16_overrides(message);
    if message.json_projection.is_some() {
        return native_projection_bytes(&serde_json::to_value(message)?, &overrides, message.json_projection.as_ref());
    }
    if overrides.is_empty() {
        return Ok(serde_json::to_vec(message)?);
    }
    to_vec_with_overrides(&serde_json::to_value(message)?, &overrides)
}

/// Apply a producer-owned row projection after trusted writer metadata
/// transformations, preserving exact input keys and strings without carriers.
pub fn native_projection_bytes(
    value: &serde_json::Value,
    overrides: &Utf16Overrides,
    projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
) -> Result<Vec<u8>, ExactJsonError> {
    let Some(projection) = projection else { return to_vec_with_overrides(value, overrides); };
    let mut projection = projection.clone();
    projection.rebase_display_value(value.clone()).map_err(|error| ExactJsonError::InvalidOverride(error.to_string()))?;
    for (pointer, code_units) in overrides {
        if let Some(existing) = projection.strings.iter().find(|entry| entry.pointer == *pointer) {
            if existing.code_units != *code_units { return Err(ExactJsonError::InvalidOverride(pointer.clone())); }
        } else {
            projection.strings.push(lingxi_core::types::utf16_json::Utf16JsonString { pointer: pointer.clone(), code_units: code_units.clone() });
        }
    }
    projection.to_json_string().map(String::into_bytes).map_err(|error| ExactJsonError::InvalidOverride(error.to_string()))
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
            assert_eq!(message.extra["origin"]["kind"], "peer");
            assert_eq!(
                message.extra["origin"]["body"],
                String::from_utf16_lossy(&units)
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
