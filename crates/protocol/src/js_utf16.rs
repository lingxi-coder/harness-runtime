//! Exact JavaScript strings on tool-result boundaries.
//!
//! The sidecar is persisted as ordinary JSON numbers; provider encoders consume
//! it before sending the request. It is never a provider-visible content block.

use serde_json::{json, Value};

const TOOL_RESULT_STRING: &str = "lingxi_tool_result_string_utf16";

/// Store an exact string in the existing tool-result content-block sidecar.
#[must_use]
pub fn tool_result_sidecar(units: Vec<u16>) -> Vec<Value> {
    vec![json!({"type": TOOL_RESULT_STRING, "utf16_code_units": units})]
}

/// Read an exact tool-result string. Ordinary MCP arrays are not interpreted.
#[must_use]
pub fn tool_result_units(output: &Value) -> Option<Vec<u16>> {
    let blocks = output.as_array()?;
    if blocks.len() != 1 || blocks[0].get("type")?.as_str()? != TOOL_RESULT_STRING {
        return None;
    }
    blocks[0]
        .get("utf16_code_units")?
        .as_array()?
        .iter()
        .map(|value| u16::try_from(value.as_u64()?).ok())
        .collect()
}

/// Valid UTF-8 view for providers which do not accept JavaScript strings.
#[must_use]
pub fn tool_result_display(output: &Value) -> Option<String> {
    tool_result_units(output).map(|units| String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_units_survive_json_storage_without_marker_collisions() {
        let value = Value::Array(tool_result_sidecar(vec![65, 0xd83d, 10]));
        let restored: Value =
            serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(tool_result_units(&restored), Some(vec![65, 0xd83d, 10]));
        assert_eq!(
            tool_result_display(&restored).as_deref(),
            Some("A\u{fffd}\n")
        );
        assert_eq!(
            tool_result_units(&json!([{"type":"text","text":TOOL_RESULT_STRING}])),
            None
        );
        assert_eq!(
            tool_result_units(&json!([{"type":TOOL_RESULT_STRING,"utf16_code_units":[65536]}])),
            None
        );
    }
}
