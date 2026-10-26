use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use plugin::discover_recorded_plugins;
use plugin::plugin_source_sha256;
use serde::Serialize;

use crate::settings_bridge::{apply_patch_source, settings_error_message, SettingsContext};
use client::protocol::commands::WritableScopeDto;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

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
    let snapshot = context
        .snapshot()
        .map_err(|error| settings_error_message(&error))?;
    let plugins_dir = context.paths.lingxi_home.join("plugins");
    let discovered = discover_recorded_plugins(&plugins_dir, &context.plugin_archive_session).await;
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
    let empty_object = source_object()?;
    let null = source_null()?;
    let enabled = source_member(&snapshot.effective_source, "enabledPlugins")
        .unwrap_or_else(|| empty_object.clone());
    let configs = source_member(&snapshot.effective_source, "pluginConfigs")
        .unwrap_or_else(|| empty_object.clone());
    let marketplaces = source_member(&snapshot.effective_source, "extraKnownMarketplaces")
        .or_else(|| source_member(&snapshot.effective_source, "additionalMarketplaces"))
        .unwrap_or(empty_object);
    let policies = Utf16JsonProjection::object(vec![
        (
            "strictKnownMarketplaces".to_string(),
            source_member(&snapshot.effective_source, "strictKnownMarketplaces")
                .unwrap_or_else(|| null.clone()),
        ),
        (
            "allowedMarketplaces".to_string(),
            source_member(&snapshot.effective_source, "allowedMarketplaces")
                .unwrap_or_else(|| null.clone()),
        ),
        (
            "blockedMarketplaces".to_string(),
            source_member(&snapshot.effective_source, "blockedMarketplaces").unwrap_or(null),
        ),
    ])
    .map_err(|error| error.to_string())?;
    let catalog = PluginCatalog {
        installed,
        available,
        marketplaces: marketplace_rows,
        enabled_json: enabled
            .to_json_string()
            .map_err(|error| error.to_string())?,
        configs_json: configs
            .to_json_string()
            .map_err(|error| error.to_string())?,
        marketplaces_json: marketplaces
            .to_json_string()
            .map_err(|error| error.to_string())?,
        policies_json: policies
            .to_json_string()
            .map_err(|error| error.to_string())?,
        revisions: BTreeMap::from([
            (
                "user".to_string(),
                layer_revision(&snapshot.layers, "user")?,
            ),
            (
                "project".to_string(),
                layer_revision(&snapshot.layers, "project")?,
            ),
            (
                "local".to_string(),
                layer_revision(&snapshot.layers, "local")?,
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
    let target_projection = ["plugin", "name", "source"]
        .into_iter()
        .filter_map(|key| source_member(&payload, key))
        .find(|value| value.string_at("").is_some())
        .unwrap_or(source_string("configuration")?);
    let target = target_projection
        .string_at("")
        .unwrap_or("configuration")
        .to_owned();
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
    let response = Utf16JsonProjection::object(vec![
        ("action".to_string(), source_string(action)?),
        ("target".to_string(), target_projection),
        (
            "requiresConfirmation".to_string(),
            source_bool(requires_confirmation)?,
        ),
        (
            "summary".to_string(),
            source_string(&format!("{action} {target}"))?,
        ),
    ])
    .map_err(|error| error.to_string())?;
    response.to_json_string().map_err(|error| error.to_string())
}

pub async fn apply_operation(
    context: &SettingsContext,
    revision_sha256: &str,
    payload_json: &str,
) -> Result<String, String> {
    let payload = parse_payload(payload_json)?;
    let action = required_string(&payload, "action")?;
    let scope = payload
        .get_property("scope")
        .and_then(JsonValueView::as_str)
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
    if guarded
        && payload
            .get_property("confirmed")
            .and_then(JsonValueView::as_bool)
            != Some(true)
    {
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
                .get_property("config")
                .and_then(JsonValueView::array_len)
                .is_some_and(|length| length != 0)
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
                .get_property("keepData")
                .and_then(JsonValueView::as_bool)
                .unwrap_or(false);
            let prune = payload
                .get_property("prune")
                .and_then(JsonValueView::as_bool)
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
                        .get_property("sparse")
                        .and_then(JsonValueView::array_len)
                        .into_iter()
                        .flat_map(|length| 0..length)
                        .filter_map(|index| {
                            payload
                                .get_property("sparse")?
                                .get_index(index)?
                                .as_str()
                                .map(ToOwned::to_owned)
                        })
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
                    payload.get_property("name").and_then(JsonValueView::as_str),
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
    let snapshot = context
        .snapshot()
        .map_err(|error| settings_error_message(&error))?;
    check_scope_revision(&snapshot.layers, scope, revision_sha256)?;
    validate_config_payload(context, &payload).await?;
    let mut patch = Vec::new();
    if let Some(value) = source_member(&payload, "enabledPlugins") {
        patch.push(("enabledPlugins".to_string(), Some(value)));
    }
    if let Some(value) = source_member(&payload, "pluginConfigs") {
        patch.push(("pluginConfigs".to_string(), Some(value)));
    }
    if let Some(value) = source_member(&payload, "extraKnownMarketplaces")
        .or_else(|| source_member(&payload, "additionalMarketplaces"))
    {
        patch.push(("extraKnownMarketplaces".to_string(), Some(value)));
    }
    apply_patch_source(&context.paths, destination, patch)
}

async fn validate_config_payload(
    context: &SettingsContext,
    payload: &Utf16JsonProjection,
) -> Result<(), String> {
    for key in ["enabledPlugins", "pluginConfigs", "extraKnownMarketplaces"] {
        if let Some(value) = payload.get_property(key) {
            if value.object_entries().is_none() {
                return Err(format!("`{key}` must be a JSON object"));
            }
        }
    }
    let Some(configs) = source_member(payload, "pluginConfigs") else {
        return Ok(());
    };
    for plugin_entry in configs
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .ok_or_else(|| "`pluginConfigs` must be a JSON object".to_string())?
    {
        let plugin_id = String::from_utf16(plugin_entry.code_units)
            .unwrap_or_else(|_| plugin_entry.display_key.to_string());
        let entry = configs
            .subprojection_property_units(plugin_entry.code_units)
            .map_err(|error| error.to_string())?;
        let entry_members = entry
            .at_pointer("")
            .and_then(JsonValueView::object_entries)
            .ok_or_else(|| format!("pluginConfigs.{plugin_id} must be an object"))?;
        if entry_members.iter().any(|member| {
            !matches!(
                String::from_utf16(member.code_units).ok().as_deref(),
                Some("options" | "mcpServers")
            )
        }) {
            return Err(format!(
                "pluginConfigs.{plugin_id} only accepts `options` and `mcpServers`"
            ));
        }
        if let Some(options) = entry.get_property("options") {
            if options.object_entries().is_none() {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.options must be an object"
                ));
            }
        }
        if let Some(servers) = entry.get_property("mcpServers") {
            let entries = servers
                .object_entries()
                .ok_or_else(|| format!("pluginConfigs.{plugin_id}.mcpServers must be an object"))?;
            if entries
                .iter()
                .any(|entry| entry.value.object_entries().is_none())
            {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.mcpServers values must be objects"
                ));
            }
        }
    }

    let plugins_dir = context.paths.lingxi_home.join("plugins");
    for (_runtime_id, manifest, path) in
        discover_recorded_plugins(&plugins_dir, &context.plugin_archive_session).await
    {
        let plugin_id = installed_plugin_identity(&manifest, &path);
        let Some(entry) = source_member(&configs, &plugin_id) else {
            continue;
        };
        let options = source_member(&entry, "options").unwrap_or(source_object()?);
        let Some(schema) = manifest.user_config.as_ref() else {
            if options
                .at_pointer("")
                .and_then(JsonValueView::object_entries)
                .is_some_and(|entries| !entries.is_empty())
            {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.options contains values, but the plugin declares no userConfig schema"
                ));
            }
            continue;
        };
        for option in options
            .at_pointer("")
            .and_then(JsonValueView::object_entries)
            .ok_or_else(|| format!("pluginConfigs.{plugin_id}.options must be an object"))?
        {
            let key = String::from_utf16(option.code_units)
                .unwrap_or_else(|_| option.display_key.to_string());
            let field = schema.fields.get(&key).ok_or_else(|| {
                format!("pluginConfigs.{plugin_id}.options.{key} is not declared by the plugin")
            })?;
            if field.sensitive {
                return Err(format!(
                    "pluginConfigs.{plugin_id}.options.{key} is sensitive and must be saved through the Credential Broker"
                ));
            }
            validate_user_config_value(&plugin_id, &key, field, option.value)?;
        }
        for (key, field) in &schema.fields {
            if !field.sensitive
                && field.required
                && field.default.is_none()
                && !options.contains_property(key)
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
    value: JsonValueView<'_>,
) -> Result<(), String> {
    let valid_scalar =
        |value: JsonValueView<'_>| match field.value_type.as_deref().unwrap_or("string") {
            "string" | "directory" | "file" => value.as_str().is_some(),
            "boolean" => value.as_bool().is_some(),
            "number" => value.as_number().is_some(),
            _ => false,
        };
    let values = if field.multiple == Some(true) {
        let Some(length) = value.array_len() else {
            return invalid_config_type(plugin_id, key, field);
        };
        (0..length)
            .filter_map(|index| value.get_index(index))
            .collect::<Vec<_>>()
    } else {
        vec![value]
    };
    if values.iter().copied().any(|value| !valid_scalar(value)) {
        return invalid_config_type(plugin_id, key, field);
    }
    let numbers: Vec<f64> = values
        .iter()
        .filter_map(|value| value.as_number())
        .collect();
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

fn invalid_config_type<T>(
    plugin_id: &str,
    key: &str,
    field: &plugin::UserConfigField,
) -> Result<T, String> {
    let expected = field.value_type.as_deref().unwrap_or("string");
    let expected = if field.multiple == Some(true) {
        format!("an array of {expected} values")
    } else {
        format!("a {expected} value")
    };
    Err(format!(
        "pluginConfigs.{plugin_id}.options.{key} must be {expected}"
    ))
}

fn parse_payload(payload_json: &str) -> Result<Utf16JsonProjection, String> {
    let payload = Utf16JsonProjection::parse(payload_json)
        .map_err(|error| format!("payload is not valid JSON: {error}"))?;
    if payload
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .is_none()
    {
        return Err("payload must be a JSON object".to_string());
    }
    Ok(payload)
}

fn required_string<'a>(payload: &'a Utf16JsonProjection, key: &str) -> Result<&'a str, String> {
    payload
        .get_property(key)
        .and_then(JsonValueView::as_str)
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

fn check_scope_revision(
    layers: &BTreeMap<&'static str, Utf16JsonProjection>,
    scope: &str,
    expected: &str,
) -> Result<(), String> {
    let actual = layer_revision(layers, scope)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "revision conflict: expected {expected}, found {actual}"
        ))
    }
}

fn ensure_scope_revision(
    context: &SettingsContext,
    scope: &str,
    expected: &str,
) -> Result<(), String> {
    let snapshot = context
        .snapshot()
        .map_err(|error| settings_error_message(&error))?;
    check_scope_revision(&snapshot.layers, scope, expected)
}

fn layer_revision(
    layers: &BTreeMap<&'static str, Utf16JsonProjection>,
    scope: &str,
) -> Result<String, String> {
    let source = match layers.get(scope) {
        Some(layer) => layer.to_json_string().map_err(|error| error.to_string())?,
        None => "{}".to_string(),
    };
    Ok(plugin_source_sha256(source.as_bytes()))
}

fn source_member(source: &Utf16JsonProjection, key: &str) -> Option<Utf16JsonProjection> {
    let key_units: Vec<u16> = key.encode_utf16().collect();
    source.subprojection_property_units(&key_units).ok()
}

fn source_object() -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::parse("{}").map_err(|error| error.to_string())
}

fn source_null() -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::parse("null").map_err(|error| error.to_string())
}

fn source_string(value: &str) -> Result<Utf16JsonProjection, String> {
    let json = serde_json::to_string(value).map_err(|error| error.to_string())?;
    Utf16JsonProjection::parse(&json).map_err(|error| error.to_string())
}

fn source_bool(value: bool) -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .map_err(|error| error.to_string())
}

fn available_plugins(
    plugins_dir: &Path,
    installed_versions: &BTreeMap<String, String>,
) -> Result<(Vec<AvailablePluginRow>, Vec<MarketplaceRow>), String> {
    let registry_path = plugins_dir.join("known_marketplaces.json");
    let registry = match std::fs::read_to_string(&registry_path) {
        Ok(raw) => Utf16JsonProjection::parse(&raw)
            .map_err(|error| format!("{} is invalid: {error}", registry_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => source_object()?,
        Err(error) => {
            return Err(format!(
                "failed to read {}: {error}",
                registry_path.display()
            ));
        }
    };
    let registry_entries = registry
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .ok_or_else(|| {
            format!(
                "{} is invalid: expected a JSON object",
                registry_path.display()
            )
        })?;
    let mut available = Vec::new();
    let mut marketplaces = Vec::new();
    let mut seen = BTreeSet::new();
    for registry_entry in registry_entries {
        let Ok(marketplace) = String::from_utf16(registry_entry.code_units) else {
            continue;
        };
        let entry = registry
            .subprojection_property_units(registry_entry.code_units)
            .map_err(|error| error.to_string())?;
        let install_location = entry
            .get_property("installLocation")
            .and_then(exact_source_string);
        let source_json = source_member(&entry, "source")
            .map(|source| source.to_json_string())
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| "null".to_string());
        let last_updated = entry
            .get_property("lastUpdated")
            .and_then(display_source_string);
        marketplaces.push(MarketplaceRow {
            name: marketplace.clone(),
            source_json,
            install_location: install_location.clone(),
            last_updated,
        });
        let Some(root) = install_location.map(PathBuf::from) else {
            continue;
        };
        let Ok(manifest_path) = branding::marketplace_manifest_path(&root) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = Utf16JsonProjection::parse(&raw) else {
            continue;
        };
        let Some(plugins) = manifest.get_property("plugins") else {
            continue;
        };
        let Some(plugin_count) = plugins.array_len() else {
            continue;
        };
        for index in 0..plugin_count {
            let Some(plugin) = plugins.get_index(index) else {
                continue;
            };
            let Some(name) = plugin.get_property("name").and_then(exact_source_string) else {
                continue;
            };
            let id = format!("{name}@{marketplace}");
            if !seen.insert(id.clone()) {
                continue;
            }
            let version = plugin
                .get_property("version")
                .and_then(exact_source_string)
                .unwrap_or_default();
            let installed_version = installed_versions
                .get(&id)
                .or_else(|| installed_versions.get(&name));
            available.push(AvailablePluginRow {
                id,
                name: name.clone(),
                marketplace: marketplace.clone(),
                description: plugin
                    .get_property("description")
                    .and_then(display_source_string)
                    .unwrap_or_default(),
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

fn exact_source_string(value: JsonValueView<'_>) -> Option<String> {
    String::from_utf16(value.code_units()?).ok()
}

fn display_source_string(value: JsonValueView<'_>) -> Option<String> {
    value.as_str().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_bridge::SettingsPaths;
    use serde_json::{json, Value};
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
                plugin_archive_session: Arc::new(plugin::PluginArchiveSession::new()),
                fusion_layers: None,
                paths: SettingsPaths {
                    lingxi_home: home,
                    project_dir: project,
                },
                active: Arc::new(RwLock::new(crate::settings_bridge::empty_settings_source())),
                managed: crate::settings_bridge::empty_settings_source(),
                managed_diagnostics: None,
            },
            root,
        )
    }

    #[test]
    fn public_catalog_discovers_plugins_from_current_claude_marketplace_format() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        let marketplace = tmp.path().join("ora-marketplace");
        let manifest_dir = marketplace.join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        fs::create_dir_all(&plugins).unwrap();
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(
            manifest_dir.join("marketplace.json"),
            r#"{"name":"ora","owner":{"name":"ora"},"plugins":[{"name":"webmcp","version":"0.10.0","description":"MCP tools for browsers","source":"./"}]}"#,
        )
        .unwrap();
        let install_location = fs::canonicalize(&marketplace)
            .unwrap()
            .display()
            .to_string();
        fs::write(
            plugins.join("known_marketplaces.json"),
            serde_json::to_vec(&json!({
                "ora": {
                    "source": {"source": "directory", "path": install_location.clone()},
                    "installLocation": install_location
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let (available, marketplaces) =
            available_plugins(&plugins, &BTreeMap::new()).expect("catalog rows");
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "webmcp@ora");
        assert_eq!(available[0].version, "0.10.0");
        assert_eq!(available[0].description, "MCP tools for browsers");
        assert_eq!(marketplaces.len(), 1);
        assert_eq!(marketplaces[0].name, "ora");
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
                    "COUNT":{"type":"number","title":"Count","description":"count","required":true,"min":1,"max":5},
                    "LABEL":{"type":"string","title":"Label","description":"label"}
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

    #[tokio::test]
    async fn save_config_keeps_unpaired_utf16_in_plugin_options() {
        let (context, root) = context();
        install_schema_fixture(&context);

        let result = save_config(
            &context,
            &plugin_source_sha256(b"{}"),
            r#"{"scope":"user","pluginConfigs":{"secure@acme":{"options":{"COUNT":3,"LABEL":"\ud800"}}}}"#,
        )
        .await;
        result.expect("source options should save");

        let persisted = fs::read_to_string(context.paths.lingxi_home.join("settings.json"))
            .expect("persisted settings");
        let source = Utf16JsonProjection::parse(&persisted).expect("source settings json");
        assert_eq!(
            source.string_units("/pluginConfigs/secure@acme/options/LABEL"),
            Some(vec![0xd800])
        );
        let _ = fs::remove_dir_all(root);
    }
}
