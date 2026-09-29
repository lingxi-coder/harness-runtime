//! Versioned application manifest and filesystem layout.
//!
//! The manifest is the contract shared by generated code, the native data
//! bridge, and the local-apps MCP provider.  Paths are always derived from a
//! validated app id; callers never supply a database or workspace path.

use crate::error::AppError;
use crate::ids;
use crate::mcp_settings::MCP_SETTINGS_FILE;
use crate::permissions::AppCapability;
use crate::runtime_v2::RUNTIME_API_MAJOR;
use crate::types::{AppRuntimeProfile, APPS_SCHEMA_VERSION};
use lingxi_core::host::rooted_fs::{self, AtomicWriteOptions};
use lingxi_core::host::FsError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Manifest filename under `workspace/.lingxi`.
pub const APP_MANIFEST_FILE: &str = "app.manifest.json";
/// Permission settings filename under `workspace/.lingxi`.
///
/// This is the project-local permission file consumed by the permission
/// loader when a local-app workspace is used as the session cwd.
pub const WORKSPACE_SETTINGS_LOCAL_FILE: &str = "settings.local.json";
/// App-private data directory.
pub const DATA_DIR: &str = "data";
/// `SQLite` database filename.
pub const DATA_DATABASE_FILE: &str = "app.sqlite";
/// Build output root.
pub const BUILD_DIR: &str = "build";
/// Static Store/Play build output.
pub const STORE_BUILD_DIR: &str = "store";
/// Legacy Full/Direct build output retained for workspace migration.
pub const FULL_BUILD_DIR: &str = "full";
/// App-private log directory.
pub const LOGS_DIR: &str = "logs";
/// Runtime state filename.
pub const RUNTIME_STATE_FILE: &str = "runtime.json";
/// Persisted capability decisions filename.
pub const PERMISSIONS_FILE: &str = "permissions.json";
/// Durable generation queue filename.
pub const GENERATION_JOBS_FILE: &str = "generation-jobs.json";
/// App-to-conversation mailbox filename.
pub const MAILBOX_FILE: &str = "mailbox.json";
/// Host-owned MCP catalog root under one app's private data directory.
pub const MCP_DIR: &str = "mcp";
/// Directory containing immutable MCP catalogs under `mcp/`.
pub const MCP_CATALOGS_DIR: &str = "catalogs";
/// Durable MCP authoring journal under `mcp/`.
pub const MCP_AUTHORING_JOURNAL_FILE: &str = "authoring-journal.json";

const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum UTF-8 byte length for manifest, collection and field display names.
pub const MAX_MANIFEST_DISPLAY_NAME_BYTES: usize = 200;
/// Maximum number of native data collections in one manifest.
pub const MAX_MANIFEST_COLLECTIONS: usize = 64;
/// Maximum number of fields in one native data collection.
pub const MAX_COLLECTION_FIELDS: usize = 128;
/// Maximum number of options declared by an enum field.
pub const MAX_ENUM_OPTIONS: usize = 100;
/// Maximum UTF-8 byte length of one enum option.
pub const MAX_ENUM_OPTION_BYTES: usize = 500;
/// Record properties supplied by the host rather than stored in `document`.
pub const HOST_OWNED_RECORD_FIELD_IDS: [&str; 4] =
    ["recordId", "revision", "createdAtMs", "updatedAtMs"];

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered = object
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            let mut rebuilt = Map::with_capacity(ordered.len());
            for (key, value) in ordered {
                rebuilt.insert(key, value);
            }
            Value::Object(rebuilt)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

fn canonical_hash(value: Value, context: &str) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(&canonicalize_json(value))
        .map_err(|error| AppError::Io(format!("serialize {context}: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Canonical SHA-256 of an immutable MCP catalog JSON body.
pub fn hash_mcp_catalog(value: Value) -> Result<String, AppError> {
    canonical_hash(value, "MCP catalog")
}

/// Supported native collection field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataFieldKind {
    /// Single-line string.
    Text,
    /// Multi-line string.
    LongText,
    /// Signed 64-bit integer.
    Integer,
    /// Finite JSON number.
    Decimal,
    /// Boolean value.
    Boolean,
    /// ISO-8601/RFC-3339-shaped timestamp string.
    DateTime,
    /// One of the field's declared options.
    Enum,
    /// Opaque native image reference string.
    ImageRef,
}

/// One field in a native data collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataFieldSchema {
    /// Stable programmatic field id.
    pub id: String,
    /// User-facing field label.
    pub label: String,
    /// Stored value type.
    pub kind: DataFieldKind,
    /// Whether every newly written record must contain this field.
    #[serde(default)]
    pub required: bool,
    /// Allowed values when `kind` is [`DataFieldKind::Enum`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_options: Vec<String>,
}

/// A collection exposed through the controlled native record API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataCollectionSchema {
    /// Stable programmatic collection id.
    pub id: String,
    /// User-facing collection name.
    pub name: String,
    /// Record fields, in designer order.
    pub fields: Vec<DataFieldSchema>,
}

/// Native host target a generated app was confirmed for.
///
/// Host-owned by construction: the engine derives it from the native
/// client's reported host facts. It is never authored by the model and
/// never inferred from the browser user agent — the model can only see the
/// mobile runtime reminder, whose `Device class: phone` vocabulary does not
/// name an iOS form factor, so asking it to declare this pair produced a
/// value the validator below had to reject.
///
/// It records only the STABLE target pair. Viewport, safe area, color
/// scheme, reduced motion, and input mode are live values the generated page
/// reads from `window.lingxi.v2.deviceContext` at runtime; a snapshot of them
/// taken when the target was confirmed could only go stale. An absent context
/// means "unknown", which every consumer already tolerates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceContext {
    /// `ios`, `android`, `desktop`, or `unknown`.
    pub os: String,
    /// `iphone`, `ipad`, `phone`, `tablet`, `desktop`, or `unknown`.
    pub form_factor: String,
}

impl DeviceContext {
    /// Derive the confirmed native target from the native client's host facts.
    ///
    /// `None` when the client could not classify the device: every pair that
    /// names a real platform would be a guess, and an absent context already
    /// carries exactly that meaning.
    #[must_use]
    pub fn from_host_environment(
        environment: &lingxi_core::host::MobileHostEnvironment,
    ) -> Option<Self> {
        let (os, form_factor) = match (environment.host_os, environment.device_class) {
            (lingxi_core::host::MobileHostOs::Ios, lingxi_core::host::MobileDeviceClass::Phone) => {
                ("ios", "iphone")
            }
            (
                lingxi_core::host::MobileHostOs::Ios,
                lingxi_core::host::MobileDeviceClass::Tablet,
            ) => ("ios", "ipad"),
            (
                lingxi_core::host::MobileHostOs::Android,
                lingxi_core::host::MobileDeviceClass::Phone,
            ) => ("android", "phone"),
            (
                lingxi_core::host::MobileHostOs::Android,
                lingxi_core::host::MobileDeviceClass::Tablet,
            ) => ("android", "tablet"),
            (_, lingxi_core::host::MobileDeviceClass::Unknown) => return None,
        };
        Some(Self {
            os: os.into(),
            form_factor: form_factor.into(),
        })
    }

    /// Guard the persisted pair. Nothing the model writes reaches this any
    /// more, so it now guards deserialized on-disk state: a manifest written
    /// by an older build, or hand-edited, must still name a real platform.
    fn validate(&self) -> Result<(), AppError> {
        let valid_pair = matches!(
            (self.os.as_str(), self.form_factor.as_str()),
            ("ios", "iphone")
                | ("ios", "ipad")
                | ("android", "phone")
                | ("android", "tablet")
                | ("desktop", "desktop")
                | ("unknown", "unknown")
        );
        if !valid_pair {
            return Err(AppError::InvalidRequest(
                "manifest deviceContext os and formFactor do not agree".into(),
            ));
        }
        Ok(())
    }
}

/// Which scaffold an app was created from.
///
/// Chosen once, by the agent, from the confirmed specification, and then fixed:
/// the workspace on disk IS the scaffold, so changing this value later would
/// leave the generated source and the re-pinned infrastructure describing two
/// different applications. `update_manifest` rejects a change.
///
/// Deliberately NOT called a template. The fixed template catalog
/// (`AppTemplateKind`, `ListAppTemplates`, …) was removed from the protocol on
/// purpose and a regression guard keeps those symbols out; this names the SHAPE
/// an app draws, not a catalog entry.
///
/// Its wire twin is `AppSurfaceDto`: the create sheet has to show the surface
/// and let the user correct it, because a surface is fixed at scaffold time and
/// immutable afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppSurface {
    /// A routed, multi-screen interface built from Ionic components.
    Dom,
    /// A single drawn surface — a game, a 3D scene, a visualization — that owns
    /// its own frame loop and renders into a `<canvas>`.
    Canvas,
}

impl AppSurface {
    /// The wire/tool spelling, and the value persisted on the manifest.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dom => "dom",
            Self::Canvas => "canvas",
        }
    }

    /// Parse the tool argument. Unknown values are rejected rather than
    /// defaulted, so a typo cannot silently scaffold the wrong shape.
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "dom" => Ok(Self::Dom),
            "canvas" => Ok(Self::Canvas),
            other => Err(AppError::InvalidRequest(format!(
                "unknown app surface {other:?}; expected \"dom\" or \"canvas\""
            ))),
        }
    }
}

impl AppRuntimeProfile {
    /// The fixed scaffold surface this runtime family requires.
    #[must_use]
    pub fn surface(self) -> AppSurface {
        match self {
            Self::ReactDom => AppSurface::Dom,
            Self::Canvas2d | Self::Three3d | Self::Phaser2d | Self::Babylon3d => AppSurface::Canvas,
        }
    }
}

/// Immutable runtime binding stamped when a shell becomes a scaffolded app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRuntimeProfileBinding {
    /// Selected runtime family from the global catalog.
    pub family: AppRuntimeProfile,
    /// Catalog revision within the family.
    pub revision: u32,
    /// SHA-256 of the catalog contract this app is pinned to.
    pub contract_sha256: String,
}

impl AppRuntimeProfileBinding {
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if self.revision == 0 {
            return Err(AppError::InvalidRequest(
                "runtimeProfile revision must be at least 1".into(),
            ));
        }
        if !is_sha256_hex(&self.contract_sha256) {
            return Err(AppError::InvalidRequest(
                "runtimeProfile contractSha256 must be 64 lowercase hex bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Host-verified dependency snapshot for one scaffolded app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDependencySnapshot {
    /// SHA-256 of the requested dependency declaration the host approved.
    pub requested_sha256: String,
    /// SHA-256 of the effective merged package manifest written to the app.
    pub package_sha256: String,
    /// SHA-256 of the exact lockfile used to materialize the dependency tree.
    pub lockfile_sha256: String,
    /// SHA-256 of the verified installed dependency tree.
    pub dependency_tree_sha256: String,
    /// SHA-256 of the generated SBOM document.
    pub sbom_sha256: String,
    /// Toolchain identity, for example `pnpm@11.22.0/node@24.18.1`.
    pub toolchain_key: String,
    /// Runtime-profile contract digest this snapshot was verified against.
    pub verified_profile_contract_sha256: String,
}

impl AppDependencySnapshot {
    fn validate(&self) -> Result<(), AppError> {
        for (label, value) in [
            ("requestedSha256", &self.requested_sha256),
            ("packageSha256", &self.package_sha256),
            ("lockfileSha256", &self.lockfile_sha256),
            ("dependencyTreeSha256", &self.dependency_tree_sha256),
            ("sbomSha256", &self.sbom_sha256),
            (
                "verifiedProfileContractSha256",
                &self.verified_profile_contract_sha256,
            ),
        ] {
            if !is_sha256_hex(value) {
                return Err(AppError::InvalidRequest(format!(
                    "{label} must be 64 lowercase hex bytes"
                )));
            }
        }
        if self.toolchain_key.trim().is_empty() {
            return Err(AppError::InvalidRequest(
                "dependencySnapshot toolchainKey must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// Immutable provenance for the plugin template snapshot used to scaffold an
/// app.  This is Host-owned metadata: generated source never gets to change
/// the plugin identity or the bytes from which managed files are restored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppTemplateOrigin {
    pub plugin_id: String,
    pub plugin_version: String,
    pub template_id: String,
    pub template_sha256: String,
}

impl AppTemplateOrigin {
    pub const BUILTIN_PLUGIN_ID: &'static str = "lingxi-local-app@builtin";

    fn validate(&self) -> Result<(), AppError> {
        if self.plugin_id != Self::BUILTIN_PLUGIN_ID {
            return Err(AppError::InvalidRequest(
                "templateOrigin pluginId must be lingxi-local-app@builtin".into(),
            ));
        }
        for (label, value) in [
            ("pluginVersion", &self.plugin_version),
            ("templateId", &self.template_id),
        ] {
            if value.trim().is_empty()
                || value.len() > 128
                || value.contains('/')
                || value.contains('\\')
            {
                return Err(AppError::InvalidRequest(format!(
                    "templateOrigin {label} is invalid"
                )));
            }
        }
        if !is_sha256_hex(&self.template_sha256) {
            return Err(AppError::InvalidRequest(
                "templateOrigin templateSha256 must be 64 lowercase hex bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Host-owned pointer to an immutable, QA-verified MCP catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMcpCatalogRef {
    pub build_id: String,
    pub manifest_revision: u64,
    pub authoring_revision: u64,
    pub user_goal_sha256: String,
    pub proposal_sha256: String,
    pub approval_contract_sha256: String,
    pub tool_surface_sha256: String,
    pub catalog_sha256: String,
    pub mcp_verification_sha256: String,
}

/// Read-only publication projection. It is derived from trusted active
/// pointers and evidence; it is never serialized into `AppRecord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPublicationState {
    Draft,
    PublishedUnverified,
    PublishedVerified,
}

/// Derive publication state from the active build and UI verification only.
pub fn derive_publication_state(
    _manifest: &AppManifest,
    active_build_id: Option<&str>,
    ui_verified: bool,
) -> Result<AppPublicationState, AppError> {
    match active_build_id {
        None => Ok(AppPublicationState::Draft),
        Some(_) => {
            if ui_verified {
                Ok(AppPublicationState::PublishedVerified)
            } else {
                Ok(AppPublicationState::PublishedUnverified)
            }
        }
    }
}

impl AppMcpCatalogRef {
    fn validate(&self) -> Result<(), AppError> {
        if self.build_id.trim().is_empty()
            || self.build_id.len() > 128
            || self.build_id.contains('/')
        {
            return Err(AppError::InvalidRequest(
                "activeMcpCatalog buildId is invalid".into(),
            ));
        }
        if self.manifest_revision == 0 || self.authoring_revision == 0 {
            return Err(AppError::InvalidRequest(
                "activeMcpCatalog revisions must be positive".into(),
            ));
        }
        for (label, value) in [
            ("userGoalSha256", &self.user_goal_sha256),
            ("proposalSha256", &self.proposal_sha256),
            ("approvalContractSha256", &self.approval_contract_sha256),
            ("toolSurfaceSha256", &self.tool_surface_sha256),
            ("catalogSha256", &self.catalog_sha256),
            ("mcpVerificationSha256", &self.mcp_verification_sha256),
        ] {
            if !is_sha256_hex(value) {
                return Err(AppError::InvalidRequest(format!(
                    "activeMcpCatalog {label} must be 64 lowercase hex bytes"
                )));
            }
        }
        Ok(())
    }
}

/// Versioned local application manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppManifest {
    /// Persisted local-app schema version.
    pub schema_version: u32,
    /// Runtime API major used by the generated page. Schema v2 manifests must
    /// carry the published runtime API explicitly; missing or legacy values
    /// are storage corruption rather than an implicit compatibility mode.
    pub runtime_api_version: u16,
    /// Stable app id bound by the native host.
    pub app_id: String,
    /// Monotonic manifest revision.
    pub revision: u64,
    /// User-facing app name.
    pub name: String,
    /// Native data collections.
    #[serde(default)]
    pub collections: Vec<DataCollectionSchema>,
    /// HTTPS hostnames the app may ask the native network bridge to access.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Host capabilities the app may request at runtime. Declared by the
    /// confirmed plan; a pre-capability manifest deserializes as empty,
    /// meaning "no device/LLM capability was ever declared".
    #[serde(default)]
    pub capabilities: Vec<AppCapability>,
    /// Host-derived context captured when the generated target was confirmed.
    /// Older manifests omit this field and remain valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_context: Option<DeviceContext>,
    /// Which scaffold this app was created from. Stamped at creation, never
    /// changed. `None` means the app is still an unscaffolded shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<AppSurface>,
    /// Immutable runtime catalog binding for a scaffolded app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_profile: Option<AppRuntimeProfileBinding>,
    /// Host-verified dependency snapshot for a scaffolded app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_snapshot: Option<AppDependencySnapshot>,
    /// Immutable plugin/template provenance. Present exactly when scaffolded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_origin: Option<AppTemplateOrigin>,
    /// Active MCP catalog pointer. Candidates remain in Host staging/journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_mcp_catalog: Option<AppMcpCatalogRef>,
}

impl AppManifest {
    /// Build the initial native contract for a newly-created application.
    /// Collections start empty — the agent fills them in as it designs the
    /// app's data model.
    #[must_use]
    pub fn for_new_app(app_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema_version: APPS_SCHEMA_VERSION,
            runtime_api_version: RUNTIME_API_MAJOR,
            app_id: app_id.into(),
            revision: 0,
            name: name.into(),
            collections: Vec::new(),
            allowed_domains: Vec::new(),
            capabilities: Vec::new(),
            device_context: None,
            surface: None,
            runtime_profile: None,
            dependency_snapshot: None,
            template_origin: None,
            active_mcp_catalog: None,
        }
    }

    /// Validate ids, field definitions, and declared network domains.
    pub fn validate(&self) -> Result<(), AppError> {
        ids::validate_app_id(&self.app_id)?;
        if self.schema_version != APPS_SCHEMA_VERSION {
            return Err(AppError::InvalidRequest(format!(
                "manifest schemaVersion {} is unsupported (expected {APPS_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        if self.runtime_api_version != RUNTIME_API_MAJOR {
            return Err(AppError::InvalidRequest(format!(
                "manifest runtimeApiVersion {} is unsupported",
                self.runtime_api_version
            )));
        }
        if self.name.trim().is_empty() {
            return Err(AppError::InvalidRequest(
                "manifest name must not be empty".into(),
            ));
        }
        if self.name.len() > MAX_MANIFEST_DISPLAY_NAME_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "manifest name exceeds {MAX_MANIFEST_DISPLAY_NAME_BYTES} bytes"
            )));
        }
        if self.collections.len() > MAX_MANIFEST_COLLECTIONS {
            return Err(AppError::InvalidRequest(format!(
                "manifest has more than {MAX_MANIFEST_COLLECTIONS} collections"
            )));
        }

        let mut collection_ids = BTreeSet::new();
        for collection in &self.collections {
            validate_identifier("collection", &collection.id)?;
            if collection.name.trim().is_empty()
                || collection.name.len() > MAX_MANIFEST_DISPLAY_NAME_BYTES
            {
                return Err(AppError::InvalidRequest(format!(
                    "collection {:?} has an invalid display name",
                    collection.id
                )));
            }
            if !collection_ids.insert(collection.id.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "duplicate collection id {:?}",
                    collection.id
                )));
            }
            if collection.fields.len() > MAX_COLLECTION_FIELDS {
                return Err(AppError::InvalidRequest(format!(
                    "collection {:?} has more than {MAX_COLLECTION_FIELDS} fields",
                    collection.id
                )));
            }
            let mut field_ids = BTreeSet::new();
            for field in &collection.fields {
                if HOST_OWNED_RECORD_FIELD_IDS.contains(&field.id.as_str()) {
                    return Err(AppError::InvalidRequest(format!(
                        "field {:?}.{:?} uses host-owned record metadata",
                        collection.id, field.id
                    )));
                }
                validate_identifier("field", &field.id)?;
                if field.label.trim().is_empty()
                    || field.label.len() > MAX_MANIFEST_DISPLAY_NAME_BYTES
                {
                    return Err(AppError::InvalidRequest(format!(
                        "field {:?}.{:?} has an invalid label",
                        collection.id, field.id
                    )));
                }
                if !field_ids.insert(field.id.as_str()) {
                    return Err(AppError::InvalidRequest(format!(
                        "duplicate field id {:?} in collection {:?}",
                        field.id, collection.id
                    )));
                }
                match field.kind {
                    DataFieldKind::Enum => {
                        if field.enum_options.is_empty()
                            || field.enum_options.len() > MAX_ENUM_OPTIONS
                        {
                            return Err(AppError::InvalidRequest(format!(
                                "enum field {:?}.{:?} must declare 1..={MAX_ENUM_OPTIONS} options",
                                collection.id, field.id
                            )));
                        }
                        let unique: BTreeSet<_> = field.enum_options.iter().collect();
                        if unique.len() != field.enum_options.len()
                            || field.enum_options.iter().any(|option| {
                                option.is_empty() || option.len() > MAX_ENUM_OPTION_BYTES
                            })
                        {
                            return Err(AppError::InvalidRequest(format!(
                                "enum field {:?}.{:?} has duplicate or invalid options",
                                collection.id, field.id
                            )));
                        }
                    }
                    _ if !field.enum_options.is_empty() => {
                        return Err(AppError::InvalidRequest(format!(
                            "non-enum field {:?}.{:?} cannot declare enum options",
                            collection.id, field.id
                        )));
                    }
                    _ => {}
                }
            }
        }

        let mut domains = BTreeSet::new();
        for domain in &self.allowed_domains {
            validate_domain(domain)?;
            if !domains.insert(domain.as_str()) {
                return Err(AppError::InvalidRequest(format!(
                    "duplicate allowed domain {domain:?}"
                )));
            }
        }

        let mut capabilities = BTreeSet::new();
        for capability in &self.capabilities {
            if !capabilities.insert(capability) {
                return Err(AppError::InvalidRequest(format!(
                    "duplicate capability {capability:?}"
                )));
            }
        }
        if let Some(device_context) = &self.device_context {
            device_context.validate()?;
        }
        if self.active_mcp_catalog.is_some()
            && (self.surface.is_none()
                || self.runtime_profile.is_none()
                || self.dependency_snapshot.is_none()
                || self.template_origin.is_none())
        {
            return Err(AppError::InvalidRequest(
                "active_state_corrupt: activeMcpCatalog requires a complete scaffold identity"
                    .into(),
            ));
        }
        match (
            &self.surface,
            &self.runtime_profile,
            &self.dependency_snapshot,
            &self.template_origin,
        ) {
            (None, None, None, None) if self.active_mcp_catalog.is_none() => {}
            (Some(surface), Some(binding), snapshot, Some(template_origin)) => {
                binding.validate()?;
                template_origin.validate()?;
                // r2-never-wired-08: `templateOrigin` was WRITE-ONLY — stamped
                // at scaffold time and never read back — so a manifest whose
                // provenance stamp disagreed with the runtime binding it was
                // derived from opened without a word.
                //
                // Both writers derive the two from ONE catalog row:
                // `builtin_template_origin` copies `binding.contract_sha256`
                // verbatim, and the create-seed path takes
                // `selection.template_sha256` and `selection.runtime_profile`,
                // which `local_app_template_catalog` fills from the same
                // `template.contract_sha256`. So the equality below holds for
                // every honestly stamped app and fires only on drift or
                // tampering — which is the whole point of the field's doc
                // comment ("generated source never gets to change the plugin
                // identity").
                //
                // Two deliberate placement decisions:
                //   * HERE, not in the host: `load_manifest` is the single
                //     door every open path goes through and it calls
                //     `validate()`, so no entry point is left unguarded. A
                //     comparison wired into one caller is the shape of defect
                //     this backlog keeps finding.
                //   * Against `binding`, not against a catalog handle plumbed
                //     down into this crate. The template catalog lives in the
                //     host (`apps/engine-mobile`), which DEPENDS on this
                //     crate, so a live-catalog lookup here would have to be a
                //     registry the host pushes into — dormant, and silently
                //     unenforced, on any path that forgot to populate it. The
                //     binding is the honest proxy: the host resolves it
                //     through `local_app_runtime_profiles::contract_for_binding`,
                //     which recomputes the published contract digest and
                //     refuses a binding that does not match, so pinning the
                //     stamp to the binding pins it transitively to the digest
                //     the running build actually publishes.
                if template_origin.template_sha256 != binding.contract_sha256 {
                    return Err(AppError::InvalidRequest(
                        "templateOrigin templateSha256 must match runtimeProfile contractSha256"
                            .into(),
                    ));
                }
                if binding.family.surface() != *surface {
                    return Err(AppError::InvalidRequest(format!(
                        "runtimeProfile family {} does not match manifest surface {}",
                        binding.family,
                        surface.as_str()
                    )));
                }
                if let Some(snapshot) = snapshot {
                    snapshot.validate()?;
                    if binding.contract_sha256 != snapshot.verified_profile_contract_sha256 {
                        return Err(AppError::InvalidRequest(
                            "dependencySnapshot verifiedProfileContractSha256 must match runtimeProfile contractSha256".into(),
                        ));
                    }
                }
            }
            (Some(_), None, None, _) => {
                return Err(AppError::InvalidRequest(
                    "scaffolded apps must commit a runtimeProfile with the surface".into(),
                ));
            }
            (None, None, Some(_), _) | (None, Some(_), _, _) => {
                return Err(AppError::InvalidRequest(
                    "surface, runtimeProfile, and dependencySnapshot must agree on scaffold identity".into(),
                ));
            }
            _ => {
                return Err(AppError::InvalidRequest(
                    "templateOrigin/dependencySnapshot requires a matching surface and runtimeProfile".into(),
                ));
            }
        }
        if let Some(catalog) = &self.active_mcp_catalog {
            catalog.validate()?;
            if self.surface.is_none()
                || self.runtime_profile.is_none()
                || self.dependency_snapshot.is_none()
                || self.template_origin.is_none()
            {
                return Err(AppError::InvalidRequest(
                    "active_state_corrupt: activeMcpCatalog requires a complete scaffold identity"
                        .into(),
                ));
            }
            if catalog.manifest_revision > self.revision {
                return Err(AppError::InvalidRequest(
                    "activeMcpCatalog manifestRevision cannot exceed manifest revision".into(),
                ));
            }
        }
        Ok(())
    }

    /// True only when this manifest can be mounted by the direct-cutover v2
    /// runtime. Pre-release legacy manifests are rejected while loading, so
    /// this is an explicit runtime guard rather than a compatibility fallback.
    #[must_use]
    pub fn runtime_api_compatible(&self) -> bool {
        self.runtime_api_version == RUNTIME_API_MAJOR
    }

    /// Find a collection by stable id.
    #[must_use]
    pub fn collection(&self, id: &str) -> Option<&DataCollectionSchema> {
        self.collections
            .iter()
            .find(|collection| collection.id == id)
    }

    /// Stable SHA-256 of the native data contract only.
    pub fn data_contract_hash(&self) -> Result<String, AppError> {
        self.validate()?;
        canonical_hash(
            serde_json::json!({
                "collections": self.collections,
            }),
            "app data contract",
        )
    }

    /// Stable SHA-256 of the pinned runtime contract.
    pub fn runtime_contract_hash(&self) -> Result<String, AppError> {
        self.validate()?;
        canonical_hash(
            serde_json::json!({
                "runtimeApiVersion": self.runtime_api_version,
                "surface": self.surface,
                "runtimeProfile": self.runtime_profile,
            }),
            "app runtime contract",
        )
    }

    /// Stable SHA-256 of the host-verified dependency snapshot.
    pub fn dependency_snapshot_hash(&self) -> Result<String, AppError> {
        self.validate()?;
        canonical_hash(
            serde_json::json!({
                "dependencySnapshot": self.dependency_snapshot,
            }),
            "app dependency snapshot",
        )
    }

    /// Backward-compatible alias for the SQLite-bound data contract hash.
    pub fn hash(&self) -> Result<String, AppError> {
        self.data_contract_hash()
    }
}

/// Absolute and root-relative paths for one app.
#[derive(Debug, Clone)]
pub struct AppLayout {
    root: PathBuf,
    app_id: String,
}

impl AppLayout {
    /// Construct a layout from a trusted profile root and validated app id.
    pub fn new(root: impl Into<PathBuf>, app_id: impl Into<String>) -> Result<Self, AppError> {
        let app_id = app_id.into();
        ids::validate_app_id(&app_id)?;
        Ok(Self {
            root: root.into(),
            app_id,
        })
    }

    /// Trusted profile data root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stable app id.
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Root-relative `apps/<id>` directory.
    #[must_use]
    pub fn app_dir_rel(&self) -> PathBuf {
        crate::storage::app_dir_rel(&self.app_id)
    }

    /// Root-relative workspace directory.
    #[must_use]
    pub fn workspace_rel(&self) -> PathBuf {
        crate::storage::workspace_dir_rel(&self.app_id)
    }

    /// Root-relative manifest path.
    #[must_use]
    pub fn manifest_rel(&self) -> PathBuf {
        self.workspace_rel()
            .join(crate::storage::APP_STATE_DIR)
            .join(APP_MANIFEST_FILE)
    }

    /// Root-relative project-local permission settings for the app workspace.
    ///
    /// The app storage contract uses `apps/<id>/workspace` as the physical
    /// workspace root. Session ids identify transcript catalog entries under
    /// the profile's session store; they are not additional filesystem path
    /// components here.
    #[must_use]
    pub fn workspace_settings_local_rel(&self) -> PathBuf {
        self.workspace_rel()
            .join(crate::storage::APP_STATE_DIR)
            .join(WORKSPACE_SETTINGS_LOCAL_FILE)
    }

    /// Root-relative `SQLite` database path.
    #[must_use]
    pub fn database_rel(&self) -> PathBuf {
        self.app_dir_rel().join(DATA_DIR).join(DATA_DATABASE_FILE)
    }

    /// Absolute `SQLite` database path.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.root.join(self.database_rel())
    }

    /// Root-relative build directory for a channel (`store` or `full`).
    #[must_use]
    pub fn build_rel(&self, full: bool) -> PathBuf {
        self.app_dir_rel().join(BUILD_DIR).join(if full {
            FULL_BUILD_DIR
        } else {
            STORE_BUILD_DIR
        })
    }

    /// Root-relative log directory.
    #[must_use]
    pub fn logs_rel(&self) -> PathBuf {
        self.app_dir_rel().join(LOGS_DIR)
    }

    /// Root-relative runtime state path.
    #[must_use]
    pub fn runtime_rel(&self) -> PathBuf {
        self.app_dir_rel().join(RUNTIME_STATE_FILE)
    }

    /// Root-relative permissions path.
    #[must_use]
    pub fn permissions_rel(&self) -> PathBuf {
        self.app_dir_rel().join(PERMISSIONS_FILE)
    }

    /// Root-relative durable generation jobs path.
    #[must_use]
    pub fn generation_jobs_rel(&self) -> PathBuf {
        self.app_dir_rel().join(GENERATION_JOBS_FILE)
    }

    /// Root-relative app-to-conversation mailbox path.
    #[must_use]
    pub fn mailbox_rel(&self) -> PathBuf {
        self.app_dir_rel().join(MAILBOX_FILE)
    }

    /// Root-relative immutable active/candidate MCP catalog path.
    pub fn mcp_catalog_rel(&self, catalog_sha256: &str) -> Result<PathBuf, AppError> {
        if !is_sha256_hex(catalog_sha256) {
            return Err(AppError::InvalidRequest(
                "catalog digest must be 64 lowercase hex bytes".into(),
            ));
        }
        Ok(self
            .app_dir_rel()
            .join(MCP_DIR)
            .join(MCP_CATALOGS_DIR)
            .join(format!("{catalog_sha256}.json")))
    }

    /// Root-relative durable MCP authoring journal.
    #[must_use]
    pub fn mcp_authoring_journal_rel(&self) -> PathBuf {
        self.app_dir_rel()
            .join(MCP_DIR)
            .join(MCP_AUTHORING_JOURNAL_FILE)
    }

    /// Root-relative host-owned MCP settings.
    #[must_use]
    pub fn mcp_settings_rel(&self) -> PathBuf {
        self.app_dir_rel().join(MCP_DIR).join(MCP_SETTINGS_FILE)
    }

    /// Root-relative host-owned Agent session catalog.
    #[must_use]
    pub fn agent_sessions_rel(&self) -> PathBuf {
        self.app_dir_rel()
            .join(crate::agent_sessions::AGENT_SESSION_CATALOG_FILE)
    }

    /// Root-relative user-approved App Agent Profile.
    #[must_use]
    pub fn agent_profile_rel(&self) -> PathBuf {
        self.app_dir_rel()
            .join(crate::agent_sessions::AGENT_PROFILE_FILE)
    }

    /// Root-relative transcript for one host-owned app Agent session.
    pub fn agent_session_history_rel(&self, session_id: &str) -> Result<PathBuf, AppError> {
        crate::agent_sessions::validate_agent_session_id(session_id)?;
        Ok(self
            .app_dir_rel()
            .join(crate::agent_sessions::AGENT_SESSION_HISTORY_DIR)
            .join(format!("{session_id}.json")))
    }

    /// Create the complete app directory skeleton with private permissions.
    pub fn initialize(&self) -> Result<(), AppError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            AppError::Io(format!(
                "create app data root {}: {error}",
                self.root.display()
            ))
        })?;
        for relative in [
            self.workspace_rel().join(crate::storage::APP_STATE_DIR),
            self.app_dir_rel().join(DATA_DIR),
            self.build_rel(false),
            self.build_rel(true),
            self.logs_rel(),
            self.app_dir_rel()
                .join(crate::agent_sessions::AGENT_SESSION_HISTORY_DIR),
            self.app_dir_rel().join(crate::background::CANCEL_DIR),
            self.app_dir_rel().join(MCP_DIR).join(MCP_CATALOGS_DIR),
        ] {
            ensure_private_directory(&self.root, &relative)?;
        }
        Ok(())
    }
}

/// Persist one Host-owned immutable catalog. Rewriting a digest with
/// different bytes fails closed; retrying the exact same bytes is idempotent.
pub fn save_mcp_catalog(
    layout: &AppLayout,
    catalog_sha256: &str,
    catalog: &Value,
) -> Result<(), AppError> {
    let path = layout.mcp_catalog_rel(catalog_sha256)?;
    let actual = canonical_hash(catalog.clone(), "MCP catalog")?;
    if actual != catalog_sha256 {
        return Err(AppError::InvalidRequest(
            "MCP catalog bytes do not match catalog_sha256".into(),
        ));
    }
    layout.initialize()?;
    let mut body = serde_json::to_vec_pretty(catalog)
        .map_err(|error| AppError::Io(format!("serialize MCP catalog: {error}")))?;
    body.push(b'\n');
    let absolute = layout.root().join(&path);
    if absolute.exists() {
        let existing = rooted_fs::read_to_string_limited(layout.root(), &path, MAX_MANIFEST_BYTES)
            .map_err(|error| AppError::from_fs("read immutable MCP catalog", &error))?;
        let existing_value: Value = serde_json::from_str(&existing)
            .map_err(|error| AppError::StorageCorrupt(format!("MCP catalog: {error}")))?;
        if canonical_hash(existing_value, "MCP catalog")? != catalog_sha256 {
            return Err(AppError::StorageCorrupt(
                "immutable MCP catalog digest was rewritten".into(),
            ));
        }
        return Ok(());
    }
    rooted_fs::atomic_write(layout.root(), &path, &body, AtomicWriteOptions::default())
        .map_err(|error| AppError::from_fs("write immutable MCP catalog", &error))
}

/// Load one immutable Host-owned catalog by its digest.
pub fn load_mcp_catalog(layout: &AppLayout, catalog_sha256: &str) -> Result<Value, AppError> {
    let path = layout.mcp_catalog_rel(catalog_sha256)?;
    let body = rooted_fs::read_to_string_limited(layout.root(), &path, MAX_MANIFEST_BYTES)
        .map_err(|error| AppError::from_fs("read MCP catalog", &error))?;
    let catalog: Value = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("MCP catalog: {error}")))?;
    if canonical_hash(catalog.clone(), "MCP catalog")? != catalog_sha256 {
        return Err(AppError::StorageCorrupt(
            "MCP catalog bytes do not match its filename digest".into(),
        ));
    }
    Ok(catalog)
}

/// Persist a validated manifest atomically.
pub fn save_manifest(layout: &AppLayout, manifest: &AppManifest) -> Result<(), AppError> {
    manifest.validate()?;
    if layout.app_id != manifest.app_id {
        return Err(AppError::InvalidRequest(format!(
            "manifest app id {:?} does not match layout app id {:?}",
            manifest.app_id, layout.app_id
        )));
    }
    layout.initialize()?;
    save_manifest_initialized(layout, manifest)
}

/// Persist a manifest after the caller has already initialized the complete
/// app layout. App creation uses this to avoid repeating the same guarded
/// directory traversal for each metadata document in one transaction.
pub(crate) fn save_manifest_initialized(
    layout: &AppLayout,
    manifest: &AppManifest,
) -> Result<(), AppError> {
    manifest.validate()?;
    if layout.app_id != manifest.app_id {
        return Err(AppError::InvalidRequest(format!(
            "manifest app id {:?} does not match layout app id {:?}",
            manifest.app_id, layout.app_id
        )));
    }
    let mut body = serde_json::to_vec_pretty(manifest)
        .map_err(|error| AppError::Io(format!("serialize app manifest: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "app manifest is {} bytes (limit {MAX_MANIFEST_BYTES})",
            body.len()
        )));
    }
    rooted_fs::atomic_write(
        layout.root(),
        &layout.manifest_rel(),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write app manifest", &error))
}

/// Load and validate one manifest.
pub fn load_manifest(layout: &AppLayout) -> Result<AppManifest, AppError> {
    let body = rooted_fs::read_to_string_limited(
        layout.root(),
        &layout.manifest_rel(),
        MAX_MANIFEST_BYTES,
    )
    .map_err(|error| match error {
        FsError::NotFound(_) => {
            AppError::NotFound(format!("manifest for app {} was not found", layout.app_id))
        }
        other => AppError::from_fs("read app manifest", &other),
    })?;
    let manifest: AppManifest = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("app manifest: {error}")))?;
    manifest
        .validate()
        .map_err(|error| AppError::StorageCorrupt(format!("invalid app manifest: {error}")))?;
    if manifest.app_id != layout.app_id {
        return Err(AppError::StorageCorrupt(format!(
            "manifest app id {:?} does not match directory {:?}",
            manifest.app_id, layout.app_id
        )));
    }
    Ok(manifest)
}

/// Validate a programmatic collection or field identifier.
pub fn validate_identifier(kind: &str, value: &str) -> Result<(), AppError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid {kind} id {value:?}: must match ^[a-z][a-z0-9_]{{0,63}}$"
        )))
    }
}

/// Validate a manifest network hostname (HTTPS is enforced by the bridge).
pub fn validate_domain(domain: &str) -> Result<(), AppError> {
    let valid = !domain.is_empty()
        && domain.len() <= 253
        && !domain.contains(['/', ':', '@'])
        && domain == domain.to_ascii_lowercase()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid HTTPS domain {domain:?}"
        )))
    }
}

fn ensure_private_directory(root: &Path, relative: &Path) -> Result<(), AppError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(AppError::InvalidRequest(format!(
                "invalid app layout path {}",
                relative.display()
            )));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(AppError::StorageCorrupt(format!(
                    "{} is not a real directory",
                    current.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&current) {
                    Ok(()) => set_private_permissions(&current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
                            AppError::Io(format!("inspect {}: {error}", current.display()))
                        })?;
                        if !metadata.is_dir() || metadata.file_type().is_symlink() {
                            return Err(AppError::StorageCorrupt(format!(
                                "{} is not a real directory",
                                current.display()
                            )));
                        }
                    }
                    Err(error) => {
                        return Err(AppError::Io(format!(
                            "create {}: {error}",
                            current.display()
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(AppError::Io(format!(
                    "inspect {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| AppError::Io(format!("secure {}: {error}", path.display())))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> AppManifest {
        AppManifest {
            schema_version: APPS_SCHEMA_VERSION,
            runtime_api_version: RUNTIME_API_MAJOR,
            app_id: "abcd1234".into(),
            revision: 1,
            name: "Tasks".into(),
            collections: vec![DataCollectionSchema {
                id: "items".into(),
                name: "Items".into(),
                fields: vec![DataFieldSchema {
                    id: "status".into(),
                    label: "Status".into(),
                    kind: DataFieldKind::Enum,
                    required: true,
                    enum_options: vec!["todo".into(), "done".into()],
                }],
            }],
            allowed_domains: vec!["api.example.com".into()],
            capabilities: Vec::new(),
            device_context: None,
            surface: None,
            runtime_profile: None,
            dependency_snapshot: None,
            template_origin: None,
            active_mcp_catalog: None,
        }
    }

    #[test]
    fn initializes_complete_layout_and_round_trips_manifest() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        layout.initialize().unwrap();
        for relative in [
            layout.workspace_rel().join(crate::storage::APP_STATE_DIR),
            layout.app_dir_rel().join(DATA_DIR),
            layout.build_rel(false),
            layout.build_rel(true),
            layout.logs_rel(),
        ] {
            assert!(root.path().join(relative).is_dir());
        }
        let expected = manifest();
        save_manifest(&layout, &expected).unwrap();
        assert_eq!(load_manifest(&layout).unwrap(), expected);
        assert_eq!(expected.hash().unwrap().len(), 64);
    }

    #[test]
    fn runtime_and_dependency_metadata_do_not_change_the_data_contract_hash() {
        let base = manifest();
        let mut profiled = base.clone();
        profiled.surface = Some(AppSurface::Canvas);
        profiled.runtime_profile = Some(AppRuntimeProfileBinding {
            family: AppRuntimeProfile::Three3d,
            revision: 1,
            contract_sha256: "a".repeat(64),
        });
        profiled.template_origin = Some(AppTemplateOrigin {
            plugin_id: AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: "three-3d-r1".into(),
            template_sha256: "a".repeat(64),
        });
        profiled.dependency_snapshot = Some(AppDependencySnapshot {
            requested_sha256: "b".repeat(64),
            package_sha256: "c".repeat(64),
            lockfile_sha256: "d".repeat(64),
            dependency_tree_sha256: "e".repeat(64),
            sbom_sha256: "f".repeat(64),
            toolchain_key: "pnpm@11/node@24".into(),
            verified_profile_contract_sha256: "a".repeat(64),
        });
        assert_eq!(
            base.data_contract_hash().unwrap(),
            profiled.data_contract_hash().unwrap()
        );
        assert_ne!(
            base.runtime_contract_hash().unwrap(),
            profiled.runtime_contract_hash().unwrap()
        );
        assert_ne!(
            base.dependency_snapshot_hash().unwrap(),
            profiled.dependency_snapshot_hash().unwrap()
        );
    }

    #[test]
    fn canonical_hash_is_independent_of_json_object_field_order() {
        let left: Value = serde_json::from_str(
            r#"{"family":"three_3d","nested":{"revision":1,"surface":"canvas"}}"#,
        )
        .unwrap();
        let right: Value = serde_json::from_str(
            r#"{"nested":{"surface":"canvas","revision":1},"family":"three_3d"}"#,
        )
        .unwrap();
        assert_eq!(
            canonical_hash(left, "left").unwrap(),
            canonical_hash(right, "right").unwrap()
        );
    }

    #[test]
    fn scaffolded_apps_require_a_runtime_profile() {
        let mut invalid = manifest();
        invalid.surface = Some(AppSurface::Dom);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn template_origin_must_match_the_runtime_profile_contract_digest() {
        // r2-never-wired-08. A scaffolded manifest whose provenance stamp and
        // runtime binding come from the same catalog row validates; drifting
        // ONLY the stamp must be rejected on every open path (`load_manifest`
        // routes all of them through `validate`).
        let mut scaffolded = manifest();
        scaffolded.surface = Some(AppSurface::Dom);
        scaffolded.runtime_profile = Some(AppRuntimeProfileBinding {
            family: AppRuntimeProfile::ReactDom,
            revision: 1,
            contract_sha256: "a".repeat(64),
        });
        scaffolded.template_origin = Some(AppTemplateOrigin {
            plugin_id: AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: "react-dom-r1".into(),
            template_sha256: "a".repeat(64),
        });
        scaffolded.validate().expect("a consistent stamp validates");

        scaffolded
            .template_origin
            .as_mut()
            .expect("stamp")
            .template_sha256 = "b".repeat(64);
        let error = scaffolded
            .validate()
            .expect_err("a stamp that disagrees with the binding must be refused")
            .to_string();
        assert!(
            error.contains("templateOrigin templateSha256"),
            "the error must name the field that drifted, got {error:?}"
        );
    }

    #[test]
    fn dependency_snapshot_requires_a_matching_runtime_profile_contract() {
        let mut invalid = manifest();
        invalid.surface = Some(AppSurface::Dom);

        invalid.runtime_profile = Some(AppRuntimeProfileBinding {
            family: AppRuntimeProfile::ReactDom,
            revision: 1,
            contract_sha256: "a".repeat(64),
        });
        invalid.dependency_snapshot = Some(AppDependencySnapshot {
            requested_sha256: "b".repeat(64),
            package_sha256: "c".repeat(64),
            lockfile_sha256: "d".repeat(64),
            dependency_tree_sha256: "e".repeat(64),
            sbom_sha256: "f".repeat(64),
            toolchain_key: "pnpm@11/node@24".into(),
            verified_profile_contract_sha256: "0".repeat(64),
        });
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn device_context_requires_a_native_platform_form_factor_pair() {
        let mut manifest = manifest();
        manifest.device_context = Some(DeviceContext {
            os: "ios".into(),
            form_factor: "iphone".into(),
        });
        manifest.validate().unwrap();
        // The exact pair the model used to produce, reading `Host OS: iOS`
        // and `Device class: phone` off the mobile runtime reminder.
        manifest.device_context.as_mut().unwrap().form_factor = "phone".into();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn the_host_device_class_derives_the_platform_form_factor() {
        let derive = |host_os, device_class| {
            DeviceContext::from_host_environment(&lingxi_core::host::MobileHostEnvironment::new(
                host_os,
                Some("19.0".into()),
                device_class,
                lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
                lingxi_core::host::MobileLaunchMode::Interactive,
            ))
            .map(|context| (context.os, context.form_factor))
        };
        // `Device class: phone` is the ONLY class an iPhone reports, and it
        // must not reach the manifest verbatim.
        assert_eq!(
            derive(
                lingxi_core::host::MobileHostOs::Ios,
                lingxi_core::host::MobileDeviceClass::Phone
            ),
            Some(("ios".into(), "iphone".into()))
        );
        assert_eq!(
            derive(
                lingxi_core::host::MobileHostOs::Ios,
                lingxi_core::host::MobileDeviceClass::Tablet
            ),
            Some(("ios".into(), "ipad".into()))
        );
        assert_eq!(
            derive(
                lingxi_core::host::MobileHostOs::Android,
                lingxi_core::host::MobileDeviceClass::Phone
            ),
            Some(("android".into(), "phone".into()))
        );
        assert_eq!(
            derive(
                lingxi_core::host::MobileHostOs::Android,
                lingxi_core::host::MobileDeviceClass::Tablet
            ),
            Some(("android".into(), "tablet".into()))
        );
        assert_eq!(
            derive(
                lingxi_core::host::MobileHostOs::Ios,
                lingxi_core::host::MobileDeviceClass::Unknown
            ),
            None
        );
        // Every pair the derivation can produce must survive validation.
        for (host_os, device_class) in [
            (
                lingxi_core::host::MobileHostOs::Ios,
                lingxi_core::host::MobileDeviceClass::Phone,
            ),
            (
                lingxi_core::host::MobileHostOs::Ios,
                lingxi_core::host::MobileDeviceClass::Tablet,
            ),
            (
                lingxi_core::host::MobileHostOs::Android,
                lingxi_core::host::MobileDeviceClass::Phone,
            ),
            (
                lingxi_core::host::MobileHostOs::Android,
                lingxi_core::host::MobileDeviceClass::Tablet,
            ),
        ] {
            let mut manifest = manifest();
            manifest.device_context = DeviceContext::from_host_environment(
                &lingxi_core::host::MobileHostEnvironment::new(
                    host_os,
                    None,
                    device_class,
                    lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
                    lingxi_core::host::MobileLaunchMode::Interactive,
                ),
            );
            manifest
                .validate()
                .expect("a derived pair always validates");
        }
    }

    /// A manifest written before the dynamic fields were dropped still loads:
    /// the removed keys are ignored, and the target pair survives.
    #[test]
    fn a_legacy_device_context_drops_its_runtime_snapshot_fields() {
        let legacy = serde_json::json!({
            "os": "ios",
            "formFactor": "ipad",
            "viewport": {"width": 1024, "height": 1366},
            "safeArea": {"top": 24, "right": 0, "bottom": 20, "left": 0},
            "colorScheme": "dark",
            "reducedMotion": true,
            "inputMode": "touch",
        });
        let context: DeviceContext = serde_json::from_value(legacy).expect("legacy manifest loads");
        assert_eq!(context.os, "ios");
        assert_eq!(context.form_factor, "ipad");
    }

    #[test]
    fn rejects_duplicate_ids_and_non_https_host_shapes() {
        let mut candidate = manifest();
        candidate.collections.push(candidate.collections[0].clone());
        assert!(candidate.validate().is_err());

        let mut candidate = manifest();
        candidate.allowed_domains = vec!["https://example.com/path".into()];
        assert!(candidate.validate().is_err());
    }

    #[test]
    fn rejects_host_owned_record_metadata_as_collection_fields() {
        for reserved in HOST_OWNED_RECORD_FIELD_IDS {
            let mut candidate = manifest();
            candidate.collections[0].fields[0].id = reserved.into();
            let error = candidate.validate().unwrap_err().to_string();
            assert!(
                error.contains("host-owned record metadata"),
                "{reserved:?} returned an unrelated validation error: {error}"
            );
        }
    }

    #[test]
    fn manifest_limits_count_fields_and_utf8_bytes_at_the_documented_boundaries() {
        let mut candidate = manifest();
        candidate.collections = (0..MAX_MANIFEST_COLLECTIONS)
            .map(|index| DataCollectionSchema {
                id: format!("collection_{index}"),
                name: format!("Collection {index}"),
                fields: Vec::new(),
            })
            .collect();
        candidate.validate().unwrap();
        candidate.collections.push(DataCollectionSchema {
            id: "overflow".into(),
            name: "Overflow".into(),
            fields: Vec::new(),
        });
        assert!(candidate.validate().is_err());

        let mut candidate = manifest();
        candidate.collections[0].fields = (0..MAX_COLLECTION_FIELDS)
            .map(|index| DataFieldSchema {
                id: format!("field_{index}"),
                label: "x".repeat(MAX_MANIFEST_DISPLAY_NAME_BYTES),
                kind: DataFieldKind::Text,
                required: false,
                enum_options: Vec::new(),
            })
            .collect();
        candidate.validate().unwrap();

        candidate.collections[0].fields.push(DataFieldSchema {
            id: "overflow".into(),
            label: "Overflow".into(),
            kind: DataFieldKind::Text,
            required: false,
            enum_options: Vec::new(),
        });
        assert!(candidate.validate().is_err());

        let mut candidate = manifest();
        candidate.collections[0].fields[0].label = "é".repeat(100);
        candidate.collections[0].fields[0].enum_options = vec!["é".repeat(250)];
        candidate.validate().unwrap();

        candidate.collections[0].fields[0].label.push('é');
        assert!(candidate.validate().is_err());

        let mut candidate = manifest();
        candidate.collections[0].fields[0].enum_options = vec!["é".repeat(251)];
        assert!(candidate.validate().is_err());
    }

    #[test]
    fn a_v2_manifest_without_capabilities_loads_as_empty() {
        let json = format!(
            r#"{{
  "schemaVersion": {schema_version},
  "runtimeApiVersion": {runtime_api_version},
  "appId": "abcd1234",
  "revision": 3,
  "name": "Tasks",
  "collections": [],
  "allowedDomains": ["api.example.com"]
}}"#,
            schema_version = APPS_SCHEMA_VERSION,
            runtime_api_version = RUNTIME_API_MAJOR,
        );
        let loaded: AppManifest = serde_json::from_str(&json).unwrap();
        loaded.validate().unwrap();
        assert!(loaded.capabilities.is_empty());
    }

    #[test]
    fn schema_v2_manifest_without_runtime_api_is_rejected() {
        let json = format!(
            r#"{{
  "schemaVersion": {schema_version},
  "appId": "abcd1234",
  "revision": 3,
  "name": "Tasks",
  "collections": [],
  "allowedDomains": []
}}"#,
            schema_version = APPS_SCHEMA_VERSION,
        );
        assert!(
            serde_json::from_str::<AppManifest>(&json).is_err(),
            "schema v2 must not silently default a missing runtimeApiVersion"
        );
    }

    #[test]
    fn schema_v2_manifest_with_legacy_runtime_api_is_rejected() {
        let json = format!(
            r#"{{
  "schemaVersion": {schema_version},
  "runtimeApiVersion": 1,
  "appId": "abcd1234",
  "revision": 3,
  "name": "Tasks",
  "collections": [],
  "allowedDomains": []
}}"#,
            schema_version = APPS_SCHEMA_VERSION,
        );
        let loaded = serde_json::from_str::<AppManifest>(&json).expect("parse manifest");
        assert!(loaded.validate().is_err());
    }

    #[test]
    fn capabilities_round_trip_through_save_and_load() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        let mut expected = manifest();
        expected.capabilities = vec![
            crate::permissions::AppCapability::Camera,
            crate::permissions::AppCapability::Microphone,
            crate::permissions::AppCapability::Llm,
        ];
        save_manifest(&layout, &expected).unwrap();
        assert_eq!(load_manifest(&layout).unwrap(), expected);
    }

    #[test]
    fn rejects_a_duplicate_capability() {
        let mut candidate = manifest();
        candidate.capabilities = vec![
            crate::permissions::AppCapability::Camera,
            crate::permissions::AppCapability::Camera,
        ];
        assert!(candidate.validate().is_err());
    }

    #[test]
    fn a_new_app_manifest_declares_no_collections() {
        let manifest = AppManifest::for_new_app("notes", "Notes");
        assert!(
            manifest.collections.is_empty(),
            "collections now come from the LLM plan, not from a template"
        );
    }

    #[test]
    fn active_mcp_catalog_still_requires_scaffold_identity() {
        let mut shell = AppManifest::for_new_app("schema-v3", "Schema");
        shell.active_mcp_catalog = Some(AppMcpCatalogRef {
            build_id: "build".into(),
            manifest_revision: 1,
            authoring_revision: 1,
            user_goal_sha256: "0".repeat(64),
            proposal_sha256: "0".repeat(64),
            approval_contract_sha256: "0".repeat(64),
            tool_surface_sha256: "0".repeat(64),
            catalog_sha256: "0".repeat(64),
            mcp_verification_sha256: "0".repeat(64),
        });
        let error = shell.validate().unwrap_err();
        assert!(error.to_string().contains("active_state_corrupt"));

        let mut published = AppManifest::for_new_app("schema-v3", "Schema");
        published.revision = 1;
        published.surface = Some(AppSurface::Dom);
        published.runtime_profile = Some(AppRuntimeProfileBinding {
            family: AppRuntimeProfile::ReactDom,
            revision: 1,
            contract_sha256: "1".repeat(64),
        });
        published.dependency_snapshot = None;
        published.template_origin = Some(AppTemplateOrigin {
            plugin_id: AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: "react-dom-r1".into(),
            template_sha256: "1".repeat(64),
        });
        // A scaffold draft may have no dependency snapshot until install.
        assert!(published.validate().is_ok());
    }

    #[test]
    fn publication_state_only_depends_on_active_build_and_ui_verification() {
        let mut manifest = AppManifest::for_new_app("state-app", "State");
        assert_eq!(
            derive_publication_state(&manifest, None, false).unwrap(),
            AppPublicationState::Draft
        );
        assert_eq!(
            derive_publication_state(&manifest, Some("build-1"), false).unwrap(),
            AppPublicationState::PublishedUnverified
        );
        assert_eq!(
            derive_publication_state(&manifest, Some("build-1"), true).unwrap(),
            AppPublicationState::PublishedVerified
        );
        manifest.active_mcp_catalog = Some(AppMcpCatalogRef {
            build_id: "build-1".into(),
            manifest_revision: 1,
            authoring_revision: 1,
            user_goal_sha256: "0".repeat(64),
            proposal_sha256: "0".repeat(64),
            approval_contract_sha256: "0".repeat(64),
            tool_surface_sha256: "0".repeat(64),
            catalog_sha256: "0".repeat(64),
            mcp_verification_sha256: "0".repeat(64),
        });
        assert!(derive_publication_state(&manifest, Some("build-1"), false).is_ok());
        assert!(derive_publication_state(&manifest, Some("build-2"), false).is_ok());
        assert_eq!(
            derive_publication_state(&manifest, None, false).unwrap(),
            AppPublicationState::Draft
        );
    }

    #[test]
    fn immutable_mcp_catalog_round_trips_and_rejects_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "catalog-app").unwrap();
        let catalog = serde_json::json!({"tools": [{"name": "read"}]});
        let digest = crate::mcp_authoring::approval_contract_sha256(catalog.clone()).unwrap();
        save_mcp_catalog(&layout, &digest, &catalog).unwrap();
        assert_eq!(load_mcp_catalog(&layout, &digest).unwrap(), catalog);
        let different = serde_json::json!({"tools": [{"name": "write"}]});
        assert!(save_mcp_catalog(&layout, &digest, &different).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_in_layout() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("apps")).unwrap();
        symlink("/tmp", root.path().join("apps/abcd1234")).unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        assert!(matches!(
            layout.initialize(),
            Err(AppError::StorageCorrupt(_))
        ));
    }
}
