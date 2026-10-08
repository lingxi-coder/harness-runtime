//! Pure native 2.1.287 Q/ee/he/kd settings effort and session selection.
use super::effort::EffortSettingsLayer;
use crate::settings::tracer::Source;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq)]
pub enum SessionEffort {
    #[default]
    Inherit,
    Default,
    Level(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableOptions {
    /// QYn model whose user/project defaults are suppressed by managed state.
    pub override_model: Option<String>,
    /// Native ye first-start model exemptions, canonicalized by the caller.
    pub excluded_user_models: Vec<String>,
    pub fable_override: Option<String>,
}
impl Default for TableOptions {
    fn default() -> Self {
        Self {
            override_model: None,
            fable_override: None,
            excluded_user_models: vec![
                "claude-opus-4-7".into(),
                "claude-opus-4-8".into(),
                "claude-fable-5".into(),
            ],
        }
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(_) => true,
    }
}
impl TableOptions {
    #[must_use]
    pub fn from_global(global: &Value, fable_override: Option<String>) -> Self {
        let mut result = Self {
            override_model: None,
            excluded_user_models: Vec::new(),
            fable_override: None,
        };
        if global.get("firstStartVersion").is_some() {
            return result;
        }
        for (key, model) in [
            ("unpinOpus47LaunchEffort", "claude-opus-4-7"),
            ("unpinOpus48LaunchEffort", "claude-opus-4-8"),
            ("unpinFable5LaunchEffort", "claude-fable-5"),
        ] {
            if !truthy(global.get(key)) {
                result.excluded_user_models.push(model.into());
            }
        }
        if !truthy(global.get("unpinFable5LaunchEffort")) {
            result.fable_override = fable_override.filter(|value| !value.is_empty());
        }
        result
    }

    #[must_use]
    pub fn excluded_models(&self, canonical: impl Fn(&str) -> String) -> Vec<String> {
        let mut models: Vec<String> = self
            .excluded_user_models
            .iter()
            .map(|model| canonical(model))
            .collect();
        if let Some(model) = &self.fable_override {
            let model = canonical(model);
            if !model.starts_with("claude-fable-") {
                models.push(model);
            }
        }
        models
    }
}

#[must_use]
pub fn object_prototype_key(key: &str) -> bool {
    matches!(
        key,
        "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__"
            | "__proto__"
            | "constructor"
            | "hasOwnProperty"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "toLocaleString"
            | "toString"
            | "valueOf"
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsEffortTable {
    pub default: Option<String>,
    /// Current native user root field; not a Harness compatibility fallback.
    pub user_effort: Option<String>,
    /// A present None suppresses inheritance for that model.
    pub by_model: BTreeMap<String, Option<String>>,
}

fn persistable(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| matches!(*value, "low" | "medium" | "high" | "xhigh"))
        .map(str::to_string)
}
fn index(key: &str) -> Option<u32> {
    let number: u32 = key.parse().ok()?;
    (number != u32::MAX && number.to_string() == key).then_some(number)
}

fn normalized_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut output = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
                if output.file_name().is_some()
                    && output.file_name() != Some(std::ffi::OsStr::new("..")) =>
            {
                output.pop();
            }
            std::path::Component::ParentDir if output.has_root() => {}
            component => output.push(component.as_os_str()),
        }
    }
    output
}

/// Native U6r source/path suppression; policy tiers become the one effective
/// policy slot before Q, while K continues to consume every original tier.
fn sources(layers: &[EffortSettingsLayer]) -> Vec<EffortSettingsLayer> {
    let mut output: Vec<EffortSettingsLayer> = Vec::new();
    let mut seen = BTreeSet::new();
    for layer in layers {
        if matches!(layer.source, Source::Defaults | Source::Env) {
            continue;
        }
        if let Some(path) = &layer.path {
            if !seen.insert(normalized_path(path)) && layer.source != Source::Cli {
                continue;
            }
        }
        if layer.source == Source::Managed {
            if let Some(previous) = output
                .iter_mut()
                .find(|previous| previous.source == Source::Managed)
            {
                if layer.effort_level.is_some() {
                    previous.effort_level.clone_from(&layer.effort_level);
                }
                for (key, value) in &layer.model_settings {
                    previous
                        .model_settings
                        .entry(key.clone())
                        .and_modify(|old| {
                            if value.effort_level.is_some() {
                                old.effort_level.clone_from(&value.effort_level);
                            }
                        })
                        .or_insert_with(|| value.clone());
                }
                continue;
            }
        }
        output.push(layer.clone());
    }
    output
}

#[must_use]
pub fn settings_table(
    layers: &[EffortSettingsLayer],
    options: &TableOptions,
    canonical: impl Fn(&str) -> String,
) -> SettingsEffortTable {
    let mut layers = sources(layers);
    let mut table = SettingsEffortTable::default();
    let mut top_set = false;
    for layer in &layers {
        if layer.source == Source::User {
            table.user_effort = persistable(layer.effort_level.as_deref());
        } else if layer.effort_level.is_some() {
            top_set = true;
            table.default = persistable(layer.effort_level.as_deref());
        }
    }
    if top_set {
        table.user_effort = None;
    }
    layers.reverse();
    let maps: Vec<BTreeMap<String, String>> = layers
        .iter()
        .map(|layer| {
            let mut entries: Vec<_> = layer.model_settings.iter().collect();
            entries.sort_by_key(|(key, _)| (index(key).is_none(), index(key).unwrap_or_default()));
            let mut entries_by_identity = BTreeMap::new();
            for (key, value) in entries {
                let Some(effort) = &value.effort_level else {
                    continue;
                };
                let identity = canonical(key);
                if *key == identity || !entries_by_identity.contains_key(&identity) {
                    entries_by_identity.insert(identity, effort.clone());
                }
            }
            entries_by_identity
        })
        .collect();
    let override_model = options
        .override_model
        .as_ref()
        .map(|model| canonical(model));
    let mut models = BTreeSet::new();
    if let Some(model) = &override_model {
        models.insert(model.clone());
    }
    for entries in &maps {
        models.extend(entries.keys().cloned());
    }
    for model in models {
        if model == "__proto__" {
            continue;
        }
        if override_model.as_ref() == Some(&model) {
            table.by_model.insert(model.clone(), None);
        }
        for (layer, entries) in layers.iter().zip(&maps) {
            if override_model.as_ref() == Some(&model)
                && !matches!(layer.source, Source::Cli | Source::Managed)
            {
                continue;
            }
            if let Some(value) = entries.get(&model) {
                table
                    .by_model
                    .insert(model.clone(), persistable(Some(value)));
                break;
            }
            if layer.source != Source::User && layer.effort_level.is_some() {
                table
                    .by_model
                    .insert(model.clone(), persistable(layer.effort_level.as_deref()));
                break;
            }
        }
    }
    if table.user_effort.is_some() {
        for model in options.excluded_models(canonical) {
            if !object_prototype_key(&model) {
                table.by_model.entry(model).or_insert(None);
            }
        }
    }
    table
}

const USER_MODELS: &[&str] = &[
    "claude-3-5-haiku",
    "claude-3-5-sonnet",
    "claude-3-7-sonnet",
    "claude-haiku-4-5",
    "claude-sonnet-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
];
impl SettingsEffortTable {
    #[must_use]
    pub fn inherited(&self, identity: &str) -> Option<String> {
        if let Some(value) = self.by_model.get(identity) {
            return value.clone();
        }
        if self.default.is_some() || self.user_effort.is_none() {
            return self.default.clone();
        }
        if USER_MODELS.contains(&identity) || !super::claude_model_identity::recognized(identity) {
            self.user_effort.clone()
        } else {
            None
        }
    }
}
impl SessionEffort {
    #[must_use]
    pub fn resolve(&self, table: Option<&SettingsEffortTable>, identity: &str) -> Option<Value> {
        match self {
            Self::Level(value) => Some(value.clone()),
            Self::Default => None,
            Self::Inherit => table
                .and_then(|table| table.inherited(identity))
                .map(Value::String),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prototype_filter_precedes_entry_validation_and_policy_is_one_slot() {
        let lower: crate::settings::SettingsJson = serde_json::from_str(r#"{"effortLevel":"low","modelSettings":{"__proto__":false,"constructor":17,"claude-sonnet-4-6":{"effortLevel":"medium"}}}"#).unwrap();
        let upper: crate::settings::SettingsJson =
            serde_json::from_str(r#"{"modelSettings":{"sonnet":{"effortLevel":"low"}}}"#).unwrap();
        assert_eq!(lower.model_settings.as_ref().unwrap().len(), 1);
        let layers = [
            EffortSettingsLayer::new(Source::Managed, &lower),
            EffortSettingsLayer::new(Source::Managed, &upper),
        ];
        let table = settings_table(&layers, &TableOptions::default(), |key| {
            if key == "sonnet" {
                "claude-sonnet-4-6".into()
            } else {
                key.into()
            }
        });
        assert_eq!(
            table.inherited("claude-sonnet-4-6").as_deref(),
            Some("medium"),
            "canonical entry wins within the one merged policy slot"
        );
    }

    #[test]
    fn current_native_settings_effort_table_and_session_states() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/effort_table_2_1_287.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let input = &case["input"];
            let canonical = |model: &str| {
                input["identities"]
                    .get(model)
                    .and_then(Value::as_str)
                    .unwrap_or(model)
                    .strip_suffix("[1m]")
                    .unwrap_or(
                        input["identities"]
                            .get(model)
                            .and_then(Value::as_str)
                            .unwrap_or(model),
                    )
                    .to_string()
            };
            let layers: Vec<_> = input["layers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|layer| {
                    let settings: crate::settings::SettingsJson =
                        serde_json::from_value(layer["settings"].clone()).unwrap();
                    let source = match layer["source"].as_str().unwrap() {
                        "userSettings" => Source::User,
                        "projectSettings" => Source::Project,
                        "localSettings" => Source::Local,
                        "flagSettings" => Source::Cli,
                        "policySettings" => Source::Managed,
                        other => panic!("{other}"),
                    };
                    let mut value = EffortSettingsLayer::new(source, &settings);
                    value.path = layer
                        .get("path")
                        .and_then(Value::as_str)
                        .map(std::path::PathBuf::from);
                    value
                })
                .collect();
            let mut options = TableOptions::from_global(
                &input["global"],
                input["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"]
                    .as_str()
                    .map(str::to_string),
            );
            options.override_model = input["override"].as_str().map(str::to_string);
            let table = settings_table(&layers, &options, canonical);
            let expected = &case["expected"];
            assert_eq!(
                serde_json::to_value(&table.default).unwrap(),
                expected["default"],
                "{input}"
            );
            assert_eq!(
                serde_json::to_value(&table.user_effort).unwrap(),
                expected["user_effort"],
                "{input}"
            );
            assert_eq!(
                serde_json::to_value(table.by_model.iter().collect::<Vec<_>>()).unwrap(),
                expected["by_model"],
                "{input}"
            );
            assert_eq!(
                serde_json::to_value(options.excluded_models(canonical)).unwrap(),
                expected["excluded"],
                "{input}"
            );
            for (model_index, model) in input["models"].as_array().unwrap().iter().enumerate() {
                let identity = canonical(model.as_str().unwrap());
                let uses_user = USER_MODELS.contains(&identity.as_str())
                    || !super::super::claude_model_identity::recognized(&identity);
                assert_eq!(
                    Value::Bool(uses_user),
                    expected["model_user"][model_index],
                    "{model}"
                );
                for (session_index, session) in
                    input["sessions"].as_array().unwrap().iter().enumerate()
                {
                    let state = match session["kind"].as_str() {
                        Some("level") => SessionEffort::Level(session["value"].clone()),
                        Some("default") => SessionEffort::Default,
                        _ => SessionEffort::Inherit,
                    };
                    let value = state.resolve(Some(&table), &identity);
                    assert_eq!(
                        Value::Bool(value.is_some()),
                        expected["resolved"][session_index][model_index]["present"],
                        "{input}"
                    );
                    assert_eq!(
                        value.unwrap_or(Value::Null),
                        expected["resolved"][session_index][model_index]["value"],
                        "{input}"
                    );
                }
            }
        }
        for key in fixture["prototypes"].as_array().unwrap() {
            assert!(object_prototype_key(key.as_str().unwrap()));
        }
        assert_eq!(SessionEffort::Inherit.resolve(None, "model"), None);
        assert_eq!(SessionEffort::Default.resolve(None, "model"), None);
    }
}
