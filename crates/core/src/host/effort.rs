//! Pure Claude Code 2.1.287 iF/$9/jb/T effort resolution. The host supplies
//! selected-model capabilities and settings/catalog state; no ambient I/O.
use serde_json::Value;

/// One admitted settings source before ordinary last-wins merging. Keeping
/// these inputs separate is required by native K(e)'s minimum-cap policy.
#[derive(Debug, Clone, PartialEq)]
pub struct EffortSettingsLayer {
    pub source: crate::settings::tracer::Source,
    pub path: Option<std::path::PathBuf>,
    pub effort_level: Option<String>,
    pub max_effort_level: Option<String>,
    pub model_settings: indexmap::IndexMap<String, crate::settings::schema::ModelSettings>,
}

impl EffortSettingsLayer {
    #[must_use]
    pub fn new(
        source: crate::settings::tracer::Source,
        settings: &crate::settings::SettingsJson,
    ) -> Self {
        Self {
            source,
            path: None,
            effort_level: settings.effort_level.clone(),
            max_effort_level: settings.max_effort_level.clone(),
            model_settings: settings.model_settings.clone().unwrap_or_default(),
        }
    }
}

/// K(e): minimum over original sources. A matching per-model cap replaces
/// that source's top-level cap, including a per-model "max" exemption. The
/// host supplies qSe identity resolution; prefix matching is never used here.
#[must_use]
pub fn settings_cap(
    layers: &[EffortSettingsLayer],
    model: &str,
    canonical: impl Fn(&str) -> String,
) -> Option<String> {
    let identity = layers
        .iter()
        .any(|layer| {
            layer
                .model_settings
                .values()
                .any(|entry| entry.max_effort_level.is_some())
        })
        .then(|| canonical(model));
    layers
        .iter()
        .filter_map(|layer| {
            let per_model = identity.as_ref().and_then(|identity| {
                layer
                    .model_settings
                    .iter()
                    .filter_map(|(key, entry)| {
                        let cap = entry.max_effort_level.as_deref()?;
                        (rank(cap).is_some() && canonical(key) == *identity).then_some(cap)
                    })
                    .min_by_key(|cap| rank(cap))
            });
            per_model.or(layer.max_effort_level.as_deref())
        })
        .filter(|cap| *cap != "max" && rank(cap).is_some())
        .min_by_key(|cap| rank(cap))
        .map(str::to_string)
}

pub const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffortState {
    pub turn: Option<Value>,
    pub hook: Option<Value>,
    pub carried: Option<Value>,
    /// K(e): lowest settings cap, with "max" treated as uncapped.
    pub settings_cap: Option<String>,
    /// S2t's provider row cap; combined with settings by minimum rank.
    pub provider_cap: Option<String>,
    pub managed_default: Option<Value>,
    pub flag_default: Option<Value>,
    pub model_override_default: Option<Value>,
    pub catalog_default: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffortCapabilities {
    pub supported: bool,
    pub max: bool,
    pub xhigh: bool,
    pub thinking_disabled_cap: bool,
}

/// Trusted command view of the same native inputs used by physical preparation.
/// No credentials, network traffic or ambient settings discovery is needed.
#[derive(Debug, Clone, PartialEq)]
pub struct EffortCommandSnapshot {
    pub model: String,
    pub settings_key: String,
    pub session: super::effort_table::SessionEffort,
    pub primary: Option<Value>,
    pub capabilities: EffortCapabilities,
    pub state: EffortState,
    pub user_settings_path: Option<std::path::PathBuf>,
    pub save_default: bool,
    pub organization_start_effort: Option<String>,
}
impl EffortCommandSnapshot {
    #[must_use]
    pub fn resolved(&self, environment: Option<&str>) -> Option<Value> {
        resolve_effort(
            self.capabilities,
            &self.state,
            self.primary.as_ref(),
            environment,
        )
    }
    #[must_use]
    pub fn displayed(&self, environment: Option<&str>) -> String {
        self.resolved(environment)
            .as_ref()
            .and_then(Value::as_str)
            .filter(|level| LEVELS.contains(level))
            .unwrap_or("high")
            .to_owned()
    }
    #[must_use]
    pub fn cap(&self) -> Option<&str> {
        cap(&self.state)
    }
}

fn rank(value: &str) -> Option<usize> {
    LEVELS.iter().position(|&level| level == value)
}
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// ECMAScript whitespace used by String.trim and regular-expression whitespace.
pub fn javascript_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

/// Native String.trim whitespace, also used by Rt's settings model keys.
#[must_use]
pub fn trim_js_whitespace(value: &str) -> &str {
    value.trim_matches(javascript_whitespace)
}

/// Native iF string branch: levels are lowercased without trimming; numeric
/// strings use decimal parseInt prefix semantics and must yield a finite integer.
pub fn parse_effort(value: &str) -> Option<Value> {
    if value.is_empty() {
        return None;
    }
    let text = value.to_lowercase();
    let level = if text == "med" {
        "medium"
    } else {
        text.as_str()
    };
    if rank(level).is_some() {
        return Some(Value::String(level.into()));
    }
    let number = text.trim_start_matches(javascript_whitespace);
    let offset = usize::from(number.starts_with(['+', '-']));
    let digits = number.as_bytes()[offset..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let number: f64 = number[..offset + digits].parse().ok()?;
    serde_json::Number::from_f64(number).map(Value::Number)
}

/// Explicit environment input; Some(Null) is the native auto/unset sentinel.
pub fn environment_override(value: Option<&str>) -> Option<Value> {
    let value = value?;
    if matches!(value.to_lowercase().as_str(), "auto" | "unset") {
        return Some(Value::Null);
    }
    parse_effort(value)
}

fn cap(state: &EffortState) -> Option<&str> {
    [state.settings_cap.as_deref(), state.provider_cap.as_deref()]
        .into_iter()
        .flatten()
        .filter(|value| rank(value).is_some())
        .min_by_key(|value| rank(value))
        .filter(|&value| value != "max")
}

pub fn resolve_effort(
    capabilities: EffortCapabilities,
    state: &EffortState,
    primary: Option<&Value>,
    environment: Option<&str>,
) -> Option<Value> {
    if !capabilities.supported {
        return None;
    }
    let settings_capped = state
        .settings_cap
        .as_deref()
        .is_some_and(|value| rank(value).is_some() && value != "max");
    let mut value = if let Some(hook) = state.hook.as_ref() {
        hook.clone()
    } else {
        let environment = environment_override(environment);
        let cleared = environment.as_ref().is_some_and(Value::is_null);
        if cleared && !settings_capped {
            return None;
        }
        let default = [
            state.managed_default.as_ref(),
            state.flag_default.as_ref(),
            state.carried.as_ref(),
            state.model_override_default.as_ref(),
            state.catalog_default.as_ref(),
        ]
        .into_iter()
        .find_map(present)
        .cloned()
        .unwrap_or_else(|| Value::String("high".into()));
        present(environment.as_ref())
            .cloned()
            .or_else(|| cleared.then(|| default.clone()))
            .or_else(|| present(state.turn.as_ref()).cloned())
            .or_else(|| present(primary).cloned())
            .unwrap_or(default)
    };
    if value.is_number() && settings_capped {
        value = Value::String("high".into());
    }
    if let Value::String(level) = &mut value {
        if let (Some(current), Some(limit)) = (rank(level), cap(state)) {
            if current > rank(limit).expect("valid cap") {
                *level = limit.into();
            }
        }
        if (level == "max" && !capabilities.max) || (level == "xhigh" && !capabilities.xhigh) {
            *level = "high".into();
        }
    }
    Some(value)
}

pub fn clamp_side_effort(
    value: Option<Value>,
    capabilities: EffortCapabilities,
    thinking_disabled: bool,
) -> Option<Value> {
    value.map(|value| {
        if thinking_disabled
            && capabilities.thinking_disabled_cap
            && value.as_str().and_then(rank).is_some_and(|rank| rank > 2)
        {
            Value::String("high".into())
        } else {
            value
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn native_equal(actual: &Value, expected: &Value) -> bool {
        match (actual.as_f64(), expected.as_f64()) {
            (Some(actual), Some(expected)) => actual == expected,
            _ => actual == expected,
        }
    }
    #[test]
    fn current_native_effort_resolution_matches_pinned_source() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/effort_resolution_2_1_287.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let input = &case["input"];
            let object = input["state"].as_object().unwrap();
            let value = |key: &str| object.get(key).cloned();
            let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
            let state = EffortState {
                turn: value("turn"),
                hook: value("hook"),
                carried: value("carried"),
                settings_cap: text("settings_cap"),
                provider_cap: text("provider_cap"),
                managed_default: value("managed_default"),
                flag_default: value("flag_default"),
                model_override_default: value("model_override_default"),
                catalog_default: value("catalog_default"),
            };
            let caps = &input["caps"];
            let capabilities = EffortCapabilities {
                supported: caps["supported"].as_bool().unwrap(),
                max: caps["max"].as_bool().unwrap(),
                xhigh: caps["xhigh"].as_bool().unwrap(),
                thinking_disabled_cap: caps["thinking_disabled_cap"].as_bool().unwrap(),
            };
            let environment = input["env"].as_str();
            assert!(
                native_equal(
                    &environment.and_then(parse_effort).unwrap_or(Value::Null),
                    &case["expected"]["parsed"]
                ),
                "{case}"
            );
            let overridden = environment_override(environment);
            assert_eq!(
                overridden.as_ref().is_some_and(Value::is_null),
                case["expected"]["env_cleared"],
                "{case}"
            );
            assert!(
                native_equal(&overridden.unwrap_or(Value::Null), &case["expected"]["env"]),
                "{case}"
            );
            let resolved = resolve_effort(capabilities, &state, input.get("primary"), environment);
            assert_eq!(resolved.is_some(), case["expected"]["has_value"], "{case}");
            assert!(
                native_equal(
                    resolved.as_ref().unwrap_or(&Value::Null),
                    &case["expected"]["value"]
                ),
                "{case}"
            );
            let side = clamp_side_effort(
                resolved,
                capabilities,
                input["thinking_disabled"].as_bool().unwrap(),
            );
            assert_eq!(side.is_some(), case["expected"]["has_side_value"], "{case}");
            assert!(
                native_equal(&side.unwrap_or(Value::Null), &case["expected"]["side"]),
                "{case}"
            );
        }
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn settings_cap_matches_native_287() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/effort_settings_2_1_287.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let input = &case["input"];
            let layers: Vec<EffortSettingsLayer> = input["layers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|layer| {
                    let settings: crate::settings::SettingsJson =
                        serde_json::from_value(layer.clone()).unwrap();
                    EffortSettingsLayer::new(crate::settings::tracer::Source::User, &settings)
                })
                .collect();
            let actual = settings_cap(&layers, input["model"].as_str().unwrap(), |key| {
                let identity = input["identities"]
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or(key);
                identity
                    .strip_suffix("[1m]")
                    .unwrap_or(identity)
                    .to_string()
            });
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                case["expected"],
                "{input}"
            );
        }
    }
}
