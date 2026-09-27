use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use plugin::discover_recorded_plugins;
use plugin::plugin_source_sha256;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::settings_bridge::{apply_patch, build_snapshot, SettingsContext};
use client_protocol::commands::WritableScopeDto;

#[derive(Debug, Serialize)]
struct InstalledPluginRow {
    id: String,
    name: String,
    display_name: String,
    version: String,
    path: String,
    default_enabled: bool,
    description: String,
    dependencies: Vec<String>,
    config_schema_json: String,
    secret_configured: BTreeMap<String, bool>,
}

#[derive(Debug, Serialize)]
struct AvailablePluginRow {
    id: String,
    name: String,
    marketplace: String,
    version: String,
    description: String,
    installed: bool,
    upgrade_available: bool,
}

#[derive(Debug, Serialize)]
struct MarketplaceRow {
    name: String,
    source_json: String,
    install_location: Option<String>,
    last_updated: Option<String>,
}

#[derive(Debug, Serialize)]
struct PluginCatalog {
    installed: Vec<InstalledPluginRow>,
    available: Vec<AvailablePluginRow>,
    marketplaces: Vec<MarketplaceRow>,
    enabled_json: String,
    configs_json: String,
    marketplaces_json: String,
    policies_json: String,
    revisions: BTreeMap<String, String>,
}

pub async fn catalog_json(
    context: &SettingsContext,
    credentials: Option<&secret::CredentialManager>,
) -> Result<String, String> {
    let snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let plugins_dir = context.paths.lingxi_home.join("plugins");
    let discovered = discover_recorded_plugins(&plugins_dir).await;
    let mut installed_versions = BTreeMap::new();
    let mut installed = Vec::new();
    for (_runtime_id, manifest, path) in discovered {
        let id = installed_plugin_identity(&manifest, &path);
        installed_versions.insert(id.clone(), manifest.version.clone());
        installed_versions.insert(manifest.name.clone(), manifest.version.clone());
        let mut secret_configured = BTreeMap::new();
        if let (Some(credentials), Some(schema)) = (credentials, manifest.user_config.as_ref()) {
            for (key, field) in &schema.fields {
                if field.sensitive {
                    let configured = credentials
                        .get_plugin_secret(&id, key)
                        .await
                        .ok()
                        .flatten()
                        .is_some();
                    secret_configured.insert(key.clone(), configured);
                }
            }
        }
        let owner_marketplace = id.split_once('@').map_or("", |(_, owner)| owner);
        let dependencies = manifest
            .dependencies
            .iter()
            .map(|dependency| dependency.resolved_id(owner_marketplace))
            .collect();
        installed.push(InstalledPluginRow {
            id,
            name: manifest.name.clone(),
            display_name: manifest
                .display_name
                .clone()
                .unwrap_or_else(|| manifest.name.clone()),
            version: manifest.version.clone(),
            path: path.to_string_lossy().into_owned(),
            default_enabled: manifest.default_enabled,
            description: manifest.description.clone(),
            dependencies,
            config_schema_json: serde_json::to_string(&manifest.user_config)
                .map_err(|error| error.to_string())?,
            secret_configured,
        });
    }
    installed.sort_by(|left, right| left.id.cmp(&right.id));
    let (available, marketplace_rows) = available_plugins(&plugins_dir, &installed_versions)?;
    let enabled = snapshot
        .effective
        .get("enabledPlugins")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let configs = snapshot
        .effective
        .get("pluginConfigs")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let marketplaces = snapshot
        .effective
        .get("extraKnownMarketplaces")
        .cloned()
        .or_else(|| snapshot.effective.get("additionalMarketplaces").cloned())
        .unwrap_or(Value::Object(Map::new()));
    let policies = json!({
        "strictKnownMarketplaces": snapshot.effective.get("strictKnownMarketplaces"),
        "allowedMarketplaces": snapshot.effective.get("allowedMarketplaces"),
        "blockedMarketplaces": snapshot.effective.get("blockedMarketplaces"),
    });
    let catalog = PluginCatalog {
        installed,
        available,
        marketplaces: marketplace_rows,
        enabled_json: serde_json::to_string_pretty(&enabled).map_err(|error| error.to_string())?,
        configs_json: serde_json::to_string_pretty(&configs).map_err(|error| error.to_string())?,
        marketplaces_json: serde_json::to_string_pretty(&marketplaces)
            .map_err(|error| error.to_string())?,
        policies_json: serde_json::to_string_pretty(&policies)
            .map_err(|error| error.to_string())?,
        revisions: BTreeMap::from([
            (
                "user".to_string(),
                plugin_source_sha256(
                    serde_json::to_string(snapshot.layers.get("user").unwrap_or(&Map::new()))
                        .map_err(|error| error.to_string())?
                        .as_bytes(),
                ),
            ),
            (
                "project".to_string(),
                plugin_source_sha256(
                    serde_json::to_string(snapshot.layers.get("project").unwrap_or(&Map::new()))
                        .map_err(|error| error.to_string())?
                        .as_bytes(),
                ),
            ),
            (
                "local".to_string(),
                plugin_source_sha256(
                    serde_json::to_string(snapshot.layers.get("local").unwrap_or(&Map::new()))
                        .map_err(|error| error.to_string())?
                        .as_bytes(),
                ),
            ),
        ]),
    };
    serde_json::to_string(&catalog).map_err(|error| error.to_string())
}

pub async fn preview_operation(
    context: Option<&SettingsContext>,
    revision_sha256: Option<&str>,
    payload_json: &str,
) -> Result<String, String> {
    let payload = parse_payload(payload_json)?;
    let action = required_string(&payload, "action")?;
    let target = payload
        .get("plugin")
        .or_else(|| payload.get("name"))
        .or_else(|| payload.get("source"))
        .and_then(Value::as_str)
        .unwrap_or("configuration");
    let requires_confirmation = matches!(
        action,
        "install"
            | "uninstall"
            | "update"
            | "marketplace_add"
            | "marketplace_remove"
            | "marketplace_update"
    );
    if !matches!(
        action,
        "enable"
            | "disable"
            | "install"
            | "uninstall"
            | "update"
            | "marketplace_add"
            | "marketplace_remove"
            | "marketplace_update"
            | "save_config"
    ) {
        return Err(format!("unsupported plugin operation `{action}`"));
    }
    if action == "save_config" {
        let context = context.ok_or_else(|| {
            "plugin config preview unavailable: missing settings context".to_string()
        })?;
        let scope = required_string(&payload, "scope")?;
        if let Some(revision_sha256) = revision_sha256 {
            ensure_scope_revision(context, scope, revision_sha256)?;
        }
        validate_config_payload(context, &payload).await?;
    }
    serde_json::to_string(&json!({
        "action": action,
        "target": target,
        "requiresConfirmation": requires_confirmation,
        "summary": format!("{action} {target}"),
    }))
    .map_err(|error| error.to_string())
}

pub async fn apply_operation(
    context: &SettingsContext,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<String, String> {
    let payload = parse_payload(payload_json)?;
    let action = required_string(&payload, "action")?;
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();
    ensure_scope_revision(context, &scope, revision_sha256)?;
    let guarded = matches!(
        action,
        "install"
            | "uninstall"
            | "update"
            | "marketplace_add"
            | "marketplace_remove"
            | "marketplace_update"
    );
    if guarded && payload.get("confirmed").and_then(Value::as_bool) != Some(true) {
        return Err("plugin operation requires confirmed:true after preflight".to_string());
    }

    let home = context.paths.lingxi_home.clone();
    let cwd = context.paths.project_dir.clone();
    let plugins_dir = home.join("plugins");
    match action {
        "enable" => {
            let plugin = required_string(&payload, "plugin")?;
            crate::plugin_settings::run_enable(plugin, Some(&scope), &home, &cwd)
        }
        "disable" => {
            let plugin = required_string(&payload, "plugin")?;
            crate::plugin_settings::run_disable(Some(plugin), Some(&scope), false, &home, &cwd)
        }
        "install" => {
            let plugin = required_string(&payload, "plugin")?.to_string();
            if payload
                .get("config")
                .and_then(Value::as_array)
                .is_some_and(|values| !values.is_empty())
            {
                return Err(
                    "install config must be saved through pluginConfigs and the credential broker"
                        .to_string(),
                );
            }
            tokio::task::spawn_blocking(move || {
                crate::plugin_install::run_install(
                    &plugin,
                    Some(&scope),
                    &[],
                    &plugins_dir,
                    &home,
                    &cwd,
                )
            })
            .await
            .map_err(|error| format!("plugin install task failed: {error}"))?
        }
        "uninstall" => {
            let plugin = required_string(&payload, "plugin")?.to_string();
            let keep_data = payload
                .get("keepData")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let prune = payload
                .get("prune")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            tokio::task::spawn_blocking(move || {
                crate::plugin_install::run_uninstall(
                    &plugin,
                    Some(&scope),
                    keep_data,
                    prune,
                    true,
                    &plugins_dir,
                    &home,
                    &cwd,
                )
            })
            .await
            .map_err(|error| format!("plugin uninstall task failed: {error}"))?
        }
        "update" => {
            let plugin = required_string(&payload, "plugin")?.to_string();
            tokio::task::spawn_blocking(move || {
                crate::plugin_install::run_update(&plugin, &scope, &plugins_dir, &home, &cwd)
            })
            .await
            .map_err(|error| format!("plugin update task failed: {error}"))?
        }
        "marketplace_add" | "marketplace_remove" | "marketplace_update" => {
            let action = action.to_string();
            let payload = payload.clone();
            tokio::task::spawn_blocking(move || match action.as_str() {
                "marketplace_add" => {
                    let source = required_string(&payload, "source")?;
                    let sparse = payload
                        .get("sparse")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>();
                    crate::plugin_marketplace::run_add(
                        source,
                        Some(&scope),
                        &sparse,
                        &plugins_dir,
                        &home,
                        &cwd,
                    )
                }
                "marketplace_remove" => crate::plugin_marketplace::run_remove(
                    required_string(&payload, "name")?,
                    Some(&scope),
                    &plugins_dir,
                    &home,
                    &cwd,
                ),
                "marketplace_update" => crate::plugin_marketplace::run_update(
                    payload.get("name").and_then(Value::as_str),
                    &plugins_dir,
                    &home,
                    &cwd,
                ),
                _ => unreachable!(),
            })
            .await
            .map_err(|error| format!("plugin operation task failed: {error}"))?
        }
        other => Err(format!("unsupported plugin operation `{other}`")),
    }
}

pub async fn save_config(
    context: &SettingsContext,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<(), String> {
    let payload = parse_payload(payload_json)?;
    let scope = required_string(&payload, "scope")?;
    let destination = destination(scope)?;
    let snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let current = Value::Object(snapshot.layers.get(scope).cloned().unwrap_or_default());
    let current_json = serde_json::to_string(&current).map_err(|error| error.to_string())?;
    let actual = plugin_source_sha256(current_json.as_bytes());
    if actual != revision_sha256 {
        return Err(format!(
            "revision conflict: expected {revision_sha256}, found {actual}"
        ));
    }
    let enabled_plugins = payload.get("enabledPlugins").cloned();
    let plugin_configs = payload.get("pluginConfigs").cloned();
    let extra_known_marketplaces = payload
        .get("extraKnownMarketplaces")
        .cloned()
        .or_else(|| payload.get("additionalMarketplaces").cloned());
    validate_config_payload(context, &payload).await?;
    let mut patch = Vec::new();
    if let Some(value) = enabled_plugins {
        patch.push(("enabledPlugins".to_string(), Some(value)));
    }
    if let Some(value) = plugin_configs {
        patch.push(("pluginConfigs".to_string(), Some(value)));
    }
    if let Some(value) = extra_known_marketplaces {
        patch.push(("extraKnownMarketplaces".to_string(), Some(value)));
    }
    apply_patch(&context.paths, destination, patch)
}

async fn validate_config_payload(
    context: &SettingsContext,
    payload: &Map<String, Value>,
) -> Result<(), String> {
    for key in ["enabledPlugins", "pluginConfigs", "extraKnownMarketplaces"] {
        if payload.get(key).is_some_and(|value| !value.is_object()) {
            return Err(format!("`{key}` must be a JSON object"));
        }
    }
    let Some(configs) = payload.get("pluginConfigs").and_then(Value::as_object) else {
        return Ok(());
    };
    for (plugin_id, value) in configs {
        let entry = value
            .as_object()
            .ok_or_else(|| format!("pluginConfigs.{plugin_id} must be an object"))?;
        if entry
            .keys()
            .any(|key| key != "options" && key != "mcpServers")
        {
            return Err(format!(
                "pluginConfigs.{plugin_id} only accepts `options` and `mcpServers`"
            ));
        }
        if entry.get("options").is_some_and(|value| !value.is_object()) {
            return Err(format!(
                "pluginConfigs.{plugin_id}.options must be an object"
            ));
        }
        if let Some(servers) = entry.get("mcpServers").and_then(Value::as_object) {
            if servers.values().any(|value| !value.is_object()) {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.mcpServers values must be objects"
                ));
            }
        } else if entry.get("mcpServers").is_some() {
            return Err(format!(
                "pluginConfigs.{plugin_id}.mcpServers must be an object"
            ));
        }
    }

    let plugins_dir = context.paths.lingxi_home.join("plugins");
    for (_runtime_id, manifest, path) in discover_recorded_plugins(&plugins_dir).await {
        let plugin_id = installed_plugin_identity(&manifest, &path);
        let Some(entry) = configs.get(&plugin_id).and_then(Value::as_object) else {
            continue;
        };
        let options = entry
            .get("options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let Some(schema) = manifest.user_config.as_ref() else {
            if !options.is_empty() {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.options contains values, but the plugin declares no userConfig schema"
                ));
            }
            continue;
        };
        for (key, value) in &options {
            let field = schema.fields.get(key).ok_or_else(|| {
                format!("pluginConfigs.{plugin_id}.options.{key} is not declared by the plugin")
            })?;
            if field.sensitive {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.options.{key} is sensitive and must be saved through the Credential Broker"
                ));
            }
            validate_user_config_value(&plugin_id, key, field, value)?;
        }
        for (key, field) in &schema.fields {
            if !field.sensitive
                && field.required
                && field.default.is_none()
                && !options.contains_key(key)
            {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.options.{key} is required"
                ));
            }
        }
    }
    Ok(())
}

fn installed_plugin_identity(manifest: &plugin::PluginManifest, install_dir: &Path) -> String {
    let marketplace = install_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .filter(|_| {
            install_dir
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                == Some("cache")
        });
    marketplace.map_or_else(
        || manifest.name.clone(),
        |marketplace| format!("{}@{marketplace}", manifest.name),
    )
}

fn validate_user_config_value(
    plugin_id: &str,
    key: &str,
    field: &plugin::UserConfigField,
    value: &Value,
) -> Result<(), String> {
    let valid_scalar = |value: &Value| match field.value_type.as_deref().unwrap_or("string") {
        "string" | "directory" | "file" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        _ => false,
    };
    let valid = if field.multiple == Some(true) {
        value
            .as_array()
            .is_some_and(|values| values.iter().all(valid_scalar))
    } else {
        valid_scalar(value)
    };
    if !valid {
        let expected = field.value_type.as_deref().unwrap_or("string");
        let expected = if field.multiple == Some(true) {
            format!("an array of {expected} values")
        } else {
            format!("a {expected} value")
        };
        return Err(format!(
            "pluginConfigs.{plugin_id}.options.{key} must be {expected}"
        ));
    }
    let numbers: Vec<f64> = if field.multiple == Some(true) {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_f64)
            .collect()
    } else {
        value.as_f64().into_iter().collect()
    };
    if let Some(minimum) = field.min {
        if numbers.iter().any(|number| *number < minimum) {
            return Err(format!(
                "pluginConfigs.{plugin_id}.options.{key} must be at least {minimum}"
            ));
        }
    }
    if let Some(maximum) = field.max {
        if numbers.iter().any(|number| *number > maximum) {
            return Err(format!(
                "pluginConfigs.{plugin_id}.options.{key} must be at most {maximum}"
            ));
        }
    }
    Ok(())
}

fn parse_payload(payload_json: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(payload_json) {
        Ok(Value::Object(payload)) => Ok(payload),
        Ok(_) => Err("payload must be a JSON object".to_string()),
        Err(error) => Err(format!("payload is not valid JSON: {error}")),
    }
}

fn required_string<'a>(payload: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("payload is missing `{key}`"))
}

fn destination(scope: &str) -> Result<WritableScopeDto, String> {
    match scope {
        "user" => Ok(WritableScopeDto::User),
        "project" => Ok(WritableScopeDto::Project),
        "local" => Ok(WritableScopeDto::Local),
        _ => Err(format!("unknown plugin scope `{scope}`")),
    }
}

fn ensure_scope_revision(
    context: &SettingsContext,
    scope: &str,
    expected: &str,
) -> Result<(), String> {
    let snapshot = build_snapshot(
        &context.paths,
        context.active_snapshot(),
        context.managed.clone(),
    );
    let current = Value::Object(snapshot.layers.get(scope).cloned().unwrap_or_default());
    let actual = plugin_source_sha256(
        serde_json::to_string(&current)
            .map_err(|error| error.to_string())?
            .as_bytes(),
    );
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "revision conflict: expected {expected}, found {actual}"
        ))
    }
}

fn available_plugins(
    plugins_dir: &Path,
    installed_versions: &BTreeMap<String, String>,
) -> Result<(Vec<AvailablePluginRow>, Vec<MarketplaceRow>), String> {
    let registry_path = plugins_dir.join("known_marketplaces.json");
    let registry = match std::fs::read_to_string(&registry_path) {
        Ok(raw) => serde_json::from_str::<Map<String, Value>>(&raw)
            .map_err(|error| format!("{} is invalid: {error}", registry_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(error) => {
            return Err(format!(
                "failed to read {}: {error}",
                registry_path.display()
            ))
        }
    };
    let mut available = Vec::new();
    let mut marketplaces = Vec::new();
    let mut seen = BTreeSet::new();
    for (marketplace, entry) in registry {
        let install_location = entry
            .get("installLocation")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        marketplaces.push(MarketplaceRow {
            name: marketplace.clone(),
            source_json: serde_json::to_string(entry.get("source").unwrap_or(&Value::Null))
                .unwrap_or_else(|_| "null".to_string()),
            install_location: install_location.clone(),
            last_updated: entry
                .get("lastUpdated")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        });
        let Some(root) = install_location.map(PathBuf::from) else {
            continue;
        };
        let manifest_path = root
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("marketplace.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        for plugin in manifest
            .get("plugins")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = plugin.get("name").and_then(Value::as_str) else {
                continue;
            };
            let id = format!("{name}@{marketplace}");
            if !seen.insert(id.clone()) {
                continue;
            }
            let version = plugin
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let installed_version = installed_versions
                .get(&id)
                .or_else(|| installed_versions.get(name));
            available.push(AvailablePluginRow {
                id,
                name: name.to_string(),
                marketplace: marketplace.clone(),
                description: plugin
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                upgrade_available: installed_version
                    .is_some_and(|installed| !version.is_empty() && installed != &version),
                installed: installed_version.is_some(),
                version,
            });
        }
    }
    available.sort_by(|left, right| left.id.cmp(&right.id));
    marketplaces.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((available, marketplaces))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_bridge::SettingsPaths;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Arc, RwLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn context() -> (SettingsContext, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "lingxi-plugin-admin-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let home = root.join("home");
        let project = root.join("project");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&project).expect("project");
        (
            SettingsContext {
                paths: SettingsPaths {
                    lingxi_home: home,
                    project_dir: project,
                },
                active: Arc::new(RwLock::new(BTreeMap::new())),
                managed: BTreeMap::new(),
            },
            root,
        )
    }

    fn install_schema_fixture(context: &SettingsContext) {
        let plugins = context.paths.lingxi_home.join("plugins");
        let install = plugins.join("cache/acme/secure/1.0.0");
        fs::create_dir_all(install.join(branding::PLUGIN_MANIFEST_DIR)).expect("manifest dir");
        fs::write(
            install
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"secure",
                "version":"1.0.0",
                "description":"fixture",
                "userConfig":{
                    "TOKEN":{"type":"string","title":"Token","description":"secret","sensitive":true},
                    "COUNT":{"type":"number","title":"Count","description":"count","required":true,"min":1,"max":5}
                }
            }"#,
        )
        .expect("manifest");
        fs::write(
            plugins.join("installed_plugins.json"),
            serde_json::to_vec(&json!({
                "version": 2,
                "plugins": {
                    "secure@acme": [{
                        "scope": "user",
                        "installPath": install,
                        "version": "1.0.0"
                    }]
                }
            }))
            .expect("registry json"),
        )
        .expect("registry");
    }

    #[tokio::test]
    async fn config_validation_rejects_sensitive_plaintext_and_checks_declared_types() {
        let (context, root) = context();
        install_schema_fixture(&context);

        let sensitive = parse_payload(
            r#"{"scope":"user","pluginConfigs":{"secure@acme":{"options":{"TOKEN":"plaintext"}}}}"#,
        )
        .expect("payload");
        let error = validate_config_payload(&context, &sensitive)
            .await
            .expect_err("sensitive plaintext must fail");
        assert!(error.contains("Credential Broker"));

        let wrong_type = parse_payload(
            r#"{"scope":"user","pluginConfigs":{"secure@acme":{"options":{"COUNT":"many"}}}}"#,
        )
        .expect("payload");
        let error = validate_config_payload(&context, &wrong_type)
            .await
            .expect_err("schema type must fail");
        assert!(error.contains("must be a number value"));

        let missing =
            parse_payload(r#"{"scope":"user","pluginConfigs":{"secure@acme":{"options":{}}}}"#)
                .expect("payload");
        let error = validate_config_payload(&context, &missing)
            .await
            .expect_err("required values must fail");
        assert!(error.contains("is required"));

        let out_of_range = parse_payload(
            r#"{"scope":"user","pluginConfigs":{"secure@acme":{"options":{"COUNT":9}}}}"#,
        )
        .expect("payload");
        let error = validate_config_payload(&context, &out_of_range)
            .await
            .expect_err("numeric bounds must fail");
        assert!(error.contains("at most 5"));

        let valid = parse_payload(
            r#"{"scope":"user","pluginConfigs":{"secure@acme":{"options":{"COUNT":3}}}}"#,
        )
        .expect("payload");
        validate_config_payload(&context, &valid)
            .await
            .expect("valid config");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn destructive_preview_requires_a_separate_confirmed_apply() {
        let preview = preview_operation(
            None,
            None,
            r#"{"action":"uninstall","plugin":"secure@acme"}"#,
        )
        .await
        .expect("preview");
        let preview: Value = serde_json::from_str(&preview).expect("preview json");
        assert_eq!(preview["requiresConfirmation"], true);

        let (context, root) = context();
        let revision = plugin_source_sha256(b"{}");
        let error = apply_operation(
            &context,
            &revision,
            r#"{"action":"uninstall","scope":"user","plugin":"secure@acme"}"#,
        )
        .await
        .expect_err("apply needs confirmation");
        assert!(error.contains("confirmed:true"));
        let _ = fs::remove_dir_all(root);
    }
}
