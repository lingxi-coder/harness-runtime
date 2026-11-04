//! Public, camelCase schema-2 records. No credentials or runtime policy enter a release.
use super::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
/// A native host's reference to the project containing an app's source.
pub enum ProjectRef {
    /// A desktop project identified by its filesystem path.
    DesktopPath {
        /// Desktop project filesystem path.
        path: String,
    },
    /// A mobile project identified by its host project ID.
    MobileProject {
        /// Mobile host project identifier.
        project_id: String,
    },
}

/// Stable host binding for a mutable authoring workspace. A partial binding
/// needs an explicit native bind before the host can resume authoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoringRef {
    /// Source directory associated with this authoring workspace.
    pub source_directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Native workspace identifier, when a workspace has been bound.
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Host project associated with the authoring source, when known.
    pub project_ref: Option<ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Persisted authoring session identifier, when one has been bound.
    pub authoring_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Associated app identifier, when this binding has an owner.
    pub app_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Host-provided context used to match an existing authoring session.
pub struct AuthoringSessionContext {
    /// Workspace identifier associated with the session.
    pub workspace_id: String,
    /// Source directory associated with the session.
    pub source_directory: String,
    /// Host project associated with the session.
    pub project_ref: ProjectRef,
    /// The engine's persisted session mode (`code` is required for authoring).
    pub mode: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Market release provenance associated with an app or release.
pub struct MarketOrigin {
    /// App identifier in the Market.
    pub app_id: String,
    /// Market release identifier.
    pub release_id: String,
    /// Digest identifying the Market release.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Market app identifier recorded as the remix source, when present.
    pub remixed_from: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Mutable library record for an installed app and its release history.
pub struct AppRecord {
    /// Stable identifier assigned to the local app.
    pub app_id: String,
    /// Display name from the most recently published or activated manifest.
    pub name: String,
    /// Summary from the most recently published or activated manifest.
    pub summary: String,
    /// Active release revision used by default operations.
    pub current_revision: u64,
    /// Most recently published revision, used for publication conflicts.
    pub latest_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Preferred native authoring binding, when available.
    pub authoring: Option<AuthoringRef>,
    /// Immutable source authorities explicitly associated with this app.
    /// Current authoring is only a preferred session pointer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authoring_source_ids: Vec<String>,
    /// Durable native publication outcomes, committed with the app record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authoring_publications: Vec<AuthoringPublicationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Market provenance, when the app originated from a Market release.
    pub market_origin: Option<MarketOrigin>,
    /// Stable local origin port allocated to this app.
    pub origin_port: u16,
    /// Schema version of the app's retained data.
    pub data_schema_version: u32,
    /// Release digests for which execution has been granted.
    pub execution_grants: Vec<String>,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Last record update time in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Whether the app has been marked removed.
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Durable result of an authoring publication request.
pub struct AuthoringPublicationReceipt {
    /// Identifier used to deduplicate the publication request.
    pub request_id: String,
    /// SHA-256 digest of the publication request payload.
    pub payload_sha256: String,
    /// Published release revision.
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Authoring binding of the publisher, when recorded.
    pub publisher: Option<AuthoringRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Immutable authoring source identifier, when recorded.
    pub source_id: Option<String>,
    /// Record returned by the committed publication, without receipt history.
    pub record: serde_json::Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Inventory entry for a file in an immutable app release.
pub struct ReleaseFile {
    /// Path of the file relative to the release source directory.
    pub path: String,
    /// SHA-256 digest of the file contents.
    pub sha256: String,
    /// Length of the file in bytes.
    pub bytes: u64,
    #[serde(default)]
    /// Whether the packaged file has executable permission.
    pub executable: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Immutable published app release and its source inventory.
pub struct AppRelease {
    /// Identifier of the app that owns this release.
    pub app_id: String,
    /// Revision number assigned to this release.
    pub revision: u64,
    /// Digest derived from the manifest and file inventory.
    pub digest: String,
    /// Validated manifest published with the release.
    pub manifest: Manifest,
    /// File inventory used to verify the release.
    pub files: Vec<ReleaseFile>,
    /// Publication time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Market provenance associated with the release, when present.
    pub market_origin: Option<MarketOrigin>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Lifecycle phase recorded for an app run.
pub enum RunPhase {
    /// The library is preparing the run environment.
    Preparing,
    /// The host is starting the run.
    Starting,
    /// The host reports the run as running.
    Running,
    /// The host is stopping the run.
    Stopping,
    /// The run has stopped.
    Stopped,
    /// The run failed.
    Failed,
    /// The run was interrupted.
    Interrupted,
    /// The run is suspended.
    Suspended,
}
impl RunPhase {
    /// Returns whether this phase represents a live or transitioning run.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Starting | Self::Running | Self::Stopping
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Service endpoint reported for an app run.
pub struct AppService {
    /// Name of the declared app service.
    pub name: String,
    /// Port assigned to the service.
    pub port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Persistent state and service information for one app run.
pub struct AppRun {
    /// Identifier of the owning app.
    pub app_id: String,
    /// Identifier of this run.
    pub run_id: String,
    /// Release revision used for the run.
    pub revision: u64,
    /// Digest of the release used for the run.
    pub release_digest: String,
    /// Most recently recorded lifecycle phase.
    pub phase: RunPhase,
    /// Service endpoints reported for the run.
    pub services: Vec<AppService>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Failure detail, when the host recorded one.
    pub error: Option<String>,
    /// Time recorded when the run was prepared, in Unix epoch milliseconds.
    pub started_at_ms: u64,
    /// Last update time in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// Request to publish a source directory as an app release.
pub struct PublishRequest {
    #[serde(default)]
    /// Existing app ID, or `None` to create a new app.
    pub app_id: Option<String>,
    /// Expected latest revision; a new app requires zero.
    pub expected_revision: u64,
    /// Path to the source directory to publish.
    pub source_path: String,
    #[serde(default)]
    /// Authoring binding to associate with the publication, if supplied.
    pub authoring: Option<AuthoringRef>,
    #[serde(default)]
    /// Market provenance to associate with the publication, if supplied.
    pub market_origin: Option<MarketOrigin>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// App record and immutable release produced by publication.
pub struct PublishResult {
    /// Updated app record.
    pub record: AppRecord,
    /// Newly published release.
    pub release: AppRelease,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Library data and paths needed by a host to prepare a run.
pub struct RunPreparation {
    /// Record of the app being prepared.
    pub record: AppRecord,
    /// Verified release selected for the run.
    pub release: AppRelease,
    /// New run record in the preparing phase.
    pub run: AppRun,
    /// Path to the immutable release source.
    pub source_path: String,
    /// Path to the run's working environment, which may be reused.
    pub work_path: String,
    /// Path to the app's retained data directory.
    pub data_path: String,
    /// Scratch data for install/build/venv steps. Only service processes receive dataPath.
    pub preparation_data_path: String,
    /// Whether an existing prepared environment was reused.
    pub environment_prepared: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Filesystem locations associated with an app release.
pub struct AppLayout {
    /// Path to the selected release's source directory.
    pub source_path: String,
    /// Path to the app's retained data directory.
    pub data_path: String,
    /// Path containing the app's immutable releases.
    pub releases_path: String,
    /// Path containing the app's run records and work directories.
    pub runs_path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Host routing decision for resuming or creating an authoring session.
pub struct AuthoringResolution {
    /// `resumeSession`, `createSession`, `sourceMissing`, or `rebindRequired`.
    /// This is a routing decision, not a session mutation.
    pub action: String,
    /// Source directory for the authoring decision.
    pub source_directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Authoring binding used for the decision, when available.
    pub authoring: Option<AuthoringRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Session identifier to resume, when one was selected.
    pub authoring_session_id: Option<String>,
}
/// Canonical library operations. Hosts additionally implement `start`, `stop`,
/// `restart`, and `repair`; those cannot succeed without a real process adapter.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum LibraryRequest {
    /// List installed app records.
    List {},
    /// Get one installed app record.
    Get {
        #[serde(rename = "appId")]
        /// Identifier of the requested app.
        app_id: String,
    },
    /// Publish a source directory as a release.
    Publish {
        #[serde(flatten)]
        /// Publication parameters.
        request: PublishRequest,
    },
    /// List releases for an app.
    Releases {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
    },
    /// Get one immutable release.
    GetRelease {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        /// Revision of the requested release.
        revision: u64,
    },
    /// Get app storage paths for a release.
    Layout {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        #[serde(default)]
        /// Release revision, or `None` for the current revision.
        revision: Option<u64>,
    },
    /// Grant execution consent for a release digest.
    Grant {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        /// Release digest receiving execution consent.
        digest: String,
    },
    /// Revoke the app's execution grants.
    Revoke {
        #[serde(rename = "appId")]
        /// Identifier of the app whose grants are revoked.
        app_id: String,
    },
    /// Select an existing release as the current revision.
    ActivateRelease {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        /// Revision to activate.
        revision: u64,
    },
    /// Prepare library state and paths for a host-managed run.
    PrepareRun {
        #[serde(rename = "appId")]
        /// Identifier of the app to run.
        app_id: String,
        #[serde(default)]
        /// Release revision, or `None` for the current revision.
        revision: Option<u64>,
        #[serde(default)]
        /// Whether to bypass a reusable prepared environment.
        rebuild: bool,
    },
    /// Record updated state for an existing run.
    UpdateRun {
        /// Run state to persist.
        run: AppRun,
    },
    /// List recorded runs for an app.
    Runs {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
    },
    /// Read the log for an app run.
    Logs {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        #[serde(rename = "runId")]
        /// Identifier of the run whose log is requested.
        run_id: String,
    },
    /// Append text to an app run's log.
    AppendLog {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        #[serde(rename = "runId")]
        /// Identifier of the run receiving the log text.
        run_id: String,
        /// Text to append.
        text: String,
    },
    /// Check whether a release is compatible with a host target.
    CheckTarget {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        #[serde(default)]
        /// Release revision, or `None` for the current revision.
        revision: Option<u64>,
        /// Host target against which the release is checked.
        target: super::Target,
    },
    /// Mark an app as removed.
    Remove {
        #[serde(rename = "appId")]
        /// Identifier of the app to remove.
        app_id: String,
    },
    /// Export an immutable app release.
    Export {
        #[serde(rename = "appId")]
        /// Identifier of the owning app.
        app_id: String,
        /// Revision to export.
        revision: u64,
        /// Destination path for the export.
        destination: String,
    },
    /// Import a source directory as a new app.
    Import {
        #[serde(rename = "sourcePath")]
        /// Path of the source directory to import.
        source_path: String,
        #[serde(default, rename = "marketOrigin")]
        /// Market provenance to associate with the imported app, if supplied.
        market_origin: Option<MarketOrigin>,
    },
    /// Choose how the host should open an app's authoring source.
    PrepareAuthoring {
        #[serde(rename = "appId")]
        /// Identifier of the app being authored.
        app_id: String,
        #[serde(rename = "sessionBindings", default)]
        /// Host session contexts indexed by session identifier.
        session_bindings: BTreeMap<String, AuthoringSessionContext>,
        #[serde(rename = "sourceExists")]
        /// Whether the host observed that the source directory exists.
        source_exists: bool,
    },
}
