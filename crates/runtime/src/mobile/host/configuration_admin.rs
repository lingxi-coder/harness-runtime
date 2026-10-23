//! Mobile administration uses the same revisioned filesystem operations as Desktop.
//! Mutations report RestartRequired until this handle can prove a live registry reload.

use super::MobileEngineHandle;
use ::configuration_admin::{config_admin, hook_admin, mcp_admin, plugin_admin, skills_admin};
use client::adapter::ClientEventSink;
use client::protocol::commands::{
    HookAdminCommandDto, McpAdminCommandDto, PluginAdminCommandDto, SkillAdminCommandDto,
};
use client::protocol::events::{ClientEvent, ErrorKindDto};
use client::protocol::listings::{
    ConfigurationDomainDto as Domain, ConfigurationEffectDto as Effect,
    ConfigurationOperationStatusDto as Status,
};
use client::protocol::text::ClientTextDto;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};

async fn protocol_error(sink: &dyn ClientEventSink, message: impl Into<String>) {
    sink.emit(ClientEvent::Error {
        kind: ErrorKindDto::Protocol,
        message: client::protocol::text::ClientTextDto::from(message.into()),
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

fn source_string(value: impl Into<Utf16JsonProjection>) -> Utf16JsonProjection {
    value.into()
}

fn source_with_prefix(prefix: &str, value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(
        value
            .string_units("")
            .expect("mobile catalog text fields are root strings"),
    );
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("prefixing text preserves the source string")
}

fn source_projection(value: &ClientTextDto) -> Utf16JsonProjection {
    let units = value.code_units.clone();
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("ClientTextDto display is derived from its exact units")
}

fn strip_source_prefix<'a>(units: &'a [u16], prefix: &str) -> Option<&'a [u16]> {
    let prefix_units = prefix.encode_utf16().collect::<Vec<_>>();
    units.strip_prefix(prefix_units.as_slice())
}

fn has_source_prefix(units: &[u16], prefix: &str) -> bool {
    strip_source_prefix(units, prefix).is_some()
}

fn source_object(fields: Vec<(String, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(fields).expect("generated mobile skill objects have unique fields")
}

impl MobileEngineHandle {
    fn skills_admin_context(&self) -> Result<skills_admin::SkillsAdminContext, String> {
        Ok(skills_admin::SkillsAdminContext {
            cwd: self
                .current_mobile_config()
                .map_err(|error| error.to_string())?
                .cwd,
            lingxi_home: self.lingxi_home.clone(),
        })
    }

    pub(super) async fn emit_skill_catalog(&self, sink: &dyn ClientEventSink) {
        let result = self
            .skills_admin_context()
            .and_then(|context| skills_admin::catalog_json(&context))
            .and_then(|raw| {
                let mut catalog =
                    Utf16JsonProjection::parse(&raw).map_err(|error| error.to_string())?;
                let entry_count = catalog
                    .array_len("/entries")
                    .ok_or("skill catalog entries missing")?;
                let mut entries = Vec::with_capacity(entry_count);
                // The shared disk catalog includes Desktop builtins. Replace only that
                // synthetic source; retain all real user/project/managed entries.
                for index in 0..entry_count {
                    let entry = catalog
                        .subprojection(&format!("/entries/{index}"))
                        .map_err(|error| error.to_string())?;
                    if entry.string_at("/source") != Some("bundled") {
                        entries.push(entry);
                    }
                }
                catalog
                    .set_field(
                        "entries",
                        Utf16JsonProjection::array(entries).map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
                Ok(catalog)
            });
        match result {
            Ok(mut catalog) => {
                let registry = self.inner.slash_registry.read().await;
                let entry_count = catalog.array_len("/entries").unwrap_or_default();
                let mut entries = (0..entry_count)
                    .filter_map(|index| catalog.subprojection(&format!("/entries/{index}")).ok())
                    .collect::<Vec<_>>();
                {
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
                            entries.push(source_object(vec![
                                ("id".into(), source_with_prefix("command:", &command.name)),
                                ("name".into(), command.name.clone()),
                                ("source".into(), source_string("plugin")),
                                ("pluginOwner".into(), source_string(plugin_id.to_string())),
                                (
                                    "rootDir".into(),
                                    source_string(root.to_string_lossy().into_owned()),
                                ),
                                (
                                    "directory".into(),
                                    source_string(root.to_string_lossy().into_owned()),
                                ),
                                (
                                    "writable".into(),
                                    Utf16JsonProjection::parse("false").expect("boolean JSON"),
                                ),
                                (
                                    "readonlyReason".into(),
                                    source_string("Managed by the owning plugin."),
                                ),
                                ("description".into(), command.description.clone()),
                            ]));
                        }
                    }
                }
                catalog
                    .set_field(
                        "entries",
                        Utf16JsonProjection::array(entries)
                            .expect("mobile skill catalog entries are valid JSON"),
                    )
                    .expect("skill catalog root has an entries field");
                drop(registry);
                sink.emit(ClientEvent::SkillCatalog {
                    catalog_json: catalog
                        .to_json_string()
                        .expect("mobile skill catalog is a valid source JSON object"),
                })
                .await;
            }
            Err(message) => config_admin::emit_error(sink, message).await,
        }
    }

    async fn emit_mobile_skill_document(&self, target: &ClientTextDto, sink: &dyn ClientEventSink) {
        let target_units = target.code_units.as_slice();
        let result = if has_source_prefix(target_units, "<bundled:") {
            Err("This bundled skill is not available on mobile.".to_string())
        } else if let Some(name_units) = strip_source_prefix(target_units, "command:") {
            let registry = self.inner.slash_registry.read().await;
            registry
                .list_all()
                .into_iter()
                .find(|command| command.name.string_units("").as_deref() == Some(name_units))
                .ok_or_else(|| "Unknown plugin skill.".to_string())
                .and_then(|command| {
                    if let command_api::SlashCommandKind::Plugin {
                        file_path,
                        plugin_id,
                        ..
                    } = &command.kind
                    {
                        let markdown = std::fs::read_to_string(file_path)
                            .map_err(|error| format!("Cannot read plugin skill: {error}"))?;
                        let root = command
                            .skill_root
                            .as_deref()
                            .or_else(|| file_path.parent())
                            .unwrap_or(file_path);
                        let revision = config_admin::sha256_hex(markdown.as_bytes());
                        source_object(vec![
                            ("id".into(), source_projection(target)),
                            ("name".into(), command.name.clone()),
                            ("source".into(), source_string("plugin")),
                            ("pluginOwner".into(), source_string(plugin_id.to_string())),
                            (
                                "rootDir".into(),
                                source_string(root.to_string_lossy().into_owned()),
                            ),
                            (
                                "directory".into(),
                                source_string(root.to_string_lossy().into_owned()),
                            ),
                            ("markdown".into(), source_string(markdown)),
                            ("revision".into(), source_string(revision)),
                            (
                                "writable".into(),
                                Utf16JsonProjection::parse("false").expect("boolean JSON"),
                            ),
                            (
                                "readonlyReason".into(),
                                source_string("Managed by the owning plugin."),
                            ),
                        ])
                        .to_json_string()
                        .map_err(|error| error.to_string())
                    } else {
                        Err("This command is not a plugin skill document.".into())
                    }
                })
        } else {
            self.skills_admin_context()
                .and_then(|context| skills_admin::document_json(target, &context))
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
            "get_document" => match command.target.as_ref() {
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
                    self.skills_admin_context()
                        .and_then(|context| skills_admin::handle(command, &context))
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
        let current_settings = self.current_settings();
        let Some(settings) = current_settings.as_ref() else {
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
        let paths = match self.mobile_mcp_paths() {
            Ok(paths) => paths,
            Err(message) => {
                config_admin::emit_error(sink, message).await;
                return;
            }
        };
        match mcp_admin::snapshot_json(settings, &paths, runtime) {
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
        let paths = match self.mobile_mcp_paths() {
            Ok(paths) => paths,
            Err(message) => {
                config_admin::emit_error(sink, message).await;
                return;
            }
        };
        started(sink, Domain::Mcp, id).await;
        let result = {
            let _guard = self.settings_write_lock.lock().await;
            match command.action.as_str() {
                "save_server" => mcp_admin::save_server_entry(&paths, revision, payload),
                "remove_server" => mcp_admin::remove_server_entry(&paths, revision, payload),
                _ => self
                    .current_settings()
                    .as_ref()
                    .ok_or_else(|| "Settings context is unavailable.".to_string())
                    .and_then(|settings| {
                        if [
                            "enabledMcpjsonServers",
                            "disabledMcpjsonServers",
                            "enableAllProjectMcpServers",
                        ]
                        .iter()
                        .any(|key| settings.managed.contains_property(key))
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
        let current_settings = self.current_settings();
        let Some(settings) = current_settings.as_ref() else {
            config_admin::emit_error(sink, "Settings context is unavailable.").await;
            return;
        };
        match plugin_admin::catalog_json(settings, Some(self.inner.credentials.as_ref())).await {
            Ok(catalog_json) => {
                sink.emit(ClientEvent::PluginCatalog { catalog_json }).await;
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
        let current_settings = self.current_settings();
        let Some(settings) = current_settings.as_ref() else {
            config_admin::emit_error(sink, "Settings context is unavailable.").await;
            return;
        };
        if serde_json::from_str::<Value>(payload).is_err() {
            protocol_error(sink, "Invalid plugin payload JSON.").await;
            return;
        }
        if ["enabledPlugins", "pluginConfigs", "extraKnownMarketplaces"]
            .iter()
            .any(|key| settings.managed.contains_property(key))
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
        let current_settings = self.current_settings();
        let Some(settings) = current_settings.as_ref() else {
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
                "save_document" => match (
                    self.current_settings().as_ref(),
                    command.revision.as_deref(),
                ) {
                    (Some(settings), Some(revision))
                        if !settings.managed.contains_property("hooks") =>
                    {
                        hook_admin::save_document(settings, revision, payload)
                    }
                    (Some(settings), _) if settings.managed.contains_property("hooks") => {
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
    fn mobile_skill_json_keeps_source_utf16_text() {
        let description = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&[u16::from(b'A'), 0xd800, u16::from(b'B')]),
            vec![u16::from(b'A'), 0xd800, u16::from(b'B')],
        )
        .expect("valid source string");
        let raw = source_object(vec![("description".into(), description)])
            .to_json_string()
            .expect("source object serializes as JSON");
        let decoded = Utf16JsonProjection::parse(&raw).expect("serialized source JSON parses");
        assert_eq!(
            decoded.string_units("/description"),
            Some(vec![u16::from(b'A'), 0xd800, u16::from(b'B')])
        );
    }

    #[test]
    fn mobile_command_skill_prefix_keeps_exact_name_units() {
        let name_units = vec![
            u16::from(b'r'),
            u16::from(b'e'),
            0xd800,
            u16::from(b'v'),
            u16::from(b'i'),
            u16::from(b'e'),
            u16::from(b'w'),
        ];
        let name = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&name_units),
            name_units.clone(),
        )
        .unwrap();
        let id = source_with_prefix("command:", &name);
        let target = ClientTextDto::from_code_units(id.string_units("").unwrap());
        let loaded_name = strip_source_prefix(&target.code_units, "command:")
            .expect("catalog id has the command prefix");
        assert_eq!(loaded_name, name_units);
        let replacement = [
            u16::from(b'r'),
            u16::from(b'e'),
            0xfffd,
            u16::from(b'v'),
            u16::from(b'i'),
            u16::from(b'e'),
            u16::from(b'w'),
        ];
        assert_ne!(loaded_name, replacement);
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
        std::fs::write(
            home.join("settings.json"),
            json!({"enabledPlugins":{"fixture@tests":false}}).to_string(),
        )
        .unwrap();
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
            .resolve(&Utf16JsonProjection::from("fixture:ping"))
            .is_none());
        drop(disabled);
        std::fs::write(workspace.join(branding::DOT_DIR).join("settings.local.json"),json!({"enabledPlugins":{"fixture@tests":true},"pluginConfigs":{"fixture@tests":{"options":{"REGION":"eu"}}}}).to_string()).unwrap();
        let missing_secret = build(cfg.clone()).await.unwrap();
        assert!(missing_secret
            .slash_registry
            .read()
            .await
            .resolve(&Utf16JsonProjection::from("fixture:ping"))
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
                .resolve(&Utf16JsonProjection::from("fixture:ping"))
                .is_some(),
            "layered enable and secure required config must be consumed by the real boot path"
        );
    }
}
