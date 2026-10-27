//! Public, camelCase schema-2 records. No credentials or runtime policy enter a release.
use super::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Host project containing an authoring workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ProjectRef {
    /// A desktop workspace identified by its filesystem path.
    DesktopPath {
        /// Path to the desktop workspace.
        path: String,
    },
    /// A mobile project identified by its host project ID.
    MobileProject {
        /// Identifier of the mobile project.
        project_id: String,
    },
}

/// Stable host binding for a mutable authoring workspace. Optional binding
/// fields let schema-2 records from before this contract load so the host can
/// offer an explicit rebind; they are never accepted by `bindAuthoring`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoringRef {
    /// Path to the authoring source directory.
    pub source_directory: String,
    /// Identifier of the authoring workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Reference to the authoring project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<ProjectRef>,
    /// Identifier of the authoring session, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring_session_id: Option<String>,
    /// Identifier of the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

/// Session context used to resolve an authoring binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoringSessionContext {
    /// Identifier of the authoring workspace.
    pub workspace_id: String,
    /// Path to the authoring source directory.
    pub source_directory: String,
    /// Reference to the authoring project.
    pub project_ref: ProjectRef,
    /// The engine's persisted session mode (`code` is required for authoring).
    pub mode: String,
}
/// Marketplace provenance for an app release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketOrigin {
    /// Identifier of the app.
    pub app_id: String,
    /// Identifier of the marketplace release.
    pub release_id: String,
    /// Content digest.
    pub digest: String,
    /// Identifier of the source app, when remixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remixed_from: Option<String>,
}
/// Mutable metadata for an app in the local library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRecord {
    /// Identifier of the app.
    pub app_id: String,
    /// Name of this item.
    pub name: String,
    /// Short description.
    pub summary: String,
    /// Currently active release revision.
    pub current_revision: u64,
    /// Latest published release revision.
    pub latest_revision: u64,
    /// Authoring workspace binding, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring: Option<AuthoringRef>,
    /// Marketplace origin, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_origin: Option<MarketOrigin>,
    /// Local origin port assigned to the app.
    pub origin_port: u16,
    /// Version of the app data schema.
    pub data_schema_version: u32,
    /// Release digests authorized for execution.
    pub execution_grants: Vec<String>,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Last update time in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Whether the app is marked removed.
    pub removed: bool,
}
/// File recorded in an immutable release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseFile {
    /// Path associated with this item.
    pub path: String,
    /// SHA-256 digest.
    pub sha256: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Whether the file is executable.
    #[serde(default)]
    pub executable: bool,
}
/// Immutable release of a local app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRelease {
    /// Identifier of the app.
    pub app_id: String,
    /// Release revision.
    pub revision: u64,
    /// Content digest.
    pub digest: String,
    /// Release manifest.
    pub manifest: Manifest,
    /// Files included in the release or package.
    pub files: Vec<ReleaseFile>,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Marketplace origin, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_origin: Option<MarketOrigin>,
}
/// Lifecycle phase of an app run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunPhase {
    /// The run is being prepared.
    Preparing,
    /// The run is starting.
    Starting,
    /// Execution is running.
    Running,
    /// The run is stopping.
    Stopping,
    /// The run has stopped.
    Stopped,
    /// Execution failed.
    Failed,
    /// Execution was interrupted.
    Interrupted,
    /// The run is suspended.
    Suspended,
}
impl RunPhase {
    /// Return whether the run is in an active lifecycle phase.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Starting | Self::Running | Self::Stopping
        )
    }
}
/// Service exposed by a running app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppService {
    /// Name of this item.
    pub name: String,
    /// Service port.
    pub port: u16,
}
/// State of one app run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRun {
    /// Identifier of the app.
    pub app_id: String,
    /// Identifier of the run.
    pub run_id: String,
    /// Release revision.
    pub revision: u64,
    /// Digest identifying the release.
    pub release_digest: String,
    /// Current execution phase.
    pub phase: RunPhase,
    /// Services exposed by the run.
    pub services: Vec<AppService>,
    /// Error details, when the operation failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Start time in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// Last update time in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}
/// Request to publish an app release.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishRequest {
    /// Identifier of the app.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Revision expected by the caller.
    pub expected_revision: u64,
    /// Path to the source.
    pub source_path: String,
    /// Authoring workspace binding, when available.
    #[serde(default)]
    pub authoring: Option<AuthoringRef>,
    /// Marketplace origin, when present.
    #[serde(default)]
    pub market_origin: Option<MarketOrigin>,
}
/// Published app record and release.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    /// App library record.
    pub record: AppRecord,
    /// Immutable app release.
    pub release: AppRelease,
}
/// Paths and records needed to prepare an app run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunPreparation {
    /// App library record.
    pub record: AppRecord,
    /// Immutable app release.
    pub release: AppRelease,
    /// App run record.
    pub run: AppRun,
    /// Path to the source.
    pub source_path: String,
    /// Path to the run working directory.
    pub work_path: String,
    /// Path to the app data directory.
    pub data_path: String,
    /// Scratch data for install/build/venv steps. Only service processes receive dataPath.
    pub preparation_data_path: String,
    /// Whether the run environment is already prepared.
    pub environment_prepared: bool,
}
/// Filesystem paths associated with an app release.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLayout {
    /// Path to the source.
    pub source_path: String,
    /// Path to the app data directory.
    pub data_path: String,
    /// Path to the releases directory.
    pub releases_path: String,
    /// Path to the runs directory.
    pub runs_path: String,
}
/// Host routing decision for an authoring workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoringResolution {
    /// `resumeSession`, `createSession`, `sourceMissing`, or `rebindRequired`.
    /// This is a routing decision, not a session mutation.
    pub action: String,
    /// Path to the authoring source directory.
    pub source_directory: String,
    /// Authoring workspace binding, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring: Option<AuthoringRef>,
    /// Identifier of the authoring session, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authoring_session_id: Option<String>,
}
/// An archived legacy app library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyArchive {
    /// Path associated with this item.
    pub path: String,
    /// Kind of archive.
    pub kind: String,
    /// Current status.
    pub status: String,
}
/// An app available for legacy import.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyEntry {
    /// Identifier of the legacy app.
    pub legacy_id: String,
    /// Name of this item.
    pub name: String,
    /// Short description.
    pub summary: String,
    /// Release revision.
    pub revision: u64,
    /// Path to the source.
    pub source_path: String,
    /// Current status.
    pub status: String,
}

/// Canonical library operations. Hosts additionally implement `start`, `stop`,
/// `restart`, and `repair`; those cannot succeed without a real process adapter.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum LibraryRequest {
    /// List apps.
    List {},
    /// Read an app record.
    Get {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
    },
    /// Publish an app release.
    Publish {
        /// Publish request.
        #[serde(flatten)]
        request: PublishRequest,
    },
    /// List published releases.
    Releases {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
    },
    /// Read a release.
    GetRelease {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        revision: u64,
    },
    /// Read the app filesystem layout.
    Layout {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        #[serde(default)]
        revision: Option<u64>,
    },
    /// Grant execution consent for a release.
    Grant {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Content digest.
        digest: String,
    },
    /// Revoke execution consent.
    Revoke {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
    },
    /// Activate a published release.
    ActivateRelease {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        revision: u64,
    },
    /// Prepare a release for execution.
    PrepareRun {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        #[serde(default)]
        revision: Option<u64>,
        /// Whether to rebuild the run environment.
        #[serde(default)]
        rebuild: bool,
    },
    /// Update a run record.
    UpdateRun {
        /// App run record.
        run: AppRun,
    },
    /// List runs for an app.
    Runs {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
    },
    /// Read a run log.
    Logs {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Identifier of the run.
        #[serde(rename = "runId")]
        run_id: String,
    },
    /// Append text to a run log.
    AppendLog {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Identifier of the run.
        #[serde(rename = "runId")]
        run_id: String,
        /// Text content.
        text: String,
    },
    /// Check compatibility with a host target.
    CheckTarget {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        #[serde(default)]
        revision: Option<u64>,
        /// Host target to check.
        target: super::Target,
    },
    /// Mark an app removed.
    Remove {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
    },
    /// Export a release.
    Export {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        revision: u64,
        /// Destination path.
        destination: String,
    },
    /// Import a package.
    Import {
        /// Path to the source.
        #[serde(rename = "sourcePath")]
        source_path: String,
        /// Authoring workspace binding, when available.
        #[serde(default)]
        authoring: Option<AuthoringRef>,
        /// Marketplace origin, when present.
        #[serde(default, rename = "marketOrigin")]
        market_origin: Option<MarketOrigin>,
    },
    /// Resolve an authoring workspace.
    PrepareAuthoring {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Known authoring session bindings.
        #[serde(rename = "sessionBindings", default)]
        session_bindings: BTreeMap<String, AuthoringSessionContext>,
        /// Whether the source directory exists.
        #[serde(rename = "sourceExists")]
        source_exists: bool,
    },
    /// Bind an app to an authoring workspace.
    BindAuthoring {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Authoring workspace binding, when available.
        authoring: AuthoringRef,
    },
    /// List legacy archives.
    LegacyArchives {},
    /// List legacy app entries.
    LegacyEntries {},
    /// Import a legacy app.
    ImportLegacy {
        /// Identifier of the legacy app.
        #[serde(rename = "legacyId")]
        legacy_id: String,
        /// Path to the source.
        #[serde(rename = "sourcePath")]
        source_path: String,
        /// Authoring workspace binding, when available.
        #[serde(default)]
        authoring: Option<AuthoringRef>,
    },
    /// Restore an authoring workspace.
    RestoreAuthoring {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        #[serde(default)]
        revision: Option<u64>,
        /// Path to the source.
        #[serde(rename = "sourcePath")]
        source_path: String,
        /// Authoring workspace binding, when available.
        authoring: AuthoringRef,
    },
    /// Create a remix from a release.
    Remix {
        /// Identifier of the app.
        #[serde(rename = "appId")]
        app_id: String,
        /// Release revision.
        #[serde(default)]
        revision: Option<u64>,
        /// Path to the source.
        #[serde(rename = "sourcePath")]
        source_path: String,
        /// Authoring workspace binding, when available.
        authoring: AuthoringRef,
        /// Name of this item.
        #[serde(default)]
        name: Option<String>,
    },
}
