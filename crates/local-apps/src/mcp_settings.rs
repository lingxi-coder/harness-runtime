//! Host-owned MCP settings persisted independently from the app manifest.

use crate::error::AppError;
use crate::manifest::{AppLayout, AppManifest};
use lingxi_core::host::rooted_fs::{self, AtomicWriteOptions};
use lingxi_core::host::FsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const MAX_SETTINGS_BYTES: u64 = 256 * 1024;

/// Settings filename under `apps/<id>/mcp`.
pub const MCP_SETTINGS_FILE: &str = "settings.json";
/// Schema version for the host-owned MCP settings file.
pub const MCP_SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Host-owned toggle state for one Local App MCP service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMcpSettings {
    /// Persisted MCP settings schema version.
    pub schema_version: u32,
    /// Monotonic settings revision bumped by the Host on each write.
    pub revision: u64,
    /// Whether this app's MCP service is exposed to the LLM.
    pub enabled: bool,
    /// Whitelist of catalog tool names currently enabled for model use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
}

impl Default for AppMcpSettings {
    fn default() -> Self {
        Self {
            schema_version: MCP_SETTINGS_SCHEMA_VERSION,
            revision: 0,
            enabled: false,
            enabled_tools: Vec::new(),
        }
    }
}

impl AppMcpSettings {
    /// Validate persisted shape and tool ids.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != MCP_SETTINGS_SCHEMA_VERSION {
            return Err(AppError::InvalidRequest(format!(
                "MCP settings schemaVersion {} is unsupported (expected {MCP_SETTINGS_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        let mut seen = BTreeSet::new();
        for tool_name in &self.enabled_tools {
            if tool_name.trim().is_empty()
                || tool_name.len() > 128
                || tool_name.contains('/')
                || tool_name.contains('\\')
            {
                return Err(AppError::InvalidRequest(format!(
                    "invalid MCP tool name {tool_name:?} in settings"
                )));
            }
            if !seen.insert(tool_name.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "duplicate MCP tool name {tool_name:?} in settings"
                )));
            }
        }
        Ok(())
    }

    /// Reconcile toggles against the currently approved catalog.
    ///
    /// Removed tools are dropped. When `enable_new_tools` is true, tools newly
    /// present in the catalog are added to the whitelist automatically.
    pub fn reconcile_with_catalog(
        &self,
        catalog: &Value,
        enable_new_tools: bool,
    ) -> Result<Self, AppError> {
        let catalog_tools = mcp_catalog_tool_names(catalog)?;
        let previously_enabled: BTreeSet<&str> =
            self.enabled_tools.iter().map(String::as_str).collect();
        let mut enabled_tools = Vec::new();
        for tool_name in catalog_tools {
            if enable_new_tools || previously_enabled.contains(tool_name.as_str()) {
                enabled_tools.push(tool_name);
            }
        }
        Ok(Self {
            schema_version: self.schema_version,
            revision: self.revision,
            enabled: self.enabled,
            enabled_tools,
        })
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Coarse service status derived from trusted Host state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMcpStatus {
    /// No approved catalog exists yet.
    NeedsSetup,
    /// The app is currently authoring or validating an MCP revision.
    Authoring,
    /// A previously approved catalog no longer matches the active app contract.
    NeedsRevalidation,
    /// An approved catalog exists but service exposure is disabled.
    Disabled,
    /// The approved catalog is active and currently exposed.
    Enabled,
    /// Host state is inconsistent or a fatal MCP error is latched.
    Error,
}

/// Derive the effective MCP status for one app.
#[must_use]
pub fn derive_mcp_status(
    manifest: &AppManifest,
    settings: &AppMcpSettings,
    authoring_in_progress: bool,
    needs_revalidation: bool,
    has_error: bool,
) -> AppMcpStatus {
    if has_error || (settings.enabled && manifest.active_mcp_catalog.is_none()) {
        return AppMcpStatus::Error;
    }
    if authoring_in_progress {
        return AppMcpStatus::Authoring;
    }
    if needs_revalidation {
        return AppMcpStatus::NeedsRevalidation;
    }
    if settings.enabled {
        return AppMcpStatus::Enabled;
    }
    if manifest.active_mcp_catalog.is_some() {
        return AppMcpStatus::Disabled;
    }
    AppMcpStatus::NeedsSetup
}

/// Extract tool names from an immutable MCP catalog in declared order.
pub fn mcp_catalog_tool_names(catalog: &Value) -> Result<Vec<String>, AppError> {
    let tools = catalog
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::InvalidRequest("MCP catalog must contain a tools array".into()))?;
    let mut out = Vec::with_capacity(tools.len());
    let mut seen = BTreeSet::new();
    for tool in tools {
        let name = tool
            .get("definition")
            .and_then(|definition| definition.get("name"))
            .or_else(|| tool.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AppError::InvalidRequest(
                    "MCP catalog tools must declare definition.name or name".into(),
                )
            })?;
        if !seen.insert(name) {
            return Err(AppError::InvalidRequest(format!(
                "duplicate MCP catalog tool name {name:?}"
            )));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// Stable digest of the currently effective model-visible tool surface.
///
/// The digest intentionally excludes the service-level enabled flag so a
/// disable/reenable cycle does not force stale-tool churn when the allowed
/// tool set is unchanged.
pub fn effective_tool_surface_sha256(
    active_tool_surface_sha256: &str,
    enabled_tools: &[String],
) -> Result<String, AppError> {
    if !is_sha256_hex(active_tool_surface_sha256) {
        return Err(AppError::InvalidRequest(
            "active MCP tool surface digest must be 64 lowercase hex bytes".into(),
        ));
    }
    let mut canonical_tools = BTreeSet::new();
    for tool_name in enabled_tools {
        if tool_name.trim().is_empty()
            || tool_name.len() > 128
            || tool_name.contains('/')
            || tool_name.contains('\\')
        {
            return Err(AppError::InvalidRequest(format!(
                "invalid MCP tool name {tool_name:?} in effective surface"
            )));
        }
        if !canonical_tools.insert(tool_name.as_str()) {
            return Err(AppError::InvalidRequest(format!(
                "duplicate MCP tool name {tool_name:?} in effective surface"
            )));
        }
    }
    let bytes = serde_json::to_vec(&serde_json::json!({
        "toolSurfaceSha256": active_tool_surface_sha256,
        "enabledTools": canonical_tools.into_iter().collect::<Vec<_>>(),
    }))
    .map_err(|error| AppError::Io(format!("serialize effective MCP tool surface: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Load settings, defaulting to "disabled with no enabled tools" before the
/// first explicit write.
pub fn load_mcp_settings(layout: &AppLayout) -> Result<AppMcpSettings, AppError> {
    let path = layout.mcp_settings_rel();
    let body = match rooted_fs::read_to_string_limited(layout.root(), &path, MAX_SETTINGS_BYTES) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(AppMcpSettings::default()),
        Err(other) => return Err(AppError::from_fs("read MCP settings", &other)),
    };
    let settings: AppMcpSettings = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("MCP settings: {error}")))?;
    settings
        .validate()
        .map_err(|error| AppError::StorageCorrupt(format!("invalid MCP settings: {error}")))?;
    Ok(settings)
}

/// Persist settings atomically with an optional expected-revision CAS.
pub fn save_mcp_settings(
    layout: &AppLayout,
    settings: &AppMcpSettings,
    expected_revision: Option<u64>,
) -> Result<AppMcpSettings, AppError> {
    settings.validate()?;
    layout.initialize()?;
    let current = load_mcp_settings(layout)?;
    if let Some(expected) = expected_revision {
        if expected != current.revision {
            return Err(AppError::RevisionConflict {
                expected,
                actual: current.revision,
            });
        }
    }
    let persisted = AppMcpSettings {
        schema_version: MCP_SETTINGS_SCHEMA_VERSION,
        revision: current.revision + 1,
        enabled: settings.enabled,
        enabled_tools: settings.enabled_tools.clone(),
    };
    persisted.validate()?;
    let mut body = serde_json::to_vec_pretty(&persisted)
        .map_err(|error| AppError::Io(format!("serialize MCP settings: {error}")))?;
    body.push(b'\n');
    rooted_fs::atomic_write(
        layout.root(),
        &layout.mcp_settings_rel(),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write MCP settings", &error))?;
    Ok(persisted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        AppDependencySnapshot, AppManifest, AppMcpCatalogRef, AppRuntimeProfileBinding, AppSurface,
        AppTemplateOrigin,
    };
    use crate::runtime_v2::RUNTIME_API_MAJOR;
    use crate::types::AppRuntimeProfile;

    fn tempdir_and_layout() -> (tempfile::TempDir, AppLayout) {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "mcpapp01").unwrap();
        (root, layout)
    }

    fn scaffolded_manifest() -> AppManifest {
        AppManifest {
            schema_version: crate::types::APPS_SCHEMA_VERSION,
            runtime_api_version: RUNTIME_API_MAJOR,
            app_id: "mcpapp01".into(),
            revision: 3,
            name: "MCP".into(),
            collections: Vec::new(),
            allowed_domains: Vec::new(),
            capabilities: Vec::new(),
            device_context: None,
            surface: Some(AppSurface::Dom),
            runtime_profile: Some(AppRuntimeProfileBinding {
                family: AppRuntimeProfile::ReactDom,
                revision: 1,
                contract_sha256: "a".repeat(64),
            }),
            dependency_snapshot: Some(AppDependencySnapshot {
                requested_sha256: "b".repeat(64),
                package_sha256: "c".repeat(64),
                lockfile_sha256: "d".repeat(64),
                dependency_tree_sha256: "e".repeat(64),
                sbom_sha256: "f".repeat(64),
                toolchain_key: "pnpm@11/node@24".into(),
                verified_profile_contract_sha256: "a".repeat(64),
            }),
            template_origin: Some(AppTemplateOrigin {
                plugin_id: AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
                plugin_version: "builtin".into(),
                template_id: "react-dom-r2".into(),
                template_sha256: "a".repeat(64),
            }),
            active_mcp_catalog: Some(AppMcpCatalogRef {
                build_id: "build-1".into(),
                manifest_revision: 3,
                authoring_revision: 1,
                user_goal_sha256: "0".repeat(64),
                proposal_sha256: "1".repeat(64),
                approval_contract_sha256: "2".repeat(64),
                tool_surface_sha256: "3".repeat(64),
                catalog_sha256: "4".repeat(64),
                mcp_verification_sha256: "5".repeat(64),
            }),
        }
    }

    #[test]
    fn missing_settings_default_to_disabled() {
        let (_root, layout) = tempdir_and_layout();
        assert_eq!(
            load_mcp_settings(&layout).unwrap(),
            AppMcpSettings::default()
        );
    }

    #[test]
    fn settings_save_uses_expected_revision_cas() {
        let (_root, layout) = tempdir_and_layout();
        let first = save_mcp_settings(
            &layout,
            &AppMcpSettings {
                enabled: true,
                enabled_tools: vec!["search".into()],
                ..AppMcpSettings::default()
            },
            Some(0),
        )
        .unwrap();
        assert_eq!(first.revision, 1);
        let err = save_mcp_settings(
            &layout,
            &AppMcpSettings {
                enabled: false,
                enabled_tools: vec!["search".into()],
                ..AppMcpSettings::default()
            },
            Some(0),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            AppError::RevisionConflict {
                expected: 0,
                actual: 1
            }
        ));
    }

    #[test]
    fn disabling_service_preserves_tool_whitelist() {
        let (_root, layout) = tempdir_and_layout();
        let saved = save_mcp_settings(
            &layout,
            &AppMcpSettings {
                enabled: false,
                enabled_tools: vec!["search".into(), "lookup".into()],
                ..AppMcpSettings::default()
            },
            Some(0),
        )
        .unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.enabled_tools, vec!["search", "lookup"]);
        assert_eq!(load_mcp_settings(&layout).unwrap(), saved);
    }

    #[test]
    fn reconcile_drops_removed_tools_and_can_enable_new_tools() {
        let current = AppMcpSettings {
            enabled: true,
            enabled_tools: vec!["search".into(), "legacy".into()],
            ..AppMcpSettings::default()
        };
        let catalog = serde_json::json!({
            "tools": [
                {"definition": {"name": "search"}},
                {"definition": {"name": "compose"}}
            ]
        });
        assert_eq!(
            current
                .reconcile_with_catalog(&catalog, false)
                .unwrap()
                .enabled_tools,
            vec!["search"]
        );
        assert_eq!(
            current
                .reconcile_with_catalog(&catalog, true)
                .unwrap()
                .enabled_tools,
            vec!["search", "compose"]
        );
    }

    #[test]
    fn effective_surface_digest_is_order_independent_and_sensitive_to_tool_set() {
        let left =
            effective_tool_surface_sha256(&"a".repeat(64), &["search".into(), "compose".into()])
                .unwrap();
        let right =
            effective_tool_surface_sha256(&"a".repeat(64), &["compose".into(), "search".into()])
                .unwrap();
        let different = effective_tool_surface_sha256(&"a".repeat(64), &["search".into()]).unwrap();
        assert_eq!(left, right);
        assert_ne!(left, different);
    }

    #[test]
    fn status_distinguishes_setup_disabled_enabled_revalidation_and_errors() {
        let manifest = scaffolded_manifest();
        assert_eq!(
            derive_mcp_status(&manifest, &AppMcpSettings::default(), false, false, false),
            AppMcpStatus::Disabled
        );
        assert_eq!(
            derive_mcp_status(
                &manifest,
                &AppMcpSettings {
                    enabled: true,
                    enabled_tools: vec!["search".into()],
                    ..AppMcpSettings::default()
                },
                false,
                false,
                false,
            ),
            AppMcpStatus::Enabled
        );
        assert_eq!(
            derive_mcp_status(&manifest, &AppMcpSettings::default(), true, false, false),
            AppMcpStatus::Authoring
        );
        assert_eq!(
            derive_mcp_status(&manifest, &AppMcpSettings::default(), false, true, false),
            AppMcpStatus::NeedsRevalidation
        );
        let mut no_catalog = manifest.clone();
        no_catalog.active_mcp_catalog = None;
        assert_eq!(
            derive_mcp_status(&no_catalog, &AppMcpSettings::default(), false, false, false),
            AppMcpStatus::NeedsSetup
        );
        assert_eq!(
            derive_mcp_status(
                &no_catalog,
                &AppMcpSettings {
                    enabled: true,
                    enabled_tools: vec!["search".into()],
                    ..AppMcpSettings::default()
                },
                false,
                false,
                false,
            ),
            AppMcpStatus::Error
        );
    }
}
