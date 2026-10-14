//! Shared model-role editing for independent named Fusion strategy configurations.

use crate::host::fusion::{FusionModelChoice, FusionModelRole};
use crate::settings::schema::{FusionModelSelectionJson, FusionSettingsJson};

/// The model roles as configured (or not) in a settings file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FusionModelRoles {
    /// Panel roster in the operator's priority order. Empty = unconfigured.
    pub panels: Vec<FusionModelChoice>,
    /// Judge model. `None` = unconfigured.
    pub analyst: Option<FusionModelChoice>,
}

impl FusionModelRoles {
    /// Read the model roles from one independent strategy configuration.
    #[must_use]
    pub fn from_strategy_config(config: &FusionSettingsJson) -> Self {
        let choice =
            |model: &FusionModelSelectionJson| FusionModelChoice::new(&model.profile, &model.model);
        Self {
            panels: config
                .panel_models
                .as_ref()
                .map(|models| models.iter().map(choice).collect())
                .unwrap_or_default(),
            analyst: config.analyst_model.as_ref().map(choice),
        }
    }

    /// Replace only model roles, preserving the strategy's advanced parameters.
    pub fn write_strategy_config(&self, config: &mut FusionSettingsJson) {
        let selection = |model: &FusionModelChoice| FusionModelSelectionJson {
            profile: model.profile.clone(),
            model: model.model.clone(),
        };
        config.panel_models =
            (!self.panels.is_empty()).then(|| self.panels.iter().map(selection).collect());
        config.analyst_model = self.analyst.as_ref().map(selection);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> FusionModelRoles {
        FusionModelRoles {
            panels: vec![
                FusionModelChoice::new("anthropic", "opus"),
                FusionModelChoice::new("openai", "gpt"),
            ],
            analyst: Some(FusionModelChoice::new("openai", "gpt")),
        }
    }

    #[test]
    fn model_edits_preserve_advanced_strategy_settings() {
        let mut config = FusionSettingsJson {
            max_panel: Some(4),
            ..Default::default()
        };
        roles().write_strategy_config(&mut config);
        assert_eq!(FusionModelRoles::from_strategy_config(&config), roles());
        assert_eq!(config.max_panel, Some(4));
        FusionModelRoles::default().write_strategy_config(&mut config);
        assert!(config.panel_models.is_none());
        assert!(config.analyst_model.is_none());
        assert_eq!(config.max_panel, Some(4));
    }

    #[test]
    fn setup_requires_two_panels_and_an_analyst() {
        assert!(roles().is_configured());
        let mut partial = roles();
        partial.panels.pop();
        assert_eq!(partial.missing_roles(), vec![FusionModelRole::Panels]);
        assert!(startup_notice(&partial).unwrap().contains("/fusion setup"));
        assert!(startup_notice(&roles()).is_none());
        assert!(startup_notice(&FusionModelRoles::default())
            .unwrap()
            .contains("/fusion setup"));
    }
}
