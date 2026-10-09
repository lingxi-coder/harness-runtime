//! Mobile administration uses the same revisioned filesystem operations as Desktop.
//! Mutations report RestartRequired until this handle can prove a live registry reload.

use super::MobileEngineHandle;
use ::configuration_admin::{
    config_admin, hook_admin, mcp_admin, plugin_admin, skills_admin,
};
use client::adapter::ClientEventSink;
use client::protocol::commands::{
    HookAdminCommandDto, McpAdminCommandDto, PluginAdminCommandDto, SkillAdminCommandDto,
};
use client::protocol::events::{ClientEvent, ErrorKindDto};
use client::protocol::listings::{
    ConfigurationDomainDto as Domain, ConfigurationEffectDto as Effect,
    ConfigurationOperationStatusDto as Status,
};
use lingxi_core::host::OrchestratorHandle;
use serde_json::{json, Value};

async fn protocol_error(sink: &dyn ClientEventSink, message: impl Into<String>) {
    sink.emit(ClientEvent::Error {
        kind: ErrorKindDto::Protocol,
        message: message.into(),
    })
    .await;
}

async fn started(sink: &dyn ClientEventSink, domain: Domain, id: u64) {
    config_admin::emit_operation(
        sink,
        domain,
        id,
        Status::Started,
        Effect::NotApplicable,
        Some("Validating configuration operation.".into()),
        None,
    )
    .await;
}

async fn finish(
    sink: &dyn ClientEventSink,
    domain: Domain,
    id: u64,
    result: Result<Option<String>, String>,
    effect: Effect,
) -> bool {
    match result {
        Ok(details) => {
            let message = if effect == Effect::RestartRequired {
                "Saved. Reconnect the engine to apply this configuration to the runtime."
            } else {
                "Configuration validation completed."
            };
            config_admin::emit_operation(
                sink,
                domain,
                id,
                Status::Succeeded,
                effect,
                Some(message.into()),
                details,
            )
            .await;
            true
        }
        Err(message) => {
            config_admin::emit_operation(
                sink,
                domain,
                id,
                Status::Failed,
                Effect::NotApplicable,
                Some(message),
                None,
            )
            .await;
            false
        }
    }
}

fn mutation_fields<'a>(
    operation_id: Option<u64>,
    revision: Option<&'a str>,
    payload: Option<&'a str>,
) -> Result<(u64, &'a str, &'a str), String> {
    Ok((
        operation_id.ok_or("operation_id is required")?,
        revision.ok_or("revision is required")?,
        payload.ok_or("payload_json is required")?,
    ))
}

fn mobile_bundled_entries() -> Vec<Value> {
    let mut registry = skill_api::SkillRegistry::new();
    skill_api::builtin::register_mobile(&mut registry);
    let mut names = registry.names();
    names.sort_unstable();
    names
        .into_iter()
        .filter_map(|name| {
            registry.get(name).map(|skill| json!({
        "id": format!("<bundled:{name}>"), "name": name, "source": "bundled",
        "directory": format!("<bundled:{name}>"), "rootDir": "<bundled>", "writable": false,
        "readonlyReason": "Bundled mobile skills are compiled into the app.",
        "revision": config_admin::sha256_hex(skill.content.as_bytes()),
        "description": skill.description, "whenToUse": skill.frontmatter.when_to_use,
    }))
        })
        .collect()
}

fn mobile_bundled_document(target: &str) -> Result<String, String> {
    let name = target
        .strip_prefix("<bundled:")
        .and_then(|v| v.strip_suffix('>'))
        .ok_or("invalid bundled skill identifier")?;
    let mut registry = skill_api::SkillRegistry::new();
    skill_api::builtin::register_mobile(&mut registry);
    let skill = registry
        .get(name)
        .ok_or_else(|| "This bundled skill is not available on mobile.".to_string())?;
    Ok(json!({"id":target,"name":name,"source":"bundled","rootDir":"<bundled>","directory":target,
        "markdown":skill.content,"writable":false,"revision":config_admin::sha256_hex(skill.content.as_bytes()),
        "readonlyReason":"Bundled mobile skills are compiled into the app."}).to_string())
}

impl MobileEngineHandle {
    fn skills_admin_context(&self) -> skills_admin::SkillsAdminContext {
        skills_admin::SkillsAdminContext {
            cwd: self.session_cwd.clone().into(),
            lingxi_home: self.lingxi_home.clone(),
        }
    }

    pub(super) async fn emit_skill_catalog(&self, sink: &dyn ClientEventSink) {
        let result = skills_admin::catalog_json(&self.skills_admin_context()).and_then(|raw| {
            let mut catalog: Value =
                serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            let entries = catalog
                .get_mut("entries")
                .and_then(Value::as_array_mut)
                .ok_or("skill catalog entries missing")?;
            // The shared disk catalog includes Desktop builtins. Replace only that
            // synthetic source; retain all real user/project/managed entries.
            entries.retain(|entry| entry.get("source").and_then(Value::as_str) != Some("bundled"));
            entries.extend(mobile_bundled_entries());
            Ok(catalog)
        });
        match result {
            Ok(mut catalog) => {
                let registry = self.inner.slash_registry.read().await;
                if let Some(entries) = catalog.get_mut("entries").and_then(Value::as_array_mut) {
                    for command in registry.list_all() {
                        if let command_api::SlashCommandKind::Plugin {
                            plugin_id,
                            file_path,
                            ..
                        } = &command.kind
                        {
                            if command.skill_root.is_none() {
                                continue;
                            }
                            let root = command
                                .skill_root
                                .as_deref()
                                .or_else(|| file_path.parent())
                                .unwrap_or(file_path);
                            entries.push(json!({"id":format!("command:{}",command.name),"name":command.name,"source":"plugin",
                                "pluginOwner":plugin_id.to_string(),"rootDir":root.to_string_lossy(),"directory":root.to_string_lossy(),
                                "writable":false,"readonlyReason":"Managed by the owning plugin.","description":command.description}));
                        }
                    }
                }
                drop(registry);
                sink.emit(ClientEvent::SkillCatalog {
                    catalog_json: catalog.to_string(),
                })
                .await;
            }
            Err(message) => config_admin::emit_error(sink, message).await,
        }
    }

    async fn emit_mobile_skill_document(&self, target: &str, sink: &dyn ClientEventSink) {
        let result = if target.starts_with("<bundled:") {
            mobile_bundled_document(target)
        } else if let Some(name) = target.strip_prefix("command:") {
            let registry = self.inner.slash_registry.read().await;
            registry.list_all().into_iter().find(|command| command.name == name).ok_or_else(|| "Unknown plugin skill.".to_string()).and_then(|command| {
                if let command_api::SlashCommandKind::Plugin { file_path, plugin_id, .. } = &command.kind {
                    let markdown = std::fs::read_to_string(file_path).map_err(|error| format!("Cannot read plugin skill: {error}"))?;
                    let root = command.skill_root.as_deref().or_else(|| file_path.parent()).unwrap_or(file_path);
                    Ok(json!({"id":target,"name":name,"source":"plugin","pluginOwner":plugin_id.to_string(),
                        "rootDir":root.to_string_lossy(),"directory":root.to_string_lossy(),"markdown":markdown,
                        "revision":config_admin::sha256_hex(markdown.as_bytes()),"writable":false,
                        "readonlyReason":"Managed by the owning plugin."}).to_string())
                } else { Err("This command is not a plugin skill document.".into()) }
            })
        } else {
            skills_admin::document_json(target, &self.skills_admin_context())
        };
        match result {
            Ok(document_json) => {
                sink.emit(ClientEvent::SkillDocument { document_json })
                    .await
            }
            Err(message) => config_admin::emit_error(sink, message).await,
        }
    }

    pub(super) async fn apply_skill_admin(
        &self,
        command: SkillAdminCommandDto,
        sink: &dyn ClientEventSink,
    ) {
        match command.action.as_str() {
            "get_catalog" => self.emit_skill_catalog(sink).await,
            "get_document" => match command.target.as_deref() {
                Some(target) => self.emit_mobile_skill_document(target, sink).await,
                None => protocol_error(sink, "Skill document target is required.").await,
            },
            "save_document" | "create_skill" | "move_skill" | "trash_skill" | "restore_skill"
            | "purge_trash_skill" => {
                let Some(id) = command.operation_id else {
                    protocol_error(sink, "Skill mutation operation_id is required.").await;
                    return;
                };
                started(sink, Domain::Skill, id).await;
                let result = {
                    let _guard = self.settings_write_lock.lock().await;
                    skills_admin::handle(command, &self.skills_admin_context())
                };
                match result {
                    Ok(skills_admin::SkillAdminOutcome::Changed { document_json, .. }) => {
                        finish(sink, Domain::Skill, id, Ok(None), Effect::RestartRequired).await;
                        self.emit_skill_catalog(sink).await;
                        if let Some(document_json) = document_json {
                            sink.emit(ClientEvent::SkillDocument { document_json })
                                .await;
                        }
                    }
                    Ok(_) => {
                        finish(
                            sink,
                            Domain::Skill,
                            id,
                            Err("Skill mutation returned no changed document.".into()),
                            Effect::NotApplicable,
                        )
                        .await;
                    }
                    Err(error) => {
                        finish(sink, Domain::Skill, id, Err(error), Effect::NotApplicable).await;
                    }
                }
            }
            _ => protocol_error(sink, "Unsupported skill administration action.").await,
        }
    }

    pub(super) async fn emit_mcp_configuration_snapshot(&self, sink: &dyn ClientEventSink) {
        let Some(settings) = self.settings.as_ref() else {
            config_admin::emit_error(sink, "Settings context is unavailable.").await;
            return;
        };
        let mut runtime = Vec::new();
        for server in self.inner.orchestrator.list_mcp_servers().await {
            let config = self.inner.mcp_registry.get_config(&server.name).await;
            let plugin_owned = config.as_ref().is_some_and(|config| {
                config.metadata.agent_source == Some(mcp::McpAgentSource::Plugin)
            });
            let (source, writable) = if plugin_owned {
                ("plugin", false)
            } else {
                match config.as_ref().map(|config| &config.scope) {
                    Some(mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User)) => {
                        ("user", true)
                    }
                    Some(mcp::ConfigScope::Settings(
                        lingxi_core::types::SettingsScope::Project,
                    )) => ("project", true),
                    Some(mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Local)) => {
                        ("local", true)
                    }
                    _ => ("runtime", false),
                }
            };
            let status = match server.status {
                lingxi_core::host::McpStatus::Connected => json!("connected"),
                lingxi_core::host::McpStatus::Disconnected => json!("disconnected"),
                lingxi_core::host::McpStatus::Error(reason) => {
                    json!({"type":"error","reason":reason})
                }
            };
            runtime.push(json!({"name":server.name,"status":status,"transport":server.transport,"source":source,"writable":writable}));
        }
        match mcp_admin::snapshot_json(settings, &self.mobile_mcp_paths(), runtime) {
            Ok(snapshot_json) => {
                sink.emit(ClientEvent::McpConfigurationSnapshot { snapshot_json })
                    .await
            }
            Err(message) => config_admin::emit_error(sink, message).await,
        }
    }

    pub(super) async fn apply_mcp_admin(
        &self,
        command: McpAdminCommandDto,
        sink: &dyn ClientEventSink,
    ) {
        if command.action == "get_snapshot" {
            self.emit_mcp_configuration_snapshot(sink).await;
            return;
        }
        if !matches!(
            command.action.as_str(),
            "save_server" | "remove_server" | "set_approval"
        ) {
            protocol_error(sink, "Unsupported MCP administration action.").await;
            return;
        }
        let (id, revision, payload) = match mutation_fields(
            command.operation_id,
            command.revision.as_deref(),
            command.payload_json.as_deref(),
        ) {
            Ok(fields) => fields,
            Err(message) => {
                protocol_error(sink, message).await;
                return;
            }
        };
        started(sink, Domain::Mcp, id).await;
        let result = {
            let _guard = self.settings_write_lock.lock().await;
            match command.action.as_str() {
                "save_server" => {
                    mcp_admin::save_server_entry(&self.mobile_mcp_paths(), revision, payload)
                }
                "remove_server" => {
                    mcp_admin::remove_server_entry(&self.mobile_mcp_paths(), revision, payload)
                }
                _ => self
                    .settings
                    .as_ref()
                    .ok_or_else(|| "Settings context is unavailable.".to_string())
                    .and_then(|settings| {
                        if [
                            "enabledMcpjsonServers",
                            "disabledMcpjsonServers",
                            "enableAllProjectMcpServers",
                        ]
                        .iter()
                        .any(|key| settings.managed.contains_key(*key))
                        {
                            return Err("MCP approval is locked by managed policy.".into());
                        }
                        mcp_admin::set_project_approval(settings, revision, payload)
                    }),
            }
            .map(|()| None)
        };
        if finish(sink, Domain::Mcp, id, result, Effect::RestartRequired).await {
            self.emit_mcp_configuration_snapshot(sink).await;
            self.emit_settings_snapshot(sink).await;
        }
    }

    pub(super) async fn emit_plugin_catalog(&self, sink: &dyn ClientEventSink) {
        let Some(settings) = self.settings.as_ref() else {
            config_admin::emit_error(sink, "Settings context is unavailable.").await;
            return;
        };
        match plugin_admin::catalog_json(settings, Some(self.inner.credentials.as_ref())).await {
            Ok(catalog_json) => {
                sink.emit(ClientEvent::PluginCatalog { catalog_json })
                .await;
            }
            Err(message) => config_admin::emit_error(sink, message).await,
        }
    }

    pub(super) async fn apply_plugin_admin(
        &self,
        command: PluginAdminCommandDto,
        sink: &dyn ClientEventSink,
    ) {
        if command.action == "get_catalog" {
            self.emit_plugin_catalog(sink).await;
            return;
        }
        let Some(id) = command.operation_id else {
            protocol_error(sink, "Plugin operation_id is required.").await;
            return;
        };
        let Some(payload) = command.payload_json.as_deref() else {
            protocol_error(sink, "Plugin payload_json is required.").await;
            return;
        };
        let Some(settings) = self.settings.as_ref() else {
            config_admin::emit_error(sink, "Settings context is unavailable.").await;
            return;
        };
        if serde_json::from_str::<Value>(payload).is_err() {
            protocol_error(sink, "Invalid plugin payload JSON.").await;
            return;
        }
        if ["enabledPlugins", "pluginConfigs", "extraKnownMarketplaces"]
            .iter()
            .any(|key| settings.managed.contains_key(*key))
        {
            finish(
                sink,
                Domain::Plugin,
                id,
                Err("Plugin configuration is locked by managed policy.".into()),
                Effect::NotApplicable,
            )
            .await;
            return;
        }
        started(sink, Domain::Plugin, id).await;
        let preview = command.action == "preview_operation";
        let result = {
            let _guard = self.settings_write_lock.lock().await;
            match command.action.as_str() {
                "preview_operation" => plugin_admin::preview_operation(
                    Some(settings),
                    command.revision.as_deref(),
                    payload,
                )
                .await
                .map(Some),
                "apply_operation" => match command.revision.as_deref() {
                    Some(revision) => plugin_admin::apply_operation(settings, revision, payload)
                        .await
                        .map(Some),
                    None => Err("Plugin revision is required.".into()),
                },
                "save_config" => match command.revision.as_deref() {
                    Some(revision) => plugin_admin::save_config(settings, revision, payload)
                        .await
                        .map(|()| None),
                    None => Err("Plugin revision is required.".into()),
                },
                _ => Err("Unsupported plugin administration action.".into()),
            }
        };
        if finish(
            sink,
            Domain::Plugin,
            id,
            result,
            if preview {
                Effect::NotApplicable
            } else {
                Effect::RestartRequired
            },
        )
        .await
            && !preview
        {
            self.emit_plugin_catalog(sink).await;
            self.emit_settings_snapshot(sink).await;
        }
    }

    pub(super) async fn emit_hook_document(&self, scope: Option<&str>, sink: &dyn ClientEventSink) {
        let Some(settings) = self.settings.as_ref() else {
            config_admin::emit_error(sink, "Settings context is unavailable.").await;
            return;
        };
        match hook_admin::document_json(settings, scope) {
            Ok(document) => {
                config_admin::emit_operation(
                    sink,
                    Domain::Hook,
                    0,
                    Status::Succeeded,
                    Effect::NotApplicable,
                    Some("Hook document loaded.".into()),
                    Some(document),
                )
                .await;
            }
            Err(message) => config_admin::emit_error(sink, message).await,
        }
    }

    pub(super) async fn apply_hook_admin(
        &self,
        command: HookAdminCommandDto,
        sink: &dyn ClientEventSink,
    ) {
        if command.action == "get_document" {
            self.emit_hook_document(command.scope.as_deref(), sink)
                .await;
            return;
        }
        let Some(id) = command.operation_id else {
            protocol_error(sink, "Hook operation_id is required.").await;
            return;
        };
        let Some(payload) = command.payload_json.as_deref() else {
            protocol_error(sink, "Hook payload_json is required.").await;
            return;
        };
        started(sink, Domain::Hook, id).await;
        let validate = command.action == "validate_document";
        let result = {
            let _guard = self.settings_write_lock.lock().await;
            match command.action.as_str() {
                "validate_document" => hook_admin::validate_document(payload),
                "save_document" => match (self.settings.as_ref(), command.revision.as_deref()) {
                    (Some(settings), Some(revision)) if !settings.managed.contains_key("hooks") => {
                        hook_admin::save_document(settings, revision, payload)
                    }
                    (Some(settings), _) if settings.managed.contains_key("hooks") => {
                        Err("Hooks are locked by managed policy.".into())
                    }
                    _ => Err("Settings context and hook revision are required.".into()),
                },
                _ => Err("Unsupported hook administration action.".into()),
            }
            .map(|()| None)
        };
        if finish(
            sink,
            Domain::Hook,
            id,
            result,
            if validate {
                Effect::NotApplicable
            } else {
                Effect::RestartRequired
            },
        )
        .await
            && !validate
        {
            self.emit_hook_document(command.scope.as_deref(), sink)
                .await;
            self.emit_settings_snapshot(sink).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutation_requires_revision_and_payload() {
        assert!(mutation_fields(Some(1), None, Some("{}")).is_err());
        assert!(mutation_fields(None, Some("r"), Some("{}")).is_err());
        assert_eq!(
            mutation_fields(Some(1), Some("r"), Some("{}")).unwrap(),
            (1, "r", "{}")
        );
    }
    #[test]
    fn bundled_catalog_is_mobile_scoped_and_documents_are_readonly() {
        for entry in mobile_bundled_entries() {
            let document: Value = serde_json::from_str(
                &mobile_bundled_document(entry["id"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(document["writable"], false);
            assert_eq!(document["id"], entry["id"]);
            assert!(document["markdown"].is_string());
        }
    }
    #[tokio::test]
    async fn mobile_recorded_plugin_reconnect_uses_layers_and_secure_fields() {
        use crate::mobile::test_support::{
            test_config, CollectingPermissionSink, FakeListener, HostFakePlatform,
        };
        use lingxi_core::host::SecureStorage;
        use std::sync::Arc;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let home = temp.path().join("home");
        std::fs::create_dir_all(workspace.join(branding::DOT_DIR)).unwrap();
        let plugin_dir = home.join("plugins/cache/tests/fixture/1.0.0");
        std::fs::create_dir_all(plugin_dir.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::create_dir_all(plugin_dir.join("commands")).unwrap();
        std::fs::write(plugin_dir.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),r#"{"name":"fixture","version":"1.0.0","userConfig":{"API_TOKEN":{"type":"string","title":"Token","description":"fixture","required":true,"sensitive":true},"REGION":{"type":"string","title":"Region","description":"fixture","required":true}}}"#).unwrap();
        std::fs::write(
            plugin_dir.join("commands/ping.md"),
            "---\ndescription: fixture\n---\nPING",
        )
        .unwrap();
        std::fs::write(home.join("plugins/installed_plugins.json"),json!({"version":2,"plugins":{"fixture@tests":[{"scope":"user","installPath":plugin_dir,"version":"1.0.0"}]}}).to_string()).unwrap();
        std::fs::write(home.join("settings.json"),json!({"enabledPlugins":{"fixture@tests":false,(crate::mobile::MOBILE_BUILTIN_PLUGIN_NAME):false}}).to_string()).unwrap();
        let mut cfg = test_config(&workspace);
        cfg.lingxi_home = home.clone();
        let storage = Arc::new(lingxi_core::host::InMemorySecureStorage::new());
        let platform = Arc::new(
            HostFakePlatform::new(temp.path().into()).with_secure_storage(storage.clone()),
        );
        let build = |cfg| {
            super::super::build_mobile(
                cfg,
                platform.clone(),
                Arc::new(FakeListener::default()),
                Arc::new(CollectingPermissionSink::default()),
            )
        };
        let disabled = build(cfg.clone()).await.unwrap();
        assert!(disabled
            .slash_registry
            .read()
            .await
            .resolve("fixture:ping")
            .is_none());
        drop(disabled);
        std::fs::write(workspace.join(branding::DOT_DIR).join("settings.local.json"),json!({"enabledPlugins":{"fixture@tests":true},"pluginConfigs":{"fixture@tests":{"options":{"REGION":"eu"}}}}).to_string()).unwrap();
        let missing_secret = build(cfg.clone()).await.unwrap();
        assert!(missing_secret
            .slash_registry
            .read()
            .await
            .resolve("fixture:ping")
            .is_none());
        drop(missing_secret);
        storage
            .store(
                "lingxi",
                "plugin-secret-fixture@tests/API_TOKEN",
                lingxi_core::types::SecureStorageData::new(
                    b"test-only-token".to_vec(),
                    lingxi_core::types::SecureStorageMetadata {
                        created_at: std::time::UNIX_EPOCH,
                        last_accessed: None,
                        kind: lingxi_core::types::SecretKindDto(
                            r#"{"PluginSecret":{"plugin":"fixture@tests","key":"API_TOKEN"}}"#
                                .into(),
                        ),
                    },
                ),
            )
            .await
            .unwrap();
        let enabled = build(cfg).await.unwrap();
        assert!(
            enabled
                .slash_registry
                .read()
                .await
                .resolve("fixture:ping")
                .is_some(),
            "layered enable and secure required config must be consumed by the real boot path"
        );
    }
}
