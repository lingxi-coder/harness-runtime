use crate::response::ExactHookText;
use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Name-addressed MCP seam used by `mcp_tool` hooks.
///
/// The hooks crate deliberately depends on an ALREADY-CONNECTED registry owned
/// by the composition root rather than discovering or dialing MCP servers
/// itself. Callers should return `NotConnected` when the named server is not
/// currently live; the hook executor treats that as a non-blocking hook error.
#[async_trait]
pub trait HookMcpInvoker: Send + Sync {
    async fn invoke(&self, request: HookMcpInvocation) -> HookMcpInvocationResult;
}

/// One `mcp_tool` call request built by the hook executor.
fn deserialize_input_object<'de, D>(deserializer: D) -> Result<Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let input = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if input.object_entries().is_some() {
        Ok(input)
    } else {
        Err(serde::de::Error::custom("expected an object"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookMcpInvocation {
    pub server: String,
    pub tool: String,
    #[serde(
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize",
        deserialize_with = "deserialize_input_object"
    )]
    pub input: Utf16JsonProjection,
    pub timeout: Duration,
}

/// Outcome returned by the composition root's MCP registry adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HookMcpInvocationResult {
    Success {
        #[serde(with = "exact_text_content_serde")]
        text_content: Vec<ExactHookText>,
    },
    Error {
        #[serde(with = "exact_text_content_serde")]
        text_content: Vec<ExactHookText>,
        message: String,
    },
    NotConnected {
        message: String,
    },
    Timeout {
        #[serde(with = "exact_text_content_serde")]
        text_content: Vec<ExactHookText>,
    },
}

mod exact_text_content_serde {
    use crate::response::ExactHookText;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    pub fn serialize<S>(content: &Vec<ExactHookText>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let values = content
            .iter()
            .map(|text| {
                Utf16JsonProjection::root_string(
                    text.display.clone(),
                    text.utf16_code_units.clone(),
                )
                .map_err(serde::ser::Error::custom)
            })
            .collect::<Result<Vec<_>, S::Error>>()?;
        let array = Utf16JsonProjection::array(values).map_err(serde::ser::Error::custom)?;
        lingxi_core::types::utf16_json::raw_json_serde::serialize(&array, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<ExactHookText>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let source = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
        let root = source
            .at_pointer("")
            .ok_or_else(|| serde::de::Error::custom("expected an array of strings"))?;
        let length = root
            .array_len()
            .ok_or_else(|| serde::de::Error::custom("expected an array of strings"))?;
        (0..length)
            .map(|index| {
                let text = root
                    .get_index(index)
                    .and_then(|value| value.code_units())
                    .ok_or_else(|| serde::de::Error::custom("expected an array of strings"))?;
                Ok(ExactHookText::from_utf16(text.to_vec()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn invocation_input_serde_preserves_the_raw_object_member() {
        let request = HookMcpInvocation {
            server: "audit".into(),
            tool: "check".into(),
            input: Utf16JsonProjection::parse(r#"{"path":"\ud800"}"#).unwrap(),
            timeout: Duration::from_secs(5),
        };
        let source = serde_json::to_string(&request).unwrap();
        assert!(source.contains(r#""input":{"path":"\ud800"}"#), "{source}");
        let decoded: HookMcpInvocation = serde_json::from_str(&source).unwrap();
        assert_eq!(decoded.input.string_units("/path").unwrap(), vec![0xd800]);
    }

    #[test]
    fn mcp_text_content_serde_preserves_exact_string_units() {
        let result = HookMcpInvocationResult::Success {
            text_content: vec![ExactHookText::from_utf16(vec![0xd800, u16::from(b'x')])],
        };
        let source = serde_json::to_string(&result).unwrap();
        assert!(source.contains(r#""text_content":["\ud800x"]"#), "{source}");
        let decoded: HookMcpInvocationResult = serde_json::from_str(&source).unwrap();
        let HookMcpInvocationResult::Success { text_content } = decoded else {
            panic!("expected success");
        };
        assert_eq!(
            text_content[0].utf16_code_units,
            vec![0xd800, u16::from(b'x')]
        );
    }
}
