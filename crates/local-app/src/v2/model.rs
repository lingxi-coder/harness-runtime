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
pub enum ProjectRef {
    DesktopPath { path: String },
    MobileProject { project_id: String },
}

/// Stable host binding for a mutable authoring workspace. A partial binding
/// needs an explicit native bind before the host can resume authoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoringRef {
    pub source_directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoringSessionContext {
    pub workspace_id: String,
    pub source_directory: String,
    pub project_ref: ProjectRef,
    /// The engine's persisted session mode (`code` is required for authoring).
    pub mode: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketOrigin {
    pub app_id: String,
    pub release_id: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remixed_from: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRecord {
    pub app_id: String,
    pub name: String,
    pub summary: String,
    pub current_revision: u64,
    pub latest_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring: Option<AuthoringRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_origin: Option<MarketOrigin>,
    pub origin_port: u16,
    pub data_schema_version: u32,
    pub execution_grants: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub removed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub executable: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRelease {
    pub app_id: String,
    pub revision: u64,
    pub digest: String,
    pub manifest: Manifest,
    pub files: Vec<ReleaseFile>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_origin: Option<MarketOrigin>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunPhase {
    Preparing,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    Interrupted,
    Suspended,
}
impl RunPhase {
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Starting | Self::Running | Self::Stopping
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppService {
    pub name: String,
    pub port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRun {
    pub app_id: String,
    pub run_id: String,
    pub revision: u64,
    pub release_digest: String,
    pub phase: RunPhase,
    pub services: Vec<AppService>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishRequest {
    #[serde(default)]
    pub app_id: Option<String>,
    pub expected_revision: u64,
    pub source_path: String,
    #[serde(default)]
    pub authoring: Option<AuthoringRef>,
    #[serde(default)]
    pub market_origin: Option<MarketOrigin>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub record: AppRecord,
    pub release: AppRelease,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunPreparation {
    pub record: AppRecord,
    pub release: AppRelease,
    pub run: AppRun,
    pub source_path: String,
    pub work_path: String,
    pub data_path: String,
    /// Scratch data for install/build/venv steps. Only service processes receive dataPath.
    pub preparation_data_path: String,
    pub environment_prepared: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLayout {
    pub source_path: String,
    pub data_path: String,
    pub releases_path: String,
    pub runs_path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoringResolution {
    /// `resumeSession`, `createSession`, `sourceMissing`, or `rebindRequired`.
    /// This is a routing decision, not a session mutation.
    pub action: String,
    pub source_directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring: Option<AuthoringRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authoring_session_id: Option<String>,
}
/// Canonical library operations. Hosts additionally implement `start`, `stop`,
/// `restart`, and `repair`; those cannot succeed without a real process adapter.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum LibraryRequest {
    List {},
    Get {
        #[serde(rename = "appId")]
        app_id: String,
    },
    Publish {
        #[serde(flatten)]
        request: PublishRequest,
    },
    Releases {
        #[serde(rename = "appId")]
        app_id: String,
    },
    GetRelease {
        #[serde(rename = "appId")]
        app_id: String,
        revision: u64,
    },
    Layout {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(default)]
        revision: Option<u64>,
    },
    Grant {
        #[serde(rename = "appId")]
        app_id: String,
        digest: String,
    },
    Revoke {
        #[serde(rename = "appId")]
        app_id: String,
    },
    ActivateRelease {
        #[serde(rename = "appId")]
        app_id: String,
        revision: u64,
    },
    PrepareRun {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(default)]
        revision: Option<u64>,
        #[serde(default)]
        rebuild: bool,
    },
    UpdateRun {
        run: AppRun,
    },
    Runs {
        #[serde(rename = "appId")]
        app_id: String,
    },
    Logs {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(rename = "runId")]
        run_id: String,
    },
    AppendLog {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(rename = "runId")]
        run_id: String,
        text: String,
    },
    CheckTarget {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(default)]
        revision: Option<u64>,
        target: super::Target,
    },
    Remove {
        #[serde(rename = "appId")]
        app_id: String,
    },
    Export {
        #[serde(rename = "appId")]
        app_id: String,
        revision: u64,
        destination: String,
    },
    Import {
        #[serde(rename = "sourcePath")]
        source_path: String,
        #[serde(default)]
        authoring: Option<AuthoringRef>,
        #[serde(default, rename = "marketOrigin")]
        market_origin: Option<MarketOrigin>,
    },
    PrepareAuthoring {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(rename = "sessionBindings", default)]
        session_bindings: BTreeMap<String, AuthoringSessionContext>,
        #[serde(rename = "sourceExists")]
        source_exists: bool,
    },
    BindAuthoring {
        #[serde(rename = "appId")]
        app_id: String,
        authoring: AuthoringRef,
    },
    RestoreAuthoring {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(default)]
        revision: Option<u64>,
        #[serde(rename = "sourcePath")]
        source_path: String,
        authoring: AuthoringRef,
    },
    Remix {
        #[serde(rename = "appId")]
        app_id: String,
        #[serde(default)]
        revision: Option<u64>,
        #[serde(rename = "sourcePath")]
        source_path: String,
        authoring: AuthoringRef,
        #[serde(default)]
        name: Option<String>,
    },
}
