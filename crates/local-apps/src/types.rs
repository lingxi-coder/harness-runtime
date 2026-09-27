//! Core data model for local apps (spec §A).
//!
//! Serde conventions follow the repo's persisted-JSON style (see
//! `cron::tasks_file`): camelCase struct fields, `snake_case` enum variant
//! values, epoch **milliseconds** timestamps as `u64`, optionals omitted when
//! absent. These types are persistence/domain types — the client-protocol
//! crate defines its own wire DTOs and maps to/from these in the engine.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Schema version stamped on every persisted local-apps file.
pub const APPS_SCHEMA_VERSION: u32 = 4;

/// New apps use Git-backed source version control unless the user opts out
/// during creation. Missing values on older records deserialize as enabled.
pub const DEFAULT_GIT_VERSION_CONTROL: bool = true;

fn default_git_version_control() -> bool {
    DEFAULT_GIT_VERSION_CONTROL
}

/// Runtime (dev-server) state of an app (spec §C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppRuntimeState {
    /// No runtime process.
    Stopped,
    /// Runtime is starting up.
    Starting,
    /// Runtime is serving.
    Running,
    /// Runtime is shutting down.
    Stopping,
    /// Runtime failed; `last_error` explains why.
    Failed,
}

impl AppRuntimeState {
    /// Canonical `snake_case` name (the persisted/wire value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for AppRuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Workspace dependency install state for one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppDependencyState {
    /// The workspace exists but its app-local `node_modules` have not been
    /// prepared yet.
    Queued,
    /// A host-owned install task is currently preparing `node_modules`.
    Installing,
    /// The app-local dependency tree is ready for use.
    Ready,
    /// The last install attempt failed; `last_error` explains why.
    Failed,
}

impl AppDependencyState {
    /// Canonical `snake_case` name (the persisted/wire value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Installing => "installing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for AppDependencyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One fixed runtime family from the global local-app catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppRuntimeProfile {
    /// Routed Ionic/React DOM application scaffold.
    #[serde(rename = "react_dom")]
    ReactDom,
    /// Canvas 2D drawn-surface scaffold.
    #[serde(rename = "canvas_2d")]
    Canvas2d,
    /// Three.js 3D drawn-surface scaffold.
    #[serde(rename = "three_3d")]
    Three3d,
    /// Phaser 2D drawn-surface scaffold.
    #[serde(rename = "phaser_2d")]
    Phaser2d,
    /// Babylon.js 3D drawn-surface scaffold.
    #[serde(rename = "babylon_3d")]
    Babylon3d,
}

impl AppRuntimeProfile {
    /// Canonical `snake_case` persisted/wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReactDom => "react_dom",
            Self::Canvas2d => "canvas_2d",
            Self::Three3d => "three_3d",
            Self::Phaser2d => "phaser_2d",
            Self::Babylon3d => "babylon_3d",
        }
    }

    /// Parse the persisted or tool spelling of one runtime profile family.
    pub fn parse(value: &str) -> Result<Self, crate::error::AppError> {
        match value {
            "react_dom" => Ok(Self::ReactDom),
            "canvas_2d" => Ok(Self::Canvas2d),
            "three_3d" => Ok(Self::Three3d),
            "phaser_2d" => Ok(Self::Phaser2d),
            "babylon_3d" => Ok(Self::Babylon3d),
            other => Err(crate::error::AppError::InvalidRequest(format!(
                "unknown app runtime profile {other:?}; expected react_dom, canvas_2d, three_3d, phaser_2d, or babylon_3d"
            ))),
        }
    }
}

impl fmt::Display for AppRuntimeProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of the create-time MCP interview: whether the agent asked the
/// user, during the requirements interview, if they want MCP capabilities set up
/// for this app, and if so, what they answered.
///
/// THREE states, deliberately not a bool and not a plain list — those two
/// shapes cannot tell "never asked" apart from "asked and the user said no",
/// and the Settings flow that later offers MCP authorization needs exactly
/// that distinction: an app whose record carries `None` here should still be
/// offered the interview (an old app, or one whose interview skipped this
/// step), while `Some(Declined)` means it already ran and the user does not
/// want it repeated on every visit.
///
/// - Absent from the record (`None`): never asked.
/// - [`Self::Declined`]: asked; the user said no.
/// - [`Self::Requested`]: asked; the user wants these MCP capabilities. `capabilities`
///   are concrete names drawn from `LocalAppTemplateCatalog`'s
///   `mcpSuggestions` for the confirmed shape's family, never free text the
///   agent invented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AppMcpIntent {
    /// Asked; the user declined MCP for this app.
    Declined,
    /// Asked; the user wants these MCP capabilities set up.
    Requested {
        /// Concrete MCP capability names, drawn from
        /// `LocalAppTemplateCatalog`'s `mcpSuggestions`.
        capabilities: Vec<String>,
    },
}

/// One app as listed in `apps/index.json` (and mirrored into the app's
/// `workspace/.lingxi/app.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRecord {
    /// Stable app id matching `^[a-z0-9][a-z0-9-]{0,53}$`.
    pub id: String,
    /// User-facing display name.
    pub name: String,
    /// One-line description the user gave at creation time. The agent reads
    /// it for context; the list page displays it. Stored ONCE — a second
    /// copy would inevitably drift.
    pub brief: String,
    /// Provider-qualified model selected for app-creation workflows. The
    /// mobile workflow launcher reads this from the app-scoped metadata file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_model: Option<String>,
    /// Outcome of the create-time MCP interview. See [`AppMcpIntent`] —
    /// `None` means the question was never asked, NOT that the user
    /// declined; do not conflate the two when deciding whether to offer the
    /// interview again from Settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_intent: Option<AppMcpIntent>,
    /// Whether Git controls this app's source checkpoints and restores.
    #[serde(default = "default_git_version_control")]
    pub git_enabled: bool,
    /// Creation time, epoch milliseconds.
    pub created_at_ms: u64,
    /// Last mutation time, epoch milliseconds.
    pub updated_at_ms: u64,
    /// Conversation the app was created from (`origin: chat`), if any —
    /// the SOURCE link only; the app's own conversations live in its
    /// workspace-scoped session catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// The catalog/scope the app was created FROM — the working directory the
    /// creating connection was anchored to, captured ONCE at create time.
    ///
    /// Why it is stored instead of re-derived: the engine mints the app's
    /// pinned init session by forking the origin conversation, and that fork
    /// needs the SOURCE catalog to resolve. At create time the caller's cwd
    /// is that catalog, but the boot-time repair that backfills a missing pin
    /// runs much later, on whatever connection happens to be open — its cwd
    /// is the scope the engine was built in, not the app's. Forking from the
    /// wrong scope finds no source and silently degrades the pin to an empty
    /// anchor (`mint_app_init_session` in engine-mobile `host.rs`). This field
    /// is what lets that repair fork from the app's real origin.
    ///
    /// `None` is the honest "origin scope unknown" — the app was not created
    /// from a chat, or it is a dev-store record written before this field
    /// existed. It is NOT a compatibility shim: `Option` is the field's real
    /// domain, the same as `conversation_id` right above. Callers MUST fall
    /// back to their own cwd on `None` (the pre-existing behaviour), and the
    /// create path stores absence rather than `Some("")` so that fallback has
    /// exactly one trigger.
    ///
    /// Set only by the create path; never rewritten afterwards (the origin
    /// scope of an app cannot change), and never used as a filesystem path
    /// without the caller's own containment check — it is a remembered
    /// string, not a validated live directory. Stored verbatim: only the
    /// present-vs-absent decision looks at whitespace (see
    /// `AppService::create_app_with_git_and_workflow_model_and_initializer`).
    ///
    /// EXPOSURE — weigh this before adding another host-scoped field to this
    /// struct. `AppRecord` is serialized WHOLE into two places: the
    /// host-private `apps/index.json`, and the mirror at
    /// `apps/<id>/workspace/.lingxi/app.json`, which sits INSIDE the app
    /// workspace that the app's own build tooling — and any agent working in
    /// that app — can read. `origin_cwd` is the first ABSOLUTE HOST path this
    /// record puts there (the user's project directory, not an app-scoped
    /// one). It is not filtered out of the mirror on purpose:
    /// `storage::repair_torn_commit` compares the mirror against the index
    /// record for WHOLE equality, so a field written to one and not the other
    /// would make every load look like a torn commit and rewrite the index.
    /// If that exposure is judged unacceptable, the fix is a mirror-side
    /// projection of the record, not a `skip_serializing` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_cwd: Option<String>,
    /// The app's pinned "init" session (bare uuid) — the conversation the
    /// app was set up in, listed first in the app's session catalog. Minted
    /// by the engine at create time (fork of the origin chat, or an empty
    /// anchor) and backfilled at boot for apps that predate it. Same
    /// default+skip serde shape as `conversation_id`, so old stores load
    /// unchanged and the goldens stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_session_id: Option<String>,
    /// Workspace directory relative to the data root, always
    /// `apps/<id>/workspace` with forward slashes.
    pub workspace_rel: String,
    /// 工作区里是否已经落下脚手架。
    ///
    /// 每一条新记录都显式写入；缺字段是无效的旧 store（§A.1），不是空壳判据。
    ///
    /// 三个写入点，缺一不可：
    ///   1. `CreateMode::Shell` 在构造记录时写 `false`；
    ///   2. `CreateMode::Scaffolded`（`LocalAppCreate` 的 create+scaffold 路径）
    ///      在构造记录时写 `true`；
    ///   3. `LocalAppScaffold` 的提交点把 `false` 翻成 `true`。
    ///
    /// ⛔ 不要加 `#[serde(default)]`：缺字段必须加载失败并提示清除开发数据，
    /// 而不是静默变成一个可以被 `LocalAppScaffold` 清空的 shell。
    pub scaffolded: bool,
}

/// Why a checkpoint was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppCheckpointKind {
    /// Initial scaffold committed.
    ScaffoldCreated,
    /// Generation output passed validation.
    GenerationValidated,
    /// User approved the preview.
    PreviewApproved,
    /// Explicit user-requested checkpoint.
    UserApproved,
    /// Automatic safety checkpoint taken before a restore.
    PreRestore,
}

/// One restorable checkpoint of an app workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppCheckpoint {
    /// Stable checkpoint id.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Why the checkpoint was recorded.
    pub kind: AppCheckpointKind,
    /// Creation time, epoch milliseconds.
    pub created_at_ms: u64,
}

/// Per-app runtime record persisted at `apps/<id>/runtime.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppRuntimeMode {
    /// Store/Play static export served by the Rust loopback asset server.
    StaticExport,
    /// Legacy persisted mode from builds that used a framework server. New
    /// runtimes always write [`Self::StaticExport`].
    NextProduction,
}

/// Per-app runtime record persisted at `apps/<id>/runtime.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRuntimeRecord {
    /// Persisted schema version ([`APPS_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// App the record belongs to.
    pub app_id: String,
    /// Current runtime state.
    pub state: AppRuntimeState,
    /// Explicit runtime mode; absent only for pre-1.2 migrated records that
    /// have not been started since upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AppRuntimeMode>,
    /// Dev-server port. Once assigned it is NEVER reassigned —
    /// the `localhost:<port>` origin anchors the app's `IndexedDB` data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Dev-server pid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Last runtime failure, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Last mutation time, epoch milliseconds.
    pub updated_at_ms: u64,
}

/// Per-app dependency-install record persisted at `apps/<id>/dependencies.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDependencyRecord {
    /// Persisted schema version ([`APPS_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// App the record belongs to.
    pub app_id: String,
    /// Current dependency install state.
    pub state: AppDependencyState,
    /// SHA-256 of the host-managed dependency lockfile used for the install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_sha256: Option<String>,
    /// Toolchain identity (for example `pnpm@11.22.0/node@24.18.1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain_key: Option<String>,
    /// Monotonic count of attempted installs.
    #[serde(default)]
    pub install_attempts: u32,
    /// Last install failure, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Last mutation time, epoch milliseconds.
    pub updated_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_serialize_to_spec_snake_case_strings() {
        assert_eq!(
            serde_json::to_string(&AppRuntimeState::Stopped).unwrap(),
            "\"stopped\""
        );
        assert_eq!(
            serde_json::to_string(&AppDependencyState::Installing).unwrap(),
            "\"installing\""
        );
        assert_eq!(
            serde_json::to_string(&AppRuntimeProfile::Babylon3d).unwrap(),
            "\"babylon_3d\""
        );
        assert_eq!(
            serde_json::to_string(&AppCheckpointKind::PreRestore).unwrap(),
            "\"pre_restore\""
        );
    }

    #[test]
    fn record_serializes_camel_case_and_omits_absent_conversation() {
        let record = AppRecord {
            id: "abc123".into(),
            name: "Habits".into(),
            brief: "Track daily habits".into(),
            workflow_model: None,
            mcp_intent: None,
            git_enabled: true,
            scaffolded: true,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_001,
            conversation_id: None,
            origin_cwd: None,
            init_session_id: None,
            workspace_rel: "apps/abc123/workspace".into(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"createdAtMs\":1700000000000"));
        assert!(json.contains("\"workspaceRel\":\"apps/abc123/workspace\""));
        assert!(json.contains("\"brief\":\"Track daily habits\""));
        assert!(json.contains("\"gitEnabled\":true"));
        assert!(json.contains("\"scaffolded\":true"));
        assert!(!json.contains("conversationId"));
        assert!(
            !json.contains("mcpIntent"),
            "a never-asked mcp_intent must be omitted from the wire, not written as null"
        );
        let back: AppRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn record_round_trips_a_requested_mcp_intent() {
        let record = AppRecord {
            id: "abc123".into(),
            name: "Habits".into(),
            brief: "Track daily habits".into(),
            workflow_model: None,
            mcp_intent: Some(AppMcpIntent::Requested {
                capabilities: vec!["github".into()],
            }),
            git_enabled: true,
            scaffolded: true,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_001,
            conversation_id: None,
            origin_cwd: None,
            init_session_id: None,
            workspace_rel: "apps/abc123/workspace".into(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""mcpIntent":{"status":"requested","capabilities":["github"]}"#));
        let back: AppRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn origin_cwd_round_trips_and_a_record_without_one_reads_as_unknown() {
        let mut record = AppRecord {
            id: "abc123".into(),
            name: "Habits".into(),
            brief: "Track daily habits".into(),
            workflow_model: None,
            mcp_intent: None,
            git_enabled: true,
            scaffolded: true,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_001,
            conversation_id: Some("conv-1".into()),
            origin_cwd: None,
            init_session_id: None,
            workspace_rel: "apps/abc123/workspace".into(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            !json.contains("originCwd"),
            "an unknown origin scope is omitted from the wire, not written as null"
        );
        assert_eq!(
            serde_json::from_str::<AppRecord>(&json).unwrap().origin_cwd,
            None,
            "a record written without originCwd reads back as unknown"
        );

        record.origin_cwd = Some("/home/dev/projects/atlas".into());
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""originCwd":"/home/dev/projects/atlas""#));
        assert_eq!(serde_json::from_str::<AppRecord>(&json).unwrap(), record);
    }

    #[test]
    fn a_legacy_record_without_mcp_intent_deserializes_as_never_asked() {
        let json = r#"{
            "id": "abc123",
            "name": "Habits",
            "brief": "Track daily habits",
            "scaffolded": true,
            "createdAtMs": 1,
            "updatedAtMs": 2,
            "workspaceRel": "apps/abc123/workspace"
        }"#;
        let record: AppRecord = serde_json::from_str(json).unwrap();
        assert_eq!(
            record.mcp_intent, None,
            "a pre-existing record with no mcpIntent key must deserialize as never-asked"
        );
    }

    #[test]
    fn a_legacy_record_without_git_enabled_still_defaults_true() {
        let json = r#"{
            "id": "abc123",
            "name": "Habits",
            "brief": "Track daily habits",
            "scaffolded": true,
            "createdAtMs": 1,
            "updatedAtMs": 2,
            "workspaceRel": "apps/abc123/workspace"
        }"#;
        let record: AppRecord = serde_json::from_str(json).unwrap();
        assert!(record.git_enabled, "missing gitEnabled defaults to true");
    }
}
