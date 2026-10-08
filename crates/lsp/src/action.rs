//! LSP tool action payload.
//!
//! `LspTool` accepts a single tagged-union JSON object describing which
//! LSP capability the model wants to invoke. `LspAction` is the on-wire
//! representation and `LspResponse` is the opaque JSON return value.
//!
//! See spec §25.4 (LSP tool).

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};

/// One invocation of the `LSP` tool.
///
/// The discriminator key is `action` and values are snake-cased (matching
/// the LSP method namespace).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum LspAction {
    /// `textDocument/hover` at a position.
    Hover {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/definition` at a position.
    Definition {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/references` at a position.
    References {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/publishDiagnostics` — most recent diagnostics for the
    /// active document.
    Diagnostics,
    /// `workspace/symbol` (when `query` is `Some`) or
    /// `textDocument/documentSymbol` (when `query` is `None`).
    Symbols {
        /// Optional fuzzy filter.
        #[serde(
            default,
            serialize_with = "exact_json_string_serde::serialize_option",
            deserialize_with = "exact_json_string_serde::deserialize_option"
        )]
        query: Option<Utf16JsonProjection>,
    },
    /// `textDocument/completion` at a position.
    Completion {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
    },
    /// `textDocument/formatting` on the active document.
    Formatting,
    /// `textDocument/rename` on the symbol at a position.
    Rename {
        /// Zero-based line number.
        line: u32,
        /// Zero-based UTF-16 character offset.
        character: u32,
        /// New identifier to apply.
        #[serde(with = "exact_json_string_serde")]
        new_name: Utf16JsonProjection,
    },
}

/// Opaque JSON value returned by an LSP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspResponse(
    /// Raw JSON-RPC `result` payload.
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub Utf16JsonProjection,
);

mod exact_json_string_serde {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::value::RawValue;

    pub fn serialize<S>(value: &Utf16JsonProjection, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        lingxi_core::types::utf16_json::raw_json_serde::serialize(value, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Utf16JsonProjection, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        parse_string(raw.get()).map_err(serde::de::Error::custom)
    }

    pub fn serialize_option<S>(
        value: &Option<Utf16JsonProjection>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => serialize(value, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize_option<'de, D>(
        deserializer: D,
    ) -> Result<Option<Utf16JsonProjection>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        if raw.get() == "null" {
            return Ok(None);
        }
        parse_string(raw.get())
            .map(Some)
            .map_err(serde::de::Error::custom)
    }

    fn parse_string(source: &str) -> Result<Utf16JsonProjection, &'static str> {
        let value = Utf16JsonProjection::parse(source).map_err(|_| "invalid JSON string")?;
        if value.string_units("").is_none() {
            return Err("expected a JSON string");
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{LspAction, LspResponse};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    #[test]
    fn response_keeps_raw_utf16_json_payload() {
        let response = LspResponse(Utf16JsonProjection::parse(r#"{"probe":"\ud800"}"#).unwrap());
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"probe":"\ud800"}"#
        );
    }

    #[test]
    fn action_string_fields_preserve_lone_utf16_units() {
        let symbols: LspAction =
            serde_json::from_str(r#"{"action":"symbols","query":"q\ud800"}"#).unwrap();
        let LspAction::Symbols { query } = symbols else {
            panic!("symbols action expected");
        };
        assert_eq!(
            query.unwrap().string_units(""),
            Some(vec![b'q' as u16, 0xd800])
        );

        let rename: LspAction = serde_json::from_str(
            r#"{"action":"rename","line":1,"character":2,"new_name":"n\ud800"}"#,
        )
        .unwrap();
        let LspAction::Rename { new_name, .. } = rename else {
            panic!("rename action expected");
        };
        assert_eq!(new_name.string_units(""), Some(vec![b'n' as u16, 0xd800]));
    }
}
