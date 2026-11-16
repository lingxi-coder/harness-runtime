//! Exact UTF-16 text carried through client JSON and UniFFI boundaries.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// A JavaScript string represented as exact UTF-16 code units.
///
/// Serde keeps the established protocol representation as an ordinary JSON
/// string. `display()` is only for the final UI rendering boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ClientTextDto {
    /// UTF-16 code units, including isolated surrogates.
    pub code_units: Vec<u16>,
}

/// Decode retained assistant text with the same exact parser as live events.
/// Native JSON parsers can reject a fragment ending in an isolated surrogate.
/// Other or invalid events remain the caller's responsibility.
#[must_use]
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn retained_text_delta(event_json: String) -> Option<ClientTextDto> {
    match serde_json::from_str::<super::events::ClientEvent>(&event_json).ok()? {
        super::events::ClientEvent::TextDelta { text } => Some(text),
        _ => None,
    }
}

impl ClientTextDto {
    /// Build exact units from ordinary UTF-8 text.
    #[must_use]
    pub fn from_utf8(value: &str) -> Self {
        Self {
            code_units: value.encode_utf16().collect(),
        }
    }

    /// Construct from an existing UTF-16 sequence.
    #[must_use]
    pub fn from_code_units(code_units: Vec<u16>) -> Self {
        Self { code_units }
    }

    /// Display-safe text for an explicit UI rendering sink.
    #[must_use]
    pub fn display(&self) -> String {
        String::from_utf16_lossy(&self.code_units)
    }

    /// Copy a root-string source projection into the FFI carrier.
    #[cfg(feature = "adapter")]
    pub fn from_projection(
        value: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<Self, &'static str> {
        value
            .string_units("")
            .map(|code_units| Self { code_units })
            .ok_or("expected a root-string source projection")
    }

    /// Restore a source projection without deriving it from display text.
    #[cfg(feature = "adapter")]
    #[must_use]
    pub fn into_projection(self) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        let display = String::from_utf16_lossy(&self.code_units);
        lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(display, self.code_units)
            .expect("display is derived from the exact UTF-16 units")
    }
}

impl From<&str> for ClientTextDto {
    fn from(value: &str) -> Self {
        Self::from_utf8(value)
    }
}

impl From<String> for ClientTextDto {
    fn from(value: String) -> Self {
        Self::from_utf8(&value)
    }
}

impl Serialize for ClientTextDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw = RawValue::from_string(encode_json_string(&self.code_units))
            .map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClientTextDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        decode_json_string(raw.get()).map_err(serde::de::Error::custom)
    }
}

pub(crate) fn decode_json_string(raw: &str) -> Result<ClientTextDto, String> {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') {
        return Err("expected a JSON string".into());
    }
    let mut at = 1;
    let mut code_units = Vec::new();
    while at + 1 < bytes.len() {
        match bytes[at] {
            b'\\' => {
                at += 1;
                let escape = *bytes
                    .get(at)
                    .ok_or_else(|| "unterminated JSON string escape".to_string())?;
                match escape {
                    b'"' => code_units.push(u16::from(b'"')),
                    b'\\' => code_units.push(u16::from(b'\\')),
                    b'/' => code_units.push(u16::from(b'/')),
                    b'b' => code_units.push(0x0008),
                    b'f' => code_units.push(0x000c),
                    b'n' => code_units.push(0x000a),
                    b'r' => code_units.push(0x000d),
                    b't' => code_units.push(0x0009),
                    b'u' => {
                        let end = at + 5;
                        if end >= bytes.len() {
                            return Err("short JSON unicode escape".into());
                        }
                        let digits = &raw[at + 1..end];
                        let unit = u16::from_str_radix(digits, 16)
                            .map_err(|_| "invalid JSON unicode escape".to_string())?;
                        code_units.push(unit);
                        at = end - 1;
                    }
                    _ => return Err("invalid JSON string escape".into()),
                }
                at += 1;
            }
            byte if byte < 0x20 => {
                return Err("unescaped control in JSON string".into());
            }
            byte if byte < 0x80 => {
                code_units.push(u16::from(byte));
                at += 1;
            }
            _ => {
                let character = raw[at..]
                    .chars()
                    .next()
                    .ok_or_else(|| "invalid UTF-8 in JSON string".to_string())?;
                let mut units = [0; 2];
                code_units.extend_from_slice(character.encode_utf16(&mut units));
                at += character.len_utf8();
            }
        }
    }
    Ok(ClientTextDto { code_units })
}

pub(crate) fn encode_json_string(code_units: &[u16]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(code_units.len() + 2);
    output.push('"');
    let mut index = 0;
    while index < code_units.len() {
        let unit = code_units[index];
        match unit {
            0x0022 => output.push_str("\\\""),
            0x005c => output.push_str("\\\\"),
            0x0008 => output.push_str("\\b"),
            0x000c => output.push_str("\\f"),
            0x000a => output.push_str("\\n"),
            0x000d => output.push_str("\\r"),
            0x0009 => output.push_str("\\t"),
            0x0000..=0x001f => write!(&mut output, "\\u{unit:04x}").expect("String write"),
            0xd800..=0xdbff
                if code_units
                    .get(index + 1)
                    .is_some_and(|next| (0xdc00..=0xdfff).contains(next)) =>
            {
                let low = code_units[index + 1];
                let scalar =
                    0x10000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
                output.push(char::from_u32(scalar).expect("valid surrogate pair"));
                index += 1;
            }
            0xd800..=0xdfff => write!(&mut output, "\\u{unit:04x}").expect("String write"),
            _ => output.push(char::from_u32(u32::from(unit)).expect("BMP scalar")),
        }
        index += 1;
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_deltas_keep_each_exact_fragment_without_changing_the_wire_shape() {
        let units = [0x4e2d, 0xd83d, 0xde00, 0xd800, 0x61, 0xdc00];
        for split in 0..=units.len() {
            let mut restored = Vec::new();
            for fragment in [&units[..split], &units[split..]] {
                let event = super::super::events::ClientEvent::TextDelta {
                    text: ClientTextDto::from_code_units(fragment.to_vec()),
                };
                let json = serde_json::to_string(&event).unwrap();
                let text = retained_text_delta(json).unwrap();
                assert_eq!(text.code_units, fragment);
                restored.extend(text.code_units);
            }
            assert_eq!(restored, units);
        }
        for other in [
            r#"{"type":"turn_started","turn_id":1}"#,
            r#"{"type":"text_delta"}"#,
            r#"{"type":"text_delta","text":4}"#,
            r#"{"type":"unknown","text":"ignored"}"#,
        ] {
            assert!(retained_text_delta(other.into()).is_none());
        }
    }

    #[test]
    fn exact_utf16_text_round_trips_as_a_plain_json_string() {
        let text = ClientTextDto::from_code_units(vec![b'Q' as u16, 0xd800, b'!' as u16]);
        let encoded = serde_json::to_string(&text).unwrap();
        assert_eq!(encoded, r#""Q\ud800!""#);
        let decoded: ClientTextDto = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, text);
        assert_eq!(text.display(), "Q�!");
    }

    #[cfg(feature = "adapter")]
    #[test]
    fn source_projection_conversion_preserves_lone_surrogate_units() {
        let units = vec![b't' as u16, 0xd800];
        let source = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&units),
            units.clone(),
        )
        .unwrap();
        let text = ClientTextDto::from_projection(&source).unwrap();
        assert_eq!(text.code_units, units);
        let restored = text.into_projection();
        assert_eq!(restored.string_units(""), Some(units));
    }

    #[test]
    fn scalar_text_uses_the_same_wire_shape() {
        let text = ClientTextDto::from_utf8("hi 🧭");
        let encoded = serde_json::to_string(&text).unwrap();
        assert_eq!(encoded, r#""hi 🧭""#);
        assert_eq!(
            serde_json::from_str::<ClientTextDto>(&encoded).unwrap(),
            text
        );
    }
}
