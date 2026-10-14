//! Fusion strategy identity and source-aware resolution shared by all hosts.

use crate::settings::schema::{FusionRegistrySettingsJson, FusionSettingsJson};
use crate::settings::tracer::Source;
use crate::types::scope::WritableScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable identity; naming and default changes never retarget this reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionStrategyRef {
    #[serde(with = "scope_wire")]
    pub scope: WritableScope,
    #[serde(deserialize_with = "deserialize_strategy_id")]
    pub id: String,
}

fn deserialize_strategy_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let id = String::deserialize(deserializer)?;
    if crate::settings::schema::valid_fusion_strategy_id(&id) {
        Ok(id)
    } else {
        Err(serde::de::Error::custom("Fusion strategy id must contain only ASCII letters, digits, hyphens, underscores or periods"))
    }
}

impl std::fmt::Display for FusionStrategyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", scope_wire::name(self.scope), self.id)
    }
}

impl std::str::FromStr for FusionStrategyRef {
    type Err = FusionStrategyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (scope, id) = value.split_once(':').ok_or_else(|| {
            FusionStrategyError::InvalidConfiguration(
                "Fusion strategy must be <user|project|local>:<id>".into(),
            )
        })?;
        let scope = scope_wire::parse(scope).ok_or_else(|| {
            FusionStrategyError::InvalidConfiguration(
                "Fusion strategy scope must be user, project or local".into(),
            )
        })?;
        if !crate::settings::schema::valid_fusion_strategy_id(id) {
            return Err(FusionStrategyError::InvalidConfiguration(
                "Fusion strategy id must contain only ASCII letters, digits, hyphens, underscores or periods".into(),
            ));
        }
        Ok(Self {
            scope,
            id: id.into(),
        })
    }
}

mod scope_wire {
    use super::*;

    pub(super) fn name(scope: WritableScope) -> &'static str {
        match scope {
            WritableScope::User => "user",
            WritableScope::Project => "project",
            WritableScope::Local => "local",
        }
    }

    pub(super) fn parse(value: &str) -> Option<WritableScope> {
        match value {
            "user" => Some(WritableScope::User),
            "project" => Some(WritableScope::Project),
            "local" => Some(WritableScope::Local),
            _ => None,
        }
    }

    pub(super) fn serialize<S: serde::Serializer>(
        scope: &WritableScope,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(name(*scope))
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<WritableScope, D::Error> {
        let value = String::deserialize(deserializer)?;
        parse(&value).ok_or_else(|| {
            serde::de::Error::custom("Fusion strategy scope must be user, project or local")
        })
    }
}

/// Immutable prepared selection. Execution uses this exact configuration;
/// live checks resolve `reference` again only to enforce current restrictions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusionStrategySelection {
    pub reference: FusionStrategyRef,
    pub name: String,
    pub config: FusionSettingsJson,
    pub revision: String,
}

/// One admitted source before ordinary settings merging. The path is retained
/// for diagnostics; it never grants a CLI or managed source a writable scope.
#[derive(Debug, Clone, PartialEq)]
pub struct FusionSettingsLayer {
    pub source: Source,
    pub path: Option<std::path::PathBuf>,
    pub registry: FusionRegistrySettingsJson,
}

impl FusionSettingsLayer {
    pub fn new(source: Source, settings: &crate::settings::SettingsJson) -> Option<Self> {
        settings.fusion.clone().map(|registry| Self {
            source,
            path: None,
            registry,
        })
    }

    pub fn scope(&self) -> Option<WritableScope> {
        match self.source {
            Source::User => Some(WritableScope::User),
            Source::Project => Some(WritableScope::Project),
            Source::Local => Some(WritableScope::Local),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FusionStrategyError {
    #[error("Select a Fusion strategy or configure a default strategy")]
    SelectionRequired,
    #[error("Fusion strategy `{0}` no longer exists in its scope")]
    NotFound(FusionStrategyRef),
    #[error("{0}")]
    InvalidConfiguration(String),
}

fn validate_layers(layers: &[FusionSettingsLayer]) -> Result<(), FusionStrategyError> {
    for layer in layers {
        layer.registry.validate().map_err(|error| {
            let location = layer.path.as_ref().map_or_else(
                || format!("{:?}", layer.source),
                |path| path.display().to_string(),
            );
            FusionStrategyError::InvalidConfiguration(format!(
                "Fusion settings at {location}: {error}"
            ))
        })?;
        if !matches!(layer.source, Source::Cli | Source::Managed)
            && layer.registry.overrides.is_some()
        {
            return Err(FusionStrategyError::InvalidConfiguration("Fusion runtime overrides are allowed only in CLI and managed settings; put execution parameters inside a strategy config".into()));
        }
        if layer.scope().is_none()
            && (!layer.registry.strategies.is_empty()
                || layer.registry.default_strategy_id.is_some())
        {
            return Err(FusionStrategyError::InvalidConfiguration("Named Fusion strategies and defaults must belong to user, project or local settings".into()));
        }
    }
    Ok(())
}

/// Default selection uses local > project > user independently of input order.
/// Invalid registries and dangling references never fall back to lower defaults.
pub fn default_fusion_strategy(
    layers: &[FusionSettingsLayer],
) -> Result<Option<FusionStrategyRef>, FusionStrategyError> {
    validate_layers(layers)?;
    for scope in [
        WritableScope::Local,
        WritableScope::Project,
        WritableScope::User,
    ] {
        if let Some(id) = layers
            .iter()
            .rev()
            .find(|layer| layer.scope() == Some(scope))
            .and_then(|layer| layer.registry.default_strategy_id.clone())
        {
            return Ok(Some(FusionStrategyRef { scope, id }));
        }
    }
    Ok(None)
}

/// Effective global controls, useful for discovery before a strategy is chosen.
pub fn fusion_global_settings(
    layers: &[FusionSettingsLayer],
) -> Result<FusionSettingsJson, FusionStrategyError> {
    validate_layers(layers)?;
    apply_layers(FusionSettingsJson::default(), layers)
}

/// Resolve once before defaults, credentials, quote or timeout computation.
pub fn resolve_fusion_strategy(
    layers: &[FusionSettingsLayer],
    requested: Option<&FusionStrategyRef>,
) -> Result<FusionStrategySelection, FusionStrategyError> {
    validate_layers(layers)?;
    let reference = requested
        .cloned()
        .or(default_fusion_strategy(layers)?)
        .ok_or(FusionStrategyError::SelectionRequired)?;
    let strategy = layers
        .iter()
        .rev()
        .filter(|layer| layer.scope() == Some(reference.scope))
        .flat_map(|layer| &layer.registry.strategies)
        .find(|strategy| strategy.id == reference.id)
        .ok_or_else(|| FusionStrategyError::NotFound(reference.clone()))?;
    let config = apply_layers(strategy.config.clone(), layers)?;
    config
        .validate()
        .map_err(|error| FusionStrategyError::InvalidConfiguration(error.to_string()))?;
    // Renaming is harmless to preparation; identity and all effective runtime
    // controls must stay unchanged between credential preparation and execution.
    let revision = content_revision(&(&reference, &config));
    Ok(FusionStrategySelection {
        reference,
        name: strategy.name.clone(),
        config,
        revision,
    })
}

fn apply_layers(
    mut config: FusionSettingsJson,
    layers: &[FusionSettingsLayer],
) -> Result<FusionSettingsJson, FusionStrategyError> {
    // Managed policy always applies last, including when a host constructed
    // the source list itself rather than using the canonical settings loader.
    for layer in layers
        .iter()
        .filter(|layer| layer.source != Source::Managed)
        .chain(
            layers
                .iter()
                .filter(|layer| layer.source == Source::Managed),
        )
    {
        let registry = &layer.registry;
        if let Some(overrides) = &registry.overrides {
            let mut value = serde_json::to_value(&config)
                .map_err(|error| FusionStrategyError::InvalidConfiguration(error.to_string()))?;
            merge_config_value(
                &mut value,
                serde_json::to_value(overrides).map_err(|error| {
                    FusionStrategyError::InvalidConfiguration(error.to_string())
                })?,
            );
            config = serde_json::from_value(value)
                .map_err(|error| FusionStrategyError::InvalidConfiguration(error.to_string()))?;
        }
        config.enabled = registry.enabled.or(config.enabled);
        config.proactive = registry.proactive.or(config.proactive);
        config.allowed_profiles = registry
            .allowed_profiles
            .clone()
            .or(config.allowed_profiles);
        config.allow_cross_provider_for_agent = registry
            .allow_cross_provider_for_agent
            .or(config.allow_cross_provider_for_agent);
        config.slash_cross_provider_default = registry
            .slash_cross_provider_default
            .or(config.slash_cross_provider_default);
    }
    Ok(config)
}

fn merge_config_value(previous: &mut serde_json::Value, next: serde_json::Value) {
    if let (Some(previous), Some(next)) = (previous.as_object_mut(), next.as_object()) {
        for (key, value) in next {
            if let Some(existing) = previous.get_mut(key) {
                merge_config_value(existing, value.clone());
            } else {
                previous.insert(key.clone(), value.clone());
            }
        }
    } else {
        *previous = next;
    }
}

/// Content version of one registry, usable by in-process optimistic writers.
/// File writers may instead hash the raw Fusion value to preserve unknown data.
pub fn fusion_registry_revision(registry: &FusionRegistrySettingsJson) -> String {
    content_revision(registry)
}

fn content_revision(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("Fusion configuration is serializable");
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{FileLayerScope, LoadInputs, Settings, SettingsJson, SupplementalLayers};
    use serde_json::json;

    fn layer(source: Source, registry: serde_json::Value) -> FusionSettingsLayer {
        let settings: SettingsJson = serde_json::from_value(json!({"fusion": registry})).unwrap();
        FusionSettingsLayer::new(source, &settings).unwrap()
    }

    fn named(
        source: Source,
        id: &str,
        config: serde_json::Value,
        default: bool,
    ) -> FusionSettingsLayer {
        layer(
            source,
            json!({
                "enabled": true,
                "defaultStrategyId": default.then_some(id),
                "strategies": [{"id": id, "name": "Review", "config": config}]
            }),
        )
    }

    #[test]
    fn fusion_scope_reference_wire_and_cli_are_strict() {
        let reference: FusionStrategyRef = "project:review-id".parse().unwrap();
        assert_eq!(reference.scope, WritableScope::Project);
        assert_eq!(reference.to_string(), "project:review-id");
        assert_eq!(
            serde_json::to_value(&reference).unwrap(),
            json!({"scope": "project", "id": "review-id"})
        );
        for value in [
            "team:review",
            "managed:review",
            "user:",
            "user:a:b",
            "user: a",
            "user:a b",
            "user:a\nb",
        ] {
            assert!(value.parse::<FusionStrategyRef>().is_err(), "{value}");
        }
        assert!(
            serde_json::from_value::<FusionStrategyRef>(json!({"scope":"team","id":"a"})).is_err()
        );
    }

    #[test]
    fn fusion_cross_scope_names_and_ids_coexist_without_parameter_inheritance() {
        let layers = vec![
            named(Source::User, "same", json!({"qualityPanelCount":4}), true),
            named(Source::Project, "same", json!({"preset":"fast"}), true),
            named(Source::Local, "same", json!({}), true),
        ];
        for (scope, preset, count) in [
            ("user", None, Some(4)),
            ("project", Some("fast"), None),
            ("local", None, None),
        ] {
            let selected =
                resolve_fusion_strategy(&layers, Some(&format!("{scope}:same").parse().unwrap()))
                    .unwrap();
            assert_eq!(selected.name, "Review");
            assert_eq!(selected.config.preset.as_deref(), preset);
            assert_eq!(selected.config.quality_panel_count, count);
        }
        assert_eq!(
            resolve_fusion_strategy(&layers, None)
                .unwrap()
                .reference
                .scope,
            WritableScope::Local
        );
        assert_eq!(
            resolve_fusion_strategy(&layers[..2], None)
                .unwrap()
                .reference
                .scope,
            WritableScope::Project
        );
        assert_eq!(
            resolve_fusion_strategy(&layers[..1], None)
                .unwrap()
                .reference
                .scope,
            WritableScope::User
        );
    }

    #[test]
    fn fusion_same_scope_duplicate_or_untrimmed_names_are_diagnostics() {
        for strategies in [
            json!([{"id":"a","name":"Review","config":{}},{"id":"b","name":"Review","config":{}}]),
            json!([{"id":"a","name":"Review","config":{}},{"id":"a","name":"Other","config":{}}]),
            json!([{"id":"a","name":" Review ","config":{}}]),
            json!([{"id":"a","name":" ","config":{}}]),
        ] {
            let invalid = layer(Source::User, json!({"strategies": strategies}));
            assert!(invalid.registry.diagnostic.is_some());
            assert!(resolve_fusion_strategy(&[invalid], None).is_err());
        }
    }

    #[test]
    fn fusion_missing_or_invalid_selection_never_picks_another_strategy() {
        let layers = vec![named(Source::User, "a", json!({}), false)];
        assert_eq!(
            resolve_fusion_strategy(&layers, None),
            Err(FusionStrategyError::SelectionRequired)
        );
        let missing: FusionStrategyRef = "user:missing".parse().unwrap();
        assert_eq!(
            resolve_fusion_strategy(&layers, Some(&missing)),
            Err(FusionStrategyError::NotFound(missing))
        );
        let invalid = layer(
            Source::Local,
            json!({"defaultStrategyId":"deleted","strategies":[]}),
        );
        let layers = vec![named(Source::User, "a", json!({}), true), invalid];
        assert!(matches!(
            resolve_fusion_strategy(&layers, None),
            Err(FusionStrategyError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn fusion_managed_policy_applies_after_selection_and_cli_overrides() {
        let managed = layer(
            Source::Managed,
            json!({
                "enabled":false,"allowedProfiles":["managed-profile"],"allowCrossProviderForAgent":false,
                "overrides":{"panelMaxTurns":2}
            }),
        );
        // Intentionally supply managed first to test the enforcement boundary.
        let layers = vec![
            managed,
            named(Source::User, "a", json!({"panelMaxTurns":10}), true),
            layer(
                Source::Cli,
                json!({"enabled":true,"overrides":{"panelMaxTurns":8}}),
            ),
        ];
        let selected = resolve_fusion_strategy(&layers, None).unwrap();
        assert_eq!(selected.config.enabled, Some(false));
        assert_eq!(selected.config.panel_max_turns, Some(2));
        assert_eq!(
            selected.config.allowed_profiles,
            Some(vec!["managed-profile".into()])
        );
        assert_eq!(selected.config.allow_cross_provider_for_agent, Some(false));
    }

    #[test]
    fn fusion_runtime_overrides_are_not_scoped_strategies() {
        let layers = vec![
            named(Source::User, "a", json!({}), true),
            layer(Source::Cli, json!({"overrides":{"preset":"fast"}})),
        ];
        let selected = resolve_fusion_strategy(&layers, None).unwrap();
        assert_eq!(selected.reference.scope, WritableScope::User);
        assert_eq!(selected.config.preset.as_deref(), Some("fast"));
        let synthetic = named(Source::Cli, "fake", json!({}), true);
        assert!(resolve_fusion_strategy(&[synthetic], None).is_err());
        let file_override = layer(Source::Project, json!({"overrides":{"panelMaxTurns":3}}));
        assert!(resolve_fusion_strategy(&[file_override], None).is_err());
        let defaults_override = layer(Source::Defaults, json!({"overrides":{"panelMaxTurns":3}}));
        assert!(resolve_fusion_strategy(&[defaults_override], None).is_err());
    }

    #[test]
    fn fusion_partial_overrides_validate_against_selected_stage_times() {
        for source in [Source::Cli, Source::Managed] {
            let selected = named(
                Source::User,
                "a",
                json!({
                    "panelTotalTimeoutMs":30000,"analystTimeoutMs":30000,"totalTimeoutMs":100000
                }),
                true,
            );
            let overlay = layer(
                source,
                json!({"overrides":{
                    "analystTimeoutMs":40000,"totalTimeoutMs":120000
                }}),
            );
            assert!(overlay.registry.diagnostic.is_none());
            let resolved = resolve_fusion_strategy(&[selected.clone(), overlay], None).unwrap();
            assert_eq!(resolved.config.panel_total_timeout_ms, Some(30000));
            assert_eq!(resolved.config.analyst_timeout_ms, Some(40000));
            let too_short = layer(
                source,
                json!({"overrides":{
                    "analystTimeoutMs":40000,"totalTimeoutMs":100000
                }}),
            );
            assert!(too_short.registry.diagnostic.is_none());
            assert!(resolve_fusion_strategy(&[selected, too_short], None).is_err());
            assert!(layer(source, json!({"overrides":{"panelMaxTurns":0}}))
                .registry
                .diagnostic
                .is_some());
        }
    }

    #[test]
    fn fusion_leaf_cannot_bypass_global_controls() {
        for config in [
            json!({"enabled":true}),
            json!({"allowedProfiles":[]}),
            json!({"allowCrossProviderForAgent":true}),
            json!({"proactive":true}),
        ] {
            let invalid = named(Source::User, "a", config, true);
            assert!(invalid.registry.diagnostic.is_some());
            assert!(resolve_fusion_strategy(&[invalid], None).is_err());
        }
    }

    #[test]
    fn fusion_snapshot_is_immutable_and_revision_fences_identity_and_config() {
        let mut layers = vec![named(Source::User, "a", json!({"panelMaxTurns":4}), true)];
        let captured = resolve_fusion_strategy(&layers, None).unwrap();
        layers[0].registry.strategies[0].name = "Renamed".into();
        let renamed = resolve_fusion_strategy(&layers, None).unwrap();
        assert_eq!(renamed.revision, captured.revision);
        assert_eq!(captured.name, "Review");
        layers[0].registry.strategies[0].config.panel_max_turns = Some(5);
        assert_ne!(
            resolve_fusion_strategy(&layers, None).unwrap().revision,
            captured.revision
        );
        assert_eq!(captured.config.panel_max_turns, Some(4));
        layers[0].registry.strategies[0].config.panel_max_turns = Some(4);
        layers[0].registry.strategies[0].id = "b".into();
        layers[0].registry.default_strategy_id = Some("b".into());
        assert_ne!(
            resolve_fusion_strategy(&layers, None).unwrap().revision,
            captured.revision
        );
        assert!(matches!(
            resolve_fusion_strategy(&layers, Some(&captured.reference)),
            Err(FusionStrategyError::NotFound(_))
        ));
    }

    #[test]
    fn fusion_loader_retains_scope_layers_and_respects_admission() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user.json");
        let project = temp.path().join("project");
        let shared = crate::settings::loader::project_settings_path(&project);
        let local = crate::settings::loader::local_settings_path(&project);
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        for (path, source) in [
            (&user, Source::User),
            (&shared, Source::Project),
            (&local, Source::Local),
        ] {
            std::fs::write(
                path,
                serde_json::to_vec(&json!({"fusion":named(source, "a", json!({}), true).registry}))
                    .unwrap(),
            )
            .unwrap();
        }
        let env = Default::default();
        let load = |scope| {
            Settings::load_with_layers_from_user_path(
                LoadInputs {
                    env: &env,
                    project_dir: &project,
                    defaults: SettingsJson::default(),
                },
                scope,
                SupplementalLayers::default(),
                Some(&user),
            )
            .unwrap()
        };
        let all = load(FileLayerScope::ALL);
        assert_eq!(all.fusion_layers.len(), 3);
        assert_eq!(all.fusion_layers[0].path.as_ref(), Some(&user));
        assert_eq!(
            resolve_fusion_strategy(&all.fusion_layers, None)
                .unwrap()
                .reference
                .scope,
            WritableScope::Local
        );
        // A source excluded by --setting-sources cannot inject either a
        // strategy or a blocking legacy-format diagnostic.
        std::fs::write(&local, r#"{"fusion":{"panelModels":[]}}"#).unwrap();
        let gated = load(FileLayerScope {
            include_user: true,
            include_project: true,
            include_local: false,
        });
        assert_eq!(gated.fusion_layers.len(), 2);
        assert_eq!(
            resolve_fusion_strategy(&gated.fusion_layers, None)
                .unwrap()
                .reference
                .scope,
            WritableScope::Project
        );
    }

    #[test]
    fn fusion_legacy_format_preserves_file_and_unrelated_settings_and_blocks_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user.json");
        let project = temp.path().join("project");
        let shared = crate::settings::loader::project_settings_path(&project);
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(
            &user,
            serde_json::to_vec(&json!({"fusion":named(Source::User,"a",json!({}),true).registry}))
                .unwrap(),
        )
        .unwrap();
        let legacy = r#"{"model":"preserved-model","permissions":{"deny":["Bash(rm *)"]},"fusion":{"enabled":true,"panelModels":[]}}"#;
        std::fs::write(&shared, legacy).unwrap();
        let env = Default::default();
        let effective = Settings::load_with_layers_from_user_path(
            LoadInputs {
                env: &env,
                project_dir: &project,
                defaults: SettingsJson::default(),
            },
            FileLayerScope::ALL,
            SupplementalLayers::default(),
            Some(&user),
        )
        .unwrap();
        assert_eq!(effective.settings.model.as_deref(), Some("preserved-model"));
        assert!(effective.settings.permissions.is_some());
        assert!(effective.fusion_layers[1]
            .registry
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("old single-configuration format"));
        assert!(matches!(
            resolve_fusion_strategy(&effective.fusion_layers, None),
            Err(FusionStrategyError::InvalidConfiguration(_))
        ));
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), legacy);
    }
}
