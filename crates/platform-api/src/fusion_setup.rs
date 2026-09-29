//! The Fusion model configuration as the UIs see it: read it out of a
//! `settings.json` value, report which roles are still missing, and merge a new
//! selection back in without touching anything else in the file.
//!
//! This lives in `platform-api` because three crates that cannot see each other
//! need the SAME key spellings: the TUI wizard (`tui::fusion`), the CLI
//! composition root that performs the write (`apps/cli`), and the desktop
//! bridge that serves the Electron settings panel. The typed reader on the
//! engine side is `lingxi_core::settings::schema::FusionSettingsJson`; `core`
//! and `platform-api` do not depend on each other, so the two spellings are
//! pinned together by a test in the one crate that depends on both
//! (`fusion::config`'s `the_settings_reader_and_the_setup_writer_agree`).
//!
//! Nothing here validates against a live catalog. Whether a named route exists,
//! is reachable with the current credentials, has usable capacity, or can emit
//! constrained JSON are facts about the running machine; a settings file that
//! names a model this machine cannot reach must still load, because the same
//! file follows the operator to other machines. Those checks belong to Fusion
//! preflight (`fusion::model_resolver`), which can name the exact route.

use crate::fusion::{FusionModelChoice, FusionModelRole};
use serde_json::{Map, Value};

/// Top-level settings key holding every Fusion setting.
pub const FUSION_SETTINGS_KEY: &str = "fusion";
/// `fusion.enabled` — the agent-listing master switch.
pub const ENABLED_KEY: &str = "enabled";
/// `fusion.panelModels`.
pub const PANEL_MODELS_KEY: &str = "panelModels";
/// `fusion.analystModel`.
pub const ANALYST_MODEL_KEY: &str = "analystModel";

/// The model roles as configured (or not) in a settings file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FusionModelRoles {
    /// Panel roster in the operator's priority order. Empty = unconfigured.
    pub panels: Vec<FusionModelChoice>,
    /// Judge model. `None` = unconfigured.
    pub analyst: Option<FusionModelChoice>,
}

impl FusionModelRoles {
    /// Read the roles out of a parsed `settings.json` value.
    ///
    /// A malformed entry reads as ABSENT rather than as an error: this is the
    /// read used to decide whether to offer the setup wizard, and a file that
    /// cannot be understood is exactly the case where the wizard should be
    /// offered. The engine's own load path (`FusionSettingsJson::validate`)
    /// still rejects a malformed file loudly.
    #[must_use]
    pub fn from_settings_json(settings: &Value) -> Self {
        let Some(fusion) = settings.get(FUSION_SETTINGS_KEY).and_then(Value::as_object) else {
            return Self::default();
        };
        Self {
            panels: fusion
                .get(PANEL_MODELS_KEY)
                .and_then(Value::as_array)
                .map(|rows| rows.iter().filter_map(choice_from_json).collect())
                .unwrap_or_default(),
            analyst: fusion.get(ANALYST_MODEL_KEY).and_then(choice_from_json),
        }
    }

    /// Merge these roles into `settings`, leaving every other key alone.
    ///
    /// A role that is `None`/empty here REMOVES its key rather than writing
    /// `null`: a null would deserialize to `None` anyway, so leaving one behind
    /// would put a value in the file that the engine never resolves.
    pub fn write_settings_json(&self, settings: &mut Value) {
        let fusion = ensure_object(
            ensure_object(settings)
                .entry(FUSION_SETTINGS_KEY.to_string())
                .or_insert_with(|| Value::Object(Map::new())),
        );
        if self.panels.is_empty() {
            fusion.remove(PANEL_MODELS_KEY);
        } else {
            fusion.insert(
                PANEL_MODELS_KEY.to_string(),
                Value::Array(self.panels.iter().map(choice_to_json).collect()),
            );
        }
        write_optional(fusion, ANALYST_MODEL_KEY, self.analyst.as_ref());
    }

    /// Roles with nothing usable configured, in [`FusionModelRole::ALL`] order.
    #[must_use]
    pub fn missing_roles(&self) -> Vec<FusionModelRole> {
        let mut missing = Vec::new();
        // A one-model "panel" is not a panel: the whole premise is independent
        // answers that can disagree, so a roster below two reads as missing
        // rather than as a small configuration.
        if self.panels.len() < 2 {
            missing.push(FusionModelRole::Panels);
        }
        if self.analyst.is_none() {
            missing.push(FusionModelRole::Analyst);
        }
        missing
    }

    /// Whether every role is configured.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.missing_roles().is_empty()
    }

    /// Whether the file says nothing at all about Fusion models — the shape a
    /// first run sees, as distinct from a half-finished configuration.
    #[must_use]
    pub fn is_untouched(&self) -> bool {
        self.panels.is_empty() && self.analyst.is_none()
    }
}

/// Read `fusion.enabled`. Absent reads as `false`, matching
/// `fusion::FusionRuntimeConfig::defaults`.
#[must_use]
pub fn enabled_from_settings_json(settings: &Value) -> bool {
    settings
        .get(FUSION_SETTINGS_KEY)
        .and_then(|fusion| fusion.get(ENABLED_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Set `fusion.enabled`, leaving every other key alone.
pub fn write_enabled_settings_json(settings: &mut Value, enabled: bool) {
    let fusion = ensure_object(
        ensure_object(settings)
            .entry(FUSION_SETTINGS_KEY.to_string())
            .or_insert_with(|| Value::Object(Map::new())),
    );
    fusion.insert(ENABLED_KEY.to_string(), Value::Bool(enabled));
}

/// The one-time startup line shown when Fusion has no model configuration.
///
/// Returns `None` once every role is configured, so a configured session sees
/// nothing. Deliberately a NOTICE and not a prompt: a user who never runs
/// Fusion should not have to dismiss anything to start working.
#[must_use]
pub fn startup_notice(roles: &FusionModelRoles) -> Option<String> {
    let missing = roles.missing_roles();
    if missing.is_empty() {
        return None;
    }
    if roles.is_untouched() {
        return Some(
            "Fusion (multi-model deliberation) has no models configured. \
             Run /fusion setup to choose its panel and analyst models."
                .to_string(),
        );
    }
    let names = missing
        .iter()
        .map(|role| role.label())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Fusion is partly configured — still missing: {names}. Run /fusion setup to finish."
    ))
}

fn write_optional(fusion: &mut Map<String, Value>, key: &str, choice: Option<&FusionModelChoice>) {
    match choice {
        Some(choice) => {
            fusion.insert(key.to_string(), choice_to_json(choice));
        }
        None => {
            fusion.remove(key);
        }
    }
}

fn choice_to_json(choice: &FusionModelChoice) -> Value {
    let mut object = Map::new();
    object.insert("profile".to_string(), Value::String(choice.profile.clone()));
    object.insert("model".to_string(), Value::String(choice.model.clone()));
    Value::Object(object)
}

fn choice_from_json(value: &Value) -> Option<FusionModelChoice> {
    let object = value.as_object()?;
    let profile = object.get("profile")?.as_str()?.trim();
    let model = object.get("model")?.as_str()?.trim();
    if profile.is_empty() || model.is_empty() {
        return None;
    }
    Some(FusionModelChoice::new(profile, model))
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value
        .as_object_mut()
        .expect("value was just replaced with an object")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roles() -> FusionModelRoles {
        FusionModelRoles {
            panels: vec![
                FusionModelChoice::new("anthropic", "claude-opus-5"),
                FusionModelChoice::new("openai", "gpt-5.6-sol"),
            ],
            analyst: Some(FusionModelChoice::new("openai", "gpt-5.6-terra")),
        }
    }

    #[test]
    fn a_write_preserves_every_unrelated_key_inside_and_outside_the_fusion_block() {
        let mut settings = json!({
            "theme": "dark",
            "fusion": { "enabled": true, "qualityPanelCount": 3 }
        });
        roles().write_settings_json(&mut settings);
        assert_eq!(settings["theme"], json!("dark"));
        assert_eq!(settings["fusion"]["enabled"], json!(true));
        assert_eq!(settings["fusion"]["qualityPanelCount"], json!(3));
        assert_eq!(
            settings["fusion"]["panelModels"],
            json!([
                {"profile": "anthropic", "model": "claude-opus-5"},
                {"profile": "openai", "model": "gpt-5.6-sol"}
            ])
        );
        assert_eq!(
            FusionModelRoles::from_settings_json(&settings),
            roles(),
            "a write must round-trip through the read the wizard seeds from"
        );
    }

    #[test]
    fn clearing_a_role_removes_its_key_rather_than_writing_null() {
        let mut settings = json!({});
        roles().write_settings_json(&mut settings);
        FusionModelRoles::default().write_settings_json(&mut settings);
        let fusion = settings["fusion"].as_object().unwrap();
        // `"analystModel": null` would deserialize to `None` on the engine side
        // too, but it would sit in the file under a layer's name as a value the
        // engine never resolves.
        assert!(!fusion.contains_key(PANEL_MODELS_KEY));
        assert!(!fusion.contains_key(ANALYST_MODEL_KEY));
    }

    #[test]
    fn a_single_panel_reads_as_a_missing_roster_not_a_small_one() {
        let one = FusionModelRoles {
            panels: vec![FusionModelChoice::new("anthropic", "claude-opus-5")],
            ..roles()
        };
        assert_eq!(one.missing_roles(), vec![FusionModelRole::Panels]);
        assert!(!one.is_configured());
        assert!(!one.is_untouched());
        assert!(startup_notice(&one)
            .expect("a partial configuration still warrants a notice")
            .contains("panel models"));
    }

    #[test]
    fn a_malformed_entry_reads_as_absent_so_the_wizard_is_still_offered() {
        let settings = json!({"fusion": {
            "panelModels": [
                {"profile": "anthropic", "model": "claude-opus-5"},
                {"profile": "", "model": "gpt-5.6-sol"},
                {"model": "no-profile"},
                "not-an-object"
            ],
            "analystModel": {"profile": "openai"}
        }});
        let read = FusionModelRoles::from_settings_json(&settings);
        assert_eq!(
            read.panels,
            vec![FusionModelChoice::new("anthropic", "claude-opus-5")]
        );
        assert_eq!(read.analyst, None);
        assert_eq!(
            read.missing_roles(),
            FusionModelRole::ALL.to_vec(),
            "every role that could not be read must be reported missing"
        );
    }

    #[test]
    fn a_fully_configured_file_produces_no_startup_notice() {
        assert_eq!(startup_notice(&roles()), None);
        assert!(roles().is_configured());
    }

    #[test]
    fn an_untouched_file_names_the_command_that_fixes_it() {
        let notice = startup_notice(&FusionModelRoles::default())
            .expect("an untouched file must produce the first-run notice");
        assert!(notice.contains("/fusion setup"), "{notice}");
    }

    #[test]
    fn the_enabled_switch_reads_and_writes_independently_of_the_roles() {
        let mut settings = json!({"fusion": {"panelModels": []}});
        assert!(!enabled_from_settings_json(&settings));
        write_enabled_settings_json(&mut settings, true);
        assert!(enabled_from_settings_json(&settings));
        roles().write_settings_json(&mut settings);
        assert!(
            enabled_from_settings_json(&settings),
            "writing roles must not disturb the master switch"
        );
    }

    #[test]
    fn a_non_object_settings_root_is_replaced_rather_than_panicking() {
        let mut settings = json!("this file was not an object");
        roles().write_settings_json(&mut settings);
        assert_eq!(FusionModelRoles::from_settings_json(&settings), roles());
    }
}
