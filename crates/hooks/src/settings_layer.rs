use crate::definition::{HookDefinition, HookShell, HookSource};
use crate::events::HookEventType;
use crate::loader::{
    build_condition, build_executor, event_type_name, parse_event_type, HookEntry, MatcherGroup,
};
use permission::PermissionRuleValue;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HookLayerDocument {
    #[serde(flatten)]
    pub events: BTreeMap<String, Vec<MatcherGroup>>,
}

#[derive(Debug, Clone)]
pub struct ParsedHookLayer {
    pub document: HookLayerDocument,
    pub hooks: Vec<HookDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookValidationIssue {
    pub path: String,
    pub message: String,
}

#[derive(Debug)]
pub enum HookLayerError {
    Json(serde_json::Error),
    Validation(Vec<HookValidationIssue>),
}

impl HookLayerError {
    #[must_use]
    pub fn issues(&self) -> &[HookValidationIssue] {
        match self {
            Self::Json(_) => &[],
            Self::Validation(issues) => issues,
        }
    }
}

impl fmt::Display for HookLayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "hook layer JSON is invalid: {error}"),
            Self::Validation(issues) => {
                write!(f, "hook layer validation failed")?;
                if let Some(first) = issues.first() {
                    write!(f, " at {}: {}", first.path, first.message)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for HookLayerError {}

impl From<serde_json::Error> for HookLayerError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn parse_hook_layer_strict(
    raw: &str,
    source: HookSource,
) -> Result<ParsedHookLayer, HookLayerError> {
    let document: HookLayerDocument = serde_json::from_str(raw)?;
    compile_hook_layer(document, source)
}

pub fn parse_hook_layer_value_strict(
    value: &Value,
    source: HookSource,
) -> Result<ParsedHookLayer, HookLayerError> {
    let document: HookLayerDocument = serde_json::from_value(value.clone())?;
    compile_hook_layer(document, source)
}

pub fn serialize_hook_layer(document: &HookLayerDocument) -> Result<Value, serde_json::Error> {
    serde_json::to_value(document)
}

fn compile_hook_layer(
    document: HookLayerDocument,
    source: HookSource,
) -> Result<ParsedHookLayer, HookLayerError> {
    let issues = validate_document(&document);
    if !issues.is_empty() {
        return Err(HookLayerError::Validation(issues));
    }

    let mut hooks = Vec::new();
    for (event_name, groups) in &document.events {
        let event_type = parse_event_type(event_name).expect("validated event name");
        for group in groups {
            for entry in &group.hooks {
                let (name, executor) = build_executor(entry).expect("validated hook entry");
                hooks.push(HookDefinition {
                    id: protocol::HookId::new(),
                    name,
                    events: vec![event_type.clone()],
                    if_condition: build_condition(
                        group.matcher.as_deref(),
                        entry.if_pattern.as_deref(),
                    ),
                    executor,
                    source,
                    blocking: !entry.r#async.unwrap_or(false),
                    timeout: entry.timeout.map(Duration::from_secs),
                    priority: entry.priority.unwrap_or(0),
                    once: entry.once.unwrap_or(false),
                    status_message: entry.status_message.clone(),
                    async_rewake: entry.async_rewake.unwrap_or(false),
                    async_timeout: entry
                        .async_timeout
                        .filter(|timeout| *timeout > 0)
                        .map(Duration::from_millis),
                    rewake_message: entry
                        .rewake_message
                        .clone()
                        .filter(|message| !message.trim().is_empty()),
                });
            }
        }
    }

    Ok(ParsedHookLayer { document, hooks })
}

fn validate_document(document: &HookLayerDocument) -> Vec<HookValidationIssue> {
    let mut issues = Vec::new();
    for (event_name, groups) in &document.events {
        let path = format!("hooks.{event_name}");
        let Some(event_type) = parse_event_type(event_name) else {
            issues.push(HookValidationIssue {
                path,
                message: "unsupported hook event".into(),
            });
            continue;
        };

        for (group_index, group) in groups.iter().enumerate() {
            let group_path = format!("hooks.{event_name}[{group_index}]");
            if let Some(matcher) = group.matcher.as_deref() {
                if let Err(message) = validate_matcher(matcher, &event_type) {
                    issues.push(HookValidationIssue {
                        path: format!("{group_path}.matcher"),
                        message,
                    });
                }
            }
            if group.hooks.is_empty() {
                issues.push(HookValidationIssue {
                    path: format!("{group_path}.hooks"),
                    message: "matcher group must contain at least one hook".into(),
                });
            }
            for (hook_index, entry) in group.hooks.iter().enumerate() {
                validate_entry(entry, event_name, group_index, hook_index, &mut issues);
            }
        }
    }
    issues
}

fn validate_entry(
    entry: &HookEntry,
    event_name: &str,
    group_index: usize,
    hook_index: usize,
    issues: &mut Vec<HookValidationIssue>,
) {
    let base = format!("hooks.{event_name}[{group_index}].hooks[{hook_index}]");
    let Some(kind) = entry.kind.as_deref() else {
        issues.push(HookValidationIssue {
            path: format!("{base}.type"),
            message: "hook type is required".into(),
        });
        return;
    };

    if let Some(if_pattern) = entry.if_pattern.as_deref() {
        validate_if_pattern(if_pattern, &format!("{base}.if"), issues);
    }

    match kind {
        "command" => {
            validate_required_string(entry.command.as_deref(), &format!("{base}.command"), issues);
            if let Some(shell) = entry.shell.as_deref() {
                if HookShell::from_wire(shell).is_none() {
                    issues.push(HookValidationIssue {
                        path: format!("{base}.shell"),
                        message: "shell must be \"bash\" or \"powershell\"".into(),
                    });
                }
            }
        }
        "http" => {
            let url_path = format!("{base}.url");
            validate_required_string(entry.url.as_deref(), &url_path, issues);
            if let Some(url) = entry.url.as_deref().filter(|url| !url.trim().is_empty()) {
                if Url::parse(url).is_err() {
                    issues.push(HookValidationIssue {
                        path: url_path,
                        message: "URL must be absolute and valid".into(),
                    });
                }
            }
            if let Some(headers) = &entry.headers {
                validate_headers(headers, &format!("{base}.headers"), issues);
            }
            if let Some(allowed) = &entry.allowed_env_vars {
                validate_env_var_names(allowed, &format!("{base}.allowedEnvVars"), issues);
            }
        }
        "agent" => {
            validate_required_string(entry.prompt.as_deref(), &format!("{base}.prompt"), issues);
        }
        "prompt" => {
            validate_required_string(entry.prompt.as_deref(), &format!("{base}.prompt"), issues);
        }
        "mcp_tool" => {
            validate_required_string(entry.server.as_deref(), &format!("{base}.server"), issues);
            validate_required_string(entry.tool.as_deref(), &format!("{base}.tool"), issues);
            if let Some(input) = &entry.input {
                validate_mcp_input(input, &format!("{base}.input"), issues);
            }
        }
        _ => issues.push(HookValidationIssue {
            path: format!("{base}.type"),
            message: "supported hook types are command, http, agent, prompt, mcp_tool".into(),
        }),
    }
}

fn validate_required_string(
    value: Option<&str>,
    path: &str,
    issues: &mut Vec<HookValidationIssue>,
) {
    if !value.is_some_and(|value| !value.trim().is_empty()) {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "field is required".into(),
        });
    }
}

fn validate_if_pattern(value: &str, path: &str, issues: &mut Vec<HookValidationIssue>) {
    if value.trim().is_empty() {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "`if` condition must not be empty".into(),
        });
        return;
    }
    let parsed = PermissionRuleValue::from_rule_string(value);
    if parsed.tool_name.trim().is_empty() {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "`if` condition must start with a tool name".into(),
        });
    }
}

fn validate_headers(
    headers: &HashMap<String, String>,
    path: &str,
    issues: &mut Vec<HookValidationIssue>,
) {
    for (name, value) in headers {
        if name.trim().is_empty() {
            issues.push(HookValidationIssue {
                path: path.to_string(),
                message: "header names must not be empty".into(),
            });
        }
        if value.contains('\n') || value.contains('\r') {
            issues.push(HookValidationIssue {
                path: format!("{path}.{name}"),
                message: "header values must not contain newlines".into(),
            });
        }
    }
}

fn validate_env_var_names(names: &[String], path: &str, issues: &mut Vec<HookValidationIssue>) {
    for (index, name) in names.iter().enumerate() {
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
            && name
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase() || ch == '_');
        if !valid {
            issues.push(HookValidationIssue {
                path: format!("{path}[{index}]"),
                message: "environment variable names must match [A-Z_][A-Z0-9_]*".into(),
            });
        }
    }
}

fn validate_mcp_input(
    input: &HashMap<String, Value>,
    path: &str,
    issues: &mut Vec<HookValidationIssue>,
) {
    for (key, value) in input {
        if key.trim().is_empty() {
            issues.push(HookValidationIssue {
                path: path.to_string(),
                message: "input keys must not be empty".into(),
            });
        }
        validate_interpolation_value(value, &format!("{path}.{key}"), issues);
    }
}

fn validate_interpolation_value(value: &Value, path: &str, issues: &mut Vec<HookValidationIssue>) {
    match value {
        Value::String(text) => {
            let bytes = text.as_bytes();
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index] == b'$' && bytes.get(index + 1) == Some(&b'{') {
                    let start = index + 2;
                    let Some(close_rel) = text[start..].find('}') else {
                        issues.push(HookValidationIssue {
                            path: path.to_string(),
                            message: "unterminated ${path} placeholder".into(),
                        });
                        return;
                    };
                    let close = start + close_rel;
                    if text[start..close].trim().is_empty() {
                        issues.push(HookValidationIssue {
                            path: path.to_string(),
                            message: "${path} placeholders must not be empty".into(),
                        });
                    }
                    index = close + 1;
                    continue;
                }
                index += 1;
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                validate_interpolation_value(item, &format!("{path}[{index}]"), issues);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                validate_interpolation_value(item, &format!("{path}.{key}"), issues);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn validate_matcher(matcher: &str, event_type: &HookEventType) -> Result<(), String> {
    if matcher.is_empty() || matcher == "*" {
        return Ok(());
    }
    if is_simple_pattern(matcher, comma_mode_for(event_type)) {
        return Ok(());
    }
    Regex::new(matcher)
        .map(|_| ())
        .map_err(|error| format!("matcher is not a valid regex: {error}"))
}

fn comma_mode_for(event_type: &HookEventType) -> bool {
    matches!(
        event_type,
        HookEventType::PreToolUse
            | HookEventType::PostToolUse
            | HookEventType::PostToolUseFailure
            | HookEventType::PermissionRequest
            | HookEventType::PermissionDenied
            | HookEventType::UserPromptExpansion
            | HookEventType::SessionStart
            | HookEventType::SessionEnd
            | HookEventType::Setup
            | HookEventType::PreCompact
            | HookEventType::PostCompact
            | HookEventType::Notification
            | HookEventType::SubagentStart
            | HookEventType::SubagentStop
            | HookEventType::Elicitation
            | HookEventType::ElicitationResult
            | HookEventType::ConfigChange
            | HookEventType::InstructionsLoaded
    )
}

fn is_simple_pattern(value: &str, comma_mode: bool) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || byte == b'_'
                || byte == b'|'
                || (comma_mode && (byte == b',' || byte == b' ' || byte == b'-'))
        })
}

#[must_use]
pub fn file_changed_matchers_from_hooks(hooks: &[HookDefinition]) -> Vec<String> {
    hooks
        .iter()
        .filter(|hook| hook.events.contains(&HookEventType::FileChanged))
        .filter_map(|hook| hook.matcher().map(ToString::to_string))
        .collect()
}

#[must_use]
pub fn event_name_for_hook(event_type: &HookEventType) -> &'static str {
    event_type_name(event_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::HookExecutor;

    #[test]
    fn strict_parse_round_trips_supported_fields() {
        let raw = serde_json::json!({
            "PreToolUse": [{
                "matcher": "Bash, Write",
                "hooks": [{
                    "type": "command",
                    "command": "./fmt.sh",
                    "args": ["--check"],
                    "shell": "bash",
                    "timeout": 30,
                    "priority": 25,
                    "async": true,
                    "asyncRewake": true,
                    "asyncTimeout": 5000,
                    "rewakeMessage": "done",
                    "statusMessage": "Formatting",
                    "once": true,
                    "if": "Bash(git status:*)"
                }]
            }],
            "PostToolUse": [{
                "hooks": [{
                    "type": "mcp_tool",
                    "server": "audit",
                    "tool": "lint",
                    "input": {
                        "path": "${tool_input.file_path}",
                        "meta": {"kind": "${hook_event_name}"}
                    }
                }]
            }]
        });

        let parsed = parse_hook_layer_value_strict(
            &raw,
            HookSource::Settings(protocol::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(serialize_hook_layer(&parsed.document).unwrap(), raw);
        assert_eq!(parsed.hooks.len(), 2);
        assert!(parsed.hooks.iter().any(|hook| hook.priority == 25));
        assert!(parsed
            .hooks
            .iter()
            .any(|hook| matches!(hook.executor, HookExecutor::McpTool { .. })));
    }

    #[test]
    fn strict_parse_rejects_invalid_event_matcher_and_url() {
        let raw = serde_json::json!({
            "Nope": [{ "hooks": [{ "type": "command", "command": "x" }] }],
            "SessionStart": [{
                "matcher": "[",
                "hooks": [{ "type": "http", "url": "not a url" }]
            }]
        });

        let error = parse_hook_layer_value_strict(
            &raw,
            HookSource::Settings(protocol::SettingsScope::User),
        )
        .unwrap_err();
        let issues = error.issues();
        assert!(issues.iter().any(|issue| issue.path == "hooks.Nope"));
        assert!(issues
            .iter()
            .any(|issue| issue.path == "hooks.SessionStart[0].matcher"));
        assert!(issues
            .iter()
            .any(|issue| issue.path == "hooks.SessionStart[0].hooks[0].url"));
    }

    #[test]
    fn strict_parse_rejects_invalid_mcp_placeholders() {
        let raw = serde_json::json!({
            "PostToolUse": [{
                "hooks": [{
                    "type": "mcp_tool",
                    "server": "audit",
                    "tool": "lint",
                    "input": {
                        "path": "${}",
                        "other": "${unterminated"
                    }
                }]
            }]
        });

        let error = parse_hook_layer_value_strict(
            &raw,
            HookSource::Settings(protocol::SettingsScope::Local),
        )
        .unwrap_err();
        let issues = error.issues();
        assert!(issues
            .iter()
            .any(|issue| issue.path == "hooks.PostToolUse[0].hooks[0].input.path"));
        assert!(issues
            .iter()
            .any(|issue| issue.path == "hooks.PostToolUse[0].hooks[0].input.other"));
    }

    #[test]
    fn file_changed_matchers_filter_to_file_changed_hooks() {
        let parsed = parse_hook_layer_value_strict(
            &serde_json::json!({
                "FileChanged": [{
                    "matcher": ".env|.envrc",
                    "hooks": [{ "type": "command", "command": "./watch.sh" }]
                }],
                "Stop": [{
                    "matcher": "ignored",
                    "hooks": [{ "type": "command", "command": "./stop.sh" }]
                }]
            }),
            HookSource::Settings(protocol::SettingsScope::User),
        )
        .unwrap();

        assert_eq!(
            file_changed_matchers_from_hooks(&parsed.hooks),
            vec![".env|.envrc".to_string()]
        );
    }
}
