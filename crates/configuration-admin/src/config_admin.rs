use client::adapter::ClientEventSink;
use client::protocol::events::{ClientEvent, ErrorKindDto};
use client::protocol::listings::{
    ConfigurationDomainDto, ConfigurationEffectDto, ConfigurationOperationStatusDto,
};
use serde_json::{Map, Value};
use std::path::Path;

pub fn sha256_hex(input: &[u8]) -> String {
    plugin::plugin_source_sha256(input)
}

pub fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

pub fn read_json_object(path: &Path) -> Result<Map<String, Value>, String> {
    let text = read_text(path)?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?
        .as_object()
        .cloned()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))
}

pub fn ensure_revision(expected: Option<&str>, actual: &str, label: &str) -> Result<(), String> {
    if let Some(expected) = expected {
        if expected != actual {
            return Err(format!(
                "{label} changed on disk (expected revision {expected}, found {actual})"
            ));
        }
    }
    Ok(())
}

pub fn parse_payload_object(payload_json: &str) -> Result<Map<String, Value>, String> {
    serde_json::from_str::<Value>(payload_json)
        .map_err(|error| format!("payload is not valid JSON: {error}"))?
        .as_object()
        .cloned()
        .ok_or_else(|| "payload must be a JSON object".to_string())
}

pub fn require_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("payload.{key} must be a non-empty string"))
}

pub fn optional_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

pub fn stable_json_revision(value: &Value) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| format!("failed to serialize revision payload: {error}"))
}

pub fn file_text_revision(path: &Path) -> Result<String, String> {
    let text = read_text(path)?;
    Ok(sha256_hex(text.as_bytes()))
}

pub async fn emit_operation(
    sink: &dyn ClientEventSink,
    domain: ConfigurationDomainDto,
    operation_id: u64,
    status: ConfigurationOperationStatusDto,
    effect: ConfigurationEffectDto,
    message: Option<String>,
    details_json: Option<String>,
) {
    sink.emit(ClientEvent::ConfigurationOperation {
        domain,
        operation_id,
        status,
        effect,
        message,
        details_json,
    })
    .await;
}

pub async fn emit_error(sink: &dyn ClientEventSink, message: impl Into<String>) {
    sink.emit(ClientEvent::Error {
        kind: ErrorKindDto::Internal,
        message: message.into(),
    })
    .await;
}
