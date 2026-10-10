use async_trait::async_trait;
use jsonrpc::Connection;
use lingxi_core::types::McpConnectionId;
use lsp::{LspClient, LspPathMapper};
use lsp_types::Url;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::Duration;

use lingxi_core::host::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
    NewDiagnosticsSource,
};
use mobile_linux_api::{
    map_host_path_to_guest, MobileLinuxRuntime, MountPurpose, MountSpec, NetworkPolicy,
    RawStdioOpenRequest, RawStdioSessionHandle, ResourceLimits, SandboxBackend,
};

pub(crate) const GLOBAL_TYPESCRIPT_LSP_SERVER_NAME: &str =
    "plugin:lingxi-typescript-lsp:typescript-native";

pub(crate) fn global_typescript_lsp_plugin_id() -> lingxi_core::types::PluginId {
    lingxi_core::types::PluginId::parse_prefixed("7a63cd80-0da6-4e77-9c47-a358da94f5bb")
        .expect("hard-coded TypeScript LSP plugin id")
}

/// Host-owned, immutable TypeScript 7 descriptor. Keeping this out of the
/// plugin manifests makes the global setting independent of any plugin's
/// enable/disable lifecycle while still using the registry's plugin-only
/// registration boundary.
pub(crate) fn global_typescript_lsp_config() -> LspServerConfig {
    LspServerConfig {
        name: GLOBAL_TYPESCRIPT_LSP_SERVER_NAME.to_string(),
        command: "/opt/lingxi/toolchains/typescript/7.0.2/tsc".to_string(),
        args: vec!["--lsp".to_string(), "--stdio".to_string()],
        extension_to_language: HashMap::from([
            (".js".to_string(), "javascript".to_string()),
            (".jsx".to_string(), "javascriptreact".to_string()),
            (".mjs".to_string(), "javascript".to_string()),
            (".cjs".to_string(), "javascript".to_string()),
        ]),
        startup_timeout: Some(20_000),
        shutdown_timeout: Some(3_000),
        restart_on_crash: Some(true),
        max_restarts: Some(2),
        diagnostics: Some(true),
        ..LspServerConfig::default()
    }
}

struct ConnectionEntry {
    session: RawStdioSessionHandle,
    connection: Arc<Connection>,
    client: Arc<LspClient>,
    config: LspServerConfig,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    temp_dir: PathBuf,
    alive: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct WorkspaceMapping {
    host_root: PathBuf,
    guest_root: PathBuf,
    purpose: MountPurpose,
}

pub(crate) struct MobileLinuxGuestLspPathMapper {
    runtime: Arc<dyn MobileLinuxRuntime>,
    mappings: RwLock<Vec<WorkspaceMapping>>,
}

impl MobileLinuxGuestLspPathMapper {
    pub(crate) fn new(runtime: Arc<dyn MobileLinuxRuntime>) -> Self {
        Self {
            runtime,
            mappings: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn register_workspace_root(
        &self,
        host_root: PathBuf,
        guest_root: PathBuf,
        purpose: MountPurpose,
    ) {
        let mut mappings = self.mappings.write().expect("mobile lsp mappings");
        mappings.retain(|entry| entry.host_root != host_root);
        mappings.push(WorkspaceMapping {
            host_root,
            guest_root,
            purpose,
        });
    }

    pub(crate) fn unregister_workspace_root(&self, host_root: &Path) {
        self.mappings
            .write()
            .expect("mobile lsp mappings")
            .retain(|entry| entry.host_root != host_root);
    }

    fn mapping_for_host_root(&self, host_root: &Path) -> Option<WorkspaceMapping> {
        self.mappings
            .read()
            .expect("mobile lsp mappings")
            .iter()
            .find(|entry| entry.host_root == host_root)
            .cloned()
    }

    fn ensure_workspace_mapping(&self, host_root: &Path) -> WorkspaceMapping {
        let host_root =
            std::fs::canonicalize(host_root).unwrap_or_else(|_| host_root.to_path_buf());
        if let Some(mapping) = self.mapping_for_host_root(&host_root) {
            return mapping;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        host_root.hash(&mut hasher);
        let mapping = WorkspaceMapping {
            host_root,
            guest_root: PathBuf::from(format!("/workspace/lingxi-lsp-{:016x}", hasher.finish())),
            purpose: MountPurpose::External,
        };
        self.register_workspace_root(
            mapping.host_root.clone(),
            mapping.guest_root.clone(),
            mapping.purpose,
        );
        mapping
    }

    fn mapping_for_guest_root(&self, guest_root: &Path) -> Option<WorkspaceMapping> {
        self.mappings
            .read()
            .expect("mobile lsp mappings")
            .iter()
            .find(|entry| entry.guest_root == guest_root)
            .cloned()
    }

    fn mapping_for_host_path(&self, host_path: &Path) -> Option<WorkspaceMapping> {
        self.mappings
            .read()
            .expect("mobile lsp mappings")
            .iter()
            .filter(|entry| host_path == entry.host_root || host_path.starts_with(&entry.host_root))
            .max_by_key(|entry| entry.host_root.components().count())
            .cloned()
    }

    fn guest_path_for_host_path(&self, host_path: &Path) -> Result<PathBuf, LspError> {
        if let Some(mapping) = self.mapping_for_host_path(host_path) {
            let relative = host_path.strip_prefix(&mapping.host_root).map_err(|_| {
                LspError::Transport(format!(
                    "path {} is outside workspace {}",
                    host_path.display(),
                    mapping.host_root.display()
                ))
            })?;
            return Ok(mapping.guest_root.join(relative));
        }
        map_host_path_to_guest(host_path, &self.runtime.current_mounts())
            .map(PathBuf::from)
            .ok_or_else(|| {
                LspError::Transport(format!(
                    "path {} is not mounted into mobile linux",
                    host_path.display()
                ))
            })
    }

    fn readonly_mounts_for_workspace_folder(
        &self,
        workspace_folder: Option<&str>,
    ) -> Vec<MountSpec> {
        if let Some(workspace_folder) = workspace_folder {
            let workspace_folder = PathBuf::from(workspace_folder);
            if let Some(mapping) = self.mapping_for_guest_root(&workspace_folder) {
                return vec![MountSpec {
                    host_path: mapping.host_root,
                    guest_path: mapping.guest_root.display().to_string(),
                    read_only: true,
                    purpose: mapping.purpose,
                }];
            }
        }
        self.runtime
            .current_mounts()
            .into_iter()
            .map(|mut mount| {
                mount.read_only = true;
                mount
            })
            .collect()
    }
}

impl LspPathMapper for MobileLinuxGuestLspPathMapper {
    fn map_host_path(
        &self,
        path: &Path,
        workspace_cwd: &Path,
    ) -> Result<lsp::LspDocumentPath, LspError> {
        let host_path = path.to_path_buf();
        self.ensure_workspace_mapping(workspace_cwd);
        let server_path = if let Some(mapping) = self.mapping_for_host_path(path) {
            let relative = host_path.strip_prefix(&mapping.host_root).map_err(|_| {
                LspError::Transport(format!(
                    "document path {} is outside workspace {}",
                    host_path.display(),
                    mapping.host_root.display()
                ))
            })?;
            mapping.guest_root.join(relative)
        } else if host_path.is_absolute()
            && host_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(LspError::Transport(format!(
                "document path {} is not canonical enough for mobile LSP",
                host_path.display()
            )));
        } else {
            let workspace_root = self.workspace_root_for(workspace_cwd)?;
            let relative = host_path.strip_prefix(workspace_cwd).map_err(|_| {
                LspError::Transport(format!(
                    "document path {} is outside workspace {}",
                    host_path.display(),
                    workspace_cwd.display()
                ))
            })?;
            workspace_root.join(relative)
        };
        let uri = Url::from_file_path(&server_path).map_err(|()| {
            LspError::Transport(format!(
                "cannot convert guest path to file URI: {}",
                server_path.display()
            ))
        })?;
        Ok(lsp::LspDocumentPath {
            host_path,
            server_path,
            uri,
        })
    }

    fn workspace_root_for(&self, workspace_cwd: &Path) -> Result<PathBuf, LspError> {
        Ok(self.ensure_workspace_mapping(workspace_cwd).guest_root)
    }

    fn uri_for_host_path(&self, path: &Path) -> Result<Url, LspError> {
        if let Some(mapping) = self.mapping_for_guest_root(path) {
            return Url::from_file_path(&mapping.guest_root).map_err(|()| {
                LspError::Transport(format!(
                    "cannot convert guest path to file URI: {}",
                    mapping.guest_root.display()
                ))
            });
        }
        if path.is_absolute()
            && !path.starts_with("/Users")
            && !path.starts_with("/private")
            && !path.starts_with("/var")
        {
            return Url::from_file_path(path).map_err(|()| {
                LspError::Transport(format!(
                    "cannot convert guest path to file URI: {}",
                    path.display()
                ))
            });
        }
        let guest = self.guest_path_for_host_path(path)?;
        Url::from_file_path(guest).map_err(|()| {
            LspError::Transport(format!(
                "cannot convert guest path to file URI: {}",
                path.display()
            ))
        })
    }

    fn host_path_for_uri(&self, uri: &Url) -> Result<Option<PathBuf>, LspError> {
        let Ok(server_path) = uri.to_file_path() else {
            return Ok(None);
        };
        if let Some(mapping) = self.mapping_for_guest_root(&server_path) {
            return Ok(Some(mapping.host_root));
        }
        let mapping = self
            .mappings
            .read()
            .expect("mobile lsp mappings")
            .iter()
            .filter(|entry| {
                server_path == entry.guest_root || server_path.starts_with(&entry.guest_root)
            })
            .max_by_key(|entry| entry.guest_root.components().count())
            .cloned();
        let Some(mapping) = mapping else {
            return Ok(Some(server_path));
        };
        let relative = server_path.strip_prefix(&mapping.guest_root).map_err(|_| {
            LspError::Transport(format!(
                "server path {} is outside guest workspace {}",
                server_path.display(),
                mapping.guest_root.display()
            ))
        })?;
        Ok(Some(mapping.host_root.join(relative)))
    }
}

/// Reference-counted workspace lifetime for mobile workflow agents.
///
/// A workflow can run multiple agents against the same app concurrently. The
/// first source acquires the workspace lease and the last source to close
/// tears down only that workspace's LSP processes and diagnostic state.
pub(crate) struct MobileWorkspaceLspLeaseManager {
    registry: Arc<lsp::LspRegistry>,
    diagnostics: lsp::LspDiagnosticRegistry,
    path_mapper: Option<Arc<MobileLinuxGuestLspPathMapper>>,
    leases: std::sync::Mutex<HashMap<PathBuf, usize>>,
}

impl MobileWorkspaceLspLeaseManager {
    pub(crate) fn new(
        registry: Arc<lsp::LspRegistry>,
        diagnostics: lsp::LspDiagnosticRegistry,
        path_mapper: Option<Arc<MobileLinuxGuestLspPathMapper>>,
    ) -> Self {
        Self {
            registry,
            diagnostics,
            path_mapper,
            leases: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn diagnostics_source(
        self: &Arc<Self>,
        host_root: Option<PathBuf>,
        settle_timeout: Option<Duration>,
    ) -> Arc<dyn NewDiagnosticsSource> {
        let host_root = host_root.map(|root| fs::canonicalize(&root).unwrap_or(root));
        if let Some(root) = &host_root {
            let mut leases = self.leases.lock().expect("mobile LSP lease map");
            *leases.entry(root.clone()).or_default() += 1;
        }
        Arc::new(MobileWorkspaceDiagnosticsSource {
            inner: self
                .diagnostics
                .diagnostics_source(host_root.clone(), settle_timeout),
            manager: Arc::clone(self),
            host_root,
            closed: AtomicBool::new(false),
        })
    }

    async fn release_workspace(&self, host_root: &Path) {
        let final_lease = {
            let mut leases = self.leases.lock().expect("mobile LSP lease map");
            let Some(count) = leases.get_mut(host_root) else {
                return;
            };
            *count -= 1;
            if *count == 0 {
                leases.remove(host_root);
                true
            } else {
                false
            }
        };
        if !final_lease {
            return;
        }

        if let Err(error) = self.registry.shutdown_workspace(host_root).await {
            tracing::warn!(
                workspace = %host_root.display(),
                %error,
                "failed to shut down mobile workspace LSP"
            );
        }
        self.diagnostics.clear_under_host_root(host_root).await;
        if let Some(mapper) = &self.path_mapper {
            mapper.unregister_workspace_root(host_root);
        }
    }
}

struct MobileWorkspaceDiagnosticsSource {
    inner: Arc<dyn NewDiagnosticsSource>,
    manager: Arc<MobileWorkspaceLspLeaseManager>,
    host_root: Option<PathBuf>,
    closed: AtomicBool,
}

#[async_trait]
impl NewDiagnosticsSource for MobileWorkspaceDiagnosticsSource {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        self.inner.take_new_diagnostics_block().await
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(host_root) = &self.host_root {
            self.manager.release_workspace(host_root).await;
        }
    }
}

impl Drop for MobileWorkspaceDiagnosticsSource {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (Some(host_root), Ok(handle)) = (
            self.host_root.clone(),
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        let manager = Arc::clone(&self.manager);
        handle.spawn(async move {
            manager.release_workspace(&host_root).await;
        });
    }
}

#[derive(Default)]
pub(crate) struct MobileLinuxLspTransport {
    runtime: Option<Arc<dyn MobileLinuxRuntime>>,
    path_mapper: Option<Arc<MobileLinuxGuestLspPathMapper>>,
    temp_root: Option<PathBuf>,
    available: bool,
    connections: Mutex<HashMap<McpConnectionId, ConnectionEntry>>,
}

impl MobileLinuxLspTransport {
    pub(crate) fn unavailable() -> Self {
        Self::default()
    }

    pub(crate) fn new(
        runtime: Arc<dyn MobileLinuxRuntime>,
        path_mapper: Arc<MobileLinuxGuestLspPathMapper>,
        temp_root: PathBuf,
    ) -> Self {
        Self {
            available: matches!(
                runtime.backend(),
                SandboxBackend::AndroidProot | SandboxBackend::IosIsh
            ),
            runtime: Some(runtime),
            path_mapper: Some(path_mapper),
            temp_root: Some(temp_root),
            connections: Mutex::new(HashMap::new()),
        }
    }

    fn runtime(&self) -> Result<Arc<dyn MobileLinuxRuntime>, LspError> {
        self.runtime.clone().ok_or(LspError::Unavailable)
    }

    fn path_mapper(&self) -> Result<Arc<MobileLinuxGuestLspPathMapper>, LspError> {
        self.path_mapper.clone().ok_or(LspError::Unavailable)
    }

    fn temp_root(&self) -> Result<PathBuf, LspError> {
        self.temp_root.clone().ok_or(LspError::Unavailable)
    }

    async fn lookup_entry(
        &self,
        id: McpConnectionId,
    ) -> Result<(Arc<Connection>, Arc<LspClient>, LspServerConfig), LspError> {
        let guard = self.connections.lock().await;
        let entry = guard.get(&id).ok_or(LspError::Unavailable)?;
        Ok((
            Arc::clone(&entry.connection),
            Arc::clone(&entry.client),
            entry.config.clone(),
        ))
    }

    async fn remove_entry(&self, id: McpConnectionId) -> Option<ConnectionEntry> {
        self.connections.lock().await.remove(&id)
    }
}

#[async_trait]
impl LspTransport for MobileLinuxLspTransport {
    async fn start_server(&self, config: &LspServerConfig) -> Result<LspRawConnection, LspError> {
        if !self.available {
            return Err(LspError::Unavailable);
        }
        let runtime = self.runtime()?;
        let mapper = self.path_mapper()?;
        let temp_root = self.temp_root()?;
        fs::create_dir_all(&temp_root)
            .map_err(|error| LspError::Transport(format!("create lsp temp root: {error}")))?;

        let connection_id = McpConnectionId::new();
        let temp_dir = temp_root.join(connection_id.to_string());
        fs::create_dir_all(&temp_dir)
            .map_err(|error| LspError::Transport(format!("create lsp temp dir: {error}")))?;

        let mut mounts =
            mapper.readonly_mounts_for_workspace_folder(config.workspace_folder.as_deref());
        let guest_temp = format!("/tmp/lingxi-lsp-{}", connection_id);
        mounts.push(MountSpec {
            host_path: temp_dir.clone(),
            guest_path: guest_temp.clone(),
            read_only: false,
            purpose: MountPurpose::Temp,
        });
        let mut env = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        for key in [
            "HOME",
            "TMPDIR",
            "TMP",
            "TEMP",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ] {
            env.insert(key.to_string(), guest_temp.clone());
        }
        let session = match runtime
            .open_raw_stdio(RawStdioOpenRequest {
                command: config.command.clone(),
                args: config.args.clone(),
                cwd: config.workspace_folder.clone(),
                env,
                network: NetworkPolicy::Disabled,
                resource_limits: ResourceLimits {
                    max_memory_mb: Some(384),
                    ..ResourceLimits::default()
                },
                mounts,
            })
            .await
        {
            Ok(session) => session,
            Err(error) => {
                let _ = fs::remove_dir_all(&temp_dir);
                return Err(LspError::Transport(error.to_string()));
            }
        };

        let (stdin_writer, stdin_reader) = duplex(64 * 1024);
        let (stdout_writer, stdout_reader) = duplex(64 * 1024);
        let connection = Arc::new(Connection::new_lsp(stdout_reader, stdin_writer));
        let client = Arc::new(LspClient::with_shared(
            config.name.clone(),
            Arc::clone(&connection),
        ));
        let alive = Arc::new(AtomicBool::new(true));

        let runtime_for_write = Arc::clone(&runtime);
        let session_for_write = session.clone();
        let alive_for_write = Arc::clone(&alive);
        let write_task = tokio::spawn(async move {
            let mut reader = stdin_reader;
            let mut buf = vec![0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(read) => {
                        if runtime_for_write
                            .write_raw_stdio(&session_for_write, buf[..read].to_vec())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            alive_for_write.store(false, Ordering::Release);
        });

        let runtime_for_read = Arc::clone(&runtime);
        let session_for_read = session.clone();
        let alive_for_read = Arc::clone(&alive);
        let read_task = tokio::spawn(async move {
            let mut writer = stdout_writer;
            loop {
                match runtime_for_read
                    .read_raw_stdio(&session_for_read, 64 * 1024)
                    .await
                {
                    Ok(chunk) => {
                        if !chunk.stdout.is_empty()
                            && writer.write_all(&chunk.stdout).await.is_err()
                        {
                            break;
                        }
                        if chunk.closed {
                            let _ = writer.shutdown().await;
                            break;
                        }
                        if chunk.stdout.is_empty() && chunk.stderr.is_empty() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                    Err(_) => {
                        let _ = writer.shutdown().await;
                        break;
                    }
                }
            }
            alive_for_read.store(false, Ordering::Release);
        });

        self.connections.lock().await.insert(
            connection_id,
            ConnectionEntry {
                session,
                connection,
                client,
                config: config.clone(),
                tasks: vec![write_task, read_task],
                temp_dir,
                alive,
            },
        );

        Ok(LspRawConnection { connection_id })
    }

    async fn initialize(
        &self,
        conn: &LspRawConnection,
        root_uri: &str,
    ) -> Result<LspServerCapabilities, LspError> {
        let (_, client, config) = self.lookup_entry(conn.connection_id).await?;
        let capabilities = client.initialize(root_uri, &config).await?;
        Ok(LspServerCapabilities {
            text_document_sync: capabilities
                .text_document_sync
                .as_ref()
                .map(|sync| match sync {
                    lsp_types::TextDocumentSyncCapability::Kind(kind) => format!("{kind:?}"),
                    lsp_types::TextDocumentSyncCapability::Options(_) => "options".to_string(),
                }),
            completion: capabilities.completion_provider.is_some(),
            hover: capabilities.hover_provider.is_some(),
            definition: capabilities.definition_provider.is_some(),
            references: capabilities.references_provider.is_some(),
            diagnostics: true,
            symbols: capabilities.document_symbol_provider.is_some()
                || capabilities.workspace_symbol_provider.is_some(),
            formatting: capabilities.document_formatting_provider.is_some(),
            rename: capabilities.rename_provider.is_some(),
            code_action: capabilities.code_action_provider.is_some(),
        })
    }

    async fn request(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Value,
    ) -> Result<Value, LspError> {
        let (_, client, _) = self.lookup_entry(conn.connection_id).await?;
        client.request(method, params).await
    }

    async fn notify(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Value,
    ) -> Result<(), LspError> {
        let (_, client, _) = self.lookup_entry(conn.connection_id).await?;
        client.notify(method, params).await
    }

    async fn connection(&self, conn_id: McpConnectionId) -> Result<Arc<Connection>, LspError> {
        let (connection, _, _) = self.lookup_entry(conn_id).await?;
        Ok(connection)
    }

    async fn is_alive(&self, conn_id: McpConnectionId) -> bool {
        self.connections
            .lock()
            .await
            .get(&conn_id)
            .is_some_and(|entry| entry.alive.load(Ordering::Acquire))
    }

    async fn shutdown(&self, conn_id: McpConnectionId) -> Result<(), LspError> {
        let Some(entry) = self.remove_entry(conn_id).await else {
            return Ok(());
        };

        let shutdown_result = entry
            .client
            .shutdown_with_timeout(entry.config.shutdown_timeout)
            .await;
        for task in entry.tasks {
            task.abort();
        }
        let close_result = self
            .runtime()?
            .close_raw_stdio(&entry.session)
            .await
            .map_err(|error| LspError::Transport(error.to_string()));
        let _ = fs::remove_dir_all(&entry.temp_dir);

        shutdown_result?;
        close_result
    }

    async fn terminate(&self, conn_id: McpConnectionId) -> Result<(), LspError> {
        let Some(entry) = self.remove_entry(conn_id).await else {
            return Ok(());
        };
        for task in entry.tasks {
            task.abort();
        }
        let result = self
            .runtime()?
            .close_raw_stdio(&entry.session)
            .await
            .map_err(|error| LspError::Transport(error.to_string()));
        let _ = fs::remove_dir_all(&entry.temp_dir);
        result
    }

    fn is_available(&self) -> bool {
        self.available
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp::DiagnosticEntry;
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    #[test]
    fn global_typescript_descriptor_is_fixed_and_native() {
        let config = global_typescript_lsp_config();
        assert_eq!(config.name, GLOBAL_TYPESCRIPT_LSP_SERVER_NAME);
        assert_eq!(
            config.command,
            "/opt/lingxi/toolchains/typescript/7.0.2/tsc"
        );
        assert_eq!(config.args, ["--lsp", "--stdio"]);
        assert_eq!(
            config.extension_to_language.get(".jsx").map(String::as_str),
            Some("javascriptreact")
        );
    }

    #[tokio::test]
    async fn final_workspace_lease_clears_only_after_all_sources_close() {
        let diagnostics = lsp::LspDiagnosticRegistry::new();
        let registry = Arc::new(
            lsp::LspRegistry::new(Arc::new(MobileLinuxLspTransport::unavailable()))
                .with_diagnostics(diagnostics.clone()),
        );
        let manager = Arc::new(MobileWorkspaceLspLeaseManager::new(
            registry,
            diagnostics.clone(),
            None,
        ));
        let root = PathBuf::from("/apps/lease-test");
        let uri = Url::parse("file:///workspace/lease-test/app.js").expect("URI");
        diagnostics
            .record_document_sync(
                &root.join("app.js"),
                Path::new("/workspace/lease-test/app.js"),
                uri.clone(),
                Some(1),
                true,
            )
            .await;
        diagnostics
            .publish(
                uri,
                DiagnosticEntry {
                    version: Some(1),
                    diagnostics: vec![Diagnostic {
                        range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                        severity: Some(DiagnosticSeverity::ERROR),
                        message: "seeded".into(),
                        ..Diagnostic::default()
                    }],
                },
            )
            .await;

        let first = manager.diagnostics_source(Some(root.clone()), None);
        let second = manager.diagnostics_source(Some(root.clone()), None);
        first.close().await;
        assert_eq!(
            diagnostics.diagnostics_under_host_root(&root).await.len(),
            1
        );
        second.close().await;
        assert!(diagnostics
            .diagnostics_under_host_root(&root)
            .await
            .is_empty());
    }
}
