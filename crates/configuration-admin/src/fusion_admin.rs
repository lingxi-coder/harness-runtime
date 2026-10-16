//! Revision-checked management of independent, scoped Fusion strategies.

use client::adapter::ClientEventSink;
use client::protocol::commands::FusionAdminCommandDto;
use client::protocol::events::ClientEvent;
use client::protocol::listings::{
    ConfigurationDomainDto as Domain, ConfigurationEffectDto as Effect,
    ConfigurationOperationStatusDto as Status,
};
use lingxi_core::host::fusion_strategy::{
    default_fusion_strategy, fusion_global_settings, resolve_fusion_strategy, FusionSettingsLayer,
    FusionStrategyRef,
};
use lingxi_core::settings::schema::{FusionRegistrySettingsJson, FusionStrategy};
use lingxi_core::settings::tracer::Source;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::WritableScope;
use migrations::settings_update::{
    read_settings_map, settings_path, transform_settings_with_before_publish,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config_admin::{emit_error, emit_operation, stable_json_revision};
use crate::settings_bridge::SettingsContext;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Globals {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proactive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slash_cross_provider_default: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_cross_provider_for_agent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_profiles: Option<Vec<String>>,
}

impl From<&FusionRegistrySettingsJson> for Globals {
    fn from(registry: &FusionRegistrySettingsJson) -> Self {
        Self {
            enabled: registry.enabled,
            proactive: registry.proactive,
            slash_cross_provider_default: registry.slash_cross_provider_default,
            allow_cross_provider_for_agent: registry.allow_cross_provider_for_agent,
            allowed_profiles: registry.allowed_profiles.clone(),
        }
    }
}

impl Globals {
    fn overlay(&mut self, higher: Self) {
        self.enabled = higher.enabled.or(self.enabled);
        self.proactive = higher.proactive.or(self.proactive);
        self.slash_cross_provider_default = higher
            .slash_cross_provider_default
            .or(self.slash_cross_provider_default);
        self.allow_cross_provider_for_agent = higher
            .allow_cross_provider_for_agent
            .or(self.allow_cross_provider_for_agent);
        self.allowed_profiles = higher.allowed_profiles.or(self.allowed_profiles.take());
    }
}

const SCOPES: [(WritableScope, Source, &str); 3] = [
    (WritableScope::User, Source::User, "user"),
    (WritableScope::Project, Source::Project, "project"),
    (WritableScope::Local, Source::Local, "local"),
];

fn registry(raw: Option<&Value>) -> Result<FusionRegistrySettingsJson, String> {
    match raw.filter(|value| !value.is_null()) {
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| error.to_string()),
        None => Ok(FusionRegistrySettingsJson::default()),
    }
}

fn managed_registry(source: &Utf16JsonProjection) -> Result<FusionRegistrySettingsJson, String> {
    match source.get_property("fusion") {
        Some(value) if !value.is_null() => {
            let raw = source
                .subprojection_property_units(&"fusion".encode_utf16().collect::<Vec<_>>())
                .and_then(|value| value.to_json_string())
                .map_err(|error| error.to_string())?;
            serde_json::from_str(&raw).map_err(|error| error.to_string())
        }
        _ => Ok(FusionRegistrySettingsJson::default()),
    }
}

fn raw_revision(raw: Option<&Value>) -> Result<String, String> {
    stable_json_revision(raw.unwrap_or(&Value::Null))
}

fn file_registry(raw: Option<&Value>) -> Result<FusionRegistrySettingsJson, String> {
    let mut value = registry(raw)?;
    if value.overrides.is_some() {
        value.diagnostic =
            Some("Fusion runtime overrides are only permitted in CLI and managed settings".into());
    }
    Ok(value)
}

/// Read every strategy with the revision of its own source registry.
pub fn catalog_json(context: &SettingsContext) -> Result<String, String> {
    let mut scopes = Vec::new();
    let mut globals = Globals {
        enabled: Some(false),
        proactive: Some(false),
        slash_cross_provider_default: Some(true),
        allow_cross_provider_for_agent: Some(false),
        allowed_profiles: Some(Vec::new()),
    };
    let authoritative_layers = context
        .fusion_layers
        .as_ref()
        .map(|load| load())
        .transpose()?;
    let mut layers = Vec::new();
    for (scope, source, name) in SCOPES {
        let path = settings_path(
            scope,
            &context.paths.lingxi_home,
            &context.paths.project_dir,
        );
        let (registry, revision, readable) = match read_settings_map(&path) {
            Ok(map) => (
                file_registry(map.get("fusion"))?,
                raw_revision(map.get("fusion"))?,
                true,
            ),
            Err(error) => {
                // A broken file already gets skipped by the runtime loader.
                // Keep the rest of the catalog usable, and surface the error
                // on this source rather than hiding valid admitted strategies.
                // Hash the broken bytes when readable. An unreadable source
                // uses a diagnostic sentinel, which cannot match a writable
                // registry revision; mutations still refuse the read error.
                let revision = match std::fs::read(&path) {
                    Ok(bytes) => format!("{:x}", Sha256::digest(bytes)),
                    Err(_) => format!("unavailable:{:x}", Sha256::digest(error.as_bytes())),
                };
                (
                    FusionRegistrySettingsJson {
                        diagnostic: Some(error),
                        ..FusionRegistrySettingsJson::default()
                    },
                    revision,
                    false,
                )
            }
        };
        let admitted = readable
            && authoritative_layers
                .as_ref()
                .is_none_or(|layers| layers.iter().any(|layer| layer.scope() == Some(scope)));
        if readable {
            layers.push(FusionSettingsLayer {
                source,
                path: Some(path.clone()),
                registry: registry.clone(),
            });
        }
        scopes.push(json!({
            "scope":name,
            "admitted":admitted,
            "path":path,
            "revision":revision,
            "strategies":registry.strategies,
            "default_strategy_id":registry.default_strategy_id,
            "globals":Globals::from(&registry),
            "diagnostic":registry.diagnostic,
        }));
    }
    let managed = managed_registry(&context.managed)?;
    layers.push(FusionSettingsLayer {
        source: Source::Managed,
        path: None,
        registry: managed,
    });
    let layers = authoritative_layers.unwrap_or(layers);
    let mut diagnostics: Vec<String> = layers
        .iter()
        .filter_map(|layer| {
            layer
                .registry
                .diagnostic
                .as_ref()
                .map(|diagnostic| format!("{:?}: {diagnostic}", layer.source))
        })
        .collect();
    let default_strategy = match default_fusion_strategy(&layers) {
        Ok(reference) => reference,
        Err(error) => {
            if diagnostics.is_empty() {
                diagnostics.push(error.to_string());
            }
            None
        }
    };
    if let Ok(config) = fusion_global_settings(&layers) {
        globals.overlay(Globals {
            enabled: config.enabled,
            proactive: config.proactive,
            slash_cross_provider_default: config.slash_cross_provider_default,
            allow_cross_provider_for_agent: config.allow_cross_provider_for_agent,
            allowed_profiles: config.allowed_profiles,
        });
    }
    serde_json::to_string(&json!({"scopes":scopes,"effective_globals":globals,"default_strategy":default_strategy,"diagnostics":diagnostics})).map_err(|error| error.to_string())
}

/// Capture the selected strategy before any caller prepares credentials.
pub fn resolve_json(context: &SettingsContext, payload: Option<&str>) -> Result<String, String> {
    #[derive(Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Request {
        strategy: Option<FusionStrategyRef>,
    }
    let request: Request = payload
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| format!("Invalid Fusion selection: {error}"))?
        .unwrap_or_default();
    let layers = match &context.fusion_layers {
        Some(load_layers) => load_layers()?,
        None => file_layers(context)?,
    };
    let selection = resolve_fusion_strategy(&layers, request.strategy.as_ref())
        .map_err(|error| error.to_string())?;
    serde_json::to_string(&selection).map_err(|error| error.to_string())
}

fn file_layers(context: &SettingsContext) -> Result<Vec<FusionSettingsLayer>, String> {
    let mut layers = Vec::new();
    for (scope, source, _) in SCOPES {
        let path = settings_path(
            scope,
            &context.paths.lingxi_home,
            &context.paths.project_dir,
        );
        // Like the canonical runtime loader, skip syntactically broken files.
        // Valid JSON with an invalid Fusion block is retained for its diagnostic.
        let Ok(map) = read_settings_map(&path) else {
            continue;
        };
        layers.push(FusionSettingsLayer {
            source,
            path: Some(path),
            registry: file_registry(map.get("fusion"))?,
        });
    }
    if context.managed.get_property("fusion").is_some() {
        layers.push(FusionSettingsLayer {
            source: Source::Managed,
            path: None,
            registry: managed_registry(&context.managed)?,
        });
    }
    Ok(layers)
}

/// Apply a single operation under the same settings-path lock as other writers.
pub fn apply_command(
    context: &SettingsContext,
    command: &FusionAdminCommandDto,
) -> Result<(), String> {
    let scope = match command.scope.as_deref() {
        Some("user") => WritableScope::User,
        Some("project") => WritableScope::Project,
        Some("local") => WritableScope::Local,
        _ => return Err("Fusion scope must be user, project, or local".into()),
    };
    let expected = command
        .revision
        .as_deref()
        .ok_or("Fusion write revision is required")?;
    let path = settings_path(
        scope,
        &context.paths.lingxi_home,
        &context.paths.project_dir,
    );
    transform_settings_with_before_publish(
        &path,
        |map| {
            let actual = raw_revision(map.get("fusion"))?;
            if actual != expected {
                return Err(format!("Fusion revision conflict: expected {expected}, found {actual}. Refresh before saving."));
            }
            let mut registry = file_registry(map.get("fusion"))?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Save {
                id: String,
                name: String,
                config: lingxi_core::settings::schema::FusionSettingsJson,
                #[serde(default)]
                replace_legacy: bool,
                #[serde(default)]
                make_default: Option<bool>,
                enabled: Option<bool>,
            }
            let payload = command.payload_json.as_deref().unwrap_or("{}");
            let save = if command.action == "save" {
                Some(
                    serde_json::from_str::<Save>(payload)
                        .map_err(|error| format!("Invalid Fusion save payload: {error}"))?,
                )
            } else {
                None
            };
            if let Some(diagnostic) = registry.diagnostic.clone() {
                let named_format = map
                    .get("fusion")
                    .and_then(Value::as_object)
                    .is_some_and(|raw| raw.contains_key("strategies"));
                if named_format {
                    // Semantic validation retains the decoded records. Allow
                    // repairing/deleting the requested entry without resetting
                    // unrelated strategies, defaults or global controls.
                    let mut retained = registry.clone();
                    retained.diagnostic = None;
                    if retained == FusionRegistrySettingsJson::default() {
                        // A wire decoding error quarantines the entire block.
                        // We cannot safely edit records that were not decoded.
                        return Err(format!("{diagnostic} Repair the named Fusion registry in its settings file before editing strategies; replacement would discard existing data."));
                    }
                    registry = retained;
                } else if save.as_ref().is_some_and(|save| save.replace_legacy) {
                    registry = FusionRegistrySettingsJson::default();
                } else {
                    return Err(format!(
                        "{diagnostic} Explicit replacement is required before saving a new format."
                    ));
                }
            }
            match command.action.as_str() {
                "save" => {
                    let save = save.ok_or("Missing Fusion strategy")?;
                    match save.make_default {
                        Some(true) => registry.default_strategy_id = Some(save.id.clone()),
                        Some(false)
                            if registry.default_strategy_id.as_deref()
                                == Some(save.id.as_str()) =>
                        {
                            registry.default_strategy_id = None
                        }
                        _ => {}
                    }
                    if let Some(enabled) = save.enabled {
                        registry.enabled = Some(enabled);
                    }
                    let strategy = FusionStrategy {
                        id: save.id,
                        name: save.name,
                        config: save.config,
                    };
                    if let Some(existing) = registry
                        .strategies
                        .iter_mut()
                        .find(|entry| entry.id == strategy.id)
                    {
                        *existing = strategy;
                    } else {
                        registry.strategies.push(strategy);
                    }
                }
                "delete" => {
                    let id = command
                        .target
                        .as_deref()
                        .ok_or("Fusion delete target is required")?;
                    if !registry.strategies.iter().any(|entry| entry.id == id) {
                        return Err(format!(
                            "Fusion strategy `{id}` was not found in this scope"
                        ));
                    }
                    registry.strategies.retain(|entry| entry.id != id);
                    if registry.default_strategy_id.as_deref() == Some(id) {
                        registry.default_strategy_id = None;
                    }
                }
                "set_default" => {
                    registry.default_strategy_id = command.target.clone();
                }
                "set_globals" => {
                    let globals: Globals = serde_json::from_str(payload)
                        .map_err(|error| format!("Invalid Fusion global settings: {error}"))?;
                    registry.enabled = globals.enabled;
                    registry.proactive = globals.proactive;
                    registry.slash_cross_provider_default = globals.slash_cross_provider_default;
                    registry.allow_cross_provider_for_agent =
                        globals.allow_cross_provider_for_agent;
                    registry.allowed_profiles = globals.allowed_profiles;
                }
                other => return Err(format!("Unknown Fusion administration action `{other}`")),
            }
            if registry.overrides.is_some() {
                return Err(
                    "Fusion runtime overrides are only permitted in CLI and managed settings"
                        .into(),
                );
            }
            registry.validate().map_err(|error| error.to_string())?;
            map.insert(
                "fusion".into(),
                serde_json::to_value(registry).map_err(|error| error.to_string())?,
            );
            Ok(())
        },
        || permission::mark_internal_write(&path),
    )
}

/// Shared desktop transport dispatch and correlated operation receipts.
pub async fn handle_command(
    context: &SettingsContext,
    command: FusionAdminCommandDto,
    sink: &dyn ClientEventSink,
) {
    if command.action == "get_catalog" {
        emit_catalog(context, sink).await;
        return;
    }
    let Some(operation_id) = command.operation_id else {
        emit_error(sink, "Fusion operation_id is required").await;
        return;
    };
    if command.action == "resolve_strategy" {
        let result = resolve_json(context, command.payload_json.as_deref());
        sink.emit(match result {
            Ok(selection_json) => ClientEvent::FusionStrategyResolved {
                operation_id,
                selection_json: Some(selection_json),
                error: None,
            },
            Err(error) => ClientEvent::FusionStrategyResolved {
                operation_id,
                selection_json: None,
                error: Some(error),
            },
        })
        .await;
        return;
    }
    emit_operation(
        sink,
        Domain::Fusion,
        operation_id,
        Status::Started,
        Effect::NotApplicable,
        None,
        None,
    )
    .await;
    match apply_command(context, &command) {
        Ok(()) => {
            emit_operation(
                sink,
                Domain::Fusion,
                operation_id,
                Status::Succeeded,
                Effect::Applied,
                None,
                None,
            )
            .await;
            emit_catalog(context, sink).await;
        }
        Err(error) => {
            emit_operation(
                sink,
                Domain::Fusion,
                operation_id,
                Status::Failed,
                Effect::NotApplicable,
                Some(error),
                None,
            )
            .await
        }
    }
}

async fn emit_catalog(context: &SettingsContext, sink: &dyn ClientEventSink) {
    match catalog_json(context) {
        Ok(catalog_json) => sink.emit(ClientEvent::FusionCatalog { catalog_json }).await,
        Err(error) => emit_error(sink, error).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_bridge::SettingsPaths;
    use std::sync::{Arc, RwLock};

    fn context(root: &std::path::Path) -> SettingsContext {
        SettingsContext {
            plugin_archive_session: Arc::new(plugin::PluginArchiveSession::new()),
            fusion_layers: None,
            paths: SettingsPaths {
                lingxi_home: root.join("home"),
                project_dir: root.join("project"),
            },
            active: Arc::new(RwLock::new(
                Utf16JsonProjection::object(Vec::new()).unwrap(),
            )),
            managed: Utf16JsonProjection::object(Vec::new()).unwrap(),
            managed_diagnostics: None,
        }
    }

    fn revision(context: &SettingsContext, scope: &str) -> String {
        let catalog: Value = serde_json::from_str(&catalog_json(context).unwrap()).unwrap();
        catalog["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["scope"] == scope)
            .unwrap()["revision"]
            .as_str()
            .unwrap()
            .into()
    }

    fn command(
        context: &SettingsContext,
        action: &str,
        scope: &str,
        target: Option<&str>,
        payload: Value,
    ) -> FusionAdminCommandDto {
        FusionAdminCommandDto {
            action: action.into(),
            scope: Some(scope.into()),
            target: target.map(str::to_owned),
            revision: Some(revision(context, scope)),
            payload_json: Some(payload.to_string()),
            operation_id: Some(1),
        }
    }

    fn save(context: &SettingsContext, scope: &str, id: &str, name: &str) {
        apply_command(
            context,
            &command(
                context,
                "save",
                scope,
                None,
                json!({"id":id,"name":name,"config":{}}),
            ),
        )
        .unwrap();
    }

    #[test]
    fn names_are_unique_per_scope_and_other_keys_survive() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        let path = settings_path(
            WritableScope::User,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        migrations::settings_update::update_settings(
            &path,
            vec![("custom".into(), Some(json!({"keep":true})))],
        )
        .unwrap();
        save(&ctx, "user", "one", "Review");
        save(&ctx, "project", "one", "Review");
        let duplicate = command(
            &ctx,
            "save",
            "user",
            None,
            json!({"id":"two","name":"Review","config":{}}),
        );
        assert!(apply_command(&ctx, &duplicate)
            .unwrap_err()
            .contains("duplicated"));
        assert_eq!(
            read_settings_map(&path).unwrap()["custom"],
            json!({"keep":true})
        );
    }

    #[test]
    fn stale_writes_fail_and_delete_clears_only_own_default() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        save(&ctx, "user", "one", "One");
        let stale = command(
            &ctx,
            "save",
            "user",
            None,
            json!({"id":"two","name":"Two","config":{}}),
        );
        save(&ctx, "user", "three", "Three");
        assert!(apply_command(&ctx, &stale)
            .unwrap_err()
            .contains("revision conflict"));
        apply_command(
            &ctx,
            &command(&ctx, "set_default", "user", Some("one"), json!({})),
        )
        .unwrap();
        save(&ctx, "project", "one", "Project");
        apply_command(
            &ctx,
            &command(&ctx, "set_default", "project", Some("one"), json!({})),
        )
        .unwrap();
        apply_command(
            &ctx,
            &command(&ctx, "delete", "project", Some("one"), json!({})),
        )
        .unwrap();
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(
            catalog["default_strategy"],
            json!({"scope":"user","id":"one"})
        );
        assert_eq!(
            catalog["scopes"][0]["strategies"].as_array().unwrap().len(),
            2
        );
        assert!(catalog["scopes"][1]["default_strategy_id"].is_null());
    }

    #[test]
    fn invalid_singleton_is_diagnosed_and_replaced_only_explicitly() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        let path = settings_path(
            WritableScope::User,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        migrations::settings_update::update_settings(
            &path,
            vec![
                ("fusion".into(), Some(json!({"panelModels":[]}))),
                ("theme".into(), Some(json!("dark"))),
            ],
        )
        .unwrap();
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert!(catalog["scopes"][0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("unsupported"));
        let mut write = command(
            &ctx,
            "save",
            "user",
            None,
            json!({"id":"one","name":"One","config":{}}),
        );
        assert!(apply_command(&ctx, &write).is_err());
        write.payload_json =
            Some(json!({"id":"one","name":"One","config":{},"replace_legacy":true}).to_string());
        apply_command(&ctx, &write).unwrap();
        assert_eq!(read_settings_map(&path).unwrap()["theme"], "dark");
    }

    fn invalid_named_registry() -> Value {
        json!({
            "enabled":true,
            "proactive":true,
            "allowedProfiles":["provider"],
            "defaultStrategyId":"other",
            "strategies":[
                {"id":"bad","name":"Bad","config":{"maxPanel":9}},
                {"id":"other","name":"Other","config":{"panelMaxTurns":2}}
            ]
        })
    }

    #[test]
    fn repairing_named_strategy_preserves_other_records_even_with_replacement_flag() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        let path = settings_path(
            WritableScope::User,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        for replace_legacy in [false, true] {
            let original = invalid_named_registry();
            migrations::settings_update::update_settings(
                &path,
                vec![("fusion".into(), Some(original.clone()))],
            )
            .unwrap();
            let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
            assert!(catalog["scopes"][0]["diagnostic"].is_string());
            assert_eq!(
                catalog["scopes"][0]["strategies"].as_array().unwrap().len(),
                2
            );
            apply_command(&ctx, &command(&ctx, "save", "user", None, json!({
                "id":"bad","name":"Bad","config":{"maxPanel":4},"replace_legacy":replace_legacy
            }))).unwrap();
            let mut expected = original;
            expected["strategies"][0]["config"]["maxPanel"] = json!(4);
            assert_eq!(read_settings_map(&path).unwrap()["fusion"], expected);
        }
    }

    #[test]
    fn deleting_invalid_named_strategy_preserves_other_records_and_controls() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        let path = settings_path(
            WritableScope::User,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        let original = invalid_named_registry();
        migrations::settings_update::update_settings(
            &path,
            vec![("fusion".into(), Some(original.clone()))],
        )
        .unwrap();
        apply_command(
            &ctx,
            &command(&ctx, "delete", "user", Some("bad"), json!({})),
        )
        .unwrap();
        let mut expected = original;
        expected["strategies"].as_array_mut().unwrap().remove(0);
        assert_eq!(read_settings_map(&path).unwrap()["fusion"], expected);
    }

    #[test]
    fn undecodable_named_registry_cannot_be_replaced_or_partially_edited() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        let path = settings_path(
            WritableScope::User,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        for original in [
            json!({"enabled":true,"strategies":[{"id":"bad","name":"Bad","config":{"maxPanel":"nine"}}]}),
            json!({"enabled":true,"strategies":"invalid"}),
            json!({"enabled":true,"strategies":[],"unknown":true}),
        ] {
            migrations::settings_update::update_settings(
                &path,
                vec![("fusion".into(), Some(original.clone()))],
            )
            .unwrap();
            let before = std::fs::read(&path).unwrap();
            for action in ["save", "delete"] {
                let write = command(
                    &ctx,
                    action,
                    "user",
                    Some("bad"),
                    json!({
                        "id":"bad","name":"Bad","config":{"maxPanel":4},"replace_legacy":true
                    }),
                );
                let error = apply_command(&ctx, &write).unwrap_err();
                assert!(
                    error.contains("Repair the named Fusion registry"),
                    "{error}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), before);
            }
        }
    }

    #[test]
    fn named_registry_replacement_cannot_bypass_file_override_restrictions() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        let path = settings_path(
            WritableScope::User,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        let original = json!({"strategies":[{"id":"one","name":"One","config":{}}],"overrides":{"enabled":true}});
        migrations::settings_update::update_settings(
            &path,
            vec![("fusion".into(), Some(original.clone()))],
        )
        .unwrap();
        let write = command(
            &ctx,
            "save",
            "user",
            None,
            json!({"id":"one","name":"One","config":{},"replace_legacy":true}),
        );
        assert!(apply_command(&ctx, &write)
            .unwrap_err()
            .contains("only permitted in CLI and managed settings"));
        assert_eq!(read_settings_map(&path).unwrap()["fusion"], original);
    }

    #[test]
    fn simultaneous_writes_cannot_both_win_same_revision() {
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(context(root.path()));
        save(&ctx, "user", "base", "Base");
        let first = command(
            &ctx,
            "save",
            "user",
            None,
            json!({"id":"a","name":"A","config":{}}),
        );
        let second = command(
            &ctx,
            "save",
            "user",
            None,
            json!({"id":"b","name":"B","config":{}}),
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|cmd| {
                let ctx = Arc::clone(&ctx);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    apply_command(&ctx, &cmd)
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| error.contains("revision conflict")));
    }

    // Invoked in isolated subprocesses by the test below. An ordinary test
    // run has no fixture environment and immediately returns.
    #[test]
    fn cross_process_worker() {
        let Some(root) = std::env::var_os("FUSION_CAS_TEST_ROOT") else {
            return;
        };
        let ctx = context(std::path::Path::new(&root));
        let command: FusionAdminCommandDto =
            serde_json::from_str(&std::env::var("FUSION_CAS_TEST_COMMAND").unwrap()).unwrap();
        match apply_command(&ctx, &command) {
            Ok(()) => println!("FUSION_CAS_RESULT=accepted"),
            Err(error) => {
                assert!(error.contains("revision conflict"), "{error}");
                println!("FUSION_CAS_RESULT=conflict");
            }
        }
    }

    #[test]
    fn independent_processes_cannot_overwrite_the_same_revision() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        save(&ctx, "user", "base", "Base");
        let commands = [
            command(
                &ctx,
                "save",
                "user",
                None,
                json!({"id":"a","name":"A","config":{}}),
            ),
            command(
                &ctx,
                "save",
                "user",
                None,
                json!({"id":"b","name":"B","config":{}}),
            ),
        ];
        let children: Vec<_> = commands
            .into_iter()
            .map(|command| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "fusion_admin::tests::cross_process_worker",
                        "--nocapture",
                    ])
                    .env("FUSION_CAS_TEST_ROOT", root.path())
                    .env(
                        "FUSION_CAS_TEST_COMMAND",
                        serde_json::to_string(&command).unwrap(),
                    )
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let outcomes: Vec<_> = children
            .into_iter()
            .map(|child| {
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success());
                String::from_utf8(output.stdout).unwrap()
            })
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|output| output.contains("FUSION_CAS_RESULT=accepted"))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|output| output.contains("FUSION_CAS_RESULT=conflict"))
                .count(),
            1
        );
    }

    #[test]
    fn resolve_uses_default_precedence_and_managed_limits() {
        let root = tempfile::tempdir().unwrap();
        let mut ctx = context(root.path());
        for scope in ["user", "project", "local"] {
            save(&ctx, scope, "same", scope);
            apply_command(
                &ctx,
                &command(&ctx, "set_default", scope, Some("same"), json!({})),
            )
            .unwrap();
        }
        ctx.managed
            .set_field(
                "fusion",
                Utf16JsonProjection::from_serde_value(
                    &json!({"allowCrossProviderForAgent":false,"overrides":{"panelMaxTurns":2}}),
                )
                .unwrap(),
            )
            .unwrap();
        let selected: Value = serde_json::from_str(&resolve_json(&ctx, None).unwrap()).unwrap();
        assert_eq!(selected["reference"], json!({"scope":"local","id":"same"}));
        assert_eq!(selected["config"]["panelMaxTurns"], 2);
        assert_eq!(selected["config"]["allowCrossProviderForAgent"], false);
        let explicit = resolve_json(
            &ctx,
            Some(r#"{"strategy":{"scope":"user","id":"missing"}}"#),
        );
        assert!(explicit.unwrap_err().contains("no longer exists"));
    }

    #[test]
    fn runtime_admission_and_cli_overrides_match_the_captured_runtime_selection() {
        let root = tempfile::tempdir().unwrap();
        let mut ctx = context(root.path());
        save(&ctx, "project", "excluded", "Excluded");
        apply_command(
            &ctx,
            &command(&ctx, "set_default", "project", Some("excluded"), json!({})),
        )
        .unwrap();
        apply_command(
            &ctx,
            &command(
                &ctx,
                "set_globals",
                "project",
                None,
                json!({"proactive":true}),
            ),
        )
        .unwrap();
        let layers = vec![
            FusionSettingsLayer { source: Source::User, path: None, registry: serde_json::from_value(json!({
                "strategies":[{"id":"admitted","name":"Admitted","config":{"panelMaxTurns":3}}], "defaultStrategyId":"admitted", "enabled":true
            })).unwrap() },
            FusionSettingsLayer { source: Source::Cli, path: None, registry: serde_json::from_value(json!({"overrides":{"panelMaxTurns":2}})).unwrap() },
        ];
        let expected = resolve_fusion_strategy(&layers, None).unwrap();
        ctx.fusion_layers = Some(Arc::new(move || Ok(layers.clone())));
        let actual: lingxi_core::host::fusion_strategy::FusionStrategySelection =
            serde_json::from_str(&resolve_json(&ctx, None).unwrap()).unwrap();
        assert_eq!(actual, expected);
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(
            catalog["default_strategy"],
            json!({"scope":"user","id":"admitted"})
        );
        assert_eq!(catalog["effective_globals"]["enabled"], true);
        assert_eq!(catalog["effective_globals"]["proactive"], false);
        assert_eq!(catalog["scopes"][1]["strategies"][0]["id"], "excluded");
        assert_eq!(catalog["scopes"][0]["admitted"], true);
        assert_eq!(catalog["scopes"][1]["admitted"], false);
        assert_eq!(catalog["scopes"][2]["admitted"], false);
        let project_path = settings_path(
            WritableScope::Project,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        migrations::settings_update::update_settings(
            &project_path,
            vec![("fusion".into(), Some(json!({"panelModels":[]})))],
        )
        .unwrap();
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert!(catalog["scopes"][1]["diagnostic"].is_string());
        assert_eq!(catalog["scopes"][1]["admitted"], false);
        assert_eq!(catalog["diagnostics"], json!([]));
        assert_eq!(
            catalog["default_strategy"],
            json!({"scope":"user","id":"admitted"})
        );
    }

    #[test]
    fn broken_excluded_settings_do_not_hide_valid_strategies_or_allow_replacement() {
        let root = tempfile::tempdir().unwrap();
        let mut ctx = context(root.path());
        save(&ctx, "user", "review", "Review");
        apply_command(
            &ctx,
            &command(&ctx, "set_default", "user", Some("review"), json!({})),
        )
        .unwrap();
        let admitted = file_layers(&ctx).unwrap();
        let layers = admitted
            .into_iter()
            .filter(|layer| layer.source == Source::User)
            .collect::<Vec<_>>();
        ctx.fusion_layers = Some(Arc::new(move || Ok(layers.clone())));
        let project_path = settings_path(
            WritableScope::Project,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        let broken = "{ invalid json";
        std::fs::write(&project_path, broken).unwrap();
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(catalog["scopes"][0]["strategies"][0]["id"], "review");
        assert_eq!(
            catalog["default_strategy"],
            json!({"scope":"user","id":"review"})
        );
        assert_eq!(catalog["diagnostics"], json!([]));
        assert_eq!(catalog["scopes"][1]["admitted"], false);
        assert!(catalog["scopes"][1]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("Invalid JSON"));
        assert!(resolve_json(&ctx, None).is_ok());
        ctx.fusion_layers = None;
        let fallback: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(fallback["default_strategy"], catalog["default_strategy"]);
        assert_eq!(fallback["scopes"][1]["admitted"], false);
        assert!(resolve_json(&ctx, None).is_ok());
        let replace = command(
            &ctx,
            "save",
            "project",
            None,
            json!({"id":"new","name":"New","config":{},"replace_legacy":true}),
        );
        assert!(apply_command(&ctx, &replace)
            .unwrap_err()
            .contains("Invalid JSON"));
        assert_eq!(std::fs::read_to_string(&project_path).unwrap(), broken);
    }

    #[test]
    fn unreadable_excluded_scope_has_a_local_diagnostic_and_cannot_be_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let mut ctx = context(root.path());
        save(&ctx, "user", "review", "Review");
        apply_command(
            &ctx,
            &command(&ctx, "set_default", "user", Some("review"), json!({})),
        )
        .unwrap();
        let layers = file_layers(&ctx)
            .unwrap()
            .into_iter()
            .filter(|layer| layer.source == Source::User)
            .collect::<Vec<_>>();
        ctx.fusion_layers = Some(Arc::new(move || Ok(layers.clone())));
        let project_path = settings_path(
            WritableScope::Project,
            &ctx.paths.lingxi_home,
            &ctx.paths.project_dir,
        );
        // A directory deterministically fails both settings and byte reads,
        // including when the test process can bypass file permission bits.
        std::fs::create_dir_all(&project_path).unwrap();
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(catalog["scopes"][0]["strategies"][0]["id"], "review");
        assert_eq!(
            catalog["default_strategy"],
            json!({"scope":"user","id":"review"})
        );
        assert_eq!(catalog["diagnostics"], json!([]));
        let excluded = &catalog["scopes"][1];
        assert_eq!(excluded["admitted"], false);
        assert!(excluded["diagnostic"]
            .as_str()
            .unwrap()
            .contains("Failed to read"));
        assert!(excluded["revision"]
            .as_str()
            .unwrap()
            .starts_with("unavailable:"));
        assert_eq!(revision(&ctx, "project"), excluded["revision"]);
        assert!(resolve_json(&ctx, None).is_ok());
        let replace = command(
            &ctx,
            "save",
            "project",
            None,
            json!({"id":"new","name":"New","config":{},"replace_legacy":true}),
        );
        assert!(apply_command(&ctx, &replace)
            .unwrap_err()
            .contains("Failed to read"));
        assert!(project_path.is_dir());
        ctx.fusion_layers = None;
        let fallback: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(fallback["default_strategy"], catalog["default_strategy"]);
        assert_eq!(fallback["scopes"][1]["admitted"], false);
        assert!(resolve_json(&ctx, None).is_ok());
    }

    #[test]
    fn globals_can_return_to_inherited_values_without_altering_strategies() {
        let root = tempfile::tempdir().unwrap();
        let ctx = context(root.path());
        save(&ctx, "user", "one", "One");
        apply_command(
            &ctx,
            &command(&ctx, "set_globals", "user", None, json!({"enabled":true})),
        )
        .unwrap();
        apply_command(&ctx, &command(&ctx, "set_globals", "user", None, json!({}))).unwrap();
        let catalog: Value = serde_json::from_str(&catalog_json(&ctx).unwrap()).unwrap();
        assert_eq!(catalog["scopes"][0]["globals"], json!({}));
        assert_eq!(
            catalog["scopes"][0]["strategies"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            catalog["effective_globals"]["slash_cross_provider_default"],
            true
        );
    }
}
