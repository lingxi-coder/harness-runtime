use super::canonical_cwd_string;
use super::optional_json_string;
use super::value_sha256;
use super::LocalAppsHostBroker;
use super::PersistedMcpCandidate;
use super::LOCAL_APP_WIDGET_DIR;
use super::LOCAL_APP_WIDGET_FILE;
use super::LOCAL_APP_WIDGET_MIME;
use crate::host::HostEvent;
use crate::publication::{Exposure, ManagedApp, ManagedRuntime, WidgetResource};
use local_app_contracts::approvals::{
    McpToolChangeKind, McpToolDiff, McpToolField, McpToolSurface, VerificationStatus,
    VerificationSummary,
};
use local_app_contracts::events::{ManagedMcpServer, ManagedMcpStatus, McpAppWidget};
use local_apps::derive_mcp_status;
use local_apps::effective_tool_surface_sha256;
use local_apps::load_manifest;
use local_apps::load_mcp_settings;
use local_apps::mcp_catalog_tool_names;
use local_apps::save_mcp_settings;
use local_apps::AppLayout;
use local_apps::AppMcpSettings;
use local_apps::AppMcpStatus;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn lower_managed_mcp_status(status: AppMcpStatus) -> ManagedMcpStatus {
    match status {
        AppMcpStatus::Disabled => ManagedMcpStatus::Disabled,
        AppMcpStatus::NeedsSetup => ManagedMcpStatus::NeedsSetup,
        AppMcpStatus::Authoring => ManagedMcpStatus::Authoring,
        AppMcpStatus::Enabled => ManagedMcpStatus::Enabled,
        AppMcpStatus::NeedsRevalidation => ManagedMcpStatus::NeedsRevalidation,
        AppMcpStatus::Error => ManagedMcpStatus::Error,
    }
}

pub(super) fn tool_meta_resource_uri(
    definition: &mcp_wire::McpToolDefinitionDto,
) -> Option<String> {
    let meta = definition.meta.as_ref()?;
    meta.get("ui")
        .and_then(Value::as_object)
        .and_then(|ui| ui.get("resourceUri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            meta.get("openai/outputTemplate")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub(super) fn resource_sha_for_app_uri(app_id: &str, uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("ui://local-app/")?;
    let mut parts = rest.split('/');
    let uri_app_id = parts.next()?;
    let resource_sha256 = parts.next()?;
    let file = parts.next()?;
    if parts.next().is_some()
        || uri_app_id != app_id
        || file != LOCAL_APP_WIDGET_FILE
        || resource_sha256.len() != 64
        || resource_sha256
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return None;
    }
    Some(resource_sha256.to_string())
}

pub(super) fn validate_managed_mcp_widget_file(
    layout: &AppLayout,
    resource_sha256: &str,
    mime_type: &str,
) -> Result<(), String> {
    if mime_type != LOCAL_APP_WIDGET_MIME {
        return Err(format!(
            "widget_invalid: Local App MCP widget MIME must be {LOCAL_APP_WIDGET_MIME}"
        ));
    }
    let relative = layout
        .app_dir_rel()
        .join(local_apps::manifest::MCP_DIR)
        .join(LOCAL_APP_WIDGET_DIR)
        .join(format!("{resource_sha256}.html"));
    let body = rooted_fs::read_to_string_limited(layout.root(), &relative, 4 * 1024 * 1024)
        .map_err(|error| format!("widget_invalid: {}: {error}", relative.display()))?;
    let actual = format!("{:x}", Sha256::digest(body.as_bytes()));
    if actual != resource_sha256 {
        return Err("widget_invalid: Local App MCP widget digest mismatch".into());
    }
    let lower = body.to_ascii_lowercase();
    for forbidden in [
        "src=\"http",
        "src='http",
        "href=\"http",
        "href='http",
        "window.openai",
    ] {
        if lower.contains(forbidden) {
            return Err(format!(
                "widget_invalid: Local App MCP widget contains forbidden token {forbidden:?}"
            ));
        }
    }
    Ok(())
}

pub(super) fn managed_mcp_widget_resource(
    layout: &AppLayout,
    app_name: &str,
    catalog: &Value,
) -> Result<Option<(McpAppWidget, WidgetResource)>, String> {
    let app_id = layout.app_id();
    if let Some(resources) = catalog.get("resources").and_then(Value::as_array) {
        for resource in resources {
            let Some(uri) = resource.get("uri").and_then(Value::as_str) else {
                continue;
            };
            let Some(resource_sha256) = resource_sha_for_app_uri(app_id, uri) else {
                continue;
            };
            let name = resource
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(app_name)
                .to_string();
            let description = resource
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mime_type = resource
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or(LOCAL_APP_WIDGET_MIME)
                .to_string();
            validate_managed_mcp_widget_file(layout, &resource_sha256, &mime_type)?;
            return Ok(Some((
                McpAppWidget {
                    resource_uri: uri.to_string(),
                    mime_type: mime_type.clone(),
                    resource_sha256: resource_sha256.clone(),
                },
                WidgetResource {
                    uri: uri.to_string(),
                    name,
                    description,
                    mime_type: Some(mime_type),
                    meta: resource.get("_meta").cloned(),
                },
            )));
        }
    }

    let entries = catalog
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "catalog_invalid: active catalog tools are missing".to_string())?;
    for entry in entries {
        let definition: mcp_wire::McpToolDefinitionDto =
            serde_json::from_value(entry.get("definition").unwrap_or(entry).clone())
                .map_err(|_| "catalog_invalid: active tool definition is invalid".to_string())?;
        let Some(uri) = tool_meta_resource_uri(&definition) else {
            continue;
        };
        let Some(resource_sha256) = resource_sha_for_app_uri(app_id, &uri) else {
            continue;
        };
        let name = definition
            .title
            .clone()
            .or_else(|| definition.description.clone())
            .unwrap_or_else(|| format!("{app_name} widget"));
        validate_managed_mcp_widget_file(layout, &resource_sha256, LOCAL_APP_WIDGET_MIME)?;
        return Ok(Some((
            McpAppWidget {
                resource_uri: uri.clone(),
                mime_type: LOCAL_APP_WIDGET_MIME.into(),
                resource_sha256: resource_sha256.clone(),
            },
            WidgetResource {
                uri,
                name,
                description: definition.description.clone(),
                mime_type: Some(LOCAL_APP_WIDGET_MIME.into()),
                meta: None,
            },
        )));
    }

    Ok(None)
}

pub(super) fn mcp_tool_surface(
    definition: mcp_wire::McpToolDefinitionDto,
    flow: Value,
    ceiling: mcp_wire::McpPermissionCeiling,
) -> Result<McpToolSurface, String> {
    Ok(McpToolSurface {
        name: definition.name,
        title: definition.title,
        description: definition.description,
        input_schema_json: serde_json::to_string(&definition.input_schema)
            .map_err(|error| error.to_string())?,
        output_schema_json: optional_json_string(definition.output_schema.as_ref())?,
        annotations_json: optional_json_string(definition.annotations.as_ref())?,
        execution_json: optional_json_string(definition.execution.as_ref())?,
        visible_meta_json: optional_json_string(definition.meta.as_ref())?,
        semantic_flow_json: serde_json::to_string(&flow).map_err(|error| error.to_string())?,
        permission_ceiling: match ceiling {
            mcp_wire::McpPermissionCeiling::Allow => "allow",
            mcp_wire::McpPermissionCeiling::Ask => "ask",
            mcp_wire::McpPermissionCeiling::Deny => "deny",
        }
        .into(),
    })
}

pub(super) fn mcp_tool_surfaces_from_catalog(
    catalog: &Value,
) -> Result<Vec<McpToolSurface>, String> {
    let entries = catalog
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "catalog_invalid: active catalog tools are missing".to_string())?;
    let mut tools = Vec::with_capacity(entries.len());
    for entry in entries {
        let definition: mcp_wire::McpToolDefinitionDto =
            serde_json::from_value(entry.get("definition").unwrap_or(entry).clone())
                .map_err(|_| "catalog_invalid: active tool definition is invalid".to_string())?;
        let flow = entry
            .get("flow")
            .cloned()
            .ok_or_else(|| "catalog_invalid: active tool Flow binding is missing".to_string())?;
        let ceiling = entry
            .get("ceiling")
            .and_then(Value::as_str)
            .and_then(mcp_wire::McpPermissionCeiling::from_policy_str)
            .ok_or_else(|| "catalog_invalid: active tool ceiling is invalid".to_string())?;
        tools.push(mcp_tool_surface(definition, flow, ceiling)?);
    }
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tools)
}

pub(super) fn mcp_tool_surfaces_from_candidate(
    candidate: &PersistedMcpCandidate,
) -> Result<Vec<McpToolSurface>, String> {
    let mut tools = candidate
        .validated
        .tools
        .iter()
        .map(|tool| {
            mcp_tool_surface(
                tool.definition.clone(),
                serde_json::to_value(&tool.flow).map_err(|error| error.to_string())?,
                tool.ceiling,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tools)
}

pub(super) fn changed_mcp_fields(
    before: &McpToolSurface,
    after: &McpToolSurface,
) -> Vec<McpToolField> {
    let mut fields = Vec::new();
    if before.title != after.title {
        fields.push(McpToolField::Title);
    }
    if before.description != after.description {
        fields.push(McpToolField::Description);
    }
    if before.input_schema_json != after.input_schema_json {
        fields.push(McpToolField::InputSchema);
    }
    if before.output_schema_json != after.output_schema_json {
        fields.push(McpToolField::OutputSchema);
    }
    if before.annotations_json != after.annotations_json {
        fields.push(McpToolField::Annotations);
    }
    if before.execution_json != after.execution_json {
        fields.push(McpToolField::Execution);
    }
    if before.visible_meta_json != after.visible_meta_json {
        fields.push(McpToolField::VisibleMeta);
    }
    if before.semantic_flow_json != after.semantic_flow_json {
        fields.push(McpToolField::SemanticFlow);
    }
    if before.permission_ceiling != after.permission_ceiling {
        fields.push(McpToolField::PermissionCeiling);
    }
    fields
}

pub(super) fn mcp_tool_diffs(
    before: Vec<McpToolSurface>,
    after: Vec<McpToolSurface>,
) -> Vec<McpToolDiff> {
    let mut before = before
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let mut after = after
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let names = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut diffs = Vec::new();
    for name in names {
        match (before.remove(&name), after.remove(&name)) {
            (None, Some(after)) => diffs.push(McpToolDiff {
                kind: McpToolChangeKind::Added,
                name,
                before: None,
                after: Some(after),
                changed_fields: Vec::new(),
            }),
            (Some(before), None) => diffs.push(McpToolDiff {
                kind: McpToolChangeKind::Removed,
                name,
                before: Some(before),
                after: None,
                changed_fields: Vec::new(),
            }),
            (Some(before), Some(after)) if before != after => {
                let changed_fields = changed_mcp_fields(&before, &after);
                diffs.push(McpToolDiff {
                    kind: McpToolChangeKind::Changed,
                    name,
                    before: Some(before),
                    after: Some(after),
                    changed_fields,
                });
            }
            _ => {}
        }
    }
    diffs
}

impl LocalAppsHostBroker {
    /// Make one enabled Local App MCP visible to one conversation and create
    /// the real logical MCP connection whose discovered tools are registered
    /// into that conversation's shared ToolRegistry.
    pub async fn expose_managed_mcp_for_conversation(
        &self,
        conversation_id: &str,
        app_id: &str,
        pin: bool,
    ) -> Result<bool, String> {
        let Some(publisher) = self.publisher() else {
            return Ok(false);
        };
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let Some(active) = manifest.active_mcp_catalog.as_ref() else {
            return Ok(false);
        };
        let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
            .map_err(|error| error.to_string())?;
        let settings = load_mcp_settings(&layout)
            .map_err(|error| error.to_string())?
            .reconcile_with_catalog(&catalog, false)
            .map_err(|error| error.to_string())?;
        if !settings.enabled || settings.enabled_tools.is_empty() {
            return Ok(false);
        }
        let effective_surface =
            effective_tool_surface_sha256(&active.tool_surface_sha256, &settings.enabled_tools)
                .map_err(|error| error.to_string())?;
        let app = ManagedApp {
            app_id: app_id.to_string(),
            catalog_sha256: active.catalog_sha256.clone(),
            effective_surface_sha256: effective_surface,
        };
        publisher
            .publish(
                &app,
                ManagedRuntime {
                    enabled: true,
                    enabled_tools: settings.enabled_tools.clone(),
                    widget: managed_mcp_widget_resource(&layout, &manifest.name, &catalog)?
                        .map(|(_, resource)| resource),
                },
            )
            .await?;
        publisher.expose(conversation_id, &app, pin).await?;
        Ok(true)
    }
    pub async fn set_managed_mcp_conversation_pinned(
        &self,
        conversation_id: &str,
        app_id: &str,
        pinned: bool,
    ) -> Result<(), String> {
        *self.active_mcp_conversation.lock().await = Some(conversation_id.to_string());
        if pinned {
            if !self
                .expose_managed_mcp_for_conversation(conversation_id, app_id, true)
                .await?
            {
                return Err("mcp_not_enabled: Local App MCP is not enabled".into());
            }
        } else if let Some(publisher) = self.publisher() {
            publisher.unpin(conversation_id, app_id).await?;
        }
        self.emit_managed_mcp_inventory().await
    }
    /// Retarget the shared model ToolRegistry to one conversation's logical
    /// Local App MCP partition. Existing per-conversation LRU/pin metadata is
    /// retained, while live logical connections from the previous session are
    /// removed before the new session is exposed.
    pub async fn activate_managed_mcp_conversation(
        &self,
        conversation_id: &str,
        cwd: &str,
    ) -> Result<(), String> {
        *self.active_mcp_conversation.lock().await = Some(conversation_id.to_string());
        let Some(publisher) = self.publisher() else {
            return Ok(());
        };
        publisher.disconnect_all().await?;

        let mut desired = publisher.exposures(conversation_id).await;
        let canonical_cwd = canonical_cwd_string(Path::new(cwd));
        if let Ok(service) = self.service() {
            for record in service.list_apps().await {
                let workspace = canonical_cwd_string(&self.root.join(&record.workspace_rel));
                if workspace == canonical_cwd
                    && !desired.iter().any(|entry| entry.app_id == record.id)
                {
                    desired.push(Exposure {
                        app_id: record.id,
                        pinned: false,
                        last_used: u64::MAX,
                    });
                    break;
                }
            }
        }
        desired.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
        for entry in desired {
            let _ = self
                .expose_managed_mcp_for_conversation(conversation_id, &entry.app_id, entry.pinned)
                .await?;
        }
        self.emit_managed_mcp_inventory().await
    }
    pub async fn set_managed_mcp_enabled(
        &self,
        app_id: &str,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<(), String> {
        let _guard = self.mcp_settings_writes.lock().await;
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let active = manifest.active_mcp_catalog.as_ref();
        if enabled && active.is_none() {
            return Err("mcp_authoring_required: Local App has no approved MCP catalog".into());
        }
        let mut settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
        if let Some(active) = active {
            let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                .map_err(|error| error.to_string())?;
            settings = settings
                .reconcile_with_catalog(&catalog, false)
                .map_err(|error| error.to_string())?;
            if enabled && settings.enabled_tools.is_empty() {
                settings.enabled_tools =
                    mcp_catalog_tool_names(&catalog).map_err(|error| error.to_string())?;
            }
        }
        if enabled && settings.enabled_tools.is_empty() {
            return Err("invalid_mcp_settings: no model-visible tools are enabled".into());
        }
        settings.enabled = enabled;
        save_mcp_settings(&layout, &settings, Some(expected_revision))
            .map_err(|error| error.to_string())?;
        drop(_guard);
        self.sync_managed_local_app_publication(app_id).await?;
        self.emit_managed_mcp_inventory().await
    }
    pub async fn set_managed_mcp_tool_enabled(
        &self,
        app_id: &str,
        tool_name: &str,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<(), String> {
        let _guard = self.mcp_settings_writes.lock().await;
        self.service()?
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let active = manifest.active_mcp_catalog.as_ref().ok_or_else(|| {
            "mcp_authoring_required: Local App has no approved MCP catalog".to_string()
        })?;
        let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
            .map_err(|error| error.to_string())?;
        let catalog_tools = mcp_catalog_tool_names(&catalog).map_err(|error| error.to_string())?;
        let mut settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
        settings = settings
            .reconcile_with_catalog(&catalog, false)
            .map_err(|error| error.to_string())?;
        if !catalog_tools.iter().any(|name| name == tool_name) {
            return Err(format!(
                "invalid_mcp_settings: tool {tool_name:?} is not in the active catalog"
            ));
        }
        let mut enabled_tools: std::collections::BTreeSet<String> =
            settings.enabled_tools.into_iter().collect();
        if enabled {
            enabled_tools.insert(tool_name.to_string());
        } else {
            enabled_tools.remove(tool_name);
        }
        settings.enabled_tools = catalog_tools
            .into_iter()
            .filter(|name| enabled_tools.contains(name))
            .collect();
        if settings.enabled_tools.is_empty() {
            settings.enabled = false;
        }
        save_mcp_settings(&layout, &settings, Some(expected_revision))
            .map_err(|error| error.to_string())?;
        drop(_guard);
        self.sync_managed_local_app_publication(app_id).await?;
        self.emit_managed_mcp_inventory().await
    }
    pub(crate) async fn sync_managed_local_app_publication(
        &self,
        app_id: &str,
    ) -> Result<(), String> {
        let Some(publisher) = self.publisher() else {
            return Ok(());
        };
        let layout = self.layout(app_id)?;
        let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
        let active_build_id =
            crate::app_build::active_build_id(&layout).map_err(|error| error.to_string())?;
        match local_apps::derive_publication_state(&manifest, active_build_id.as_deref(), false) {
            Ok(local_apps::AppPublicationState::Draft) => {
                publisher.unregister(app_id).await?;
                publisher.disconnect_app(app_id).await?;
            }
            Ok(
                local_apps::AppPublicationState::PublishedUnverified
                | local_apps::AppPublicationState::PublishedVerified,
            ) => {
                let Some(active) = manifest.active_mcp_catalog.as_ref() else {
                    publisher.unregister(app_id).await?;
                    publisher.disconnect_app(app_id).await?;
                    return Ok(());
                };
                let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                    .map_err(|error| error.to_string())?;
                let settings = load_mcp_settings(&layout)
                    .map_err(|error| error.to_string())?
                    .reconcile_with_catalog(&catalog, false)
                    .map_err(|error| error.to_string())?;
                let effective_surface = effective_tool_surface_sha256(
                    &active.tool_surface_sha256,
                    &settings.enabled_tools,
                )
                .map_err(|error| error.to_string())?;
                let runtime_enabled = settings.enabled
                    && !settings.enabled_tools.is_empty()
                    && active_build_id.as_deref() == Some(active.build_id.as_str());
                publisher
                    .publish(
                        &ManagedApp {
                            app_id: app_id.to_string(),
                            catalog_sha256: active.catalog_sha256.clone(),
                            effective_surface_sha256: effective_surface,
                        },
                        ManagedRuntime {
                            enabled: runtime_enabled,
                            enabled_tools: settings.enabled_tools,
                            widget: managed_mcp_widget_resource(&layout, &manifest.name, &catalog)?
                                .map(|(_, resource)| resource),
                        },
                    )
                    .await?;
            }
            Err(error) => {
                publisher.unregister(app_id).await?;
                return Err(error.to_string());
            }
        }
        Ok(())
    }
    pub async fn unregister_managed_local_app(&self, app_id: &str) -> Result<(), String> {
        let Some(publisher) = self.publisher() else {
            return Ok(());
        };
        publisher.unregister(app_id).await?;
        self.emit_managed_mcp_inventory().await?;
        Ok(())
    }
    pub(super) async fn rebind_active_mcp_catalog_to_current_build(
        &self,
        app_id: &str,
        layout: &AppLayout,
    ) -> Result<(), String> {
        let mut manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let Some(active) = manifest.active_mcp_catalog.clone() else {
            return Ok(());
        };
        let active_build_id = crate::app_build::active_build_id(layout)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "active_state_corrupt: published app is missing its active build".to_string()
            })?;
        if active.build_id == active_build_id {
            self.sync_managed_local_app_publication(app_id).await?;
            return Ok(());
        }
        let mut catalog = local_apps::load_mcp_catalog(layout, &active.catalog_sha256)
            .map_err(|error| error.to_string())?;
        if catalog.get("appId").and_then(Value::as_str) != Some(app_id)
            || catalog.get("buildId").and_then(Value::as_str) != Some(active.build_id.as_str())
        {
            return Err("active_state_corrupt: active MCP catalog identity mismatch".into());
        }
        let contexts = self.load_active_mcp_flow_contexts(layout).ok();
        let flow_contract_unchanged = contexts.as_ref().is_some_and(|contexts| {
            catalog
                .get("execution")
                .and_then(Value::as_array)
                .is_some_and(|bindings| {
                    !bindings.is_empty()
                        && bindings.iter().all(|binding| {
                            let flow_id = binding
                                .get("flow")
                                .and_then(|flow| flow.get("flowId"))
                                .and_then(Value::as_str);
                            let expected = binding.get("contextSha256").and_then(Value::as_str);
                            match (flow_id, expected) {
                                (Some(flow_id), Some(expected)) => contexts
                                    .get(flow_id)
                                    .and_then(|context| serde_json::to_value(context).ok())
                                    .and_then(|context| value_sha256(&context).ok())
                                    .is_some_and(|actual| actual == expected),
                                _ => false,
                            }
                        })
                })
        });
        if !flow_contract_unchanged {
            let _guard = self.mcp_settings_writes.lock().await;
            let mut settings = load_mcp_settings(layout).map_err(|error| error.to_string())?;
            if settings.enabled {
                let expected = settings.revision;
                settings.enabled = false;
                save_mcp_settings(layout, &settings, Some(expected))
                    .map_err(|error| error.to_string())?;
            }
            drop(_guard);
            self.sync_managed_local_app_publication(app_id).await?;
            self.emit_managed_mcp_inventory().await?;
            return Ok(());
        }
        catalog["buildId"] = Value::String(active_build_id.clone());
        let catalog_sha256 =
            local_apps::hash_mcp_catalog(catalog.clone()).map_err(|error| error.to_string())?;
        local_apps::save_mcp_catalog(layout, &catalog_sha256, &catalog)
            .map_err(|error| error.to_string())?;
        if let Some(current) = manifest.active_mcp_catalog.as_mut() {
            current.build_id = active_build_id;
            current.catalog_sha256 = catalog_sha256;
        }
        local_apps::save_manifest(layout, &manifest).map_err(|error| error.to_string())?;
        self.sync_managed_local_app_publication(app_id).await?;
        self.emit_managed_mcp_inventory().await?;
        Ok(())
    }
    pub async fn emit_managed_mcp_inventory(&self) -> Result<(), String> {
        let service = self.service()?;
        let publisher = self.publisher();
        let active_conversation = self.active_mcp_conversation.lock().await.clone();
        let pinned_apps: std::collections::HashSet<String> =
            if let (Some(publisher), Some(conversation_id)) =
                (publisher.as_ref(), active_conversation.as_deref())
            {
                publisher
                    .exposures(conversation_id)
                    .await
                    .into_iter()
                    .filter(|entry| entry.pinned)
                    .map(|entry| entry.app_id)
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
        let mut servers = Vec::new();
        for record in service.list_apps().await {
            let layout = self.layout(&record.id)?;
            let manifest = load_manifest(&layout).map_err(|error| error.to_string())?;
            let active_build_id =
                crate::app_build::active_build_id(&layout).map_err(|error| error.to_string())?;
            let ui_verification = self.qa_ui_verification_summary(&record.id).await;
            let publication = local_apps::derive_publication_state(
                &manifest,
                active_build_id.as_deref(),
                ui_verification.status == VerificationStatus::Passed,
            )
            .map_err(|error| error.to_string())?;
            if matches!(publication, local_apps::AppPublicationState::Draft) {
                continue;
            }
            let authoring_in_progress = local_apps::load_candidate_journal(&layout)
                .ok()
                .is_some_and(|journal| {
                    journal.app_id == record.id
                        && journal.stage < local_apps::McpAuthoringStage::Promoted
                });
            let mut settings = load_mcp_settings(&layout).map_err(|error| error.to_string())?;
            let mut status = if authoring_in_progress {
                AppMcpStatus::Authoring
            } else {
                AppMcpStatus::NeedsSetup
            };
            let mut server_name = format!("local_app_{}", record.id);
            let mut enabled = false;
            let mut enabled_tools = settings.enabled_tools.clone();
            let mut build_id = active_build_id.clone().unwrap_or_default();
            let mut catalog_sha256 = String::new();
            let mut tool_surface_sha256 = String::new();
            let mut tool_count = 0u32;
            let mut authoring_revision = 0u64;
            let mut widget = None;
            let mut tools = Vec::new();
            let mut mcp_verification = VerificationSummary {
                status: VerificationStatus::Unverified,
                summary: "No approved Local App MCP catalog is active yet.".into(),
                code: Some("needs_setup".into()),
            };

            if let Some(active) = manifest.active_mcp_catalog.as_ref() {
                let catalog = local_apps::load_mcp_catalog(&layout, &active.catalog_sha256)
                    .map_err(|error| error.to_string())?;
                // r3-never-wired-05: a catalog whose recorded identity does not
                // match this app and build used to `return Err(...)` from here,
                // which aborted the WHOLE managed-MCP inventory listing — every
                // other app vanished from both clients because ONE app's state
                // was corrupt. Mark this one app `Failed` and keep listing.
                // This is also the production producer for
                // `VerificationStatus::Failed`, which both clients
                // already render (`local_apps_verification_status_failed`) and
                // which had none.
                let identity_matches = catalog.get("appId").and_then(Value::as_str)
                    == Some(record.id.as_str())
                    && catalog.get("buildId").and_then(Value::as_str)
                        == Some(active.build_id.as_str());
                if !identity_matches {
                    status = AppMcpStatus::Error;
                    // `tools`, `catalog_sha256` and `tool_surface_sha256` keep
                    // their pre-catalog defaults, so nothing derived from the
                    // untrusted catalog is presented as a callable surface.
                    // (`enabled`/`enabled_tools` may still be overwritten below
                    // from the live registry — that reflects what is actually
                    // registered, which is a fact about the process, not a claim
                    // about this catalog.)
                    mcp_verification = VerificationSummary {
                        status: VerificationStatus::Failed,
                        summary:
                            "The approved MCP catalog does not match this app and build, so it \
                             cannot be trusted. Re-run MCP authoring to rebuild it."
                                .into(),
                        // No `clients/translations/` key exists for this code yet,
                        // so both clients fall back to `summary` verbatim (their
                        // `default:`/`else ->` arm) while the STATUS badge beside
                        // it is localized. Naming it anyway keeps the `code == nil`
                        // arm — which both clients map to "verification passed" —
                        // from ever being reached by a failure.
                        code: Some("active_state_corrupt".into()),
                    };
                } else {
                    settings = settings
                        .reconcile_with_catalog(&catalog, false)
                        .map_err(|error| error.to_string())?;
                    enabled_tools = settings.enabled_tools.clone();
                    let needs_revalidation =
                        active_build_id.as_deref() != Some(active.build_id.as_str());
                    enabled = settings.enabled && !enabled_tools.is_empty() && !needs_revalidation;
                    status = derive_mcp_status(
                        &manifest,
                        &AppMcpSettings {
                            enabled,
                            enabled_tools: enabled_tools.clone(),
                            ..settings.clone()
                        },
                        authoring_in_progress,
                        needs_revalidation,
                        false,
                    );
                    build_id = active.build_id.clone();
                    catalog_sha256 = active.catalog_sha256.clone();
                    tool_surface_sha256 = if enabled {
                        effective_tool_surface_sha256(&active.tool_surface_sha256, &enabled_tools)
                            .map_err(|error| error.to_string())?
                    } else {
                        active.tool_surface_sha256.clone()
                    };
                    authoring_revision = active.authoring_revision;
                    tools = mcp_tool_surfaces_from_catalog(&catalog)?;
                    tool_count = u32::try_from(tools.len()).unwrap_or(u32::MAX);
                    widget = managed_mcp_widget_resource(&layout, &record.name, &catalog)?
                        .map(|(widget, _)| widget);
                    mcp_verification = if needs_revalidation {
                        VerificationSummary {
                            status: VerificationStatus::Unverified,
                            summary: "The approved MCP catalog no longer matches the active build and must be revalidated.".into(),
                            code: Some("needs_revalidation".into()),
                        }
                    } else {
                        VerificationSummary {
                            status: VerificationStatus::Passed,
                            summary: "MCP schema, Flow, call and isolation verification passed."
                                .into(),
                            code: None,
                        }
                    };
                }
            }

            if let Some(publisher) = publisher.as_ref() {
                let published = publisher.published(&record.id).await;
                if let Some(published_name) = published.server_name {
                    server_name = published_name;
                }
                if let Some(published_enabled) = published.enabled {
                    enabled = published_enabled;
                }
                if let Some(published_tools) = published.enabled_tools {
                    enabled_tools = published_tools;
                }
            }
            let pinned_to_current_conversation = pinned_apps.contains(&record.id);
            servers.push(ManagedMcpServer {
                server_name,
                app_id: record.id,
                app_name: record.name,
                enabled,
                status: lower_managed_mcp_status(status),
                settings_revision: settings.revision,
                enabled_tools,
                pinned_to_current_conversation,
                build_id,
                catalog_sha256,
                tool_surface_sha256,
                tool_count,
                authoring_revision,
                publication_state: publication,
                mcp_verification,
                ui_verification,
                widget,
                tools,
            });
        }
        servers.sort_by(|left, right| left.app_id.cmp(&right.app_id));
        // r2-never-wired-01: `VerificationSummaryChanged` had zero producers
        // anywhere in the engine, so both clients' per-app verification
        // fields could never become non-nil. The publication/verification
        // triple is already computed per server above; derive the summary
        // event from the SAME values rather than recomputing them, so the
        // two events can never disagree.
        for server in &servers {
            self.event_sink
                .emit(HostEvent::VerificationSummaryChanged {
                    app_id: server.app_id.clone(),
                    publication_state: server.publication_state,
                    mcp_verification: server.mcp_verification.clone(),
                    ui_verification: server.ui_verification.clone(),
                })
                .await;
        }
        self.event_sink
            .emit(HostEvent::ManagedMcpInventoryChanged { servers })
            .await;
        Ok(())
    }
}
