//! App-scoped persisted and session capability grants.
//!
//! Only `always` decisions are written to disk. `allow once` is consumed by
//! the caller for one operation and `allow session` lives in
//! [`SessionPermissions`], so restarting the host cannot accidentally turn a
//! temporary grant into a durable one.

use crate::error::AppError;
use crate::manifest::{validate_domain, AppLayout};
use crate::types::APPS_SCHEMA_VERSION;
use platform_api::rooted_fs::{self, AtomicWriteOptions};
use platform_api::FsError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_PERMISSIONS_BYTES: u64 = 512 * 1024;

/// The workspace-local permission settings written when a local app is
/// created. These BYTES are what ships — there is deliberately no second
/// in-Rust copy of the rule strings to drift out of step with the template.
///
/// Two things about the spelling are easy to get wrong:
///
/// - `Edit` is the Claude Code permission verb for the whole file-editing
///   family. Verified against the 2.1.235 binary: `getPatternsByRoot` does
///   `switch(t){case"edit":return Il;case"read":return zs}` with `Il="Edit"`,
///   so a `Write` / `MultiEdit` / `NotebookEdit` call consults `Edit`-named
///   rules. A `Write(...)` rule here would look right in JSON and grant
///   nothing.
/// - `./**` does NOT anchor to "wherever this file lives". `patternWithRootFor`
///   strips the leading `./` and returns `root: null`, which
///   `matchingRuleForInput` resolves as the CURRENT working directory. The
///   grant is therefore scoped to the app workspace only because a `.localApp`
///   session's cwd IS that workspace; it is not the `./` doing the scoping.
const LOCAL_APP_WORKSPACE_PERMISSION_SETTINGS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/default-workspace-settings.local.json"
));

fn default_grant_epoch() -> u64 {
    1
}

fn is_initial_grant_epoch(value: &u64) -> bool {
    *value == 1
}

/// Agent capability that requires user authorization before mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppCapability {
    /// Insert, update, or delete native records.
    DataMutation,
    /// Click, fill, navigate, or otherwise control the app `WebView`.
    UiControl,
    /// Capture a photo with the device camera.
    Camera,
    /// Pick an image from the device photo library.
    PhotoLibrary,
    /// Record audio with the device microphone.
    Microphone,
    /// Read the device's current location, once per call.
    Location,
    /// Post local notifications on the app's behalf.
    Notifications,
    /// Read an app-private file.
    FilesRead,
    /// Write an app-private file.
    FilesWrite,
    /// Read non-sensitive device status.
    DeviceStatus,
    /// Trigger bounded haptic feedback.
    Haptics,
    /// Open an authorized external URL.
    DeepLink,
    /// Read or write the system clipboard.
    Clipboard,
    /// Open the native system share sheet.
    Share,
    /// Synthesize text using the native speech engine.
    TextToSpeech,
    /// Read calendar events through the native calendar provider.
    Calendar,
    /// Search contacts through the native contacts provider.
    Contacts,
    /// Read one app-owned retained media handle.
    Media,
    /// Send side-query requests to the user's configured LLM.
    Llm,
    /// Post events into the conversation-facing app mailbox.
    AgentNotify,
    /// Register a declarative flow with the system background scheduler.
    BackgroundSchedule,
}

impl AppCapability {
    /// Every serialized capability in stable catalog order.
    ///
    /// The MCP schema serializes this list through Serde, so its advertised
    /// strings cannot drift from manifest and permission decoding.
    pub const ALL: [Self; 21] = [
        Self::DataMutation,
        Self::UiControl,
        Self::Camera,
        Self::PhotoLibrary,
        Self::Microphone,
        Self::Location,
        Self::Notifications,
        Self::FilesRead,
        Self::FilesWrite,
        Self::DeviceStatus,
        Self::Haptics,
        Self::DeepLink,
        Self::Clipboard,
        Self::Share,
        Self::TextToSpeech,
        Self::Calendar,
        Self::Contacts,
        Self::Media,
        Self::Llm,
        Self::AgentNotify,
        Self::BackgroundSchedule,
    ];
}

/// User decision returned by a capability prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Authorize exactly the pending operation.
    AllowOnce,
    /// Authorize subsequent matching operations until the host session ends.
    AllowSession,
    /// Persist the grant for this app until explicitly revoked.
    AlwaysAllow,
    /// Deny the pending operation without persisting a denial.
    Deny,
}

/// Durable per-app grants stored in `permissions.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPermissions {
    /// Persisted local-app schema version.
    pub schema_version: u32,
    /// Capabilities the user chose to always allow.
    #[serde(default)]
    pub always_allowed_capabilities: BTreeSet<AppCapability>,
    /// HTTPS hostnames the user chose to always allow for this app.
    #[serde(default)]
    pub always_allowed_domains: BTreeSet<String>,
    /// Monotonic host-owned epoch for invalidating stale capability context.
    /// Omitted legacy files deserialize as epoch 1 and remain byte-compatible.
    #[serde(
        default = "default_grant_epoch",
        skip_serializing_if = "is_initial_grant_epoch"
    )]
    pub grant_epoch: u64,
}

impl Default for AppPermissions {
    fn default() -> Self {
        Self {
            schema_version: APPS_SCHEMA_VERSION,
            always_allowed_capabilities: BTreeSet::new(),
            always_allowed_domains: BTreeSet::new(),
            grant_epoch: 1,
        }
    }
}

impl AppPermissions {
    /// True when `capability` has a durable grant.
    #[must_use]
    pub fn allows(&self, capability: AppCapability) -> bool {
        self.always_allowed_capabilities.contains(&capability)
    }

    /// True when `domain` has a durable per-domain network grant.
    #[must_use]
    pub fn allows_domain(&self, domain: &str) -> bool {
        self.always_allowed_domains.contains(domain)
    }

    /// Add a durable capability grant.
    pub fn grant(&mut self, capability: AppCapability) {
        self.always_allowed_capabilities.insert(capability);
        self.bump_grant_epoch();
    }

    /// Revoke a durable capability grant.
    pub fn revoke(&mut self, capability: AppCapability) {
        self.always_allowed_capabilities.remove(&capability);
        self.bump_grant_epoch();
    }

    /// Add a durable domain grant after validating the hostname.
    pub fn grant_domain(&mut self, domain: impl Into<String>) -> Result<(), AppError> {
        let domain = domain.into();
        validate_domain(&domain)?;
        self.always_allowed_domains.insert(domain);
        self.bump_grant_epoch();
        Ok(())
    }

    /// Revoke a durable domain grant.
    pub fn revoke_domain(&mut self, domain: &str) {
        self.always_allowed_domains.remove(domain);
        self.bump_grant_epoch();
    }

    fn bump_grant_epoch(&mut self) {
        self.grant_epoch = self.grant_epoch.saturating_add(1).max(1);
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != APPS_SCHEMA_VERSION {
            return Err(AppError::StorageCorrupt(format!(
                "permissions schemaVersion {} is unsupported (expected {APPS_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        if self.grant_epoch == 0 {
            return Err(AppError::StorageCorrupt(
                "permissions grantEpoch must be non-zero".into(),
            ));
        }
        for domain in &self.always_allowed_domains {
            validate_domain(domain).map_err(|error| {
                AppError::StorageCorrupt(format!("invalid persisted domain: {error}"))
            })?;
        }
        Ok(())
    }
}

/// In-memory grants that expire with the host session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPermissions {
    capabilities: BTreeSet<(String, AppCapability)>,
    domains: BTreeSet<(String, String)>,
}

impl SessionPermissions {
    /// Grant `capability` to one app for this session.
    pub fn grant(&mut self, app_id: impl Into<String>, capability: AppCapability) {
        self.capabilities.insert((app_id.into(), capability));
    }

    /// Test an app-scoped session capability grant.
    #[must_use]
    pub fn allows(&self, app_id: &str, capability: AppCapability) -> bool {
        self.capabilities
            .contains(&(app_id.to_string(), capability))
    }

    /// Grant one network domain to an app for this session.
    pub fn grant_domain(
        &mut self,
        app_id: impl Into<String>,
        domain: impl Into<String>,
    ) -> Result<(), AppError> {
        let app_id = app_id.into();
        let domain = domain.into();
        crate::ids::validate_app_id(&app_id)?;
        validate_domain(&domain)?;
        self.domains.insert((app_id, domain));
        Ok(())
    }

    /// Test an app-scoped session domain grant.
    #[must_use]
    pub fn allows_domain(&self, app_id: &str, domain: &str) -> bool {
        self.domains
            .contains(&(app_id.to_string(), domain.to_string()))
    }

    /// Remove all temporary grants for one app.
    pub fn revoke_app(&mut self, app_id: &str) {
        self.capabilities.retain(|(id, _)| id != app_id);
        self.domains.retain(|(id, _)| id != app_id);
    }
}

/// Load persisted permissions, returning deny-by-default state when the file
/// has not been created yet.
pub fn load_permissions(layout: &AppLayout) -> Result<AppPermissions, AppError> {
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &layout.permissions_rel(),
        MAX_PERMISSIONS_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(AppPermissions::default()),
        Err(error) => return Err(AppError::from_fs("read app permissions", &error)),
    };
    let permissions: AppPermissions = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("app permissions: {error}")))?;
    permissions.validate()?;
    Ok(permissions)
}

/// Atomically persist the app's durable grants.
pub fn save_permissions(layout: &AppLayout, permissions: &AppPermissions) -> Result<(), AppError> {
    permissions.validate()?;
    layout.initialize()?;
    save_permissions_initialized(layout, permissions)
}

/// Persist grants after the caller has already initialized the complete app
/// layout. Used by the create transaction to avoid duplicate directory checks.
pub(crate) fn save_permissions_initialized(
    layout: &AppLayout,
    permissions: &AppPermissions,
) -> Result<(), AppError> {
    permissions.validate()?;
    let mut body = serde_json::to_vec_pretty(permissions)
        .map_err(|error| AppError::Io(format!("serialize app permissions: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_PERMISSIONS_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "app permissions are {} bytes (limit {MAX_PERMISSIONS_BYTES})",
            body.len()
        )));
    }
    rooted_fs::atomic_write(
        layout.root(),
        &layout.permissions_rel(),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write app permissions", &error))
}

/// Write the initial local-app workspace permission file.
///
/// This is deliberately separate from [`AppPermissions`]: runtime capability
/// grants (camera, LLM, notifications, etc.) remain deny-by-default, while
/// the app-build agent gets file read/edit access scoped to this workspace root.
/// The file is host-created during app creation; it is not a user capability
/// grant and does not change the global permission mode.
pub fn save_workspace_permission_settings(layout: &AppLayout) -> Result<(), AppError> {
    layout.initialize()?;
    save_workspace_permission_settings_initialized(layout)
}

/// Write workspace permission settings after layout initialization has
/// already been performed by the enclosing create transaction.
pub(crate) fn save_workspace_permission_settings_initialized(
    layout: &AppLayout,
) -> Result<(), AppError> {
    rooted_fs::atomic_write(
        layout.root(),
        &layout.workspace_settings_local_rel(),
        LOCAL_APP_WORKSPACE_PERMISSION_SETTINGS,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write workspace permission settings", &error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_permissions_round_trip_and_revoke() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        assert_eq!(
            load_permissions(&layout).unwrap(),
            AppPermissions::default()
        );

        let mut permissions = AppPermissions::default();
        permissions.grant(AppCapability::DataMutation);
        permissions.grant_domain("api.example.com").unwrap();
        assert!(permissions.grant_epoch > 1);
        save_permissions(&layout, &permissions).unwrap();
        assert_eq!(load_permissions(&layout).unwrap(), permissions);

        permissions.revoke(AppCapability::DataMutation);
        permissions.revoke_domain("api.example.com");
        assert!(!permissions.allows(AppCapability::DataMutation));
        assert!(!permissions.allows_domain("api.example.com"));
    }

    #[test]
    fn all_capability_wire_spellings_are_snake_case() {
        // These strings are the wire contract shared by plan JSON, the
        // persisted manifest, and permissions.json — pin them.
        assert_eq!(
            serde_json::to_value(AppCapability::ALL).unwrap(),
            serde_json::json!([
                "data_mutation",
                "ui_control",
                "camera",
                "photo_library",
                "microphone",
                "location",
                "notifications",
                "files_read",
                "files_write",
                "device_status",
                "haptics",
                "deep_link",
                "clipboard",
                "share",
                "text_to_speech",
                "calendar",
                "contacts",
                "media",
                "llm",
                "agent_notify",
                "background_schedule"
            ])
        );
        for (capability, wire) in [
            (AppCapability::DataMutation, "\"data_mutation\""),
            (AppCapability::UiControl, "\"ui_control\""),
            (AppCapability::Camera, "\"camera\""),
            (AppCapability::PhotoLibrary, "\"photo_library\""),
            (AppCapability::Microphone, "\"microphone\""),
            (AppCapability::Location, "\"location\""),
            (AppCapability::Notifications, "\"notifications\""),
            (AppCapability::FilesRead, "\"files_read\""),
            (AppCapability::FilesWrite, "\"files_write\""),
            (AppCapability::DeviceStatus, "\"device_status\""),
            (AppCapability::Haptics, "\"haptics\""),
            (AppCapability::DeepLink, "\"deep_link\""),
            (AppCapability::Clipboard, "\"clipboard\""),
            (AppCapability::Share, "\"share\""),
            (AppCapability::TextToSpeech, "\"text_to_speech\""),
            (AppCapability::Calendar, "\"calendar\""),
            (AppCapability::Contacts, "\"contacts\""),
            (AppCapability::Media, "\"media\""),
            (AppCapability::Llm, "\"llm\""),
            (AppCapability::AgentNotify, "\"agent_notify\""),
            (AppCapability::BackgroundSchedule, "\"background_schedule\""),
        ] {
            assert_eq!(serde_json::to_string(&capability).unwrap(), wire);
        }
    }

    #[test]
    fn device_capability_grants_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        let mut permissions = AppPermissions::default();
        permissions.grant(AppCapability::Camera);
        permissions.grant(AppCapability::Llm);
        save_permissions(&layout, &permissions).unwrap();
        assert_eq!(load_permissions(&layout).unwrap(), permissions);
        permissions.revoke(AppCapability::Camera);
        assert!(!permissions.allows(AppCapability::Camera));
        assert!(permissions.allows(AppCapability::Llm));
    }

    #[test]
    fn session_grants_for_device_capabilities_are_app_scoped() {
        let mut session = SessionPermissions::default();
        session.grant("abcd1234", AppCapability::Microphone);
        assert!(session.allows("abcd1234", AppCapability::Microphone));
        assert!(!session.allows("other123", AppCapability::Microphone));
    }

    #[test]
    fn session_permissions_are_app_scoped() {
        let mut session = SessionPermissions::default();
        session.grant("abcd1234", AppCapability::UiControl);
        session.grant_domain("abcd1234", "api.example.com").unwrap();
        assert!(session.allows("abcd1234", AppCapability::UiControl));
        assert!(!session.allows("other123", AppCapability::UiControl));
        assert!(session.allows_domain("abcd1234", "api.example.com"));
        session.revoke_app("abcd1234");
        assert!(!session.allows("abcd1234", AppCapability::UiControl));
    }

    #[test]
    fn new_workspace_gets_scoped_read_and_edit_rules() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        save_workspace_permission_settings(&layout).unwrap();

        let path = root.path().join(layout.workspace_settings_local_rel());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            value["permissions"]["allow"],
            serde_json::json!([
                "Read(./**)",
                "Edit(./**)",
                "LocalAppLogs",
                "LocalAppBuild",
                "LocalAppRuntime"
            ])
        );
        assert_eq!(
            value["permissions"]["deny"],
            serde_json::json!([
                "Edit(./.lingxi/**)",
                "Edit(./.gitignore)",
                "Edit(./LINGXI.md)",
                "Edit(./lib/lingxi-bridge.js)"
            ])
        );
        assert_eq!(
            std::fs::read(root.path().join(layout.workspace_settings_local_rel())).unwrap(),
            LOCAL_APP_WORKSPACE_PERMISSION_SETTINGS
        );
    }
}
