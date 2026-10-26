//! The outbound diagnostic schema. Content opt-ins never bypass this boundary.

use std::sync::atomic::{AtomicBool, Ordering};

use super::record::{AttrValue, Attributes};

#[derive(Debug)]
pub(crate) struct ExportPolicy {
    enabled: AtomicBool,
}

impl ExportPolicy {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
        }
    }

    pub(crate) fn allowed(&self) -> bool {
        if privacy_disabled() {
            // Opt-out is sticky: a fresh runtime is required to opt back in.
            self.revoke();
        }
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn revoke(&self) {
        self.enabled.store(false, Ordering::Release);
    }
}

pub(crate) fn privacy_disabled() -> bool {
    lingxi_core::host::traffic_mode::is_telemetry_disabled()
}

/// Keep dimensions useful for aggregate diagnostics; never copy arbitrary
/// metadata, request bodies, paths, account IDs or session IDs into telemetry.
pub(crate) fn filter_attributes(attrs: &Attributes, metrics: bool) -> Attributes {
    attrs
        .iter()
        .filter_map(|(key, value)| {
            let dimension = matches!(
                key.as_str(),
                "entrypoint"
                    | "version"
                    | "platform"
                    | "release_channel"
                    | "provider"
                    | "model"
                    | "tool_name"
                    | "tool_source"
                    | "tool_type"
                    | "status"
                    | "result"
                    | "outcome"
                    | "success"
                    | "cancelled"
                    | "error_kind"
                    | "error.type"
                    | "status_code"
                    | "token_type"
                    | "type"
                    | "language"
                    | "decision"
                    | "source"
                    | "speed"
                    | "query_source"
                    | "querySource"
                    | "request.kind"
                    | "operation"
                    | "method"
                    | "hook_event"
                    | "hook_type"
                    | "metrics_exporter"
                    | "logs_exporter"
                    | "traces_exporter"
                    | "stream"
                    | "streaming"
            );
            let measurement = matches!(
                key.as_str(),
                "duration_ms"
                    | "durationMs"
                    | "elapsed_ms"
                    | "uptime_ms"
                    | "attempt"
                    | "attempt_number"
                    | "retry_count"
                    | "turn_index"
                    | "input_tokens"
                    | "output_tokens"
                    | "cache_read_tokens"
                    | "cache_creation_tokens"
                    | "inputTokens"
                    | "outputTokens"
                    | "cachedInputTokens"
                    | "uncachedInputTokens"
                    | "cost_usd"
                    | "cost_usd_micros"
                    | "costUSD"
                    | "first_token_ms"
                    | "prompt_length"
                    | "response_length"
                    | "tool_count"
            );
            if !dimension && (metrics || !measurement) {
                return None;
            }
            let value = match value {
                AttrValue::Float(value) if !value.is_finite() => return None,
                AttrValue::Str(value) if measurement => {
                    // Some legacy events encode a numeric duration as a string.
                    let number = value.parse::<f64>().ok()?;
                    if !number.is_finite() {
                        return None;
                    }
                    AttrValue::Float(number)
                }
                AttrValue::Str(value) => AttrValue::Str(safe_dimension(key, value)?),
                value => value.clone(),
            };
            Some((key.clone(), value))
        })
        .collect()
}

fn safe_dimension(key: &str, value: &str) -> Option<String> {
    if !safe_label(value) {
        return None;
    }
    // MCP tool names may contain user-controlled server identifiers.
    if key == "tool_name" && value.starts_with("mcp__") {
        return Some("mcp".into());
    }
    Some(value.to_string())
}

pub(crate) fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        && ![
            "sk-",
            "sk_",
            "ghp_",
            "github_pat_",
            "AIza",
            "eyJ",
            "xoxb-",
            "xoxp-",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

pub(crate) fn resource_key_allowed(key: &str) -> bool {
    matches!(
        key,
        "service.name"
            | "service.version"
            | "service.namespace"
            | "deployment.environment.name"
            | "deployment.environment"
            | "os.type"
            | "host.arch"
            | "lingxi.platform"
            | "lingxi.release_channel"
            | "lingxi.harness_revision"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_removes_content_credentials_paths_ids_and_unbounded_values() {
        let attrs: Attributes = [
            ("model", AttrValue::from("gpt-5")),
            ("status", AttrValue::from("success")),
            ("duration_ms", AttrValue::from(25_i64)),
            ("prompt", AttrValue::from("private prompt")),
            ("tool_parameters", AttrValue::from("private args")),
            ("response", AttrValue::from("private answer")),
            ("path", AttrValue::from("/Users/private")),
            ("session_id", AttrValue::from("private-session")),
            ("request_id", AttrValue::from("private-request")),
            ("account_uuid", AttrValue::from("private-account")),
            ("provider", AttrValue::from("sk-secret")),
            ("error_kind", AttrValue::from("failure: private path /a")),
            ("cost_usd", AttrValue::from(f64::NAN)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let logs = filter_attributes(&attrs, false);
        assert_eq!(logs.len(), 3);
        assert!(logs.contains_key("duration_ms"));
        let metrics = filter_attributes(&attrs, true);
        assert_eq!(metrics.len(), 2);
    }

    #[test]
    fn resource_allowlist_excludes_host_and_user_identity() {
        assert!(resource_key_allowed("service.version"));
        assert!(!resource_key_allowed("service.instance.id"));
        assert!(!resource_key_allowed("host.name"));
        assert!(!resource_key_allowed("user.id"));
        assert!(!safe_label("user@example.org"));
    }
}
