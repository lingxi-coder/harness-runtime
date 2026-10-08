//! JSON string leaves which retain JavaScript's UTF-16 code units.
//!
//! Rust display strings cannot represent lone surrogates. The exact units stay
//! in a private pointer map; the native JSON string emits them as `\uXXXX`.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Write;
use thiserror::Error;

/// Exact string code units indexed by an RFC 6901 JSON pointer.
pub type Utf16Overrides = BTreeMap<String, Vec<u16>>;

/// A JSON display value and the string leaves which require exact code units.
#[derive(Debug, Clone, PartialEq)]
pub struct ExactJsonValue {
    /// Ordinary Rust-compatible display projection. Lone surrogates use U+FFFD.
    pub value: Value,
    /// Private exact strings. This map is never inserted into the native value.
    pub utf16_overrides: Utf16Overrides,
}

/// Invalid JSON or an override which does not identify a string leaf.
#[derive(Debug, Error)]
pub enum ExactJsonError {
    /// An exact string must replace an existing JSON string.
    #[error("UTF-16 JSON override does not target a string leaf: {0}")]
    InvalidOverride(String),
    /// A JSON serialization failed.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    /// The input does not contain one complete JSON value.
    #[error("invalid exact JSON at byte {offset}: {message}")]
    InvalidJson {
        /// Byte offset in the input.
        offset: usize,
        /// Reason for refusing the input.
        message: &'static str,
    },
}

/// Serialize selected string leaves with exact UTF-16 units. Valid scalar
/// strings retain serde's compact escaping and the value's object-key order.
pub fn to_vec_with_overrides(
    value: &Value,
    overrides: &Utf16Overrides,
) -> Result<Vec<u8>, ExactJsonError> {
    for pointer in overrides.keys() {
        if !canonical_string_pointer(value, pointer) {
            return Err(ExactJsonError::InvalidOverride(pointer.clone()));
        }
    }
    let mut out = Vec::new();
    write_value(&mut out, value, overrides, "")?;
    Ok(out)
}

fn canonical_string_pointer(mut value: &Value, pointer: &str) -> bool {
    if pointer.is_empty() {
        return value.is_string();
    }
    let Some(tokens) = pointer.strip_prefix('/') else {
        return false;
    };
    for token in tokens.split('/') {
        value = match value {
            Value::Object(object) => {
                let key = token.replace("~1", "/").replace("~0", "~");
                if pointer_token(&key) != token {
                    return false;
                }
                let Some(child) = object.get(&key) else {
                    return false;
                };
                child
            }
            Value::Array(items) => {
                let Ok(index) = token.parse::<usize>() else {
                    return false;
                };
                if index.to_string() != token {
                    return false;
                }
                let Some(child) = items.get(index) else {
                    return false;
                };
                child
            }
            _ => return false,
        };
    }
    value.is_string()
}

fn write_value(
    out: &mut Vec<u8>,
    value: &Value,
    overrides: &Utf16Overrides,
    pointer: &str,
) -> Result<(), ExactJsonError> {
    match value {
        Value::String(_) if overrides.contains_key(pointer) => {
            write_string(out, &overrides[pointer]);
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                write_value(out, item, overrides, &format!("{pointer}/{index}"))?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_value(
                    out,
                    item,
                    overrides,
                    &format!("{pointer}/{}", pointer_token(key)),
                )?;
            }
            out.push(b'}');
        }
        _ => serde_json::to_writer(out, value)?,
    }
    Ok(())
}

fn pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn write_string(out: &mut Vec<u8>, units: &[u16]) {
    out.push(b'"');
    for scalar in char::decode_utf16(units.iter().copied()) {
        match scalar {
            Ok(ch) => write_char(out, ch),
            Err(error) => {
                write!(out, "\\u{:04x}", error.unpaired_surrogate())
                    .expect("Vec writes cannot fail");
            }
        }
    }
    out.push(b'"');
}

fn write_char(out: &mut Vec<u8>, ch: char) {
    match ch {
        '"' => out.extend_from_slice(br#"\""#),
        '\\' => out.extend_from_slice(br"\\"),
        '\u{08}' => out.extend_from_slice(br"\b"),
        '\u{0c}' => out.extend_from_slice(br"\f"),
        '\n' => out.extend_from_slice(br"\n"),
        '\r' => out.extend_from_slice(br"\r"),
        '\t' => out.extend_from_slice(br"\t"),
        ch if ch <= '\u{1f}' => {
            write!(out, "\\u{:04x}", u32::from(ch)).expect("Vec writes cannot fail");
        }
        _ => {
            let mut bytes = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut bytes).as_bytes());
        }
    }
}

/// Parse native JSON containing escaped lone surrogates. Provenance and all
/// non-string fields remain ordinary JSON; only the display projection of an
/// unpaired string changes. No role or origin interpretation occurs here.
pub fn parse_exact_json(input: &str) -> Result<ExactJsonValue, ExactJsonError> {
    // Ordinary JSON stays on serde's parser, including its normal recursion
    // limit. The fallback exists only for JavaScript strings serde cannot hold.
    if let Ok(value) = serde_json::from_str(input) {
        return Ok(ExactJsonValue {
            value,
            utf16_overrides: Utf16Overrides::new(),
        });
    }
    let mut parser = ExactParser {
        input,
        offset: 0,
        overrides: Utf16Overrides::new(),
    };
    let value = parser.value("", 0)?;
    parser.whitespace();
    if parser.offset != input.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(ExactJsonValue {
        value,
        utf16_overrides: parser.overrides,
    })
}

struct ExactParser<'a> {
    input: &'a str,
    offset: usize,
    overrides: Utf16Overrides,
}

impl ExactParser<'_> {
    fn error(&self, message: &'static str) -> ExactJsonError {
        ExactJsonError::InvalidJson {
            offset: self.offset,
            message,
        }
    }

    fn whitespace(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.offset += 1;
        }
    }

    fn take(&mut self, expected: u8) -> bool {
        self.whitespace();
        if self.input.as_bytes().get(self.offset) == Some(&expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self, pointer: &str, depth: usize) -> Result<Value, ExactJsonError> {
        self.whitespace();
        match self.input.as_bytes().get(self.offset).copied() {
            Some(b'"') => {
                let units = self.string()?;
                let display = String::from_utf16(&units).unwrap_or_else(|_| {
                    let display = String::from_utf16_lossy(&units);
                    self.overrides.insert(pointer.to_string(), units);
                    display
                });
                Ok(Value::String(display))
            }
            Some(b'[' | b'{') if depth >= 128 => Err(self.error("recursion limit exceeded")),
            Some(b'[') => {
                self.offset += 1;
                let mut items = Vec::new();
                if !self.take(b']') {
                    loop {
                        items.push(self.value(&format!("{pointer}/{}", items.len()), depth + 1)?);
                        if self.take(b']') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(self.error("expected array comma"));
                        }
                    }
                }
                Ok(Value::Array(items))
            }
            Some(b'{') => {
                self.offset += 1;
                let mut map = Map::new();
                if !self.take(b'}') {
                    loop {
                        self.whitespace();
                        let units = self.string()?;
                        // A pointer names scalar Rust keys. Reject lossy key
                        // collisions instead of attributing text to a new key.
                        let key = String::from_utf16(&units)
                            .map_err(|_| self.error("unpaired surrogate in object key"))?;
                        if !self.take(b':') {
                            return Err(self.error("expected object colon"));
                        }
                        let child = format!("{pointer}/{}", pointer_token(&key));
                        if map.contains_key(&key) {
                            self.overrides.retain(|path, _| {
                                path != &child
                                    && !path
                                        .strip_prefix(&child)
                                        .is_some_and(|tail| tail.starts_with('/'))
                            });
                        }
                        map.insert(key, self.value(&child, depth + 1)?);
                        if self.take(b'}') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(self.error("expected object comma"));
                        }
                    }
                }
                Ok(Value::Object(map))
            }
            Some(_) => {
                let start = self.offset;
                while self.input.as_bytes().get(self.offset).is_some_and(|byte| {
                    !matches!(byte, b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}')
                }) {
                    self.offset += 1;
                }
                let value = serde_json::from_str(&self.input[start..self.offset])
                    .map_err(|_| self.error("invalid JSON scalar"))?;
                if matches!(value, Value::Null | Value::Bool(_) | Value::Number(_)) {
                    Ok(value)
                } else {
                    Err(self.error("invalid JSON scalar"))
                }
            }
            None => Err(self.error("expected JSON value")),
        }
    }

    fn string(&mut self) -> Result<Vec<u16>, ExactJsonError> {
        if !self.take(b'"') {
            return Err(self.error("expected JSON string"));
        }
        let mut units = Vec::new();
        loop {
            match self.input.as_bytes().get(self.offset).copied() {
                Some(b'"') => {
                    self.offset += 1;
                    return Ok(units);
                }
                Some(b'\\') => {
                    self.offset += 1;
                    let escaped = self.input.as_bytes().get(self.offset).copied();
                    self.offset += usize::from(escaped.is_some());
                    units.push(match escaped {
                        Some(b'"') => u16::from(b'"'),
                        Some(b'\\') => u16::from(b'\\'),
                        Some(b'/') => u16::from(b'/'),
                        Some(b'b') => 8,
                        Some(b'f') => 12,
                        Some(b'n') => 10,
                        Some(b'r') => 13,
                        Some(b't') => 9,
                        Some(b'u') => {
                            let mut unit = 0_u16;
                            for _ in 0..4 {
                                let digit = self
                                    .input
                                    .as_bytes()
                                    .get(self.offset)
                                    .and_then(|byte| char::from(*byte).to_digit(16))
                                    .ok_or_else(|| self.error("invalid unicode escape"))?;
                                unit = (unit << 4) | u16::try_from(digit).expect("hex digit");
                                self.offset += 1;
                            }
                            unit
                        }
                        _ => return Err(self.error("invalid string escape")),
                    });
                }
                Some(0..=0x1f) => return Err(self.error("control character in string")),
                Some(_) => {
                    let ch = self.input[self.offset..]
                        .chars()
                        .next()
                        .expect("nonempty suffix");
                    self.offset += ch.len_utf8();
                    let mut pair = [0; 2];
                    units.extend_from_slice(ch.encode_utf16(&mut pair));
                }
                None => return Err(self.error("unterminated string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_leaves_round_trip_high_low_pair_and_mixed_units() {
        let units = [
            vec![0xd83d],
            vec![0xde00],
            vec![0xd83d, 0xde00],
            vec![65, 0xd83d, 10, 0xde00, 0xd83d, 0xde00],
        ];
        let value = Value::Array(
            units
                .iter()
                .map(|units| Value::String(String::from_utf16_lossy(units)))
                .collect(),
        );
        let overrides: Utf16Overrides = units
            .iter()
            .enumerate()
            .map(|(i, units)| (format!("/{i}"), units.clone()))
            .collect();
        let bytes = to_vec_with_overrides(&value, &overrides).unwrap();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            "[\"\\ud83d\",\"\\ude00\",\"😀\",\"A\\ud83d\\n\\ude00😀\"]"
        );
        let decoded = parse_exact_json(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(decoded.value, value);
        assert_eq!(decoded.utf16_overrides.get("/0"), Some(&units[0]));
        assert_eq!(decoded.utf16_overrides.get("/1"), Some(&units[1]));
        assert!(!decoded.utf16_overrides.contains_key("/2"));
        assert_eq!(decoded.utf16_overrides.get("/3"), Some(&units[3]));
    }

    #[test]
    fn scalar_encoding_matches_serde_including_order_and_escaped_pointer_keys() {
        let value = json!({"z":"quotes\"\\\n\u{00}é😀", "a/~": ["💡"]});
        let overrides = Utf16Overrides::from([
            (
                "/z".into(),
                value["z"].as_str().unwrap().encode_utf16().collect(),
            ),
            ("/a~1~0/0".into(), "💡".encode_utf16().collect()),
        ]);
        assert_eq!(
            to_vec_with_overrides(&value, &overrides).unwrap(),
            serde_json::to_vec(&value).unwrap()
        );
        assert!(matches!(
            to_vec_with_overrides(
                &value,
                &Utf16Overrides::from([("/missing".into(), vec![0xd83d])])
            ),
            Err(ExactJsonError::InvalidOverride(_))
        ));
        // serde's pointer lookup is permissive about some spellings. An
        // override must use the canonical path the encoder actually visits.
        for pointer in ["/~2", "/items/00"] {
            assert!(to_vec_with_overrides(
                &json!({"~2":"x", "items":["x"]}),
                &Utf16Overrides::from([(pointer.into(), vec![0xd83d])]),
            )
            .is_err());
        }
    }

    #[test]
    fn fallback_preserves_duplicate_key_last_write_and_rejects_invalid_grammar() {
        let decoded =
            parse_exact_json(r#"{"a":{"b":"\ud83d"},"a":"ordinary","x":"\ude00"}"#).unwrap();
        assert_eq!(decoded.value["a"], "ordinary");
        assert_eq!(
            decoded.utf16_overrides,
            Utf16Overrides::from([("/x".into(), vec![0xde00])])
        );
        for invalid in [
            r#"["\ud83d",]"#,
            r#"{"x":"\ud83d",}"#,
            r#"{"x":"\ud83d"}false"#,
            r#"{"x":"\ud83d","bad":01}"#,
            r#"{"x":"\ud83d","bad":"\uQQQQ"}"#,
        ] {
            assert!(parse_exact_json(invalid).is_err(), "{invalid}");
        }
    }
}
