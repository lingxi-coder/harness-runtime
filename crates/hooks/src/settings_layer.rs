use crate::definition::{HookDefinition, HookFailureBehavior, HookShell, HookSource};
use crate::events::HookEventType;
use crate::loader::{
    build_condition, build_executor, event_type_name, parse_event_type, HookEntry, MatcherGroup,
};
use crate::matcher::{
    comma_mode_for, is_simple_pattern_units, is_valid_hook_regexp_projection, trim_js_units,
};
use lingxi_core::types::utf16_json::{
    JsonValueView, Utf16JsonProjection, Utf16JsonProjectionError,
};
use permission::PermissionRuleValue;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct HookLayerDocument {
    #[serde(flatten)]
    pub events: BTreeMap<String, Vec<MatcherGroup>>,
}

// A derived internally flattened map is buffered through Serde's generic
// `Content` representation before its field values are decoded. That loses
// the original JSON token slice required by `RawValue`, so `HookEntry::input`
// cannot use the raw JSON source adapter. Capture this root object as a
// `RawValue` first, then decode the event map directly from its JSON text.
// `MatcherGroup` and `HookEntry` are now deserialized by serde_json from the
// original member slices, allowing arbitrary MCP input to enter its UTF-16
// projection without a recursive `serde_json::Value` intermediary.
impl<'de> Deserialize<'de> for HookLayerDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as DeError;
        use serde_json::value::RawValue;

        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let events = serde_json::from_str::<BTreeMap<String, Vec<MatcherGroup>>>(raw.get())
            .map_err(D::Error::custom)?;
        Ok(Self { events })
    }
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
    Projection(Utf16JsonProjectionError),
    Validation(Vec<HookValidationIssue>),
}

impl HookLayerError {
    #[must_use]
    pub fn issues(&self) -> &[HookValidationIssue] {
        match self {
            Self::Json(_) | Self::Projection(_) => &[],
            Self::Validation(issues) => issues,
        }
    }
}

impl fmt::Display for HookLayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "hook layer JSON is invalid: {error}"),
            Self::Projection(error) => write!(f, "hook layer projection failed: {error}"),
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

impl From<Utf16JsonProjectionError> for HookLayerError {
    fn from(error: Utf16JsonProjectionError) -> Self {
        Self::Projection(error)
    }
}

pub fn parse_hook_layer_strict(
    raw: &str,
    source: HookSource,
) -> Result<ParsedHookLayer, HookLayerError> {
    let document: HookLayerDocument = serde_json::from_str(raw)?;
    compile_hook_layer(document, source)
}

pub fn serialize_hook_layer(
    document: &HookLayerDocument,
) -> Result<Utf16JsonProjection, HookLayerError> {
    let json = serde_json::to_string(document)?;
    Ok(Utf16JsonProjection::parse(&json)?)
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
                    id: lingxi_core::types::HookId::new(),
                    name,
                    events: vec![event_type.clone()],
                    if_condition: build_condition(
                        group.matcher.as_ref(),
                        entry.if_pattern.as_ref(),
                    ),
                    executor,
                    source,
                    on_failure: entry
                        .on_failure
                        .as_deref()
                        .and_then(HookFailureBehavior::from_wire),
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
            if let Some(matcher) = group.matcher.as_ref() {
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

    if let Some(if_pattern) = entry.if_pattern.as_ref() {
        validate_if_pattern(if_pattern, &format!("{base}.if"), issues);
    }
    if entry
        .on_failure
        .as_deref()
        .is_some_and(|value| HookFailureBehavior::from_wire(value).is_none())
    {
        issues.push(HookValidationIssue {
            path: format!("{base}.onFailure"),
            message: "onFailure must be \"continue\" or \"block\"".into(),
        });
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
            validate_required_source_string(
                entry.prompt.as_ref(),
                &format!("{base}.prompt"),
                issues,
            );
        }
        "prompt" => {
            validate_required_source_string(
                entry.prompt.as_ref(),
                &format!("{base}.prompt"),
                issues,
            );
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

fn validate_required_source_string(
    value: Option<&Utf16JsonProjection>,
    path: &str,
    issues: &mut Vec<HookValidationIssue>,
) {
    let non_empty = value
        .and_then(|value| value.string_units(""))
        .is_some_and(|units| !trim_js_units(&units).is_empty());
    if !non_empty {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "field is required".into(),
        });
    }
}

fn validate_if_pattern(
    value: &Utf16JsonProjection,
    path: &str,
    issues: &mut Vec<HookValidationIssue>,
) {
    let Some(units) = value.string_units("") else {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "if condition must be a string".into(),
        });
        return;
    };
    if trim_js_units(&units).is_empty() {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "if condition must not be empty".into(),
        });
        return;
    }
    let Some(parsed) = PermissionRuleValue::from_rule_projection(value) else {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "if condition must start with a tool name".into(),
        });
        return;
    };
    let tool_name_units = parsed.tool_name.encode_utf16().collect::<Vec<_>>();
    if trim_js_units(&tool_name_units).is_empty() {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "if condition must start with a tool name".into(),
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
    input: &Utf16JsonProjection,
    path: &str,
    issues: &mut Vec<HookValidationIssue>,
) {
    let Some(root) = input.at_pointer("") else {
        return;
    };
    let Some(entries) = root.object_entries() else {
        issues.push(HookValidationIssue {
            path: path.to_string(),
            message: "input must be an object".into(),
        });
        return;
    };

    let mut pending = Vec::<(JsonValueView<'_>, String)>::new();
    for entry in &entries {
        if entry.display_key.trim().is_empty() {
            issues.push(HookValidationIssue {
                path: path.to_string(),
                message: "input keys must not be empty".into(),
            });
        }
    }
    for entry in entries.into_iter().rev() {
        pending.push((entry.value, format!("{path}.{}", entry.display_key)));
    }

    while let Some((value, value_path)) = pending.pop() {
        if let Some(units) = value.code_units() {
            validate_interpolation_units(units, &value_path, issues);
            continue;
        }
        if let Some(length) = value.array_len() {
            for index in (0..length).rev() {
                if let Some(item) = value.get_index(index) {
                    pending.push((item, format!("{value_path}[{index}]")));
                }
            }
            continue;
        }
        if let Some(entries) = value.object_entries() {
            for entry in entries.into_iter().rev() {
                pending.push((entry.value, format!("{value_path}.{}", entry.display_key)));
            }
        }
    }
}

fn validate_interpolation_units(units: &[u16], path: &str, issues: &mut Vec<HookValidationIssue>) {
    let mut index = 0;
    while index < units.len() {
        if units[index] == u16::from(b'$') && units.get(index + 1) == Some(&u16::from(b'{')) {
            let start = index + 2;
            let Some(close_relative) = units[start..]
                .iter()
                .position(|unit| *unit == u16::from(b'}'))
            else {
                issues.push(HookValidationIssue {
                    path: path.to_string(),
                    message: "unterminated ${path} placeholder".into(),
                });
                return;
            };
            let close = start + close_relative;
            if utf16_text_is_whitespace(&units[start..close]) {
                issues.push(HookValidationIssue {
                    path: path.to_string(),
                    message: "${path} placeholders must not be empty".into(),
                });
            }
            index = close + 1;
        } else {
            index += 1;
        }
    }
}

fn utf16_text_is_whitespace(units: &[u16]) -> bool {
    char::decode_utf16(units.iter().copied())
        .all(|decoded| decoded.map(char::is_whitespace).unwrap_or(false))
}

fn validate_matcher(
    matcher: &Utf16JsonProjection,
    event_type: &HookEventType,
) -> Result<(), String> {
    let Some(units) = matcher.string_units("") else {
        return Err("matcher must be a string".into());
    };
    if units.is_empty() || units == [b'*' as u16] {
        return Ok(());
    }
    if is_simple_pattern_units(&units, comma_mode_for(event_type)) {
        return Ok(());
    }
    if is_valid_hook_regexp_projection(matcher) {
        Ok(())
    } else {
        Err("matcher is not a valid JavaScript regular expression".into())
    }
}

#[must_use]
pub fn file_changed_matchers_from_hooks(hooks: &[HookDefinition]) -> Vec<Utf16JsonProjection> {
    hooks
        .iter()
        .filter(|hook| hook.events.contains(&HookEventType::FileChanged))
        .filter_map(|hook| hook.matcher().cloned())
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

    fn parse_fixture(raw: &str, source: HookSource) -> Result<ParsedHookLayer, HookLayerError> {
        parse_hook_layer_strict(raw, source)
    }

    #[test]
    fn strict_parse_round_trips_supported_fields() {
        let raw = r#"{
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
        }"#;

        let parsed = parse_fixture(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert!(serialize_hook_layer(&parsed.document)
            .unwrap()
            .semantically_eq(&Utf16JsonProjection::parse(raw).unwrap()));
        assert_eq!(parsed.hooks.len(), 2);
        assert!(parsed.hooks.iter().any(|hook| hook.priority == 25));
        assert!(parsed
            .hooks
            .iter()
            .any(|hook| matches!(hook.executor, HookExecutor::McpTool { .. })));
    }

    #[test]
    fn strict_layer_serialization_preserves_unpaired_if_and_mcp_input_units() {
        let raw = r#"{
            "PreToolUse": [{
                "matcher": "^\ud800$",
                "hooks": [{
                    "type": "command",
                    "command": "./guard.sh",
                    "if": "Edit(*\ud800.rs)"
                }]
            }],
            "PostToolUse": [{
                "hooks": [{
                    "type": "mcp_tool",
                    "server": "audit",
                    "tool": "scan",
                    "input": {"path": "\ud801"}
                }]
            }]
        }"#;
        let parsed = parse_fixture(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(
            parsed
                .hooks
                .iter()
                .find(|hook| hook.events.contains(&HookEventType::PreToolUse))
                .and_then(HookDefinition::matcher)
                .and_then(|matcher| matcher.string_units("")),
            Some(vec![b'^' as u16, 0xd800, b'$' as u16])
        );
        let serialized = serialize_hook_layer(&parsed.document).unwrap();
        assert!(serialized.semantically_eq(&Utf16JsonProjection::parse(raw).unwrap()));
    }

    #[test]
    fn strict_parse_rejects_invalid_event_matcher_and_url() {
        let raw = r#"{
            "Nope": [{ "hooks": [{ "type": "command", "command": "x" }] }],
            "SessionStart": [{
                "matcher": "[",
                "hooks": [{ "type": "http", "url": "not a url" }]
            }]
        }"#;

        let error = parse_fixture(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
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
        let raw = r#"{
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
        }"#;

        let error = parse_fixture(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::Local),
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
        let parsed = parse_fixture(
            r#"{
                "FileChanged": [{
                    "matcher": ".env|.envrc",
                    "hooks": [{ "type": "command", "command": "./watch.sh" }]
                }],
                "Stop": [{
                    "matcher": "ignored",
                    "hooks": [{ "type": "command", "command": "./stop.sh" }]
                }]
            }"#,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();

        assert_eq!(
            file_changed_matchers_from_hooks(&parsed.hooks)
                .iter()
                .map(|matcher| matcher.string_at("").unwrap())
                .collect::<Vec<_>>(),
            vec![".env|.envrc"]
        );
    }

    #[test]
    fn raw_parse_retains_lone_surrogate_in_mcp_input() {
        let parsed = parse_hook_layer_strict(
            r#"{"PostToolUse":[{"hooks":[{"type":"mcp_tool","server":"audit","tool":"lint","input":{"path":"\ud800"}}]}]}"#,
            HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .expect("hook layer parses through RawValue");
        let input = parsed.document.events["PostToolUse"][0].hooks[0]
            .input
            .as_ref()
            .expect("input is retained");
        assert_eq!(
            input.string_units("/path").as_deref(),
            Some([0xD800].as_slice())
        );
    }

    #[test]
    fn on_failure_is_validated_and_preserved_in_parsed_definition() {
        let raw = r#"{
            "UserPromptSubmit": [{"hooks": [
                {"type":"command","command":"fail.sh","onFailure":"block"}
            ]}]
        }"#;
        let parsed = parse_hook_layer_strict(
            raw,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .expect("supported onFailure values parse");
        assert_eq!(parsed.hooks.len(), 1);
        assert_eq!(parsed.hooks[0].on_failure, Some(HookFailureBehavior::Block));
        assert!(serialize_hook_layer(&parsed.document)
            .unwrap()
            .semantically_eq(&Utf16JsonProjection::parse(raw).unwrap()));

        let invalid = r#"{"UserPromptSubmit":[{"hooks":[
            {"type":"command","command":"fail.sh","onFailure":"ignore"}
        ]}]}"#;
        let error = parse_hook_layer_strict(
            invalid,
            HookSource::Settings(lingxi_core::types::SettingsScope::User),
        )
        .expect_err("unsupported onFailure values are rejected");
        assert!(matches!(
            error,
            HookLayerError::Validation(ref issues)
                if issues.iter().any(|issue| issue.path.ends_with(".onFailure"))
        ));
    }
}
