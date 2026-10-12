//! Settings-file watcher → `ConfigChange` hook fire (parity: claude-code
//! `src/utils/settings/changeDetector.ts`).
//!
//! claude-code watches the settings files (user / project / local / policy)
//! and, on every detected change, fires the `ConfigChange` hook with the layer
//! `source` and the changed `file_path` BEFORE applying the change to the live
//! session (`changeDetector.ts:285-297` → `executeConfigChangeHooks`,
//! `utils/hooks.ts:4214`). The Rust port had no such watcher; this module adds
//! it at the desktop composition root, where the orchestrator (`orch.hooks`),
//! the `cwd`, the `lingxi_home`, and a `FileSystem` are all in scope.
//!
//! ## What this does
//! The watcher fires the hook first, then re-reads the managed file and macOS
//! managed-preferences inputs through one source-aware policy selector. It
//! watches the relevant `.lingxi`, managed, and macOS managed-preferences
//! directories via the in-tree `fs_watch` primitive
//! ([`lingxi_core::host::FileSystem::watch`]), classifies each changed path to a
//! [`ConfigChangeSource`] layer, and fires
//! [`ConversationOrchestrator::fire_config_change`] best-effort.
//!
//! ## `fs_watch` reuse
//! The watcher is generic over `Arc<dyn FileSystem>` and drives it ONLY through
//! the trait's `watch(dir)` method — the in-tree `notify`-backed primitive
//! (`platforms/posix::watch_helper::watch_dir_with_debounce`, exposed via
//! `PosixFileSystem::watch`). No new external dependency is introduced; the
//! composition root injects whichever `FileSystem` it built.
//!
//! ## Path → source mapping (byte-faithful)
//! Mirrors `getSourceForPath` (`changeDetector.ts:361-375`) +
//! `getSettingsFilePathForSource` (`settings.ts:274-294`) +
//! `getManagedFilePath` (`managedPath.ts`):
//! - `<lingxi_home>/settings.json`            → `UserSettings`
//! - `<cwd>/.lingxi/settings.json`            → `ProjectSettings`
//! - `<cwd>/.lingxi/settings.local.json`      → `LocalSettings`
//! - `<managed_dir>/managed-settings.json`    → `PolicySettings`
//! - any `*.json` under `<managed_dir>/managed-settings.d/` → `PolicySettings`
//! - `/Library/Managed Preferences/<user>/com.lingxi.code.plist` → `PolicySettings`
//! - `/Library/Managed Preferences/com.lingxi.code.plist` → `PolicySettings`
//!
//! where `<managed_dir>` is the OS-specific managed root
//! (`/Library/Application Support/LingXi` on macOS,
//! `C:\Program Files\LingXi` on Windows, `/etc/lingxi` elsewhere).
//!
//! ## Lifecycle
//! [`SettingsWatcher::spawn`] starts one background task per watched directory
//! and returns a [`SettingsWatcherHandle`]. Dropping the handle aborts every
//! task (RAII) and the underlying `notify` watcher is released when the
//! [`lingxi_core::host::FileSystem::watch`] stream is dropped — a clean teardown with no
//! lingering OS handles. The task is started even when no ConfigChange hook is
//! configured so the managed safety callback cannot be disabled by hook setup.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_core::Stream;
use hooks::events::ConfigChangeSource;
use lingxi_core::host::{FileEvent, FileSystem, PermissionGate};
use lingxi_core::settings::native_policy::{
    resolve_native_policy, NativePolicyDocument, NativePolicyInputs, NativePolicyLoadState,
    NativePolicyManagedFileFacts, NativePolicySelection,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

/// Narrow fire seam: the watcher fires a `ConfigChange` without depending on
/// the full orchestrator surface. The composition root injects the live
/// orchestrator (whose blanket impl below forwards to
/// [`ConversationOrchestrator::fire_config_change`]); unit tests inject a
/// recording fake so the watcher logic is exercised without a real
/// orchestrator or any real filesystem timing.
#[async_trait]
pub trait ConfigChangeFirer: Send + Sync {
    /// Fire the `ConfigChange` hook for a changed settings path. Best-effort:
    /// implementors MUST NOT propagate failures (a failing hook never breaks
    /// the watcher loop).
    async fn fire_config_change(&self, source: ConfigChangeSource, file_path: Option<PathBuf>);
}

#[async_trait]
impl ConfigChangeFirer for orchestrator::ConversationOrchestrator {
    async fn fire_config_change(&self, source: ConfigChangeSource, file_path: Option<PathBuf>) {
        orchestrator::ConversationOrchestrator::fire_config_change(self, source, file_path).await;
    }
}

/// Live main-model selection refresh seam. This stays separate from the hook
/// fire because settings changes are applied only after the `ConfigChange`
/// callback completes.
#[async_trait]
pub trait MainModelSelectionRefresher: Send + Sync {
    /// Replace the configured raw model and managed restriction snapshot.
    async fn refresh_main_model_selection(
        &self,
        raw_model_setting: Option<String>,
        provenance: lingxi_core::host::ModelProvenance,
        restriction: Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
    );
}

#[async_trait]
impl MainModelSelectionRefresher for orchestrator::ConversationOrchestrator {
    async fn refresh_main_model_selection(
        &self,
        raw_model_setting: Option<String>,
        provenance: lingxi_core::host::ModelProvenance,
        restriction: Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
    ) {
        orchestrator::ConversationOrchestrator::refresh_main_model_selection(
            self,
            raw_model_setting,
            provenance,
            restriction,
        )
        .await;
    }
}

/// Live Native prompt-settings refresh seam. It stays separate from model
/// selection because output style and response language are prompt inputs.
#[async_trait]
pub trait NativeSettingsRefresher: Send + Sync {
    /// Replace the source-preserving effective output-style and language values.
    async fn refresh_native_settings(
        &self,
        output_style: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        language: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    );

    /// Refresh the process-local `proxyAuthHelper` setting on the existing
    /// host helper. The command retains exact UTF-16 units through settings
    /// loading, and `source` is the typed winning contributor from that same
    /// effective-settings result.
    async fn refresh_proxy_auth_helper(
        &self,
        _command_utf16: Option<Vec<u16>>,
        _source: Option<lingxi_core::settings::tracer::Source>,
    ) {
    }
}

#[async_trait]
impl NativeSettingsRefresher for orchestrator::ConversationOrchestrator {
    async fn refresh_native_settings(
        &self,
        output_style: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        language: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) {
        orchestrator::ConversationOrchestrator::refresh_native_output_style(self, output_style)
            .await;
        orchestrator::ConversationOrchestrator::refresh_native_response_language(self, language)
            .await;
    }
}

/// The non-secret desktop inputs needed to re-resolve configured main-model
/// identity after a settings file changes. Keep only the settings projection
/// here; a long-lived watcher must not retain the full `DesktopConfig`, which
/// also owns credentials and injected secrets.
#[derive(Clone)]
pub struct MainModelSelectionRefresh {
    default_model: String,
    default_model_explicit: bool,
    default_model_env_pinned: bool,
    restricted: bool,
    setting_source_scope: (bool, bool),
    flag_settings: Option<lingxi_core::settings::SettingsJson>,
    cwd: PathBuf,
    lingxi_home: PathBuf,
    model_catalog: Vec<lingxi_core::host::ModelListing>,
}

impl MainModelSelectionRefresh {
    /// Snapshot only the immutable settings inputs and route catalog needed by
    /// future live watcher refreshes.
    #[must_use]
    pub fn from_config(
        config: &super::DesktopConfig,
        model_catalog: &[lingxi_core::host::ModelListing],
    ) -> Self {
        Self {
            default_model: config.default_model.clone(),
            default_model_explicit: config.default_model_explicit,
            default_model_env_pinned: config.default_model_env_pinned,
            restricted: config.restricted,
            setting_source_scope: config.setting_source_scope,
            flag_settings: config.flag_settings.clone(),
            cwd: config.cwd.clone(),
            lingxi_home: config.lingxi_home.clone(),
            model_catalog: model_catalog.to_vec(),
        }
    }

    fn settings_config(&self) -> super::DesktopConfig {
        let mut config = super::DesktopConfig::default();
        config.default_model.clone_from(&self.default_model);
        config.default_model_explicit = self.default_model_explicit;
        config.default_model_env_pinned = self.default_model_env_pinned;
        config.restricted = self.restricted;
        config.setting_source_scope = self.setting_source_scope;
        config.flag_settings.clone_from(&self.flag_settings);
        config.cwd.clone_from(&self.cwd);
        config.lingxi_home.clone_from(&self.lingxi_home);
        config
    }

    async fn refresh(
        &self,
        managed_snapshot: &ManagedSettingsSnapshot,
        model_refresher: Option<&dyn MainModelSelectionRefresher>,
        native_settings_refresher: Option<&dyn NativeSettingsRefresher>,
    ) {
        let managed_layers = managed_snapshot
            .effective_json()
            .into_iter()
            .collect::<Vec<_>>();
        let config = self.settings_config();
        let effective = super::load_effective_settings_for_config(&config, &managed_layers);
        if let Some(refresher) = native_settings_refresher {
            refresher
                .refresh_native_settings(
                    effective
                        .as_ref()
                        .and_then(|settings| settings.settings.output_style.clone()),
                    effective
                        .as_ref()
                        .and_then(|settings| settings.settings.language.clone()),
                )
                .await;
            let command_utf16 = effective.as_ref().and_then(|settings| {
                settings
                    .settings
                    .proxy_auth_helper
                    .as_ref()
                    .and_then(|command| command.string_units(""))
            });
            let source = effective.as_ref().and_then(|settings| {
                settings
                    .effective_for("proxyAuthHelper")
                    .and_then(|provenance| provenance.contributors.last().copied())
            });
            refresher
                .refresh_proxy_auth_helper(command_utf16, source)
                .await;
        }
        if let Some(refresher) = model_refresher {
            let provenance =
                super::permission_config::model_provenance_for_config(&config, effective.as_ref());

            let raw_model_setting =
                if config.default_model_explicit || config.default_model_env_pinned {
                    config.default_model.clone()
                } else {
                    super::permission_config::managed_model_setting_for_config(
                        &config,
                        effective.as_ref(),
                    )
                    .or_else(|| non_default_model_setting(effective.as_ref()))
                    .unwrap_or_else(|| config.default_model.clone())
                };

            let policy_source =
                super::permission_config::managed_model_policy_source(&managed_snapshot);
            let enforcement =
                llm_runtime::model::allowlist::resolve_enforcement(&policy_source, &mut |_| {});
            let restriction = match enforcement {
                active @ llm_runtime::model::allowlist::ModelEnforcement::Active { .. } => Some((
                    active,
                    self.model_catalog
                        .iter()
                        .map(|model| model.request_model.clone())
                        .collect(),
                )),
                llm_runtime::model::allowlist::ModelEnforcement::Inactive
                | llm_runtime::model::allowlist::ModelEnforcement::Refused => None,
            };

            refresher
                .refresh_main_model_selection(Some(raw_model_setting), provenance, restriction)
                .await;
        }
    }
}

fn non_default_model_setting(
    effective: Option<&lingxi_core::settings::EffectiveSettings>,
) -> Option<String> {
    use lingxi_core::settings::tracer::Source;

    let effective = effective?;
    let source = effective
        .effective_for("model")
        .and_then(|provenance| provenance.contributors.last())?;
    if !matches!(
        source,
        Source::Env | Source::User | Source::Project | Source::Local | Source::Cli
    ) {
        return None;
    }
    effective
        .settings
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
}

/// Env override that relocates the managed (policy) settings root. The real
/// managed path is an absolute, OS-protected directory that tests cannot write;
/// pointing this at a tempdir lets the managed-tier derivation tests exercise
/// the loader + precedence fold without root. Unset in production, where the
/// hardcoded OS path is used (faithful to claude-code's `getManagedFilePath`).
pub const MANAGED_DIR_ENV: &str = "LINGXI_MANAGED_DIR";

/// The OS-specific managed (policy) settings root, mirroring claude-code's
/// `getManagedFilePath` (`managedPath.ts`). Honors the [`MANAGED_DIR_ENV`]
/// override first (test-only relocation); otherwise returns the hardcoded
/// OS-specific path.
#[must_use]
pub fn managed_settings_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MANAGED_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from(branding::MANAGED_DIR_MACOS)
    } else if cfg!(target_os = "windows") {
        PathBuf::from(branding::MANAGED_DIR_WINDOWS)
    } else {
        PathBuf::from(branding::MANAGED_DIR_UNIX)
    }
}

/// One local managed-settings file read. The original decoded text is retained
/// beside the exact UTF-16 projection and Core admission result, including an
/// invalid document that the selector cannot consume as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ManagedPolicyFileRead {
    pub path: PathBuf,
    pub raw_text: Option<String>,
    pub projection: Option<Utf16JsonProjection>,
    pub load_state: NativePolicyLoadState,
    pub diagnostic: Option<String>,
}

/// One failed directory/file read that has no JSON document to attach to the
/// selector (for example, an unreadable drop-in directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ManagedPolicyReadDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

/// Central local policy snapshot. `selection.documents` keeps the raw,
/// candidate, and normalized UTF-16 projections with exact source paths and
/// parser provenance; `effective_projection()` is the sole effective policy
/// carrier used by consumers.
#[derive(Debug, Clone)]
pub(super) struct ManagedSettingsSnapshot {
    pub selection: NativePolicySelection,
    pub file_reads: Vec<ManagedPolicyFileRead>,
    pub drop_in_directory_state: NativePolicyLoadState,
    pub read_diagnostics: Vec<ManagedPolicyReadDiagnostic>,
    pub plist: super::managed_plist::ManagedPlistSnapshot,
}

/// Session-scoped mirror of the one authoritative local-policy selection.
/// The state distinguishes a cache that has never loaded from a successfully
/// loaded snapshot whose effective policy is absent.
#[derive(Clone)]
pub(super) struct ManagedPolicySnapshotCache {
    state: Arc<std::sync::RwLock<ManagedPolicySnapshotState>>,
    initializer: Arc<tokio::sync::Mutex<()>>,
}

impl Default for ManagedPolicySnapshotCache {
    fn default() -> Self {
        Self {
            state: Arc::new(std::sync::RwLock::new(
                ManagedPolicySnapshotState::Uninitialized,
            )),
            initializer: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedPolicySnapshotError {
    Uninitialized,
    Incomplete,
}

#[derive(Clone, Default)]
enum ManagedPolicySnapshotState {
    #[default]
    Uninitialized,
    Loaded(ManagedSettingsSnapshot),
}

impl ManagedPolicySnapshotCache {
    /// Returns `None` only before a real source read has populated this cache.
    #[must_use]
    pub fn snapshot(&self) -> Option<ManagedSettingsSnapshot> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .loaded()
    }

    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_loaded()
    }

    pub fn replace(&self, snapshot: ManagedSettingsSnapshot) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            ManagedPolicySnapshotState::Loaded(snapshot);
    }
}

impl ManagedPolicySnapshotState {
    fn loaded(self) -> Option<ManagedSettingsSnapshot> {
        match self {
            Self::Uninitialized => None,
            Self::Loaded(snapshot) => Some(snapshot),
        }
    }

    fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded(_))
    }
}

/// Async consumers share the initialized session snapshot. Direct/test
/// composition roots without a cache perform a real read instead of treating
/// an uninitialized cache as an absent policy document.
pub(super) async fn managed_settings_snapshot_for_config(
    config: &super::DesktopConfig,
) -> ManagedSettingsSnapshot {
    managed_settings_snapshot_for_cache(&config.managed_policy_snapshot_cache).await
}

pub(super) async fn managed_settings_snapshot_for_cache(
    cache: &ManagedPolicySnapshotCache,
) -> ManagedSettingsSnapshot {
    if let Some(snapshot) = cache.snapshot() {
        return snapshot;
    }
    let _initialize = cache.initializer.lock().await;
    if let Some(snapshot) = cache.snapshot() {
        return snapshot;
    }
    let snapshot = managed_settings_snapshot().await;
    cache.replace(snapshot.clone());
    snapshot
}

pub(super) async fn refresh_managed_settings_snapshot_cache(
    cache: &ManagedPolicySnapshotCache,
) -> ManagedSettingsSnapshot {
    let _initialize = cache.initializer.lock().await;
    let snapshot = managed_settings_snapshot().await;
    cache.replace(snapshot.clone());
    snapshot
}

/// Synchronous consumers must never initiate I/O or block on an async reader.
/// `None` means the session has not completed its startup policy read.
pub(super) fn managed_settings_snapshot_sync(
    config: &super::DesktopConfig,
) -> Option<ManagedSettingsSnapshot> {
    config.managed_policy_snapshot_cache.snapshot()
}

/// Return the selected policy document for a synchronous settings loader.
/// Uninitialized and incomplete reads stay distinct from a loaded, absent
/// policy; callers must choose an explicit fail-closed fallback for either
/// error instead of silently overlaying an empty managed tier.
pub(super) fn managed_policy_layer_sync(
    config: &super::DesktopConfig,
) -> Result<Option<String>, ManagedPolicySnapshotError> {
    let snapshot =
        managed_settings_snapshot_sync(config).ok_or(ManagedPolicySnapshotError::Uninitialized)?;
    if snapshot.read_failed() {
        return Err(ManagedPolicySnapshotError::Incomplete);
    }
    if snapshot.effective_projection().is_none() {
        return Ok(None);
    }
    snapshot
        .effective_json()
        .map(Some)
        .ok_or(ManagedPolicySnapshotError::Incomplete)
}

pub(super) fn managed_policy_layers_sync(
    config: &super::DesktopConfig,
) -> Result<Vec<String>, ManagedPolicySnapshotError> {
    managed_policy_layer_sync(config).map(|layer| layer.into_iter().collect())
}

impl ManagedSettingsSnapshot {
    #[must_use]
    pub fn effective_projection(&self) -> Option<&Utf16JsonProjection> {
        self.selection
            .effective
            .as_ref()
            .map(|effective| &effective.projection)
    }

    /// Compatibility for downstream text-only parsers: serialize the
    /// selector's normalized projection, never the original file tiers. The
    /// projection stringifier escapes lone UTF-16 surrogates without losing
    /// their source code units.
    #[must_use]
    pub fn effective_json(&self) -> Option<String> {
        self.effective_projection()?.to_json_string().ok()
    }

    #[must_use]
    pub fn read_failed(&self) -> bool {
        !self.selection.complete
            || self.drop_in_directory_state == NativePolicyLoadState::DidNotLoad
            || !self.read_diagnostics.is_empty()
    }
}

/// Read local managed files plus the platform's native managed source and run
/// the single source-aware policy selector. Remote policy is intentionally not
/// synthesized by this local host producer.
pub(super) async fn managed_settings_snapshot() -> ManagedSettingsSnapshot {
    let managed = managed_settings_dir();
    let (files, plist) = tokio::join!(
        read_managed_policy_files(&managed),
        super::managed_plist::read_native_mdm_plists()
    );
    select_managed_policy(files.0, files.1, files.2, plist)
}

/// Filesystem-isolated reader seam used by focused watcher/source tests. It
/// deliberately has no remote or parent input; the production entry above
/// adds the real platform MDM reader before invoking the same selector.
pub(super) async fn read_managed_settings_snapshot(managed: &Path) -> ManagedSettingsSnapshot {
    let (files, read_diagnostics, drop_in_directory_state) =
        read_managed_policy_files(managed).await;
    select_managed_policy(
        files,
        read_diagnostics,
        drop_in_directory_state,
        super::managed_plist::ManagedPlistSnapshot::default(),
    )
}

fn select_managed_policy(
    file_reads: Vec<ManagedPolicyFileRead>,
    read_diagnostics: Vec<ManagedPolicyReadDiagnostic>,
    drop_in_directory_state: NativePolicyLoadState,
    plist: super::managed_plist::ManagedPlistSnapshot,
) -> ManagedSettingsSnapshot {
    let managed_file_documents = file_reads
        .iter()
        .map(|source| {
            NativePolicyDocument::parse_managed_file(
                source.projection.clone(),
                NativePolicyManagedFileFacts {
                    path: source.path.clone(),
                    load_state: source.load_state,
                    document_has_policy_content: None,
                },
            )
        })
        .collect();
    let selection = resolve_native_policy(NativePolicyInputs {
        plist_candidates: plist.native_policy_documents(),
        managed_file_documents,
        parent: None,
        host: None,
    });

    for diagnostic in &read_diagnostics {
        tracing::warn!(
            target: "harness_runtime::desktop::managed_policy",
            path = %diagnostic.path.display(),
            error = %diagnostic.message,
            "could not read a local managed-policy source"
        );
    }
    for source in &file_reads {
        if let Some(diagnostic) = source.diagnostic.as_deref() {
            tracing::warn!(
                target: "harness_runtime::desktop::managed_policy",
                path = %source.path.display(),
                error = %diagnostic,
                "local managed-policy document is unavailable"
            );
        }
    }
    for candidate in &plist.candidates {
        let invalid_json = candidate.stdout.is_some() && candidate.projection.is_none();
        if let Some(reason) = candidate
            .unread_reason
            .as_deref()
            .or_else(|| invalid_json.then_some("plutil output was not valid JSON"))
        {
            tracing::warn!(
                target: "harness_runtime::desktop::managed_policy",
                path = %candidate.path.display(),
                source = candidate.label.native_label(),
                error = %reason,
                "macOS managed-preferences source is unavailable"
            );
        }
    }
    for unresolved in &selection.unresolved {
        tracing::warn!(
            target: "harness_runtime::desktop::managed_policy",
            source = ?unresolved.source,
            reason = %unresolved.reason,
            "local managed-policy selection is incomplete"
        );
    }

    ManagedSettingsSnapshot {
        selection,
        file_reads,
        drop_in_directory_state,
        read_diagnostics,
        plist,
    }
}

async fn read_managed_policy_files(
    managed: &Path,
) -> (
    Vec<ManagedPolicyFileRead>,
    Vec<ManagedPolicyReadDiagnostic>,
    NativePolicyLoadState,
) {
    use std::io::ErrorKind;

    let mut files = vec![read_managed_policy_file(managed.join("managed-settings.json")).await];
    let mut diagnostics = Vec::new();
    let drop_in = managed.join("managed-settings.d");
    let drop_in_directory_state = match tokio::fs::read_dir(&drop_in).await {
        Ok(mut directory) => {
            let mut names = Vec::new();
            let mut state = NativePolicyLoadState::Loaded;
            loop {
                match directory.next_entry().await {
                    Ok(Some(entry)) => {
                        let name = entry.file_name();
                        let display = name.to_string_lossy();
                        if display.ends_with(".json") && !display.starts_with('.') {
                            names.push(name);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        diagnostics.push(ManagedPolicyReadDiagnostic {
                            path: drop_in.clone(),
                            message: error.to_string(),
                        });
                        state = NativePolicyLoadState::DidNotLoad;
                        break;
                    }
                }
            }
            names.sort();
            for name in names {
                files.push(read_managed_policy_file(drop_in.join(name)).await);
            }
            state
        }
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            NativePolicyLoadState::Absent
        }
        Err(error) => {
            diagnostics.push(ManagedPolicyReadDiagnostic {
                path: drop_in,
                message: error.to_string(),
            });
            NativePolicyLoadState::DidNotLoad
        }
    };
    (files, diagnostics, drop_in_directory_state)
}

async fn read_managed_policy_file(path: PathBuf) -> ManagedPolicyFileRead {
    use std::io::ErrorKind;

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            // Native file reads use UTF-8 replacement decoding. Keep that text
            // exactly as the policy parser receives it, then project it with
            // the source-preserving JSON parser.
            let raw_text = String::from_utf8_lossy(&bytes).into_owned();
            match Utf16JsonProjection::parse(&raw_text) {
                Ok(projection) => ManagedPolicyFileRead {
                    path,
                    raw_text: Some(raw_text),
                    projection: Some(projection),
                    load_state: NativePolicyLoadState::Loaded,
                    diagnostic: None,
                },
                Err(error) => ManagedPolicyFileRead {
                    path,
                    raw_text: Some(raw_text),
                    projection: None,
                    load_state: NativePolicyLoadState::DidNotLoad,
                    diagnostic: Some(format!("managed-policy JSON is invalid: {error}")),
                },
            }
        }
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            ManagedPolicyFileRead {
                path,
                raw_text: None,
                projection: None,
                load_state: NativePolicyLoadState::Absent,
                diagnostic: None,
            }
        }
        Err(error) => ManagedPolicyFileRead {
            path,
            raw_text: None,
            projection: None,
            load_state: NativePolicyLoadState::DidNotLoad,
            diagnostic: Some(error.to_string()),
        },
    }
}

/// The set of settings paths the watcher cares about, resolved from the
/// composition root's `lingxi_home` + `cwd`. Carries both the absolute file
/// paths and the parent directories to watch.
#[derive(Debug, Clone)]
pub struct SettingsPaths {
    /// `<lingxi_home>/settings.json`.
    pub user_settings: PathBuf,
    /// `<cwd>/.lingxi/settings.json`.
    pub project_settings: PathBuf,
    /// `<cwd>/.lingxi/settings.local.json`.
    pub local_settings: PathBuf,
    /// `<managed_dir>/managed-settings.json`.
    pub policy_settings: PathBuf,
    /// `<managed_dir>/managed-settings.d/` drop-in directory; any `*.json`
    /// inside maps to [`ConfigChangeSource::PolicySettings`].
    pub policy_drop_in_dir: PathBuf,
    /// Exact per-user/device macOS managed-preferences plist source paths.
    /// Empty on platforms without Native's plist policy reader.
    pub managed_preference_files: Vec<PathBuf>,
}

impl SettingsPaths {
    /// Resolve the watched settings paths from the composition root inputs.
    /// `lingxi_home` is the user-global config root (`~/.claude`); `cwd` is the
    /// project root. The managed/policy paths come from [`managed_settings_dir`].
    #[must_use]
    pub fn resolve(lingxi_home: &Path, cwd: &Path) -> Self {
        let managed = managed_settings_dir();
        Self {
            user_settings: lingxi_home.join("settings.json"),
            project_settings: cwd.join(branding::DOT_DIR).join("settings.json"),
            local_settings: cwd.join(branding::DOT_DIR).join("settings.local.json"),
            policy_settings: managed.join("managed-settings.json"),
            policy_drop_in_dir: managed.join("managed-settings.d"),
            managed_preference_files: super::managed_plist::native_mdm_plist_paths(),
        }
    }

    /// Classify a changed path to its [`ConfigChangeSource`] layer, or `None`
    /// if the path is not a settings file we watch. Mirrors `getSourceForPath`
    /// (`changeDetector.ts:361-375`): exact-match the three user/project/local
    /// files and the policy file, and treat any path inside the
    /// `managed-settings.d/` drop-in directory as `PolicySettings`.
    #[must_use]
    pub fn classify(&self, path: &Path) -> Option<ConfigChangeSource> {
        // Drop-in directory check first (a `.json` fragment inside it).
        if path.starts_with(&self.policy_drop_in_dir) {
            // Only `.json` fragments are policy settings (TS watches the
            // dir's `.json` children); ignore editor temp files etc.
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            {
                return Some(ConfigChangeSource::PolicySettings);
            }
            return None;
        }
        if path == self.policy_settings {
            return Some(ConfigChangeSource::PolicySettings);
        }
        if self
            .managed_preference_files
            .iter()
            .any(|managed_path| managed_path == path)
        {
            return Some(ConfigChangeSource::PolicySettings);
        }
        if path == self.user_settings {
            return Some(ConfigChangeSource::UserSettings);
        }
        if path == self.project_settings {
            return Some(ConfigChangeSource::ProjectSettings);
        }
        if path == self.local_settings {
            return Some(ConfigChangeSource::LocalSettings);
        }
        None
    }

    /// The deduplicated parent directories that must be watched so a change to
    /// any of the settings files is observed. Mirrors `getWatchTargets`
    /// (`changeDetector.ts:181-249`): watch the *directories* (not the files)
    /// so files created after init are still detected.
    #[must_use]
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut push = |d: Option<&Path>| {
            if let Some(d) = d {
                let d = d.to_path_buf();
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        };
        push(self.user_settings.parent());
        push(self.project_settings.parent());
        push(self.local_settings.parent());
        push(self.policy_settings.parent());
        // The drop-in directory itself is watched directly.
        if !dirs.contains(&self.policy_drop_in_dir) {
            dirs.push(self.policy_drop_in_dir.clone());
        }
        for source_path in &self.managed_preference_files {
            if let Some(dir) = source_path.parent() {
                if !dirs.iter().any(|existing| existing == dir) {
                    dirs.push(dir.to_path_buf());
                }
            }
        }
        dirs
    }
}

/// Derive the resolved policy projection's `disableAutoMode` safety gate.
/// Incomplete policy selection closes the gate rather than being treated as
/// absent; otherwise the selector's effective value is authoritative.
#[must_use]
pub fn auto_mode_disabled_from_policy(snapshot: &ManagedSettingsSnapshot) -> bool {
    if snapshot.read_failed() {
        return true;
    }
    snapshot
        .effective_projection()
        .and_then(|projection| projection.string_at("/disableAutoMode"))
        == Some("disable")
}

/// Cloneable owner of the spawned watcher tasks. Hosts use
/// [`Self::shutdown_and_drain`] for an immediate join; dropping the last clone
/// retains the legacy abort-on-drop fallback.
#[derive(Debug, Clone)]
pub struct SettingsWatcherHandle {
    owner: Arc<SettingsWatcherOwner>,
}

#[derive(Debug)]
struct SettingsWatcherOwner {
    state: std::sync::Mutex<SettingsWatcherState>,
    drained: tokio::sync::Notify,
}

#[derive(Debug)]
enum SettingsWatcherState {
    Running(Vec<JoinHandle<()>>),
    Draining,
    Drained,
}

impl SettingsWatcherOwner {
    fn begin_drain(self: &Arc<Self>) {
        let tasks = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *state {
                SettingsWatcherState::Running(tasks) => {
                    let tasks = std::mem::take(tasks);
                    *state = SettingsWatcherState::Draining;
                    Some(tasks)
                }
                SettingsWatcherState::Draining | SettingsWatcherState::Drained => None,
            }
        };
        let Some(tasks) = tasks else {
            return;
        };
        for task in &tasks {
            task.abort();
        }
        if tasks.is_empty() {
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SettingsWatcherState::Drained;
            self.drained.notify_waiters();
            return;
        }
        let owner = self.clone();
        tokio::spawn(async move {
            for task in tasks {
                let _ = task.await;
            }
            *owner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SettingsWatcherState::Drained;
            owner.drained.notify_waiters();
        });
    }

    async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if matches!(
                &*self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SettingsWatcherState::Drained
            ) {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for SettingsWatcherOwner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let SettingsWatcherState::Running(tasks) = state {
            for task in tasks.iter() {
                task.abort();
            }
        }
    }
}

impl SettingsWatcherHandle {
    /// An empty handle that owns no tasks (e.g. when no directory could be
    /// watched). Dropping it is a no-op.
    #[must_use]
    pub fn empty() -> Self {
        Self::from_tasks(Vec::new())
    }

    /// Number of live watch tasks (one per successfully-watched directory).
    #[must_use]
    pub fn task_count(&self) -> usize {
        match &*self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            SettingsWatcherState::Running(tasks) => tasks.len(),
            SettingsWatcherState::Draining | SettingsWatcherState::Drained => 0,
        }
    }

    fn from_tasks(tasks: Vec<JoinHandle<()>>) -> Self {
        Self {
            owner: Arc::new(SettingsWatcherOwner {
                state: std::sync::Mutex::new(SettingsWatcherState::Running(tasks)),
                drained: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Abort every watch job and wait for their futures to release the
    /// orchestrator/filesystem owners. The join is owned after the first call,
    /// so cancelling one waiter cannot make a concurrent shutdown return early.
    pub async fn shutdown_and_drain(&self) {
        self.owner.begin_drain();
        self.owner.wait_drained().await;
    }
}

/// The settings watcher. Owns the resolved paths + the fire seam and spawns the
/// per-directory watch loops.
pub struct SettingsWatcher {
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
    permission_gate: Option<Arc<dyn PermissionGate>>,
    classifier_auto_mode_refresh: Option<ClassifierAutoModeRefresh>,
    main_model_selection_refresh: Option<MainModelSelectionRefresh>,
    main_model_selection_refresher: Option<Arc<dyn MainModelSelectionRefresher>>,
    native_settings_refresher: Option<Arc<dyn NativeSettingsRefresher>>,
    managed_policy_snapshot_cache: Option<ManagedPolicySnapshotCache>,
}

#[derive(Clone)]
struct ClassifierAutoModeRefresh {
    include_user_settings: bool,
    flag_settings_configured: bool,
    flag_settings_source: Option<String>,
}

impl SettingsWatcher {
    /// Construct a watcher for the given `lingxi_home` / `cwd`, firing through
    /// `firer` (the live orchestrator in production).
    #[must_use]
    pub fn new(lingxi_home: &Path, cwd: &Path, firer: Arc<dyn ConfigChangeFirer>) -> Self {
        Self {
            paths: SettingsPaths::resolve(lingxi_home, cwd),
            firer,
            permission_gate: None,
            classifier_auto_mode_refresh: None,
            main_model_selection_refresh: None,
            main_model_selection_refresher: None,
            native_settings_refresher: None,
            managed_policy_snapshot_cache: None,
        }
    }

    #[must_use]
    pub fn with_permission_gate(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.permission_gate = Some(gate);
        self
    }

    /// Recompute the configured main-model setting and managed model
    /// restriction after user/project/local/policy settings changes.
    #[must_use]
    pub fn with_main_model_selection_refresh(
        mut self,
        inputs: MainModelSelectionRefresh,
        refresher: Arc<dyn MainModelSelectionRefresher>,
    ) -> Self {
        self.main_model_selection_refresh = Some(inputs);
        self.main_model_selection_refresher = Some(refresher);
        self
    }

    /// Refresh Native prompt settings such as output style and response language
    /// after live user/project/local/policy settings changes.
    #[must_use]
    pub fn with_native_settings_refresher(
        mut self,
        refresher: Arc<dyn NativeSettingsRefresher>,
    ) -> Self {
        self.native_settings_refresher = Some(refresher);
        self
    }

    /// Keep all synchronous and asynchronous settings consumers on the same
    /// session policy snapshot, replacing it after every watched settings
    /// event (including all local PolicySettings and macOS plist paths).
    #[must_use]
    pub fn with_managed_policy_snapshot_cache(mut self, cache: ManagedPolicySnapshotCache) -> Self {
        self.managed_policy_snapshot_cache = Some(cache);
        self
    }

    /// Configure the immutable trusted CLI tier and trusted source scope used
    /// to recompute classifier AutoMode arrays after live user/policy edits.
    #[must_use]
    pub fn with_classifier_auto_mode_sources(
        mut self,
        include_user_settings: bool,
        flag_settings_configured: bool,
        flag_settings_source: Option<String>,
    ) -> Self {
        self.classifier_auto_mode_refresh = Some(ClassifierAutoModeRefresh {
            include_user_settings,
            flag_settings_configured,
            flag_settings_source,
        });
        self
    }

    /// The resolved settings paths (exposed for tests / diagnostics).
    #[must_use]
    pub fn paths(&self) -> &SettingsPaths {
        &self.paths
    }

    /// Spawn the background watch loops via the injected `FileSystem` and return
    /// the owning [`SettingsWatcherHandle`].
    ///
    /// Only directories that currently exist are watched (the in-tree
    /// `watch_dir_with_debounce` errors on a missing target); a directory that
    /// appears later is simply not observed until the next boot — matching
    /// claude-code's `dirsWithExistingFiles` init-time gate. Best-effort: a
    /// directory that fails to watch is logged and skipped, never fatal.
    pub async fn spawn(self, fs: Arc<dyn FileSystem>) -> SettingsWatcherHandle {
        let Self {
            paths,
            firer,
            permission_gate,
            classifier_auto_mode_refresh,
            main_model_selection_refresh,
            main_model_selection_refresher,
            native_settings_refresher,
            managed_policy_snapshot_cache,
        } = self;
        // Own each task before awaiting the next watch setup. Cancellation of
        // construction must abort prior loops even before a handle is returned.
        let handle = SettingsWatcherHandle::empty();
        for dir in paths.watch_dirs() {
            if !dir.is_dir() {
                continue;
            }
            let dir_str = dir.to_string_lossy().into_owned();
            let stream = match fs.watch(&dir_str).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, dir = %dir_str, "settings watch failed");
                    continue;
                }
            };
            let paths = paths.clone();
            let firer = firer.clone();
            let permission_gate = permission_gate.clone();
            let classifier_auto_mode_refresh = classifier_auto_mode_refresh.clone();
            let main_model_selection_refresh = main_model_selection_refresh.clone();
            let main_model_selection_refresher = main_model_selection_refresher.clone();
            let native_settings_refresher = native_settings_refresher.clone();
            let managed_policy_snapshot_cache = managed_policy_snapshot_cache.clone();
            let mut state = handle
                .owner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let SettingsWatcherState::Running(tasks) = &mut *state else {
                unreachable!("construction has not published the watcher handle");
            };
            tasks.push(tokio::spawn(async move {
                run_watch_loop_with_settings_refresh(
                    stream,
                    paths,
                    firer,
                    permission_gate,
                    classifier_auto_mode_refresh,
                    main_model_selection_refresh,
                    main_model_selection_refresher,
                    native_settings_refresher,
                    managed_policy_snapshot_cache,
                )
                .await;
            }));
        }
        handle
    }
}

/// Drive one directory's change stream, classifying + firing per event. Split
/// out so tests can drive it with a synthetic stream (no real `FSEvents`).
pub async fn run_watch_loop(
    stream: std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
) {
    run_watch_loop_with_permission_gate(stream, paths, firer, None).await;
}

/// Drive one directory's change stream and apply managed policy changes after
/// the ConfigChange hook has completed.
pub async fn run_watch_loop_with_permission_gate(
    stream: std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
    permission_gate: Option<Arc<dyn PermissionGate>>,
) {
    run_watch_loop_with_settings_refresh(
        stream,
        paths,
        firer,
        permission_gate,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
}

async fn run_watch_loop_with_settings_refresh(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
    permission_gate: Option<Arc<dyn PermissionGate>>,
    classifier_auto_mode_refresh: Option<ClassifierAutoModeRefresh>,
    main_model_selection_refresh: Option<MainModelSelectionRefresh>,
    main_model_selection_refresher: Option<Arc<dyn MainModelSelectionRefresher>>,
    native_settings_refresher: Option<Arc<dyn NativeSettingsRefresher>>,
    managed_policy_snapshot_cache: Option<ManagedPolicySnapshotCache>,
) {
    while let Some(event) = stream.next().await {
        handle_event_with_settings_refresh(
            &event,
            &paths,
            firer.as_ref(),
            permission_gate.as_deref(),
            classifier_auto_mode_refresh.as_ref(),
            main_model_selection_refresh.as_ref(),
            main_model_selection_refresher.as_deref(),
            native_settings_refresher.as_deref(),
            managed_policy_snapshot_cache.as_ref(),
        )
        .await;
    }
}

/// Classify a single [`FileEvent`] and fire the `ConfigChange` hook when it
/// maps to a watched settings layer. A non-settings path is silently ignored
/// (mirrors `handleChange` early-returning when `getSourceForPath` is
/// undefined). Exposed for deterministic unit tests.
pub async fn handle_event(event: &FileEvent, paths: &SettingsPaths, firer: &dyn ConfigChangeFirer) {
    handle_event_with_permission_gate(event, paths, firer, None).await;
}

pub async fn handle_event_with_permission_gate(
    event: &FileEvent,
    paths: &SettingsPaths,
    firer: &dyn ConfigChangeFirer,
    permission_gate: Option<&dyn PermissionGate>,
) {
    handle_event_with_settings_refresh(
        event,
        paths,
        firer,
        permission_gate,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
}

async fn handle_event_with_settings_refresh(
    event: &FileEvent,
    paths: &SettingsPaths,
    firer: &dyn ConfigChangeFirer,
    permission_gate: Option<&dyn PermissionGate>,
    classifier_auto_mode_refresh: Option<&ClassifierAutoModeRefresh>,
    main_model_selection_refresh: Option<&MainModelSelectionRefresh>,
    main_model_selection_refresher: Option<&dyn MainModelSelectionRefresher>,
    native_settings_refresher: Option<&dyn NativeSettingsRefresher>,
    managed_policy_snapshot_cache: Option<&ManagedPolicySnapshotCache>,
) {
    let Some(source) = paths.classify(&event.path) else {
        return;
    };
    let internal_write =
        permission::consume_internal_write(&event.path, std::time::Duration::from_secs(5));
    if !internal_write {
        firer
            .fire_config_change(source, Some(event.path.clone()))
            .await;
    }
    let managed_snapshot = match managed_policy_snapshot_cache {
        Some(cache) => refresh_managed_settings_snapshot_cache(cache).await,
        None => managed_settings_snapshot().await,
    };
    if !internal_write {
        if source == ConfigChangeSource::PolicySettings {
            if let Some(gate) = permission_gate {
                gate.update_auto_mode_disabled(auto_mode_disabled_from_policy(&managed_snapshot));
            }
        }
        if matches!(
            source,
            ConfigChangeSource::UserSettings | ConfigChangeSource::PolicySettings
        ) {
            if let (Some(gate), Some(refresh)) = (permission_gate, classifier_auto_mode_refresh) {
                let settings = super::permission_config::reload_classifier_auto_mode_settings(
                    &paths.user_settings,
                    refresh.include_user_settings,
                    refresh.flag_settings_configured,
                    refresh.flag_settings_source.as_deref(),
                    &managed_snapshot,
                )
                .await;
                gate.update_classifier_auto_mode_settings(settings);
            }
        }
    }
    // Internal writes still change the current model setting and managed
    // restriction. They suppress the user-facing ConfigChange hook above, but
    // must not leave the live orchestrator on a stale selection snapshot.
    if matches!(
        source,
        ConfigChangeSource::UserSettings
            | ConfigChangeSource::ProjectSettings
            | ConfigChangeSource::LocalSettings
            | ConfigChangeSource::PolicySettings
    ) {
        if let Some(refresh) = main_model_selection_refresh {
            refresh
                .refresh(
                    &managed_snapshot,
                    main_model_selection_refresher,
                    native_settings_refresher,
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::FileEventKind;
    use std::sync::Mutex;

    #[tokio::test]
    async fn managed_policy_cache_distinguishes_uninitialized_from_loaded_absence_and_refreshes_clones(
    ) {
        let root = tempfile::tempdir().unwrap();
        let cache = ManagedPolicySnapshotCache::default();
        let shared = cache.clone();
        assert!(!cache.is_initialized());
        assert!(cache.snapshot().is_none());

        let absent = read_managed_settings_snapshot(root.path()).await;
        assert!(!absent.read_failed());
        assert!(absent.effective_projection().is_none());
        cache.replace(absent);
        assert!(shared.is_initialized());
        assert!(shared.snapshot().unwrap().effective_projection().is_none());

        std::fs::write(
            root.path().join("managed-settings.json"),
            r#"{"disableAutoMode":"disable"}"#,
        )
        .unwrap();
        let updated = read_managed_settings_snapshot(root.path()).await;
        cache.replace(updated);
        assert_eq!(
            shared
                .snapshot()
                .unwrap()
                .effective_projection()
                .and_then(|projection| projection.string_at("/disableAutoMode")),
            Some("disable")
        );
    }

    #[derive(Default)]
    struct DropBarrier {
        entered: tokio::sync::Notify,
        released: Mutex<bool>,
        release: std::sync::Condvar,
    }

    struct BarrierGuard(Arc<DropBarrier>);

    impl Drop for BarrierGuard {
        fn drop(&mut self) {
            self.0.entered.notify_one();
            let released = self.0.released.lock().unwrap();
            let _ = self
                .0
                .release
                .wait_timeout_while(released, std::time::Duration::from_secs(5), |ready| !*ready)
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn settings_drain_survives_cancelled_waiter_and_joins_before_concurrent_return() {
        let barrier = Arc::new(DropBarrier::default());
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let guard = BarrierGuard(barrier.clone());
        let task = tokio::spawn(async move {
            let retained_owner = owner;
            let _guard = guard;
            std::future::pending::<()>().await;
            drop(retained_owner);
        });
        let handle = SettingsWatcherHandle::from_tasks(vec![task]);
        let first_handle = handle.clone();
        let first = tokio::spawn(async move { first_handle.shutdown_and_drain().await });
        barrier.entered.notified().await;
        first.abort();
        let _ = first.await;
        let second_handle = handle.clone();
        let mut second = tokio::spawn(async move { second_handle.shutdown_and_drain().await });
        let premature = tokio::time::timeout(std::time::Duration::from_millis(100), &mut second)
            .await
            .is_ok();
        *barrier.released.lock().unwrap() = true;
        barrier.release.notify_all();
        if !premature {
            tokio::time::timeout(std::time::Duration::from_secs(2), second)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(!premature);
        assert!(weak.upgrade().is_none());
        handle.shutdown_and_drain().await;
    }

    struct ConstructionFs {
        calls: std::sync::atomic::AtomicUsize,
        second_watch: tokio::sync::Notify,
        first_stream_dropped: Arc<tokio::sync::Notify>,
    }

    struct PendingStream(Arc<tokio::sync::Notify>);

    impl Stream for PendingStream {
        type Item = FileEvent;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<FileEvent>> {
            std::task::Poll::Pending
        }
    }

    impl Drop for PendingStream {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[async_trait]
    impl FileSystem for ConstructionFs {
        async fn read_file(
            &self,
            _: &str,
            _: Option<u64>,
            _: Option<u64>,
        ) -> Result<lingxi_core::host::FileContent, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io("unused".into()))
        }
        async fn write_file(&self, _: &str, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<
            std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
            lingxi_core::host::FsError,
        > {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Ok(Box::pin(PendingStream(self.first_stream_dropped.clone())));
            }
            self.second_watch.notify_one();
            std::future::pending().await
        }
        async fn append_file(&self, _: &str, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn truncate(&self, _: &str, _: u64) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn file_mtime(
            &self,
            _: &str,
        ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, _: &str) -> Result<u64, lingxi_core::host::FsError> {
            Ok(0)
        }
        async fn delete_file(&self, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn flock_exclusive(
            &self,
            _: &str,
        ) -> Result<Box<dyn lingxi_core::host::FlockGuard>, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io("unused".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelled_settings_construction_aborts_already_spawned_directory_loops() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let project = root.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let firer = Arc::new(RecordingFirer::default());
        let weak = Arc::downgrade(&firer);
        let fs = Arc::new(ConstructionFs {
            calls: std::sync::atomic::AtomicUsize::new(0),
            second_watch: tokio::sync::Notify::new(),
            first_stream_dropped: Arc::new(tokio::sync::Notify::new()),
        });
        let watcher = SettingsWatcher {
            paths: SettingsPaths {
                user_settings: user.join("settings.json"),
                project_settings: project.join("settings.json"),
                local_settings: project.join("settings.local.json"),
                policy_settings: root.path().join("absent/policy.json"),
                policy_drop_in_dir: root.path().join("absent/dropins"),
                managed_preference_files: Vec::new(),
            },
            firer,
            permission_gate: None,
            classifier_auto_mode_refresh: None,
            main_model_selection_refresh: None,
            main_model_selection_refresher: None,
            native_settings_refresher: None,
            managed_policy_snapshot_cache: None,
        };
        let task_fs = fs.clone();
        let construction = tokio::spawn(async move { watcher.spawn(task_fs).await });
        fs.second_watch.notified().await;
        construction.abort();
        let _ = construction.await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            fs.first_stream_dropped.notified().await;
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled construction must release prior directory loops and firer");
    }

    /// Recording fake firer — captures every `(source, file_path)` the watcher
    /// fires so tests can assert deterministically without a real orchestrator.
    #[derive(Default)]
    struct RecordingFirer {
        fired: Mutex<Vec<(ConfigChangeSource, Option<PathBuf>)>>,
    }

    #[async_trait]
    impl ConfigChangeFirer for RecordingFirer {
        async fn fire_config_change(&self, source: ConfigChangeSource, file_path: Option<PathBuf>) {
            self.fired.lock().unwrap().push((source, file_path));
        }
    }

    #[derive(Default)]
    struct RecordingModelSelectionRefresher(
        Mutex<
            Vec<(
                Option<String>,
                lingxi_core::host::ModelProvenance,
                Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
            )>,
        >,
    );

    #[async_trait]
    impl MainModelSelectionRefresher for RecordingModelSelectionRefresher {
        async fn refresh_main_model_selection(
            &self,
            raw_model_setting: Option<String>,
            provenance: lingxi_core::host::ModelProvenance,
            restriction: Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push((raw_model_setting, provenance, restriction));
        }
    }

    #[derive(Default)]
    struct RecordingNativeSettingsRefresher {
        native: Mutex<
            Vec<(
                Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
                Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
            )>,
        >,
        proxy_auth_helper: Mutex<
            Vec<(
                Option<Vec<u16>>,
                Option<lingxi_core::settings::tracer::Source>,
            )>,
        >,
    }

    #[async_trait]
    impl NativeSettingsRefresher for RecordingNativeSettingsRefresher {
        async fn refresh_native_settings(
            &self,
            output_style: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
            language: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        ) {
            self.native.lock().unwrap().push((output_style, language));
        }

        async fn refresh_proxy_auth_helper(
            &self,
            command_utf16: Option<Vec<u16>>,
            source: Option<lingxi_core::settings::tracer::Source>,
        ) {
            self.proxy_auth_helper
                .lock()
                .unwrap()
                .push((command_utf16, source));
        }
    }

    fn paths() -> SettingsPaths {
        // Fixed roots so the mapping assertions are platform-independent for
        // the user/project/local layers (policy uses the real managed dir).
        SettingsPaths::resolve(Path::new("/home/u/.lingxi"), Path::new("/work/proj"))
    }

    /// [`paths`] rooted at a test-specific project dir.
    ///
    /// `permission::mark_internal_write` keeps its marks in a PROCESS-GLOBAL
    /// map keyed by path, so two tests that touch the same settings path race:
    /// one test's mark suppresses the other's event, or the other consumes the
    /// mark this one was about to rely on. Any test that marks (or expects to
    /// fire for) `settings.json` must therefore own a unique root.
    fn paths_rooted(project: &str) -> SettingsPaths {
        SettingsPaths::resolve(Path::new("/home/u/.lingxi"), Path::new(project))
    }

    fn ev(path: &str) -> FileEvent {
        FileEvent {
            path: PathBuf::from(path),
            kind: FileEventKind::Modified,
        }
    }

    #[derive(Default)]
    struct ClassifierSettingsGate {
        updates: Mutex<
            Vec<Option<lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings>>,
        >,
    }

    #[async_trait]
    impl PermissionGate for ClassifierSettingsGate {
        async fn check(
            &self,
            _: &str,
            _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        ) -> lingxi_core::host::PermissionDecision {
            lingxi_core::host::PermissionDecision::Allow
        }

        fn update_classifier_auto_mode_settings(
            &self,
            settings: Option<
                lingxi_core::host::permission_gate::PermissionClassifierAutoModeSettings,
            >,
        ) {
            self.updates.lock().unwrap().push(settings);
        }
    }

    #[tokio::test]
    async fn trusted_auto_mode_settings_refresh_after_user_and_policy_changes() {
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os(MANAGED_DIR_ENV);
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("user");
        let cwd = root.path().join("project");
        let managed = root.path().join("managed");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(branding::DOT_DIR)).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        std::env::set_var(MANAGED_DIR_ENV, &managed);
        struct RestoreManagedDir(Option<std::ffi::OsString>);
        impl Drop for RestoreManagedDir {
            fn drop(&mut self) {
                if let Some(value) = self.0.take() {
                    std::env::set_var(MANAGED_DIR_ENV, value);
                } else {
                    std::env::remove_var(MANAGED_DIR_ENV);
                }
            }
        }
        let _restore = RestoreManagedDir(previous);

        let user_settings = home.join("settings.json");
        let policy_settings = managed.join("managed-settings.json");
        std::fs::write(
            &user_settings,
            r#"{"autoMode":{"allow":["user-before"],"soft_deny":["user-soft"]}}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(branding::DOT_DIR).join("settings.json"),
            r#"{"autoMode":{"allow":["project-must-not-appear"]}}"#,
        )
        .unwrap();
        std::fs::write(
            &policy_settings,
            r#"{"autoMode":{"allow":["policy-before"],"environment":["policy-env"]}}"#,
        )
        .unwrap();

        let paths = SettingsPaths::resolve(&home, &cwd);
        let firer = RecordingFirer::default();
        let gate = ClassifierSettingsGate::default();
        let refresh = ClassifierAutoModeRefresh {
            include_user_settings: true,
            flag_settings_configured: false,
            flag_settings_source: None,
        };
        handle_event_with_settings_refresh(
            &ev(user_settings.to_str().unwrap()),
            &paths,
            &firer,
            Some(&gate),
            Some(&refresh),
            None,
            None,
            None,
            None,
        )
        .await;
        let first = gate.updates.lock().unwrap()[0].clone().unwrap();
        assert_eq!(
            first
                .allow
                .iter()
                .map(|value| value.to_json_string().unwrap())
                .collect::<Vec<_>>(),
            ["\"user-before\"", "\"policy-before\""]
        );
        assert_eq!(
            first
                .soft_deny
                .iter()
                .map(|value| value.to_json_string().unwrap())
                .collect::<Vec<_>>(),
            ["\"user-soft\""]
        );

        std::fs::write(&user_settings, r#"{"autoMode":{"allow":["user-after"]}}"#).unwrap();
        std::fs::write(
            &policy_settings,
            r#"{"autoMode":{"allow":["policy-after"],"hard_deny":["policy-hard"]}}"#,
        )
        .unwrap();
        handle_event_with_settings_refresh(
            &ev(policy_settings.to_str().unwrap()),
            &paths,
            &firer,
            Some(&gate),
            Some(&refresh),
            None,
            None,
            None,
            None,
        )
        .await;
        let updates = gate.updates.lock().unwrap();
        let second = updates.last().unwrap().as_ref().unwrap();
        assert_eq!(
            second
                .allow
                .iter()
                .map(|value| value.to_json_string().unwrap())
                .collect::<Vec<_>>(),
            ["\"user-after\"", "\"policy-after\""]
        );
        assert_eq!(
            second
                .hard_deny
                .iter()
                .map(|value| value.to_json_string().unwrap())
                .collect::<Vec<_>>(),
            ["\"policy-hard\""]
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn model_selection_refresh_uses_live_source_precedence_and_policy_catalog() {
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os(MANAGED_DIR_ENV);
        struct RestoreManagedDir(Option<std::ffi::OsString>);
        impl Drop for RestoreManagedDir {
            fn drop(&mut self) {
                if let Some(previous) = self.0.take() {
                    std::env::set_var(MANAGED_DIR_ENV, previous);
                } else {
                    std::env::remove_var(MANAGED_DIR_ENV);
                }
            }
        }
        let _restore = RestoreManagedDir(previous);

        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let managed = root.path().join("managed");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(branding::DOT_DIR)).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        std::env::set_var(MANAGED_DIR_ENV, &managed);

        let user_settings = home.join("settings.json");
        let project_settings = cwd.join(branding::DOT_DIR).join("settings.json");
        let policy_settings = managed.join("managed-settings.json");
        std::fs::write(&user_settings, r#"{"model":"user-model"}"#).unwrap();
        std::fs::write(&project_settings, r#"{"model":"opusplan"}"#).unwrap();
        std::fs::write(
            &policy_settings,
            r#"{"availableModels":["opusplan"],"enforceAvailableModels":true}"#,
        )
        .unwrap();

        let model_catalog = vec![
            lingxi_core::host::ModelListing {
                display_model: "opusplan".into(),
                request_model: "opusplan".into(),
                provider_id: "anthropic".into(),
                provider_label: "Anthropic".into(),
                description: None,
                metadata: Default::default(),
                capabilities: Default::default(),
                reasoning: Default::default(),
                supports_reasoning: false,
                fusion_analyst_capable: false,
                connection: Default::default(),
            },
            lingxi_core::host::ModelListing {
                display_model: "catalog-model".into(),
                request_model: "catalog-model".into(),
                provider_id: "anthropic".into(),
                provider_label: "Anthropic".into(),
                description: None,
                metadata: Default::default(),
                capabilities: Default::default(),
                reasoning: Default::default(),
                supports_reasoning: false,
                fusion_analyst_capable: false,
                connection: Default::default(),
            },
        ];
        let config = super::super::DesktopConfig {
            cwd: cwd.clone(),
            lingxi_home: home,
            default_model: "catalog-model".into(),
            setting_source_scope: (true, true),
            ..Default::default()
        };
        let refresh = MainModelSelectionRefresh::from_config(&config, &model_catalog);
        let paths = SettingsPaths::resolve(&config.lingxi_home, &cwd);
        let firer = RecordingFirer::default();
        let refresher = RecordingModelSelectionRefresher::default();

        handle_event_with_settings_refresh(
            &ev(user_settings.to_str().unwrap()),
            &paths,
            &firer,
            None,
            None,
            Some(&refresh),
            Some(&refresher),
            None,
            None,
        )
        .await;

        assert_eq!(firer.fired.lock().unwrap().len(), 1);
        {
            let selections = refresher.0.lock().unwrap();
            assert_eq!(selections.len(), 1);
            let (raw_model, provenance, restriction) = &selections[0];
            assert_eq!(raw_model.as_deref(), Some("opusplan"));
            assert_eq!(*provenance, lingxi_core::host::ModelProvenance::UserOrEnv);
            let (enforcement, catalog) =
                restriction.as_ref().expect("managed restriction is active");
            match enforcement {
                llm_runtime::model::allowlist::ModelEnforcement::Active { allowlist, .. } => {
                    assert_eq!(
                        allowlist,
                        &Utf16JsonProjection::parse(r#"["opusplan"]"#).unwrap()
                    );
                }
                other => panic!("expected active managed enforcement, got {other:?}"),
            }
            assert_eq!(
                catalog.iter().map(String::as_str).collect::<Vec<_>>(),
                ["opusplan", "catalog-model"]
            );
        }

        std::fs::write(&project_settings, r#"{"model":"project-next"}"#).unwrap();
        permission::mark_internal_write(&project_settings);
        handle_event_with_settings_refresh(
            &ev(project_settings.to_str().unwrap()),
            &paths,
            &firer,
            None,
            None,
            Some(&refresh),
            Some(&refresher),
            None,
            None,
        )
        .await;

        assert_eq!(
            firer.fired.lock().unwrap().len(),
            1,
            "internal settings changes still suppress the config hook"
        );
        let selections = refresher.0.lock().unwrap();
        assert_eq!(selections.len(), 2);
        assert_eq!(
            selections[1].0.as_deref(),
            Some("project-next"),
            "internal model changes still refresh the live model selection"
        );
    }

    #[tokio::test]
    async fn native_settings_refresh_keeps_lone_surrogates_and_tracks_clear() {
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os(MANAGED_DIR_ENV);
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("user");
        let cwd = root.path().join("project");
        let managed = root.path().join("managed");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(branding::DOT_DIR)).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        std::env::set_var(MANAGED_DIR_ENV, &managed);
        struct RestoreManagedDir(Option<std::ffi::OsString>);
        impl Drop for RestoreManagedDir {
            fn drop(&mut self) {
                if let Some(previous) = self.0.take() {
                    std::env::set_var(MANAGED_DIR_ENV, previous);
                } else {
                    std::env::remove_var(MANAGED_DIR_ENV);
                }
            }
        }
        let _restore = RestoreManagedDir(previous);

        let user_settings = home.join("settings.json");
        std::fs::write(
            &user_settings,
            r#"{"language":"\ud800","outputStyle":"style-\ud800","proxyAuthHelper":"/helper-\ud800"}"#,
        )
        .unwrap();
        let config = super::super::DesktopConfig {
            cwd: cwd.clone(),
            lingxi_home: home.clone(),
            setting_source_scope: (true, true),
            ..Default::default()
        };
        let refresh = MainModelSelectionRefresh::from_config(&config, &[]);
        let paths = SettingsPaths::resolve(&home, &cwd);
        let firer = RecordingFirer::default();
        let native_refresher = RecordingNativeSettingsRefresher::default();

        handle_event_with_settings_refresh(
            &ev(user_settings.to_str().unwrap()),
            &paths,
            &firer,
            None,
            None,
            Some(&refresh),
            None,
            Some(&native_refresher),
            None,
        )
        .await;

        std::fs::write(&user_settings, r#"{"model":"unchanged"}"#).unwrap();
        permission::mark_internal_write(&user_settings);
        handle_event_with_settings_refresh(
            &ev(user_settings.to_str().unwrap()),
            &paths,
            &firer,
            None,
            None,
            Some(&refresh),
            None,
            Some(&native_refresher),
            None,
        )
        .await;

        let updates = native_refresher.native.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates[0].0.as_ref().unwrap().string_units(""),
            Some("style-".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            updates[0].1.as_ref().unwrap().string_units(""),
            Some(vec![0xd800])
        );
        assert!(
            updates[1].0.is_none(),
            "removing outputStyle clears its live source"
        );
        assert!(
            updates[1].1.is_none(),
            "removing language clears its live source"
        );
        let helper_updates = native_refresher.proxy_auth_helper.lock().unwrap();
        assert_eq!(helper_updates.len(), 2);
        assert_eq!(
            helper_updates[0].0.as_deref(),
            Some(
                "/helper-"
                    .encode_utf16()
                    .chain([0xd800])
                    .collect::<Vec<_>>()
                    .as_slice()
            )
        );
        assert_eq!(
            helper_updates[0].1,
            Some(lingxi_core::settings::tracer::Source::User)
        );
        assert!(helper_updates[1].0.is_none());
        assert!(helper_updates[1].1.is_none());
        assert_eq!(firer.fired.lock().unwrap().len(), 1);
    }

    #[test]
    fn classify_user_project_local() {
        let p = paths();
        assert_eq!(
            p.classify(Path::new("/home/u/.lingxi/settings.json")),
            Some(ConfigChangeSource::UserSettings)
        );
        assert_eq!(
            p.classify(Path::new("/work/proj/.lingxi/settings.json")),
            Some(ConfigChangeSource::ProjectSettings)
        );
        assert_eq!(
            p.classify(Path::new("/work/proj/.lingxi/settings.local.json")),
            Some(ConfigChangeSource::LocalSettings)
        );
    }

    #[test]
    fn classify_policy_file_and_dropin() {
        // Reads the managed dir (via `paths()`/`SettingsPaths::resolve` AND a
        // second direct call), while ~10 tests in `lib.rs` flip
        // `LINGXI_MANAGED_DIR` around their own bodies under
        // `crate::desktop::tests::MANAGED_ENV_LOCK`. Without taking the SAME lock the two
        // reads here can straddle a flip and the policy path stops classifying.
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let managed = managed_settings_dir();
        let p = paths();
        assert_eq!(
            p.classify(&managed.join("managed-settings.json")),
            Some(ConfigChangeSource::PolicySettings)
        );
        // A `.json` fragment in the drop-in dir is policy settings.
        assert_eq!(
            p.classify(&managed.join("managed-settings.d").join("10-org.json")),
            Some(ConfigChangeSource::PolicySettings)
        );
        // A non-json file in the drop-in dir is ignored.
        assert_eq!(
            p.classify(&managed.join("managed-settings.d").join("README.md")),
            None
        );
    }

    #[test]
    fn classify_and_watch_exact_managed_preference_candidates() {
        let user = PathBuf::from("/Library/Managed Preferences/alice/com.lingxi.code.plist");
        let device = PathBuf::from("/Library/Managed Preferences/com.lingxi.code.plist");
        let mut paths = paths();
        paths.managed_preference_files = vec![user.clone(), device.clone()];

        assert_eq!(
            paths.classify(&user),
            Some(ConfigChangeSource::PolicySettings)
        );
        assert_eq!(
            paths.classify(&device),
            Some(ConfigChangeSource::PolicySettings)
        );
        assert_eq!(
            paths.classify(Path::new(
                "/Library/Managed Preferences/alice/com.other.bundle.plist"
            )),
            None
        );
        let watched = paths.watch_dirs();
        assert!(watched.contains(&user.parent().unwrap().to_path_buf()));
        assert!(watched.contains(&device.parent().unwrap().to_path_buf()));
    }

    #[test]
    fn classify_unrelated_path_is_none() {
        let p = paths();
        assert_eq!(
            p.classify(Path::new("/work/proj/.lingxi/agents/x.md")),
            None
        );
        assert_eq!(p.classify(Path::new("/work/proj/src/main.rs")), None);
        // A sibling json in the project .claude dir that is NOT a watched
        // settings file maps to nothing.
        assert_eq!(p.classify(Path::new("/work/proj/.lingxi/other.json")), None);
    }

    #[tokio::test]
    async fn managed_auto_mode_uses_selected_policy_and_closes_on_unresolved_input() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("managed-settings.json");
        std::fs::write(&base, r#"{"disableAutoMode":"disable"}"#).unwrap();
        let disabled = read_managed_settings_snapshot(root.path()).await;
        assert!(auto_mode_disabled_from_policy(&disabled));

        std::fs::write(&base, r#"{"disableAutoMode":"enable"}"#).unwrap();
        let enabled = read_managed_settings_snapshot(root.path()).await;
        assert!(!auto_mode_disabled_from_policy(&enabled));

        std::fs::write(&base, "not-json").unwrap();
        let unresolved = read_managed_settings_snapshot(root.path()).await;
        assert!(unresolved.read_failed());
        assert!(auto_mode_disabled_from_policy(&unresolved));
    }

    #[tokio::test]
    async fn handle_event_fires_with_correct_source_per_path() {
        // Reads the managed dir (via `paths()`/`SettingsPaths::resolve` AND a
        // second direct call), while ~10 tests in `lib.rs` flip
        // `LINGXI_MANAGED_DIR` around their own bodies under
        // `crate::desktop::tests::MANAGED_ENV_LOCK`. Without taking the SAME lock the two
        // reads here can straddle a flip and the policy path stops classifying.
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let p = paths();
        let firer = RecordingFirer::default();

        handle_event(&ev("/home/u/.lingxi/settings.json"), &p, &firer).await;
        handle_event(&ev("/work/proj/.lingxi/settings.json"), &p, &firer).await;
        handle_event(&ev("/work/proj/.lingxi/settings.local.json"), &p, &firer).await;
        let policy = managed_settings_dir().join("managed-settings.json");
        handle_event(&ev(&policy.to_string_lossy()), &p, &firer).await;

        let recorded = firer.fired.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        assert_eq!(recorded[0].0, ConfigChangeSource::UserSettings);
        assert_eq!(
            recorded[0].1,
            Some(PathBuf::from("/home/u/.lingxi/settings.json"))
        );
        assert_eq!(recorded[1].0, ConfigChangeSource::ProjectSettings);
        assert_eq!(recorded[2].0, ConfigChangeSource::LocalSettings);
        assert_eq!(recorded[3].0, ConfigChangeSource::PolicySettings);
        assert_eq!(recorded[3].1, Some(policy));
    }

    #[tokio::test]
    async fn handle_event_non_settings_is_noop() {
        let p = paths();
        let firer = RecordingFirer::default();
        handle_event(&ev("/work/proj/src/main.rs"), &p, &firer).await;
        handle_event(&ev("/home/u/.lingxi/LINGXI.md"), &p, &firer).await;
        assert!(firer.fired.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn handle_event_suppresses_one_recent_internal_write() {
        // Own project root — see `paths_rooted`: sharing `/work/proj` with
        // `handle_event_fires_with_correct_source_per_path` made the two race
        // through the global internal-write map (each failed the other's
        // assertion, intermittently, under a full-workspace run).
        let p = paths_rooted("/work/suppress-one");
        let firer = RecordingFirer::default();
        let path = PathBuf::from("/work/suppress-one/.lingxi/settings.json");
        permission::mark_internal_write(&path);

        handle_event(&ev(&path.to_string_lossy()), &p, &firer).await;
        assert!(firer.fired.lock().unwrap().is_empty());

        handle_event(&ev(&path.to_string_lossy()), &p, &firer).await;
        assert_eq!(firer.fired.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_watch_loop_drives_synthetic_stream() {
        // Inject a synthetic change stream (no real FSEvents) and assert the
        // loop classifies + fires each event, then exits cleanly when the
        // stream ends — deterministic, no FS timing dependency.
        let p = paths();
        let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::default());
        let events = vec![
            ev("/home/u/.lingxi/settings.json"),
            ev("/work/proj/src/ignored.rs"),
            ev("/work/proj/.lingxi/settings.local.json"),
        ];
        let stream = tokio_stream::iter(events);
        run_watch_loop(Box::pin(stream), p, firer.clone()).await;

        let recorded = firer.fired.lock().unwrap();
        assert_eq!(recorded.len(), 2, "only the two settings paths fire");
        assert_eq!(recorded[0].0, ConfigChangeSource::UserSettings);
        assert_eq!(recorded[1].0, ConfigChangeSource::LocalSettings);
    }

    #[test]
    fn watch_dirs_dedup_and_cover_layers() {
        // Reads the managed dir (via `paths()`/`SettingsPaths::resolve` AND a
        // second direct call), while ~10 tests in `lib.rs` flip
        // `LINGXI_MANAGED_DIR` around their own bodies under
        // `crate::desktop::tests::MANAGED_ENV_LOCK`. Without taking the SAME lock the two
        // reads here can straddle a flip and the policy path stops classifying.
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let p = paths();
        let dirs = p.watch_dirs();
        // user dir, project .claude dir (covers both project + local), policy
        // managed dir, drop-in dir. project + local share `.lingxi/` so dedup
        // collapses them.
        assert!(dirs.contains(&PathBuf::from("/home/u/.lingxi")));
        assert!(dirs.contains(&PathBuf::from("/work/proj/.lingxi")));
        assert!(dirs.contains(&managed_settings_dir()));
        assert!(dirs.contains(&managed_settings_dir().join("managed-settings.d")));
        // `.claude` appears exactly once despite two files inside it.
        let claude_count = dirs
            .iter()
            .filter(|d| *d == &PathBuf::from("/work/proj/.lingxi"))
            .count();
        assert_eq!(claude_count, 1);
    }
}
