//! Current local `/effort` command. Native inputs come from the session's
//! admitted SDK route; provider-neutral reasoning keeps its separate default.
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::effort::{
    environment_override, trim_js_whitespace, EffortCommandSnapshot, LEVELS,
};
use lingxi_core::host::effort_table::SessionEffort;
use lingxi_core::host::{OrchestratorHandle, ReasoningSelection};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

fn parsed_level(value: &str) -> Option<&'static str> {
    let value = trim_js_whitespace(value).to_lowercase();
    let value = if value == "med" {
        "medium"
    } else {
        value.as_str()
    };
    LEVELS.into_iter().find(|level| *level == value)
}
fn description(value: &Value) -> &'static str {
    match value.as_str() {
        Some("low")=>"Quick, straightforward implementation with minimal overhead",
        Some("medium")=>"Balanced approach with standard implementation and testing",
        Some("high")=>"Comprehensive implementation with extensive testing and documentation",
        Some("xhigh")=>"Deeper reasoning than high, just below maximum (on supported models)",
        Some("max")=>"Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks.",
        Some(_)=>"undefined",
        None=>"Balanced approach with standard implementation and testing",
    }
}
fn display_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(number) => ryu_js::Buffer::new()
            .format_finite(number.as_f64().expect("finite JSON number"))
            .to_owned(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    display_value(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => value.to_string(),
    }
}
fn levels(snapshot: &EffortCommandSnapshot) -> Vec<&'static str> {
    let limit = snapshot
        .cap()
        .and_then(|cap| LEVELS.iter().position(|level| *level == cap));
    LEVELS
        .into_iter()
        .enumerate()
        .filter(|(rank, _)| limit.is_none_or(|limit| *rank <= limit))
        .map(|(_, level)| level)
        .collect()
}
fn valid_options(snapshot: &EffortCommandSnapshot, ultra_available: bool) -> String {
    format!(
        "{}, auto{}",
        levels(snapshot).join(", "),
        if ultra_available {
            ", ultracode [on|off]"
        } else {
            ""
        }
    )
}
fn usage(snapshot: &EffortCommandSnapshot, ultra_available: bool) -> String {
    let lines = levels(snapshot)
        .into_iter()
        .map(|level| {
            format!(
                "- {level}: {}\n",
                match level {
                    "low" => "Quick, straightforward implementation",
                    "medium" => "Balanced approach with standard testing",
                    "high" => "Comprehensive implementation with extensive testing",
                    "xhigh" => "Extended reasoning with thorough analysis (on supported models)",
                    _ => "Maximum capability with deepest reasoning (on supported models)",
                }
            )
        })
        .collect::<String>();
    format!("Usage: /effort [{}|auto{}]\n\nEffort levels:\n{lines}- auto: Use the default effort level for your model{}",
        levels(snapshot).join("|"),if ultra_available{"|ultracode [on|off]"}else{""},
        if ultra_available{"\n\nUltracode (any effort level, this session only):\n- ultracode [on|off]: dynamic workflows on every task"}else{""})
}
fn observed_value(snapshot: &EffortCommandSnapshot, environment: Option<&str>) -> Option<Value> {
    match environment_override(environment) {
        Some(Value::Null) => None,
        Some(value) => Some(value),
        None => snapshot.primary.clone(),
    }
}
fn current(snapshot: &EffortCommandSnapshot, environment: Option<&str>, ultra_on: bool) -> String {
    let suffix = if ultra_on { " · Ultracode on" } else { "" };
    if let Some(value) = observed_value(snapshot, environment) {
        format!(
            "Current effort level: {} ({}){suffix}",
            display_value(&value),
            description(&value)
        )
    } else {
        let org = if snapshot
            .state
            .managed_default
            .as_ref()
            .is_some_and(|value| value.as_str().is_some_and(|level| LEVELS.contains(&level)))
            && snapshot.resolved(environment).is_some()
        {
            ", set by your organization"
        } else {
            ""
        };
        format!(
            "Effort level: auto (currently {}{org}){suffix}",
            snapshot.displayed(environment)
        )
    }
}
fn clamped<'a>(level: &'a str, snapshot: &'a EffortCommandSnapshot) -> &'a str {
    match snapshot.cap() {
        Some(cap)
            if LEVELS.iter().position(|v| *v == level) > LEVELS.iter().position(|v| *v == cap) =>
        {
            cap
        }
        _ => level,
    }
}
fn persistable(level: &str) -> bool {
    matches!(level, "low" | "medium" | "high" | "xhigh")
}

/// Local effort controls bound to the authoritative session state.
#[derive(Clone)]
pub struct EffortHandler {
    handle: Arc<dyn OrchestratorHandle>,
}
impl EffortHandler {
    /// Bind the current host command interface, with no ambient fallback.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }

    async fn set_native(
        &self,
        snapshot: &EffortCommandSnapshot,
        requested: Option<&str>,
        environment: Option<&str>,
    ) -> String {
        let applied = requested.map(|level| clamped(level, snapshot));
        let changed = requested != applied;
        let save = snapshot.save_default && !changed && applied.is_none_or(persistable);
        let session = applied
            .map(|level| SessionEffort::Level(json!(level)))
            .unwrap_or(SessionEffort::Default);
        if let Err(error) = self.handle.set_session_effort(session).await {
            return format!("Failed to set effort level: {error}");
        }
        if save {
            if let Some(path) = &snapshot.user_settings_path {
                if let Err(error) = persist_model_effort_at(path, &snapshot.settings_key, applied) {
                    let _ = self
                        .handle
                        .set_session_effort(snapshot.session.clone())
                        .await;
                    return format!("Failed to set effort level: {error}");
                }
            }
        }
        let override_value = environment_override(environment);
        if let Some(level) = applied {
            if override_value
                .as_ref()
                .is_some_and(|value| value != &json!(level))
            {
                let raw = environment.unwrap_or_default();
                if !persistable(level) {
                    return format!("Not applied: {}={raw} overrides effort this session, and {level} is session-only (nothing saved)",branding::EFFORT_LEVEL_ENV);
                }
                return format!(
                    "{}={raw} overrides this session — clear it and {level} takes over",
                    branding::EFFORT_LEVEL_ENV
                );
            }
            let suffix = if save {
                snapshot.organization_start_effort.as_ref().map(|start|format!(" (saved, though your organization starts new sessions on {} at {start} effort)",snapshot.model)).unwrap_or_else(||" (saved as your default for new sessions)".into())
            } else {
                " (this session only)".into()
            };
            let desc = description(&json!(level));
            if changed {
                return format!("Effort '{}' exceeds the cap for {} set by your settings or organization; set to '{level}' instead{suffix}: {desc}",requested.unwrap_or_default(),snapshot.model);
            }
            format!("Set effort level to {level}{suffix}: {desc}")
        } else {
            if override_value.is_some_and(|value| !value.is_null()) {
                return format!(
                    "{} {}={} still controls this session",
                    if snapshot.save_default {
                        "Cleared effort from settings, but"
                    } else {
                        "Effort set to auto for this session, but"
                    },
                    branding::EFFORT_LEVEL_ENV,
                    environment.unwrap_or_default()
                );
            }
            format!(
                "Effort level set to auto{}",
                if snapshot.save_default {
                    ""
                } else {
                    " (this session only)"
                }
            )
        }
    }

    async fn generic(&self, args: &str) -> String {
        let Some(controls) = self.handle.conversation_controls().await else {
            return "Failed to set effort level: reasoning controls are unavailable".into();
        };
        let level = parsed_level(args);
        if args == "current" || args == "status" || args.is_empty() {
            return self
                .handle
                .current_effort()
                .await
                .map(|level| {
                    format!(
                        "Current effort level: {level} ({})",
                        description(&json!(level))
                    )
                })
                .unwrap_or_else(|| "Effort level: auto".into());
        }
        let selection = if matches!(args.to_lowercase().as_str(), "auto" | "unset") {
            ReasoningSelection::Automatic
        } else if let Some(level) = level {
            ReasoningSelection::Level { id: level.into() }
        } else {
            return format!(
                "Invalid argument: {args}. Valid options are: low, medium, high, xhigh, max, auto"
            );
        };
        if !matches!(selection, ReasoningSelection::Automatic)
            && !controls.reasoning_spec.available.contains(&selection)
        {
            return format!(
                "Failed to set effort level: {args} is unsupported for the active model"
            );
        }
        if let Err(error) = self.handle.set_reasoning_selection(selection.clone()).await {
            return format!("Failed to set effort level: {error}");
        }
        if level.is_none_or(persistable) {
            if let Some(path) = self.handle.reasoning_default_settings_path().await {
                if let Err(error) = persist_reasoning_default_selection_at(&path, Some(&selection))
                {
                    let _ = self
                        .handle
                        .set_reasoning_selection(controls.requested_reasoning_selection)
                        .await;
                    return format!("Failed to set effort level: {error}");
                }
            }
        }
        level
            .map(|level| {
                format!(
                    "Set effort level to {level}: {}",
                    description(&json!(level))
                )
            })
            .unwrap_or_else(|| "Effort level set to auto".into())
    }
}
#[async_trait]
impl BuiltinCommandHandler for EffortHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let args = trim_js_whitespace(&args.raw_args);
        let snapshot = match self.handle.effort_command_snapshot().await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                return CommandResult::Done {
                    display: Some(self.generic(args).await),
                }
            }
            Err(error) => {
                return CommandResult::Done {
                    display: Some(format!("Failed to read effort state: {error}")),
                }
            }
        };
        let ultra_available =
            self.handle.dynamic_workflows_enabled().await && snapshot.capabilities.xhigh;
        let environment = std::env::var(branding::EFFORT_LEVEL_ENV).ok();
        let ultra_on = self.handle.ultracode_enabled().await && ultra_available;
        let display = if matches!(args, "help" | "-h" | "--help") {
            usage(&snapshot, ultra_available)
        } else if args == "current" || args == "status" {
            current(&snapshot, environment.as_deref(), ultra_on)
        } else if args.is_empty() {
            format!(
                "Usage: /effort <{}|auto{}>",
                levels(&snapshot).join("|"),
                if ultra_available {
                    "|ultracode [on|off]"
                } else {
                    ""
                }
            )
        } else {
            let lower = args.to_lowercase();
            let tokens = lower
                .split(lingxi_core::host::effort::javascript_whitespace)
                .filter(|token| !token.is_empty())
                .collect::<Vec<_>>();
            let ultra = match tokens.as_slice() {
                ["ultracode"] | ["ultracode", "on"] => Some(true),
                ["ultracode", "off"] => Some(false),
                _ => None,
            };
            if let Some(enabled) = ultra {
                if enabled && !self.handle.dynamic_workflows_enabled().await {
                    format!("Ultracode needs dynamic workflows enabled (see /config). Valid options are: {}",valid_options(&snapshot,ultra_available))
                } else if enabled && !snapshot.capabilities.xhigh {
                    format!(
                        "Ultracode isn't available on {}. Valid options are: {}",
                        snapshot.model,
                        valid_options(&snapshot, ultra_available)
                    )
                } else if let Err(error) = self.handle.set_ultracode_enabled(enabled).await {
                    format!("Failed to set ultracode: {error}")
                } else {
                    let effort = observed_value(&snapshot, environment.as_deref())
                        .filter(|value| !value.is_null())
                        .map(|value| display_value(&value))
                        .unwrap_or_else(|| snapshot.displayed(environment.as_deref()));
                    if enabled {
                        format!("Ultracode on (this session only): dynamic workflows on every task. Effort stays {effort}.")
                    } else {
                        format!("Ultracode off. Effort stays {effort}.")
                    }
                }
            } else if lower == "auto" || lower == "unset" {
                self.set_native(&snapshot, None, environment.as_deref())
                    .await
            } else if let Some(level) = parsed_level(args) {
                self.set_native(&snapshot, Some(level), environment.as_deref())
                    .await
            } else {
                format!(
                    "Invalid argument: {args}. Valid options are: {}",
                    valid_options(&snapshot, ultra_available)
                )
            }
        };
        CommandResult::Done {
            display: Some(display),
        }
    }
    fn name(&self) -> &str {
        "effort"
    }
    fn description(&self) -> &str {
        "Set the effort level for the model"
    }
}

/// Persist only the provider-neutral structured default. Current native root
/// and per-model effort fields belong to their own command path.
pub fn persist_reasoning_default_selection_at(
    path: &Path,
    selection: Option<&lingxi_core::host::ReasoningSelection>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to read raw settings from {}: {e}", path.display()))?;
    }
    let mut map: serde_json::Map<String, Value> = match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => serde_json::Map::new(),
        Ok(content) => serde_json::from_str(&content)
            .map_err(|_| format!("Invalid JSON syntax in settings file at {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            return Err(format!(
                "Failed to read raw settings from {}: {e}",
                path.display()
            ));
        }
    };

    let default_selection = selection.and_then(|selection| match selection {
        lingxi_core::host::ReasoningSelection::Level { id }
            if matches!(id.as_str(), "low" | "medium" | "high" | "xhigh") =>
        {
            Some(json!({ "type": "level", "id": id }))
        }
        lingxi_core::host::ReasoningSelection::Disabled => Some(json!({ "type": "disabled" })),
        lingxi_core::host::ReasoningSelection::Enabled => Some(json!({ "type": "enabled" })),
        lingxi_core::host::ReasoningSelection::TokenBudget { tokens } => {
            Some(json!({ "type": "token_budget", "tokens": tokens }))
        }
        lingxi_core::host::ReasoningSelection::Automatic
        | lingxi_core::host::ReasoningSelection::Level { .. } => None,
    });

    match default_selection {
        Some(value) => {
            let reasoning = map
                .entry("reasoning".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !reasoning.is_object() {
                *reasoning = Value::Object(serde_json::Map::new());
            }
            reasoning
                .as_object_mut()
                .expect("reasoning object normalized")
                .insert("defaultSelection".to_string(), value);
        }
        None => {
            let mut remove_reasoning = false;
            if let Some(Value::Object(reasoning)) = map.get_mut("reasoning") {
                reasoning.remove("defaultSelection");
                remove_reasoning = reasoning.is_empty();
            }
            if remove_reasoning {
                map.remove("reasoning");
            }
        }
    }

    let serialized = serde_json::to_string_pretty(&map)
        .map_err(|e| format!("Failed to read raw settings from {}: {e}", path.display()))?;
    std::fs::write(path, serialized + "\n")
        .map_err(|e| format!("Failed to read raw settings from {}: {e}", path.display()))?;
    Ok(())
}

/// Native be/_e/w3: the host supplies the canonical current model key and an
/// admitted user path. None removes the field; max is not persistable. No
/// ambient paths or provider-neutral mirror are introduced here.
pub fn persist_model_effort_at(path: &Path, key: &str, level: Option<&str>) -> Result<(), String> {
    if level.is_some_and(|level| !matches!(level, "low" | "medium" | "high" | "xhigh")) {
        return Ok(());
    }
    let mut map: serde_json::Map<String, Value> = match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => serde_json::Map::new(),
        Ok(content) => serde_json::from_str(&content)
            .map_err(|_| format!("Invalid JSON syntax in settings file at {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => {
            return Err(format!(
                "Failed to read raw settings from {}: {error}",
                path.display()
            ))
        }
    };
    let target = if lingxi_core::host::effort_table::object_prototype_key(key) {
        &mut map
    } else {
        let settings = map
            .entry("modelSettings".to_owned())
            .or_insert_with(|| json!({}));
        if !settings.is_object() {
            *settings = json!({});
        }
        let model = settings
            .as_object_mut()
            .expect("model settings normalized")
            .entry(key.to_owned())
            .or_insert_with(|| json!({}));
        if !model.is_object() {
            *model = json!({});
        }
        model.as_object_mut().expect("model entry normalized")
    };
    if let Some(level) = level {
        target.insert("effortLevel".into(), json!(level));
    } else {
        target.remove("effortLevel");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to read raw settings from {}: {error}",
                path.display()
            )
        })?;
    }
    let bytes = serde_json::to_string_pretty(&map).map_err(|error| error.to_string())? + "\n";
    std::fs::write(path, bytes).map_err(|error| {
        format!(
            "Failed to read raw settings from {}: {error}",
            path.display()
        )
    })
}

/// Read the provider-neutral structured default. Native effortLevel and
/// modelSettings inheritance belongs to Q/ee; it is not a generic fallback.
pub fn load_reasoning_default_selection_at(
    path: &Path,
) -> Option<lingxi_core::host::ReasoningSelection> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    if let Some(default) = value
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("defaultSelection"))
    {
        if let Ok(selection) =
            serde_json::from_value::<lingxi_core::host::ReasoningSelection>(default.clone())
        {
            return Some(selection);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::effort::{EffortCapabilities, EffortState};
    use orchestrator::test_support::MockOrchestratorHandle;
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn args(raw: &str) -> ParsedSlashCommand {
        crate::parser::parse_slash_command(&format!("/effort {raw}")).unwrap()
    }
    fn session_json(session: &SessionEffort) -> Value {
        match session {
            SessionEffort::Inherit => json!({"kind":"inherit"}),
            SessionEffort::Default => json!({"kind":"default"}),
            SessionEffort::Level(value) => json!({"kind":"level","value":value}),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn local_command_messages_mutations_and_patch_targets_match_native_287() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os(branding::EFFORT_LEVEL_ENV);
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/effort_command_2_1_287.json"
        ))
        .unwrap();
        let root =
            std::env::temp_dir().join(format!("harness-effort-command-{}", std::process::id()));
        for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
            let input = &case["input"];
            let expected = &case["expected"];
            match input["env"].as_str() {
                Some(value) => std::env::set_var(branding::EFFORT_LEVEL_ENV, value),
                None => std::env::remove_var(branding::EFFORT_LEVEL_ENV),
            };
            let path = root.join(index.to_string()).join("settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let key = input["settings_key"].as_str().unwrap();
            let initial = json!({"effortLevel":"low","modelSettings":{key:{"effortLevel":"high","maxEffortLevel":"max"},"sibling":{"effortLevel":"medium"}},"reasoning":{"defaultSelection":{"type":"level","id":"low"}},"other":7});
            let before = if input["failure"] == true {
                "broken fixture".into()
            } else {
                serde_json::to_string(&initial).unwrap()
            };
            std::fs::write(&path, &before).unwrap();
            let mock = Arc::new(MockOrchestratorHandle::new());
            mock.set_status_snapshot(lingxi_core::host::StatusSnapshot {
                model: input["model"].as_str().unwrap().into(),
                ..Default::default()
            });
            mock.set_dynamic_workflows_gate(input["workflow"].as_bool().unwrap(), false);
            mock.set_ultracode_enabled(input["ultracode"].as_bool().unwrap())
                .await
                .unwrap();
            let state = EffortState {
                settings_cap: input["state"]["settings_cap"].as_str().map(str::to_owned),
                catalog_default: input["state"].get("catalog_default").cloned(),
                managed_default: input["state"].get("managed_default").cloned(),
                ..Default::default()
            };
            let caps = &input["caps"];
            let base = EffortCommandSnapshot {
                model: input["model"].as_str().unwrap().into(),
                settings_key: key.into(),
                session: Default::default(),
                primary: input.get("primary").cloned(),
                capabilities: EffortCapabilities {
                    supported: caps["supported"].as_bool().unwrap(),
                    max: caps["max"].as_bool().unwrap(),
                    xhigh: caps["xhigh"].as_bool().unwrap(),
                    thinking_disabled_cap: false,
                },
                state,
                user_settings_path: Some(path.clone()),
                save_default: input["save"].as_bool().unwrap(),
                organization_start_effort: None,
            };
            mock.set_effort_command_source(move |_, session| {
                let mut snapshot = base.clone();
                snapshot.primary = match &session {
                    SessionEffort::Inherit => snapshot.primary,
                    SessionEffort::Default => None,
                    SessionEffort::Level(value) => Some(value.clone()),
                };
                snapshot.session = session;
                Ok(Some(snapshot))
            });
            if let Some(value) = input.get("primary") {
                mock.set_session_effort(SessionEffort::Level(value.clone()))
                    .await
                    .unwrap();
            }
            let handler = EffortHandler::new(mock.clone());
            let CommandResult::Done {
                display: Some(message),
            } = handler.handle(&args(input["args"].as_str().unwrap())).await
            else {
                panic!("expected a message")
            };
            let wanted = expected["message"]
                .as_str()
                .unwrap()
                .replace("CLAUDE_CODE_EFFORT_LEVEL", branding::EFFORT_LEVEL_ENV);
            if !wanted.starts_with("Failed to set effort level:") {
                assert_eq!(message, wanted, "case {index}: {input}");
            } else {
                assert!(
                    message.starts_with("Failed to set effort level:"),
                    "{message}"
                );
            }
            let snapshot = mock.effort_command_snapshot().await.unwrap().unwrap();
            assert_eq!(
                session_json(&snapshot.session),
                expected["state"]["sessionEffort"],
                "case {index} session"
            );
            assert_eq!(
                mock.ultracode_enabled().await,
                expected["state"]["ultracode"].as_bool().unwrap(),
                "case {index} ultracode"
            );
            let bytes = std::fs::read_to_string(&path).unwrap();
            let writes = expected["writes"].as_array().unwrap();
            if input["failure"] == true || writes.is_empty() {
                assert_eq!(
                    bytes, before,
                    "case {index} leaves persisted defaults unchanged"
                );
            } else {
                let saved: Value = serde_json::from_str(&bytes).unwrap();
                let patch = &writes[0];
                let root_field = patch.get("effortLevel");
                let value = root_field
                    .or_else(|| {
                        patch
                            .get("modelSettings")
                            .and_then(|settings| settings.get(key))
                            .and_then(|entry| entry.get("effortLevel"))
                    })
                    .unwrap();
                let result = if root_field.is_some() {
                    saved.get("effortLevel")
                } else {
                    saved
                        .get("modelSettings")
                        .and_then(|settings| settings.get(key))
                        .and_then(|entry| entry.get("effortLevel"))
                };
                if value["$undefined"] == true {
                    assert!(result.is_none(), "case {index} removes current field");
                } else {
                    assert_eq!(result, Some(value), "case {index} persists current field");
                }
                assert_eq!(saved["reasoning"], initial["reasoning"]);
                assert_eq!(
                    saved["modelSettings"]["sibling"],
                    initial["modelSettings"]["sibling"]
                );
                assert_eq!(saved["other"], 7);
                if root_field.is_none() {
                    assert_eq!(saved["effortLevel"], initial["effortLevel"]);
                }
            }
        }
        match previous {
            Some(value) => std::env::set_var(branding::EFFORT_LEVEL_ENV, value),
            None => std::env::remove_var(branding::EFFORT_LEVEL_ENV),
        };
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generic_default_has_no_native_mirror_and_preserves_current_native_settings() {
        let root =
            std::env::temp_dir().join(format!("harness-reasoning-current-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        std::fs::write(
            &path,
            r#"{"effortLevel":"low","modelSettings":{"model":{"effortLevel":"medium"}},"other":7}"#,
        )
        .unwrap();
        assert!(load_reasoning_default_selection_at(&path).is_none());
        for selection in [
            ReasoningSelection::Level { id: "high".into() },
            ReasoningSelection::TokenBudget { tokens: 12345 },
            ReasoningSelection::Automatic,
        ] {
            persist_reasoning_default_selection_at(&path, Some(&selection)).unwrap();
            let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved["effortLevel"], "low");
            assert_eq!(saved["modelSettings"]["model"]["effortLevel"], "medium");
            assert_eq!(saved["other"], 7);
            if selection == ReasoningSelection::Automatic {
                assert!(load_reasoning_default_selection_at(&path).is_none());
            } else {
                assert_eq!(load_reasoning_default_selection_at(&path), Some(selection));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
