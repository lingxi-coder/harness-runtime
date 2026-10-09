//! Desktop local-IDE discovery and lifecycle controller.
//!
//! The controller is intentionally provider-neutral: it consumes the
//! lockfile contract and the existing MCP registry, then exposes only the
//! core::host [`IdeHandle`] seam to command/orchestrator code. Local
//! lockfile tokens are held only long enough to build an `SseIde`/`WsIde`
//! transport spec and are never included in a status snapshot or log line.

use async_trait::async_trait;
use bridge::lockfile::{discover_all, IdeLockfile};
use lingxi_core::host::{IdeEndpointInfo, IdeHandle, IdeStatus, IdeTransport, McpTransportSpec};
use mcp::{ConfigScope, McpConnectionState, McpRegistry, McpServerConfig, McpServerMetadata};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const IDE_SERVER_NAME: &str = "ide";

/// Live desktop implementation of [`lingxi_core::host::IdeHandle`].
pub struct DesktopIdeHandle {
    ide_dir: PathBuf,
    mcp_registry: Arc<McpRegistry>,
    selected: RwLock<Option<String>>,
    lifecycle: Mutex<()>,
    #[cfg(test)]
    post_connect_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl DesktopIdeHandle {
    /// Build a controller over the session's MCP registry.
    #[must_use]
    pub fn new(ide_dir: PathBuf, mcp_registry: Arc<McpRegistry>) -> Self {
        Self {
            ide_dir,
            mcp_registry,
            selected: RwLock::new(None),
            lifecycle: Mutex::new(()),
            #[cfg(test)]
            post_connect_hook: None,
        }
    }

    #[cfg(test)]
    fn with_post_connect_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.post_connect_hook = Some(hook);
        self
    }

    /// Directory scanned for `<port>.lock` IDE endpoints.
    #[must_use]
    pub fn ide_dir(&self) -> &Path {
        &self.ide_dir
    }

    fn endpoint_id(endpoint: &IdeLockfile) -> String {
        endpoint.path().to_string_lossy().into_owned()
    }

    fn endpoint_info(endpoint: &IdeLockfile, connected: bool) -> IdeEndpointInfo {
        IdeEndpointInfo {
            id: Self::endpoint_id(endpoint),
            name: endpoint.ide_name().to_string(),
            transport: if endpoint.transport() == "sse" {
                IdeTransport::Sse
            } else {
                IdeTransport::Ws
            },
            port: endpoint.port(),
            workspace_folders: endpoint.workspace_folders().to_vec(),
            running_in_windows: endpoint.running_in_windows(),
            connected,
        }
    }

    async fn registry_connected_for(&self, endpoint: &IdeLockfile) -> bool {
        let connections = self.mcp_registry.connections.read().await;
        matches!(
            connections.get(IDE_SERVER_NAME),
            Some(McpConnectionState::Connected { config, .. })
                if is_owned_ide_config(config)
                    && ide_spec_matches_endpoint(&config.spec, endpoint)
        )
    }

    async fn current_slot_config(&self) -> Option<McpServerConfig> {
        let connections = self.mcp_registry.connections.read().await;
        connections
            .get(IDE_SERVER_NAME)
            .map(|state| state.config().clone())
    }

    async fn remove_expected_owned_config(
        &self,
        expected: &McpServerConfig,
    ) -> Result<bool, String> {
        if !is_owned_ide_config(expected) {
            return Ok(false);
        }
        self.mcp_registry
            .remove_without_revoking_auth_if_config(IDE_SERVER_NAME, expected)
            .await
            .map_err(|error| error.to_string())
    }

    async fn retire_owned_slot_or_reject_conflict(&self) -> Result<(), String> {
        let Some(current) = self.current_slot_config().await else {
            return Ok(());
        };
        if !is_owned_ide_config(&current) {
            return Err(ide_name_conflict());
        }
        if self.remove_expected_owned_config(&current).await? {
            Ok(())
        } else {
            Err("the MCP server named \"ide\" changed while connecting; retry /ide".to_string())
        }
    }

    async fn retire_current_owned_slot(&self) -> Result<bool, String> {
        let Some(current) = self.current_slot_config().await else {
            return Ok(false);
        };
        self.remove_expected_owned_config(&current).await
    }

    async fn status_locked(&self) -> IdeStatus {
        let endpoints = discover_all(&self.ide_dir);
        let selected = self.selected.read().await.clone();
        let mut infos = Vec::with_capacity(endpoints.len());
        for endpoint in &endpoints {
            let id = Self::endpoint_id(endpoint);
            let connected = selected.as_deref() == Some(id.as_str())
                && self.registry_connected_for(endpoint).await;
            infos.push(Self::endpoint_info(endpoint, connected));
        }
        if selected.as_deref().is_some_and(|id| {
            !infos
                .iter()
                .any(|endpoint| endpoint.id == id && endpoint.connected)
        }) {
            drop(selected);
            *self.selected.write().await = None;
            return IdeStatus {
                endpoints: infos,
                selected: None,
            };
        }
        IdeStatus {
            endpoints: infos,
            selected,
        }
    }

    async fn connect_endpoint(&self, endpoint: IdeLockfile) -> Result<IdeStatus, String> {
        let _guard = self.lifecycle.lock().await;
        // The caller's discovery snapshot may be stale by the time this
        // lifecycle lock is acquired. Re-read the same path through secure
        // discovery so a replaced lockfile cannot make us dial with an old
        // token or endpoint metadata.
        let endpoint_id = Self::endpoint_id(&endpoint);
        let endpoint = discover_all(&self.ide_dir)
            .into_iter()
            .find(|candidate| Self::endpoint_id(candidate) == endpoint_id)
            .ok_or_else(|| "IDE endpoint is unavailable or failed security checks".to_string())?;
        // A failed replacement must not leave a previous selection claiming
        // to be connected after its registry slot is retired below.
        *self.selected.write().await = None;
        // Retire only a prior controller-owned generation. A user may legally
        // configure a static MCP server named `ide`; an IDE lifecycle action
        // must never disconnect or overwrite that unrelated server.
        self.retire_owned_slot_or_reject_conflict().await?;
        let spec = match endpoint.transport() {
            "sse" => McpTransportSpec::SseIde {
                url: endpoint.endpoint_url(),
                ide_name: endpoint.ide_name().to_string(),
                auth_token: Some(endpoint.auth_token().to_string()),
                ide_running_in_windows: endpoint.running_in_windows(),
            },
            "ws" => McpTransportSpec::WsIde {
                url: endpoint.endpoint_url(),
                ide_name: endpoint.ide_name().to_string(),
                auth_token: Some(endpoint.auth_token().to_string()),
                ide_running_in_windows: endpoint.running_in_windows(),
            },
            transport => return Err(format!("unsupported IDE transport {transport:?}")),
        };
        let config = McpServerConfig {
            name: IDE_SERVER_NAME.to_string(),
            spec,
            scope: ConfigScope::Dynamic,
            disabled: false,
            // IDE catalogs are live and token-bound. Do not serve a stale
            // discovery-cache entry after an extension rotates its lockfile.
            discovery_cache: Some(false),
            timeout_ms: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: McpServerMetadata {
                transport: Some("ide".to_string()),
                ..McpServerMetadata::default()
            },
        };
        let expected_config = config.clone();
        if let Err(error) = self.mcp_registry.connect(config).await {
            // `McpRegistry::connect` retains a Disconnected state for retry;
            // conditionally remove only this failed generation. A settings
            // reload may already have installed a static same-named server.
            let _ = self.remove_expected_owned_config(&expected_config).await;
            return Err(redact_secret(&error.to_string(), endpoint.auth_token()));
        }

        #[cfg(test)]
        if let Some(hook) = &self.post_connect_hook {
            hook();
        }

        // The lockfile is an authentication generation. Re-attest it after
        // the asynchronous MCP handshake: deletion or token rotation during
        // connect must retire this exact dynamic config before its tools can be
        // returned as a usable IDE session.
        let still_current = match discover_all(&self.ide_dir)
            .into_iter()
            .find(|candidate| Self::endpoint_id(candidate) == endpoint_id)
        {
            Some(current) => self.registry_connected_for(&current).await,
            None => false,
        };
        if !still_current {
            let _ = self.remove_expected_owned_config(&expected_config).await;
            return Err("IDE endpoint changed while connecting; retry /ide connect".to_string());
        }

        *self.selected.write().await = Some(Self::endpoint_id(&endpoint));
        let status = self.status_locked().await;
        if !status.connected() {
            let _ = self.remove_expected_owned_config(&expected_config).await;
            return Err("IDE endpoint changed while connecting; retry /ide connect".to_string());
        }
        Ok(status)
    }

    async fn open_current_cwd(&self, cwd: PathBuf) -> Result<String, String> {
        let _guard = self.lifecycle.lock().await;
        let selected = self
            .selected
            .read()
            .await
            .clone()
            .ok_or_else(|| "no IDE endpoint is connected".to_string())?;
        let endpoint = discover_all(&self.ide_dir)
            .into_iter()
            .find(|endpoint| Self::endpoint_id(endpoint) == selected)
            .ok_or_else(|| "the selected IDE endpoint is no longer available".to_string())?;
        if !self.registry_connected_for(&endpoint).await {
            return Err("the selected IDE endpoint is not connected".to_string());
        }
        let tool_name = {
            let connections = self.mcp_registry.connections.read().await;
            let Some(McpConnectionState::Connected { tools, .. }) =
                connections.get(IDE_SERVER_NAME)
            else {
                return Err("the selected IDE endpoint is not connected".to_string());
            };
            tools
                .iter()
                .find(|tool| {
                    let lower = tool.tool_name.to_ascii_lowercase();
                    lower == "openfile" || lower == "open_file"
                })
                .map(|tool| tool.tool_name.clone())
                .ok_or_else(|| {
                    "the selected IDE does not advertise the openFile operation".to_string()
                })?
        };
        let full_name = format!("mcp__{IDE_SERVER_NAME}__{tool_name}");
        let result = self
            .mcp_registry
            .call_tool_with_auth_retry(
                IDE_SERVER_NAME,
                &full_name,
                lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                    json!({ "filePath": cwd.to_string_lossy() }),
                ),
                None,
                None,
            )
            .await
            .map_err(|error| redact_secret(&error.to_string(), endpoint.auth_token()))?;
        if result.is_error {
            return Err(tool_result_text(result.content, endpoint.auth_token()));
        }
        Ok(format!(
            "Opened {} in {}",
            cwd.display(),
            endpoint.ide_name()
        ))
    }
}

#[async_trait]
impl IdeHandle for DesktopIdeHandle {
    async fn status(&self) -> IdeStatus {
        let _guard = self.lifecycle.lock().await;
        let status = self.status_locked().await;
        if status.selected.is_none() {
            // A lockfile can disappear while the MCP socket is still alive.
            // Conditional removal prevents a concurrent settings refresh from
            // replacing the slot with a static server between check and tear
            // down.
            let _ = self.retire_current_owned_slot().await;
        }
        status
    }

    async fn connect(&self, endpoint_id: &str) -> Result<IdeStatus, String> {
        let endpoint = discover_all(&self.ide_dir)
            .into_iter()
            .find(|endpoint| Self::endpoint_id(endpoint) == endpoint_id)
            .ok_or_else(|| "IDE endpoint is unavailable or failed security checks".to_string())?;
        self.connect_endpoint(endpoint).await
    }

    async fn disconnect(&self) -> Result<IdeStatus, String> {
        let _guard = self.lifecycle.lock().await;
        if let Some(current) = self.current_slot_config().await {
            if !is_owned_ide_config(&current) {
                return Err(ide_name_conflict());
            }
            if !self.remove_expected_owned_config(&current).await? {
                return Err(
                    "the MCP server named \"ide\" changed while disconnecting; retry /ide"
                        .to_string(),
                );
            }
        }
        *self.selected.write().await = None;
        Ok(self.status_locked().await)
    }

    async fn open(&self, cwd: PathBuf) -> Result<String, String> {
        self.open_current_cwd(cwd).await
    }

    async fn auto_connect_if_single(&self) -> Result<bool, String> {
        let endpoints = discover_all(&self.ide_dir);
        if endpoints.len() != 1 {
            return Ok(false);
        }
        self.connect_endpoint(endpoints.into_iter().next().expect("length checked"))
            .await
            .map(|_| true)
    }
}

fn is_owned_ide_config(config: &McpServerConfig) -> bool {
    config.scope == ConfigScope::Dynamic
        && config.metadata.transport.as_deref() == Some("ide")
        && matches!(
            &config.spec,
            McpTransportSpec::SseIde { .. } | McpTransportSpec::WsIde { .. }
        )
}

fn ide_name_conflict() -> String {
    "the MCP server name \"ide\" is already used by a non-IDE configuration".to_string()
}

fn ide_spec_matches_endpoint(spec: &McpTransportSpec, endpoint: &IdeLockfile) -> bool {
    match spec {
        McpTransportSpec::SseIde {
            url, auth_token, ..
        }
        | McpTransportSpec::WsIde {
            url, auth_token, ..
        } => {
            url == &endpoint.endpoint_url() && auth_token.as_deref() == Some(endpoint.auth_token())
        }
        _ => false,
    }
}

fn tool_result_text(content: serde_json::Value, secret: &str) -> String {
    let rendered = content
        .as_array()
        .and_then(|items| {
            items.iter().find_map(|item| {
                item.get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| content.to_string());
    redact_secret(&rendered, secret)
}

fn redact_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_harness::mocks::MockMcpTransport;

    fn mock_registry() -> Arc<McpRegistry> {
        Arc::new(McpRegistry::new(Arc::new(MockMcpTransport::new())))
    }

    fn static_ide_config() -> McpServerConfig {
        McpServerConfig {
            name: IDE_SERVER_NAME.to_string(),
            spec: McpTransportSpec::InProcess {
                registry_key: "user-static-ide".to_string(),
            },
            scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            disabled: false,
            discovery_cache: None,
            timeout_ms: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: McpServerMetadata::default(),
        }
    }

    async fn assert_static_ide_slot_survives(registry: &McpRegistry) {
        let connections = registry.connections.read().await;
        let config = connections
            .get(IDE_SERVER_NAME)
            .expect("static ide slot must remain")
            .config();
        assert_eq!(
            config.scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
        );
        assert!(matches!(
            &config.spec,
            McpTransportSpec::InProcess { registry_key }
                if registry_key == "user-static-ide"
        ));
    }

    #[test]
    fn endpoint_matching_is_transport_neutral() {
        let dir = TempDir::new().unwrap();
        let lockfile = bridge::IdeLockfile::new_for_ide_dir(
            dir.path().to_path_buf(),
            43123,
            vec![PathBuf::from("/workspace")],
        );
        assert!(ide_spec_matches_endpoint(
            &McpTransportSpec::WsIde {
                url: "ws://127.0.0.1:43123".into(),
                ide_name: "LingXi".into(),
                auth_token: Some(lockfile.auth_token().into()),
                ide_running_in_windows: false,
            },
            &lockfile,
        ));
    }

    #[test]
    fn tool_errors_never_echo_a_token() {
        let token = "local-token";
        let value =
            serde_json::json!([{ "type": "text", "text": format!("editor refused: {token}") }]);
        let rendered = tool_result_text(value, token);
        assert_eq!(rendered, "editor refused: <redacted>");
        assert!(!rendered.contains(token));
    }

    #[tokio::test]
    async fn post_connect_token_rotation_retires_only_the_stale_dynamic_generation() {
        let dir = TempDir::new().unwrap();
        let first = bridge::IdeLockfile::new_for_ide_dir(
            dir.path().to_path_buf(),
            43118,
            vec![dir.path().to_path_buf()],
        );
        first.write().unwrap();
        let original_token = first.auth_token().to_string();
        let endpoint_id = first.path().to_string_lossy().into_owned();

        let replacement = bridge::IdeLockfile::new_for_ide_dir(
            dir.path().to_path_buf(),
            43118,
            vec![dir.path().to_path_buf()],
        );
        let replacement_token = replacement.auth_token().to_string();
        assert_ne!(original_token, replacement_token);
        let path = first.path();
        let rotate = Arc::new(move || {
            std::fs::remove_file(&path).unwrap();
            replacement.write().unwrap();
        });

        let registry = mock_registry();
        let handle = DesktopIdeHandle::new(dir.path().to_path_buf(), registry.clone())
            .with_post_connect_hook(rotate);
        let error = handle.connect(&endpoint_id).await.unwrap_err();

        assert!(error.contains("changed while connecting"));
        assert!(!error.contains(&original_token));
        assert!(!error.contains(&replacement_token));
        assert!(
            registry
                .connections
                .read()
                .await
                .get(IDE_SERVER_NAME)
                .is_none(),
            "the stale dynamic generation must be retired immediately"
        );
        assert!(handle.status().await.selected.is_none());
    }

    #[tokio::test]
    async fn connect_rejects_and_preserves_a_static_server_named_ide() {
        let dir = TempDir::new().unwrap();
        let endpoint = bridge::IdeLockfile::new_for_ide_dir(
            dir.path().to_path_buf(),
            43117,
            vec![dir.path().to_path_buf()],
        );
        endpoint.write().unwrap();
        let registry = mock_registry();
        registry.connect(static_ide_config()).await.unwrap();
        let handle = DesktopIdeHandle::new(dir.path().to_path_buf(), registry.clone());

        let error = handle
            .connect(&endpoint.path().to_string_lossy())
            .await
            .unwrap_err();

        assert!(error.contains("non-IDE configuration"));
        assert_static_ide_slot_survives(&registry).await;
    }

    #[tokio::test]
    async fn disconnect_rejects_and_preserves_a_static_server_named_ide() {
        let dir = TempDir::new().unwrap();
        let registry = mock_registry();
        registry.connect(static_ide_config()).await.unwrap();
        let handle = DesktopIdeHandle::new(dir.path().to_path_buf(), registry.clone());

        let error = handle.disconnect().await.unwrap_err();

        assert!(error.contains("non-IDE configuration"));
        assert_static_ide_slot_survives(&registry).await;
    }

    #[tokio::test]
    async fn status_discovers_multiple_secure_endpoints_without_tokens() {
        let dir = TempDir::new().unwrap();
        let first = bridge::IdeLockfile::new_for_ide_dir(
            dir.path().to_path_buf(),
            43121,
            vec![PathBuf::from("/workspace/one")],
        );
        let second = bridge::IdeLockfile::new_for_ide_dir(
            dir.path().to_path_buf(),
            43122,
            vec![PathBuf::from("/workspace/two")],
        );
        first.write().unwrap();
        second.write().unwrap();
        let registry = Arc::new(McpRegistry::new(Arc::new(
            platform_posix::PosixMcpTransport::new(),
        )));
        let handle = DesktopIdeHandle::new(dir.path().to_path_buf(), registry);
        let status = handle.status().await;
        assert_eq!(status.endpoints.len(), 2);
        assert!(status.selected.is_none());
        assert!(status.endpoints.iter().all(|endpoint| !endpoint.connected));
        let rendered = serde_json::to_string(&status).unwrap();
        assert!(!rendered.contains(first.auth_token()));
        assert!(!rendered.contains(second.auth_token()));
    }

    #[tokio::test]
    async fn auto_connect_requires_exactly_one_secure_endpoint() {
        let dir = TempDir::new().unwrap();
        for port in [43119, 43120] {
            bridge::IdeLockfile::new_for_ide_dir(dir.path().to_path_buf(), port, vec![])
                .write()
                .unwrap();
        }
        let registry = Arc::new(McpRegistry::new(Arc::new(
            platform_posix::PosixMcpTransport::new(),
        )));
        let handle = DesktopIdeHandle::new(dir.path().to_path_buf(), registry);
        assert!(!handle.auto_connect_if_single().await.unwrap());
        assert!(handle.status().await.selected.is_none());
    }

    #[tokio::test]
    async fn disconnect_without_selection_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(McpRegistry::new(Arc::new(
            platform_posix::PosixMcpTransport::new(),
        )));
        let handle = DesktopIdeHandle::new(dir.path().to_path_buf(), registry);
        let status = handle.disconnect().await.unwrap();
        assert!(status.endpoints.is_empty());
        assert!(status.selected.is_none());
    }
}
