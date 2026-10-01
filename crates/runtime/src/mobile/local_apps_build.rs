//! The local-app BUILD CORE: template scaffolding plus the fixed offline
//! Vite build, extracted from the generation pipeline so that
//! scaffold/build no longer belongs to the LLM-generation executor.

use crate::mobile::local_app_runtime_profiles::{
    contract_for_binding as runtime_profile_contract_for_binding, CANVAS_2D_R4_EDITABLE_FILES,
    CANVAS_2D_R4_MANAGED_FILES, EFFECTIVE_PACKAGE_FILE_REL, LOCKFILE_FILE_REL,
    PHASER_2D_R4_EDITABLE_FILES, PHASER_2D_R4_MANAGED_FILES, REACT_DOM_R4_EDITABLE_FILES,
    REACT_DOM_R4_MANAGED_FILES, REQUESTED_FILE_REL, SBOM_FILE_REL, SNAPSHOT_FILE_REL,
    THREE_3D_R4_EDITABLE_FILES, THREE_3D_R4_MANAGED_FILES, TREE_PROOF_FILE_REL,
};
use crate::mobile::local_apps_host::LocalAppsHostBroker;
use local_apps::{
    AppDataStore, AppError, AppLayout, AppManifest, AppRecord, AppRuntimeProfile,
    AppRuntimeProfileBinding,
};
use lsp_types::{DiagnosticSeverity, NumberOrString};

use mobile_linux_api::{
    LinuxCommandRequest, MobileLinuxRuntime, MountPurpose, MountSpec, NetworkPolicy, ResourceLimits,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

const BUILD_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const LOW_MEMORY_BUILD_BUDGET_MB: u32 = 2_048;
const MID_MEMORY_BUILD_BUDGET_MB: u32 = 3_072;
const HIGH_MEMORY_BUILD_BUDGET_MB: u32 = 4_096;
const MAX_BUILD_LOG_BYTES: u64 = 1024 * 1024;
const BUILD_PROVENANCE_FILE: &str = "build.json";
const BUILD_PROVENANCE_VERSION: u8 = 3;
const LSP_DIAGNOSTIC_SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
/// Vite's default deployment directory, relative to the isolated project root.
pub(crate) const VITE_OUTPUT_DIR: &str = "dist";

/// Immutable Host identity captured when a staged authoring candidate is
/// selected for a build. The candidate itself remains on disk until the
/// successful build consumes this exact identity under the app/build lock.
#[derive(Debug, Clone)]
pub(crate) struct AuthoringCandidateIdentity {
    pub(crate) handle: String,
    pub(crate) workflow_run_id: String,
    pub(crate) contract_sha256: String,
    pub(crate) base_contract_sha256: Option<String>,
}

/// Host-derived health of a scaffolded Local App runtime profile.
///
/// This is deliberately a state classification rather than an error string.
/// Details screens and MCP callers need to distinguish a user-requested
/// dependency change from a damaged runtime contract without parsing build
/// errors. The classifier below is read-only: it never repairs managed files,
/// installs packages, scans app-owned source, or changes the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppRuntimeProfileStatus {
    Verified,
    DependenciesDirty,
    CoreDependencyDrift,
    RebuildRequired,
    MigrationAvailable,
    RuntimeBundleMissing,
    RuntimeContractCorrupt,
}

impl AppRuntimeProfileStatus {
    /// Stable wire spelling used by the client protocol and MCP responses.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::DependenciesDirty => "dependencies_dirty",
            Self::CoreDependencyDrift => "core_dependency_drift",
            Self::RebuildRequired => "rebuild_required",
            Self::MigrationAvailable => "migration_available",
            Self::RuntimeBundleMissing => "runtime_bundle_missing",
            Self::RuntimeContractCorrupt => "runtime_contract_corrupt",
        }
    }
}

impl std::fmt::Display for AppRuntimeProfileStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildProvenance {
    version: u8,
    #[serde(rename = "buildId")]
    build_id: String,
    #[serde(rename = "buildKey")]
    build_key: String,
    #[serde(rename = "runtimeContractSha256")]
    runtime_contract_sha256: String,
    #[serde(rename = "dependencySnapshotSha256")]
    dependency_snapshot_sha256: String,
    #[serde(rename = "outputSha256")]
    output_sha256: String,
    /// Digest of the immutable Host authoring contract used for this build.
    ///
    /// Optional for v3 receipts written before authoring contracts existed.
    /// A missing value is distinct from a value of all zeroes: direct builds
    /// without a staged contract retain the previous (missing) identity.
    #[serde(
        rename = "authoringContractSha256",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    authoring_contract_sha256: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalAppBuildTarget {
    ReactDomR4,
    Canvas2dR4,
    Three3dR4,
    Phaser2dR4,
}

impl LocalAppBuildTarget {
    pub(crate) fn from_runtime_binding(
        binding: &AppRuntimeProfileBinding,
    ) -> Result<Self, AppError> {
        let contract = runtime_profile_contract_for_binding(binding)?;
        match (contract.family, contract.revision) {
            (AppRuntimeProfile::ReactDom, 4) => Ok(Self::ReactDomR4),
            (AppRuntimeProfile::Canvas2d, 4) => Ok(Self::Canvas2dR4),
            (AppRuntimeProfile::Three3d, 4) => Ok(Self::Three3dR4),
            (AppRuntimeProfile::Phaser2d, 4) => Ok(Self::Phaser2dR4),
            (family, revision) => Err(AppError::NotYetAvailable(format!(
                "runtime profile {family} r{revision} has no build bundle in this host build"
            ))),
        }
    }

    #[cfg(test)]
    fn surface(self) -> local_apps::AppSurface {
        match self {
            Self::ReactDomR4 => local_apps::AppSurface::Dom,
            Self::Canvas2dR4 | Self::Three3dR4 | Self::Phaser2dR4 => local_apps::AppSurface::Canvas,
        }
    }

    pub(crate) fn template_id(self) -> &'static str {
        match self {
            Self::ReactDomR4 => "runtime-profile/react-dom/r4",
            Self::Canvas2dR4 => "runtime-profile/canvas-2d/r4",
            Self::Three3dR4 => "runtime-profile/three-3d/r4",
            Self::Phaser2dR4 => "runtime-profile/phaser-2d/r4",
        }
    }

    #[cfg(test)]
    fn revision(self) -> u32 {
        4
    }

    #[cfg(test)]
    fn runtime_profile(self) -> AppRuntimeProfile {
        match self {
            Self::ReactDomR4 => AppRuntimeProfile::ReactDom,
            Self::Canvas2dR4 => AppRuntimeProfile::Canvas2d,
            Self::Three3dR4 => AppRuntimeProfile::Three3d,
            Self::Phaser2dR4 => AppRuntimeProfile::Phaser2d,
        }
    }
}

fn source_files(target: LocalAppBuildTarget) -> &'static [(&'static str, &'static [u8])] {
    match target {
        LocalAppBuildTarget::ReactDomR4 => REACT_DOM_R4_EDITABLE_FILES,
        LocalAppBuildTarget::Canvas2dR4 => CANVAS_2D_R4_EDITABLE_FILES,
        LocalAppBuildTarget::Three3dR4 => THREE_3D_R4_EDITABLE_FILES,
        LocalAppBuildTarget::Phaser2dR4 => PHASER_2D_R4_EDITABLE_FILES,
    }
}

fn preserve_committed_build_result(
    build_result: Result<(), AppError>,
    cleanup_result: Result<bool, AppError>,
    app_id: &str,
    handle: &str,
) -> Result<(), AppError> {
    if build_result.is_ok() {
        match cleanup_result {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                %app_id,
                %handle,
                "authoring candidate was not consumed after a committed build; it remains available for retry"
            ),
            Err(error) => tracing::warn!(
                %app_id,
                %handle,
                %error,
                "authoring candidate cleanup failed after a committed build; preserving build success"
            ),
        }
    }
    build_result
}

fn is_app_managed_javascript_file(relative: &str) -> bool {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || HOST_MANAGED_FILES.contains(&relative)
    {
        return false;
    }
    let writable_root = path
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(root) => root.to_str(),
            _ => None,
        })
        .is_some_and(|root| {
            matches!(
                root,
                "app" | "components" | "lib" | "styles" | "public" | "src"
            )
        });
    writable_root
        && matches!(
            path.extension().and_then(std::ffi::OsStr::to_str),
            Some("js" | "jsx" | "mjs" | "cjs")
        )
}

fn bounded_lsp_diagnostic_message(message: &str) -> String {
    const MAX_CHARS: usize = 300;
    let flattened = message.replace(['\n', '\r'], " ");
    let mut chars = flattened.chars();
    let mut bounded = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn locked_files(target: LocalAppBuildTarget) -> Vec<(&'static str, &'static [u8])> {
    match target {
        LocalAppBuildTarget::ReactDomR4 => REACT_DOM_R4_MANAGED_FILES.to_vec(),
        LocalAppBuildTarget::Canvas2dR4 => CANVAS_2D_R4_MANAGED_FILES.to_vec(),
        LocalAppBuildTarget::Three3dR4 => THREE_3D_R4_MANAGED_FILES.to_vec(),
        LocalAppBuildTarget::Phaser2dR4 => PHASER_2D_R4_MANAGED_FILES.to_vec(),
    }
}

/// Read `apps/<id>/workspace/.lingxi/app.json` — the app-scoped mirror of the
/// whole [`local_apps::AppRecord`], written next to the workspace it describes.
///
/// This is the only record fact reachable from an [`AppLayout`] alone, and it is
/// a legitimate one to read: `storage::repair_torn_commit` makes the mirror
/// AUTHORITATIVE over `apps/index.json` when a crash tears a commit, so it is
/// the persisted truth rather than a guess. That is categorically different from
/// sniffing `package.json` or `vite.config.mjs`, which the scaffold itself
/// rewrites before every build (see [`detect_build_target`]).
fn load_record_mirror(layout: &AppLayout) -> Result<local_apps::AppRecord, AppError> {
    let relative = local_apps::storage::metadata_rel(layout.app_id());
    let path = layout.root().join(&relative);
    let body = std::fs::read_to_string(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::StorageCorrupt(format!(
                "{} is missing. Every app writes this record mirror before its workspace is \
                 initialized, so its absence means the app directory is torn or was created \
                 outside the app store.",
                relative.display()
            ))
        } else {
            AppError::Io(format!("read {}: {error}", relative.display()))
        }
    })?;
    let mirror: local_apps::storage::AppMetadataFile = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("{}: {error}", relative.display())))?;
    if mirror.app.id != layout.app_id() {
        return Err(AppError::StorageCorrupt(format!(
            "{} mirrors app id {:?} but sits in the directory of {:?}",
            relative.display(),
            mirror.app.id,
            layout.app_id()
        )));
    }
    Ok(mirror.app)
}

/// Write `apps/<id>/workspace/.lingxi/app.json` back.
///
/// ⛔ TEST-ONLY. Only [`scaffold_workspace`] uses this, and only to flip
/// `scaffolded`.
///
/// In production this half is NOT this module's to write, and the `cfg` is
/// what enforces that. `AppService::with_app` persists the record through
/// `storage::save_app_files_steps`, whose `MetadataMirror` step writes exactly
/// this file whenever `record` changed — so `commit_scaffold` already lands
/// the mirror `detect_build_target` later reads back. A second writer here
/// would be a second source of truth for the same bytes, racing the service's
/// own rollback ordering.
#[cfg(test)]
fn save_record_mirror(layout: &AppLayout, record: &local_apps::AppRecord) -> Result<(), AppError> {
    let relative = local_apps::storage::metadata_rel(layout.app_id());
    let path = layout.root().join(&relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::Io(format!("create {}: {error}", parent.display())))?;
    }
    let mirror = local_apps::storage::AppMetadataFile {
        schema_version: local_apps::APPS_SCHEMA_VERSION,
        app: record.clone(),
    };
    let mut body = serde_json::to_string_pretty(&mirror)
        .map_err(|error| AppError::Io(format!("serialize {}: {error}", relative.display())))?;
    body.push('\n');
    std::fs::write(&path, body)
        .map_err(|error| AppError::Io(format!("write {}: {error}", relative.display())))
}

/// Which scaffold this workspace was built from.
///
/// The RECORD and the MANIFEST decide together, and the files on disk do not get
/// a vote. Sniffing the workspace would be circular: `package.json` and
/// `vite.config.mjs` are exactly the files `restore_host_managed_files` rewrites
/// from the compiled-in template before every build, so a mis-detection would
/// repin the wrong scaffold and then read its own output back as confirmation.
///
/// The judgement is on the COMBINATION of two independently-persisted facts —
/// `AppRecord.scaffolded` (the record mirror) and `AppManifest.runtime_profile`
/// — and the pair is checked as a pair, never field by field:
///
/// | `scaffolded` | `runtime_profile` | verdict |
/// |---|---|---|
/// | `false` | `None` | an unformed shell: refuse and name `LocalAppScaffold` |
/// | `true` | `Some(profile)` | build that profile's fixed surface/runtime family |
/// | `false` | `Some(_)` | no writer produces this — storage corruption |
/// | `true` | `None` | no writer produces this — storage corruption |
///
/// ⚠️ Mapping only `runtime_profile.family` the moment the binding is `Some(_)`
/// would be the
/// same code for the happy path and would pass every happy-path test, but a
/// torn record — scaffold committed the manifest, crashed before the index and
/// the mirror — would then build straight past the shell gate and hand the user
/// a workspace whose record still says it was never given a shape. The two
/// corrupt rows exist so that never happens silently.
///
/// The pre-split "manifest with no surface" refusal this replaced is gone
/// because the shape it described can no longer load: `AppRecord.scaffolded`
/// carries no serde default, so a store written before it fails at
/// `storage::load_all` and never reaches a build.
pub(crate) fn detect_build_target(layout: &AppLayout) -> Result<LocalAppBuildTarget, AppError> {
    let workspace = layout.root().join(layout.workspace_rel());
    if workspace.join("next.config.mjs").is_file() {
        return Err(AppError::StorageCorrupt(
            "workspace contains legacy Next build configuration; local apps now support Vite only"
                .into(),
        ));
    }

    let scaffolded = load_record_mirror(layout)?.scaffolded;
    let manifest = local_apps::load_manifest(layout)?;

    match (
        scaffolded,
        manifest.surface,
        manifest.runtime_profile.as_ref(),
    ) {
        (true, Some(surface), Some(binding)) => {
            let contract = runtime_profile_contract_for_binding(binding)?;
            if contract.surface != surface {
                return Err(AppError::StorageCorrupt(format!(
                    "app {} manifest records runtime profile {} on surface {}; the mapping is invalid",
                    layout.app_id(),
                    binding.family,
                    surface.as_str()
                )));
            }
            LocalAppBuildTarget::from_runtime_binding(binding)
        }
        (true, Some(surface), None) => Err(AppError::StorageCorrupt(format!(
            "app {}: the manifest records surface {} but no runtime profile, so there is no exact runtime contract to rebuild",
            layout.app_id(),
            surface.as_str()
        ))),
        // The `+` button lands one of these: a record, a workspace and a
        // conversation, but no shape. There is nothing to build yet, and the
        // agent reading this error is the one holding the fix.
        (false, None, None) => Err(AppError::InvalidRequest(
            "this app has no runtime profile yet — it was created as an empty shell and nothing \
             has been scaffolded into its workspace. Confirm the app name and runtime profile \
             with the user, then get the guided create flow lingxi-local-app:create-local-app \
             running — with the Skill tool yourself if you hold it, otherwise by asking the \
             calling agent or the user to run it — which lands LocalAppScaffold with the name, \
             brief and shape. Do not call LocalAppScaffold directly yourself. Building only \
             becomes possible after that."
                .into(),
        )),
        (false, _, Some(binding)) => Err(AppError::StorageCorrupt(format!(
            "app {}: the record says this workspace was never scaffolded, but the manifest \
             already records the {} runtime profile. No path in this version of the engine writes \
             that combination, so the app store is torn — most likely a scaffold that committed \
             the manifest and then crashed. Refusing to build rather than guessing which half is \
             right.",
            layout.app_id(),
            binding.family
        ))),
        (true, None, Some(binding)) => Err(AppError::StorageCorrupt(format!(
            "app {}: the manifest records runtime profile {} but no surface, so the scaffold \
             identity is incomplete and cannot be rebuilt safely",
            layout.app_id(),
            binding.family
        ))),
        (true, None, None) => Err(AppError::StorageCorrupt(format!(
            "app {}: the record says this workspace is scaffolded, but the manifest records no \
             surface or runtime profile, so there is no way to tell which scaffold/runtime \
             contract its source was seeded from. No path in this version of the engine writes \
             that combination; the app store is torn or the manifest was overwritten. Refusing \
             to build rather than guessing a scaffold.",
            layout.app_id()
        ))),
        (false, Some(surface), None) => Err(AppError::StorageCorrupt(format!(
            "app {}: the record says this workspace was never scaffolded, but the manifest \
             already records surface {}. No path in this version of the engine writes that \
             combination, so the app store is torn.",
            layout.app_id(),
            surface.as_str()
        ))),
    }
}

/// The common relative paths the host re-pins from the app's exact compiled-in
/// runtime-profile contract before every build.
///
/// The repository-verified Vite scaffold is the single source of truth for the
/// build infrastructure. Editable application code lives in `app/`, `src/`,
/// `styles/`, and non-host-managed files under `lib/`.
/// The host-managed set, IDENTICAL for every scaffold — and it has to stay that
/// way.
///
/// `permission::workspace_lease::host_owned_relative` answers "is this file
/// host-owned?" from the relative path ALONE; its signature carries no app id,
/// so it cannot know which scaffold it is looking at. If the two scaffolds
/// disagreed about this set, the lease would guard the wrong one for half the
/// apps: the agent's `Edit` would be accepted, the next build would silently
/// revert it via `restore_host_managed_files`, and the only trace would be a
/// `tracing::warn!` while the model looped against a file it could not change.
const HOST_MANAGED_FILES: &[&str] = &[
    ".gitignore",
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "jsconfig.json",
    "index.html",
    "vite.config.mjs",
    ".lingxi/source-policy.json",
    "lib/lingxi-bridge.js",
    "lib/device-context.js",
    "lib/platform-adapter.js",
    "lib/lingxi-provider.jsx",
    "lib/frame-loop.js",
    "lib/phaser-runtime.js",
    "lib/babylon-runtime.js",
    "styles/foundation.css",
];

/// Written as an exhaustive match rather than an ignored `_target` so that
/// giving a scaffold its own host-managed set is a deliberate edit here, next to
/// the comment explaining what else must move with it. The previous signature
/// took the target and discarded it, which meant a second scaffold would have
/// silently inherited this list with nothing failing.
fn repinned_host_managed_files(target: LocalAppBuildTarget) -> &'static [&'static str] {
    match target {
        LocalAppBuildTarget::ReactDomR4
        | LocalAppBuildTarget::Canvas2dR4
        | LocalAppBuildTarget::Three3dR4
        | LocalAppBuildTarget::Phaser2dR4 => HOST_MANAGED_FILES,
    }
}

/// [`HOST_MANAGED_FILES`] minus `.lingxi/source-policy.json`: the build root is
/// a copy of the workspace and does not carry host metadata.
#[cfg(test)]
fn build_locked_files(target: LocalAppBuildTarget) -> &'static [&'static str] {
    match target {
        LocalAppBuildTarget::ReactDomR4
        | LocalAppBuildTarget::Canvas2dR4
        | LocalAppBuildTarget::Three3dR4
        | LocalAppBuildTarget::Phaser2dR4 => &[
            ".gitignore",
            "package.json",
            "pnpm-lock.yaml",
            "pnpm-workspace.yaml",
            "jsconfig.json",
            "index.html",
            "vite.config.mjs",
            "lib/lingxi-bridge.js",
            "lib/device-context.js",
            "lib/platform-adapter.js",
            "lib/lingxi-provider.jsx",
            "lib/frame-loop.js",
            "lib/phaser-runtime.js",
            "lib/babylon-runtime.js",
            "styles/foundation.css",
        ],
    }
}

fn load_workspace_manifest(workspace: &Path) -> Result<Option<AppManifest>, AppError> {
    let path = workspace.join(".lingxi/app.manifest.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Io(format!(
                "read workspace manifest {}: {error}",
                path.display()
            )))
        }
    };
    let manifest: AppManifest = serde_json::from_str(&body).map_err(|error| {
        AppError::StorageCorrupt(format!("workspace manifest {}: {error}", path.display()))
    })?;
    manifest.validate()?;
    Ok(Some(manifest))
}

fn dependency_override_rel(relative: &str) -> Option<&'static str> {
    match relative {
        "package.json" => Some(EFFECTIVE_PACKAGE_FILE_REL),
        "pnpm-lock.yaml" => Some(LOCKFILE_FILE_REL),
        _ => None,
    }
}

fn authoritative_locked_file_bytes(
    workspace: &Path,
    manifest: Option<&AppManifest>,
    relative: &str,
    template_bytes: &'static [u8],
) -> Result<Vec<u8>, AppError> {
    let Some(override_rel) = dependency_override_rel(relative) else {
        return Ok(template_bytes.to_vec());
    };
    let Some(manifest) = manifest else {
        return Ok(template_bytes.to_vec());
    };
    if manifest.runtime_profile.is_none() || manifest.dependency_snapshot.is_none() {
        return Ok(template_bytes.to_vec());
    }
    let override_path = workspace.join(override_rel);
    std::fs::read(&override_path).map_err(|error| {
        AppError::StorageCorrupt(format!(
            "runtime-profile dependency snapshot is missing {}: {}",
            override_rel, error
        ))
    })
}

/// Rewrite every host-managed file from its compiled-in template unless it
/// already matches byte for byte. Called at the top of each build — the
/// enforcement point behind the workspace contract the prompts describe.
///
/// `.lingxi/source-policy.json` DECLARES `host_managed_paths`; restoring here
/// makes the build the actual enforcement point instead of leaving that
/// contract in prompt text only.
pub(crate) fn restore_host_managed_files(
    workspace: &Path,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    let managed = repinned_host_managed_files(target);
    let manifest = load_workspace_manifest(workspace)?;
    for (relative, bytes) in locked_files(target)
        .iter()
        .filter(|(relative, _)| managed.contains(relative))
    {
        let expected =
            authoritative_locked_file_bytes(workspace, manifest.as_ref(), relative, bytes)?;
        let path = ensure_safe_file_parent(workspace, relative)?;
        let is_matching_regular_file = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                std::fs::read(&path).map_err(|error| {
                    AppError::Io(format!(
                        "read host-managed workspace file {}: {error}",
                        path.display()
                    ))
                })? == expected
            }
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(AppError::Io(format!(
                    "inspect host-managed workspace file {}: {error}",
                    path.display()
                )))
            }
        };
        if is_matching_regular_file {
            continue;
        }
        tracing::warn!(
            file = %relative,
            "host-managed workspace file diverged from its pinned template; restoring before the build"
        );
        write_file(workspace, relative, &expected, true)?;
    }
    Ok(())
}

pub(crate) fn build_memory_budget_mb(physical_memory_bytes: u64) -> u32 {
    let gib = 1024_u64.pow(3);
    if physical_memory_bytes >= 8 * gib {
        HIGH_MEMORY_BUILD_BUDGET_MB
    } else if physical_memory_bytes >= 6 * gib {
        MID_MEMORY_BUILD_BUDGET_MB
    } else {
        LOW_MEMORY_BUILD_BUDGET_MB
    }
}

pub(crate) fn node_old_space_mb(build_budget_mb: u32) -> u32 {
    build_budget_mb.saturating_mul(3) / 4
}

fn build_tool_executable(build_guest_path: &str) -> String {
    format!("{build_guest_path}/node_modules/vite/bin/vite.js")
}

fn fixed_vite_build_args(build_memory_mb: u32, executable: String, out_dir: String) -> Vec<String> {
    vec![
        format!(
            "--max-old-space-size={}",
            node_old_space_mb(build_memory_mb)
        ),
        executable,
        "build".into(),
        "--outDir".into(),
        out_dir,
        "--emptyOutDir".into(),
        // PIN the config instead of letting Vite self-resolve. Vite searches
        // `DEFAULT_CONFIG_FILES` in order and `vite.config.js` sorts FIRST,
        // ahead of the `vite.config.mjs` the host writes. A Vite config is
        // executed Node code, and `Edit` creates files on a nonexistent path,
        // so an unpinned build let the workspace's own `Edit(./**)` grant
        // reach arbitrary build-time code execution. Relative: the command
        // runs with `cwd` set to the project root.
        "--config".into(),
        "vite.config.mjs".into(),
    ]
}

fn local_app_build_mount(app_id: &str, channel: &str, build_root: &Path) -> MountSpec {
    MountSpec {
        host_path: build_root.to_path_buf(),

        guest_path: lingxi_core::host::local_app_paths::local_app_build_project(app_id, channel),

        read_only: false,
        purpose: MountPurpose::LocalAppBuild,
    }
}

/// The scaffold/build half of the old generation executor: everything needed
/// to lay down the template workspace and run the fixed offline build, with
/// no dependency on the LLM pipeline.
pub(crate) struct LocalAppBuilder<'a> {
    pub(crate) mobile_linux: Option<Arc<dyn MobileLinuxRuntime>>,
    pub(crate) host: &'a LocalAppsHostBroker,
}

/// Materialize the repository-verified Vite scaffold directly into a fresh
/// workspace so app creation does not depend on `npm create`.
///
/// ⛔ TEST-ONLY — this is a FIXTURE BUILDER, not a production entry point, and
/// the `cfg` gate is deliberate rather than incidental. Both real callers
/// (`scaffold_app_value` for `CreateMode::Scaffolded`, `land_scaffold` for
/// `LocalAppScaffold`) go straight to [`scaffold_workspace_initialized`],
/// because by then the enclosing create/scaffold transaction has already
/// initialized the layout, stamped `manifest.surface`, and — at its commit
/// point — flipped `AppRecord.scaffolded` and written the record mirror
/// through `AppService`. This function re-does those three steps WITHOUT a
/// service so a unit test can reach the same on-disk state; running it in
/// production would write the manifest and the mirror behind the service's
/// back.
///
/// If you find yourself wanting to call this from a non-test path, the answer
/// is `scaffold_workspace_initialized` plus the transaction that owns the
/// record — and the compiler will tell you so, which is why the gate is here
/// and not only in this comment.
#[cfg(test)]
pub(crate) fn scaffold_workspace(
    layout: &AppLayout,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    layout.initialize()?;
    // Stamp the surface the way creation does. In production the manifest is
    // already on disk by the time the scaffold runs (the service writes it
    // pre-commit), so a workspace materialized WITHOUT one is a shape that only
    // exists in tests — and it is unbuildable, because `detect_build_target`
    // reads exactly this field. Writing it here keeps "scaffolded" meaning the
    // same thing on both sides.
    let mut manifest = local_apps::AppManifest::for_new_app(layout.app_id(), layout.app_id());
    manifest.surface = Some(target.surface());
    manifest.runtime_profile =
        crate::mobile::local_app_runtime_profiles::binding_for_family_revision(
            target.runtime_profile(),
            target.revision(),
        );
    if let Some(binding) = manifest.runtime_profile.as_ref() {
        manifest.template_origin = Some(local_apps::AppTemplateOrigin {
            plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: format!(
                "{}-r{}",
                binding.family.as_str().replace('_', "-"),
                binding.revision
            ),
            template_sha256: binding.contract_sha256.clone(),
        });
    }
    local_apps::save_manifest(layout, &manifest)?;
    // And stamp the record mirror the same way, for the same reason:
    // `detect_build_target` judges the PAIR (`scaffolded`, `surface`), so a
    // workspace that received only half the stamp is a torn record by
    // construction and would refuse to build. An existing mirror is amended in
    // place rather than replaced — the record carries the user's name, brief and
    // ids, and only `scaffolded` is this function's to change.
    let mut record = load_record_mirror(layout).unwrap_or_else(|_| {
        local_apps::AppState::create_with_git(
            layout.app_id().to_string(),
            layout.app_id().to_string(),
            layout.app_id().to_string(),
            None,
            false,
            now_ms(),
        )
        .record
    });
    record.scaffolded = true;
    save_record_mirror(layout, &record)?;
    scaffold_workspace_initialized(layout, target, true)?;

    if let Some(binding) = manifest.runtime_profile.clone() {
        let workspace = layout.root().join(layout.workspace_rel());
        let requested = br#"{"dependencies":{}}"#.to_vec();
        let effective = std::fs::read(workspace.join("package.json"))
            .map_err(|error| AppError::Io(format!("read scaffolded package.json: {error}")))?;
        let lockfile = std::fs::read(workspace.join("pnpm-lock.yaml"))
            .map_err(|error| AppError::Io(format!("read scaffolded pnpm-lock.yaml: {error}")))?;
        let tree_sha256 = hash_bytes(b"test-dependency-tree");
        let sbom = br#"{"SPDXID":"SPDXRef-DOCUMENT"}"#.to_vec();
        let snapshot = local_apps::AppDependencySnapshot {
            requested_sha256: hash_bytes(&requested),
            package_sha256: hash_bytes(&effective),
            lockfile_sha256: hash_bytes(&lockfile),
            dependency_tree_sha256: tree_sha256.clone(),
            sbom_sha256: hash_bytes(&sbom),
            toolchain_key: runtime_profile_contract_for_binding(&binding)?
                .toolchain_key
                .to_string(),
            verified_profile_contract_sha256: binding.contract_sha256.clone(),
        };
        write_file(&workspace, REQUESTED_FILE_REL, &requested, true)?;
        write_file(&workspace, EFFECTIVE_PACKAGE_FILE_REL, &effective, true)?;
        write_file(&workspace, LOCKFILE_FILE_REL, &lockfile, true)?;
        write_file(
            &workspace,
            TREE_PROOF_FILE_REL,
            serde_json::to_string_pretty(&serde_json::json!({
                "treeSha256": tree_sha256,
                "toolchainKey": snapshot.toolchain_key.clone(),
            }))
            .expect("test tree proof json")
            .as_bytes(),
            true,
        )?;
        write_file(&workspace, SBOM_FILE_REL, &sbom, true)?;
        write_file(
            &workspace,
            SNAPSHOT_FILE_REL,
            serde_json::to_string_pretty(&snapshot)
                .expect("test dependency snapshot json")
                .as_bytes(),
            true,
        )?;
        manifest.dependency_snapshot = Some(snapshot);
        local_apps::save_manifest(layout, &manifest)?;
    }
    Ok(())
}

/// Materialize the pinned template after the enclosing create transaction has
/// already initialized the app layout. Keeping the initialized variant
/// private to the host path avoids a second full directory validation pass.
///
/// `target` is passed in rather than detected: this runs at creation, before
/// the workspace exists, so there is nothing on disk to detect from. The caller
/// has already recorded the choice on the manifest, which is what every later
/// build reads back.
///
/// `first_scaffold` says whether this call is the moment the app GETS its
/// shape. It is the boundary between two opposite duties and there is no third
/// case:
///
/// - `true` — the workspace is a shell, so nothing in it is the user's app yet.
///   The editable surface is WIPED before the seed lands (see
///   [`wipe_editable_surface`]).
/// - `false` — the app is already formed, so `app/`, `src/` and anything else
///   outside the host-managed set is the USER'S work. Nothing is wiped and the
///   seed does not overwrite what is already there.
pub(crate) fn scaffold_workspace_initialized(
    layout: &AppLayout,
    target: LocalAppBuildTarget,
    first_scaffold: bool,
) -> Result<(), AppError> {
    let workspace = layout.root().join(layout.workspace_rel());
    if first_scaffold {
        wipe_editable_surface(&workspace)?;
    }
    for (relative, bytes) in locked_files(target) {
        write_file(&workspace, relative, bytes, true)?;
    }
    for (relative, bytes) in source_files(target) {
        write_file(&workspace, relative, bytes, first_scaffold)?;
    }
    Ok(())
}

/// The workspace entries a first scaffold KEEPS. Everything else at the
/// workspace top level is removed.
///
/// What makes wiping everything else SAFE is not a property of the preserved
/// set, it is a property of when a first scaffold runs. A shell's workspace is
/// created by `write_guided_contract_value`, which "lays down no source, stamps
/// no surface, and touches nothing but `workspace/LINGXI.md`". So at landing
/// time every top-level entry other than `.lingxi/` and `LINGXI.md` was
/// necessarily written AFTER creation and BEFORE the user confirmed anything —
/// which is precisely what must not reach the real app.
///
/// `node_modules` is the third, but unlike the other two it is currently a
/// no-op guard, not a live one: `install_dependencies` is absent from
/// `SHELL_ALLOWED_OPERATIONS`, so nothing can install into an unscaffolded
/// shell's workspace before this wipe runs, and every production first
/// scaffold lands via `local_apps_host.rs`'s `land_scaffold` running BEFORE
/// its own dependency install — there is nothing under `node_modules` here
/// to preserve. The entry is kept defensively for the day a create path
/// installs ahead of scaffold; re-installing a byte-identical locked tree
/// would then cost real device minutes for nothing.
///
/// ⛔ Do NOT restate this as "the preserved set is the host-owned set". The
/// invariant runs in ONE direction only:
///
///   every PRESERVED name is host-owned AND holds state the seed cannot
///   reproduce
///
/// and NOT the converse. `permission::workspace_lease::host_owned_relative`
/// also returns `true` for `package.json`, `index.html`, `.gitignore`,
/// `jsconfig.json`, `pnpm-lock.yaml`, every `vite.config.*` spelling,
/// `lib/lingxi-provider.jsx` and `styles/foundation.css` — none of which are
/// preserved, and none of which need to be: the selected runtime-profile
/// bundle re-seeds them byte-for-byte on the very next lines of
/// [`scaffold_workspace_initialized`]. Deleting and re-seeding them is the
/// point; the three names below are the ones that CANNOT be re-seeded.
///
/// `.git` is deliberately absent. It is not host-owned, so a checkpoint taken
/// during the interview would carry those same pre-confirmation bytes and
/// `LocalAppCheckpointRestore` would put them back. The first scaffold is where
/// a formed app's history starts.
const FIRST_SCAFFOLD_PRESERVED: &[&str] = &[
    // The service's own documents, including the manifest whose `surface` the
    // caller has already stamped and which every later build reads back.
    local_apps::storage::APP_STATE_DIR,
    // The workspace contract. The caller rewrites it right after this returns;
    // keeping it means the workspace is never momentarily without one.
    "LINGXI.md",
    // The installed dependency tree, IF one is ever present here (see the
    // doc comment above: no production path installs before a first
    // scaffold today, so this is currently a defensive no-op). Re-installing
    // it costs minutes on device and it is byte-identical for every app
    // built from the same locked set.
    "node_modules",
];

/// Empty the workspace of everything except [`FIRST_SCAFFOLD_PRESERVED`].
///
/// §C.0.1. A shell app has no legitimate application source by definition, so
/// wiping is safe — and it is what makes the retry path safe too: every attempt
/// starts from clean ground.
///
/// Per-path `overwrite = true` is NOT enough, and the reason is specific. Vite
/// resolves `.js` BEFORE `.jsx` in `DEFAULT_EXTENSIONS`, the pinned template
/// sets no `resolve.extensions` override, and every import in it is
/// extensionless. A pre-written `app/app.js` therefore WINS resolution over the
/// seeded `app/app.jsx`, and `copy_workspace_tree` carries it into the build
/// root — the seed is on disk and never executed. Overwriting the seed's own
/// paths does nothing about a file at a path the seed does not occupy.
fn wipe_editable_surface(workspace: &Path) -> Result<(), AppError> {
    // Prove the target BEFORE removing anything: `remove_dir_all` cannot be
    // undone, and the two ways this could be pointed somewhere else are both
    // cheap to rule out.
    //
    // A REAL directory, never a symlink, so nothing outside the app tree can be
    // reached through the workspace root itself. `write_file` makes the same
    // check via `ensure_safe_file_parent`, but it runs AFTER this one — leaning
    // on it would mean the tree was already gone by the time it fired.
    let metadata = std::fs::symlink_metadata(workspace).map_err(|error| {
        AppError::Io(format!(
            "inspect app workspace {}: {error}",
            workspace.display()
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::InvalidRequest(format!(
            "refusing to scaffold over {}: an initialized app workspace is a real directory",
            workspace.display()
        )));
    }
    // And carrying `.lingxi/`, which `AppLayout::initialize` creates and which
    // both callers have therefore already produced. A path that is wrong or
    // empty deletes nothing instead of being emptied.
    let state_dir = workspace.join(local_apps::storage::APP_STATE_DIR);
    let is_initialized = std::fs::symlink_metadata(&state_dir)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
    if !is_initialized {
        return Err(AppError::InvalidRequest(format!(
            "refusing to scaffold over {}: not an initialized app workspace, {} is missing",
            workspace.display(),
            local_apps::storage::APP_STATE_DIR
        )));
    }

    for entry in std::fs::read_dir(workspace).map_err(|error| {
        AppError::Io(format!(
            "read app workspace {}: {error}",
            workspace.display()
        ))
    })? {
        let entry =
            entry.map_err(|error| AppError::Io(format!("read app workspace entry: {error}")))?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| FIRST_SCAFFOLD_PRESERVED.contains(&name))
        {
            continue;
        }
        let path = entry.path();
        // `symlink_metadata`, not `metadata`: a symlink to a directory must be
        // UNLINKED, not handed to `remove_dir_all`, which would empty a tree
        // the workspace merely points at.
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            AppError::Io(format!(
                "inspect pre-scaffold workspace entry {}: {error}",
                path.display()
            ))
        })?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path).map_err(|error| {
                AppError::Io(format!(
                    "remove pre-scaffold directory {}: {error}",
                    path.display()
                ))
            })?;
        } else {
            std::fs::remove_file(&path).map_err(|error| {
                AppError::Io(format!(
                    "remove pre-scaffold file {}: {error}",
                    path.display()
                ))
            })?;
        }
    }
    Ok(())
}

impl LocalAppBuilder<'_> {
    /// The build cannot start without the mobile Node runtime.
    fn assert_build_runtime_available(&self) -> Result<(), AppError> {
        if self.mobile_linux.is_none() {
            return Err(AppError::NotYetAvailable(
                "the verified mobile Node runtime is unavailable in this build".into(),
            ));
        }
        Ok(())
    }

    async fn run_fixed_build(
        &self,
        layout: &AppLayout,
        workspace: &Path,
        output_rel: &str,
    ) -> Result<(), AppError> {
        let runtime = self.mobile_linux.as_ref().ok_or_else(|| {
            AppError::NotYetAvailable(
                "the verified mobile Node runtime is unavailable in this build".into(),
            )
        })?;
        let build_channel = "store";
        let build_mount = local_app_build_mount(layout.app_id(), build_channel, workspace);
        let project_guest_path = build_mount.guest_path.clone();
        let tool_name = "Vite";
        let mut environment = BTreeMap::new();
        let build_state_root = workspace.join(".lingxi-build-state");
        let home_root = build_state_root.join("home");
        let temp_root = build_state_root.join("tmp");
        let xdg_cache_root = build_state_root.join("xdg-cache");
        let xdg_config_root = build_state_root.join("xdg-config");
        let xdg_data_root = build_state_root.join("xdg-data");
        // Vite's state is disposable. Reset each build-private directory so
        // repeated builds cannot accumulate cache/temp files beside the app.
        for directory in [
            &home_root,
            &temp_root,
            &xdg_cache_root,
            &xdg_config_root,
            &xdg_data_root,
        ] {
            match std::fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    std::fs::remove_dir_all(directory).map_err(|error| {
                        AppError::Io(format!("reset build state directory: {error}"))
                    })?;
                }
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    std::fs::remove_file(directory).map_err(|error| {
                        AppError::Io(format!("remove build state symlink: {error}"))
                    })?;
                }
                Ok(_) => {
                    std::fs::remove_file(directory).map_err(|error| {
                        AppError::Io(format!("remove build state entry: {error}"))
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(AppError::Io(format!(
                        "inspect build state directory: {error}"
                    )))
                }
            }
            std::fs::create_dir_all(directory)
                .map_err(|error| AppError::Io(format!("create build state directory: {error}")))?;
        }
        let executable = build_tool_executable(&project_guest_path);
        let workspace_vite = workspace.join("node_modules/vite/bin/vite.js");
        match std::fs::symlink_metadata(&workspace_vite) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::InvalidRequest(
                    "workspace Vite executable must not be a symlink".into(),
                ));
            }
            Ok(_) => {
                return Err(AppError::InvalidRequest(
                    "workspace Vite executable must be a regular file".into(),
                ));
            }
            Err(error) => {
                return Err(AppError::Io(format!(
                    "inspect workspace Vite executable: {error}"
                )));
            }
        }
        let guest_build_state_root = format!("{project_guest_path}/.lingxi-build-state");
        let guest_home_root = format!("{guest_build_state_root}/home");
        let guest_temp_root = format!("{guest_build_state_root}/tmp");
        let guest_xdg_cache_root = format!("{guest_build_state_root}/xdg-cache");
        let guest_xdg_config_root = format!("{guest_build_state_root}/xdg-config");
        let guest_xdg_data_root = format!("{guest_build_state_root}/xdg-data");
        let toolchain =
            LocalAppsHostBroker::toolchain_for_layout(layout).map_err(AppError::StorageCorrupt)?;
        environment.insert("PATH".into(), toolchain.path().into());
        environment.insert("NODE_ENV".into(), "production".into());
        environment.insert("HOME".into(), guest_home_root);
        environment.insert("TMPDIR".into(), guest_temp_root.clone());
        environment.insert("TMP".into(), guest_temp_root.clone());
        environment.insert("TEMP".into(), guest_temp_root);
        environment.insert("XDG_CACHE_HOME".into(), guest_xdg_cache_root);
        environment.insert("XDG_CONFIG_HOME".into(), guest_xdg_config_root);
        environment.insert("XDG_DATA_HOME".into(), guest_xdg_data_root);
        let build_memory_mb = build_memory_budget_mb(self.host.physical_memory_bytes());
        let resource_limits = ResourceLimits {
            max_memory_mb: Some(build_memory_mb),
            ..ResourceLimits::default()
        };
        let mounts = vec![build_mount];
        let request = LinuxCommandRequest {
            command: toolchain.node_command().into(),
            args: fixed_vite_build_args(build_memory_mb, executable, output_rel.to_string()),
            cwd: Some(project_guest_path),
            env: environment,
            stdin: None,
            timeout_ms: Some(BUILD_TIMEOUT_MS),
            network: NetworkPolicy::Disabled,
            resource_limits,
            mounts,
        };
        let result = runtime
            .run_isolated(request)
            .await
            .map_err(|error| AppError::Io(format!("fixed {tool_name} build failed: {error}")))?;
        // Record BEFORE judging. The enforcement receipt and the exit check both
        // return early, and a build that fails either one is exactly the build
        // whose output someone needs to read — discarding it there is why a
        // device failure could leave no trace at all. Best-effort on purpose:
        // a diagnostics write must never mask an enforcement failure.
        let outcome = format!(
            "exit={} timed_out={} cancelled={}",
            result.exit_code, result.timed_out, result.cancelled
        );
        if let Err(error) =
            append_build_log(layout, false, &outcome, &result.stdout, &result.stderr).await
        {
            tracing::warn!(%error, "failed to append build log");
        }
        result
            .enforcement
            .ensure_for(NetworkPolicy::Disabled, resource_limits)
            .map_err(|error| AppError::Io(error.to_string()))?;
        if result.timed_out || result.cancelled || result.exit_code != 0 {
            return Err(AppError::Io(format!(
                "fixed {tool_name} build exited {} (timed_out={}, cancelled={}): {}",
                result.exit_code,
                result.timed_out,
                result.cancelled,
                bounded_message(&result.stderr)
            )));
        }
        Ok(())
    }

    pub(crate) async fn run_vite_build(
        &self,
        layout: &AppLayout,
        workspace: &Path,
        output_rel: &str,
    ) -> Result<(), AppError> {
        self.run_fixed_build(layout, workspace, output_rel).await
    }

    async fn gate_lsp_diagnostics_before_build(&self, workspace: &Path) -> Result<(), AppError> {
        let Some(registry) = self.host.upgraded_lsp_registry() else {
            return Ok(());
        };
        let Some(settle) = registry
            .settle_diagnostics_under_host_root(workspace, LSP_DIAGNOSTIC_SETTLE_TIMEOUT)
            .await
        else {
            return Ok(());
        };
        if !matches!(
            settle.state,
            lsp::diagnostic_registry::DiagnosticSettleState::Settled
        ) {
            tracing::info!(
                root = %workspace.display(),
                tracked_documents = settle.tracked_documents,
                state = ?settle.state,
                "local-app build continuing with degraded LSP diagnostics"
            );
            return Ok(());
        }
        let mut blocking = Vec::new();
        let mut degraded = false;
        let mut advisory = 0_usize;
        for snapshot in registry.latest_diagnostics_under_host_root(workspace).await {
            let Ok(relative) = snapshot.host_path.strip_prefix(workspace) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !is_app_managed_javascript_file(&relative) {
                continue;
            }
            if !matches!(
                snapshot.freshness,
                lsp::diagnostic_registry::DiagnosticFreshness::Fresh
            ) {
                degraded = true;
                advisory = advisory.saturating_add(snapshot.diagnostics.len());
                continue;
            }
            for diagnostic in snapshot.diagnostics {
                if diagnostic.severity != Some(DiagnosticSeverity::ERROR) {
                    advisory = advisory.saturating_add(1);
                    continue;
                }
                let code = diagnostic
                    .code
                    .as_ref()
                    .map(|code| match code {
                        NumberOrString::Number(number) => number.to_string(),
                        NumberOrString::String(text) => text.clone(),
                    })
                    .map(|code| format!(" [{code}]"))
                    .unwrap_or_default();
                blocking.push(format!(
                    "{relative}:{}:{}{} {}",
                    diagnostic.range.start.line + 1,
                    diagnostic.range.start.character + 1,
                    code,
                    bounded_lsp_diagnostic_message(&diagnostic.message)
                ));
                if blocking.len() >= 20 {
                    break;
                }
            }
            if blocking.len() >= 20 {
                break;
            }
        }
        if degraded {
            tracing::info!(
                root = %workspace.display(),
                "local-app build continuing with degraded LSP diagnostics because some files had stale or versionless snapshots"
            );
        }
        if advisory > 0 {
            tracing::info!(
                root = %workspace.display(),
                advisory_diagnostics = advisory,
                "local-app LSP Warning/Info diagnostics were reported to the builder without blocking Vite"
            );
        }
        if blocking.is_empty() {
            Ok(())
        } else {
            Err(AppError::LspDiagnosticsFailed(format!(
                "lsp_diagnostics_failed: vite_started=false; fix TypeScript diagnostics before LocalAppBuild:\n{}",
                blocking.join("\n")
            )))
        }
    }

    /// Build the app from the workspace's own dependency tree and promote only
    /// the validated output into the public build root. The workspace is the
    /// only app-owned build mount; dependencies must not be supplied by a
    /// nested or shared runtime mount.
    pub(crate) async fn build_workspace(&self, layout: &AppLayout) -> Result<(), AppError> {
        // Dependency installation is host-owned and runs through the same
        // isolated mobile runtime/mount as Vite. Wait before taking the build
        // locks so an installer can never wait on a lock held by this build.
        self.assert_build_runtime_available()?;
        let dependency = match self
            .host
            .ensure_dependency_install(layout.app_id(), true)
            .await
        {
            Ok(dependency) if dependency.state == local_apps::AppDependencyState::Ready => {
                dependency
            }
            Ok(dependency) => {
                return Err(AppError::NotYetAvailable(
                    dependency.last_error.unwrap_or_else(|| {
                        format!("workspace dependencies are {}", dependency.state)
                    }),
                ));
            }
            Err(error) => return Err(AppError::NotYetAvailable(error)),
        };
        let build_lock = self.host.build_lock();
        let _build_guard = build_lock.lock().await;
        // The process-global guard serializes Node builds across every broker
        // in this process. The per-app storage lock extends exclusion for this
        // app across engine processes and the native delete path, which
        // otherwise could rename the app while promotion is between its two
        // directory renames.
        let _process_build_guard =
            local_apps::storage::lock_app_build(layout.root(), layout.app_id())?;
        self.build_workspace_locked_with_authoring(layout, &dependency, None)
            .await
    }

    /// Build with a staged Host authoring candidate. Its complete identity is
    /// revalidated while the build locks are held and consumed only after
    /// successful promotion, so a failed build leaves the candidate and the
    /// previous build receipt authoritative.
    pub(crate) async fn build_workspace_with_authoring(
        &self,
        layout: &AppLayout,
        authoring_candidate: Option<AuthoringCandidateIdentity>,
    ) -> Result<(), AppError> {
        self.assert_build_runtime_available()?;
        let dependency = match self
            .host
            .ensure_dependency_install(layout.app_id(), true)
            .await
        {
            Ok(dependency) if dependency.state == local_apps::AppDependencyState::Ready => {
                dependency
            }
            Ok(dependency) => {
                return Err(AppError::NotYetAvailable(
                    dependency.last_error.unwrap_or_else(|| {
                        format!("workspace dependencies are {}", dependency.state)
                    }),
                ));
            }
            Err(error) => return Err(AppError::NotYetAvailable(error)),
        };
        let build_lock = self.host.build_lock();
        let _build_guard = build_lock.lock().await;
        let _process_build_guard =
            local_apps::storage::lock_app_build(layout.root(), layout.app_id())?;
        self.build_workspace_locked_with_authoring(
            layout,
            &dependency,
            authoring_candidate.as_ref(),
        )
        .await
    }

    /// Build while the caller already owns both the broker-wide build mutex
    /// and this app's cross-process build lock.
    ///
    /// Dependency updates use this entry point so resolving, snapshotting,
    /// building and rollback remain one transaction. Calling
    /// [`Self::build_workspace`] there would try to reacquire the same locks
    /// and deadlock.
    pub(crate) async fn build_workspace_locked(
        &self,
        layout: &AppLayout,
        dependency: &local_apps::AppDependencyRecord,
    ) -> Result<(), AppError> {
        self.build_workspace_locked_with_authoring(layout, dependency, None)
            .await
    }

    async fn build_workspace_locked_with_authoring(
        &self,
        layout: &AppLayout,
        dependency: &local_apps::AppDependencyRecord,
        authoring_candidate: Option<&AuthoringCandidateIdentity>,
    ) -> Result<(), AppError> {
        let _perf =
            crate::mobile::local_apps_host::LocalAppPerfDiagnosticTimer::start("build_total");
        self.assert_build_runtime_available()?;
        if dependency.state != local_apps::AppDependencyState::Ready {
            return Err(AppError::NotYetAvailable(
                dependency
                    .last_error
                    .clone()
                    .unwrap_or_else(|| format!("workspace dependencies are {}", dependency.state)),
            ));
        }
        let workspace = layout.root().join(layout.workspace_rel());
        let target = detect_build_target(layout)?;
        validate_dependency_snapshot_files(layout, &workspace)?;
        if let Some(candidate) = authoring_candidate {
            self.host
                .verify_authoring_candidate_identity(layout, candidate)
                .map_err(AppError::InvalidRequest)?;
        }
        // Re-pin the host-managed files from the compiled-in templates on
        // EVERY build before Vite touches the workspace.
        restore_host_managed_files(&workspace, target)?;
        let build_root = layout.root().join(layout.build_rel(false));
        recover_build_promotion(&build_root)?;
        let manifest = local_apps::load_manifest(layout)?;
        let runtime_contract_sha256 = manifest.runtime_contract_hash()?;
        let dependency_snapshot_sha256 = manifest.dependency_snapshot_hash()?;
        let build_key = workspace_build_key(layout, &workspace)?;
        let effective_authoring_contract_sha256 = match authoring_candidate {
            Some(candidate) => Some(candidate.contract_sha256.clone()),
            None => active_build_authoring_contract_sha256(layout)?,
        };
        if build_cache_hit_with_authoring(
            &build_root,
            &build_key,
            &runtime_contract_sha256,
            &dependency_snapshot_sha256,
            effective_authoring_contract_sha256.as_deref(),
        )? {
            if let Some(candidate) = authoring_candidate {
                let _ = local_apps::authoring::consume_authoring_candidate_if_matches(
                    layout,
                    &candidate.handle,
                    &candidate.workflow_run_id,
                    &candidate.contract_sha256,
                    candidate.base_contract_sha256.as_deref(),
                )?;
            }
            return Ok(());
        }
        self.gate_lsp_diagnostics_before_build(&workspace).await?;
        let artifact_root = workspace_build_artifact_root(&workspace);
        let artifact_output_rel = workspace_build_output_rel();
        let mut build_result = async {
            let prepare_artifact_root = artifact_root.clone();
            tokio::task::spawn_blocking(move || remove_path_if_exists(&prepare_artifact_root))
                .await
                .map_err(|error| {
                    AppError::Io(format!("build preparation worker failed: {error}"))
                })??;
            self.run_vite_build(layout, &workspace, &artifact_output_rel)
                .await?;
            if let Some(candidate) = authoring_candidate {
                self.host
                    .verify_authoring_candidate_identity(layout, candidate)
                    .map_err(AppError::InvalidRequest)?;
            }
            let validate_artifact_root = artifact_root.clone();
            let validate_build_root = build_root.clone();
            let build_key_for_publish = build_key.clone();
            let runtime_contract_for_publish = runtime_contract_sha256.clone();
            let dependency_snapshot_for_publish = dependency_snapshot_sha256.clone();
            let authoring_contract_sha256 = effective_authoring_contract_sha256.clone();
            tokio::task::spawn_blocking(move || {
                let output_sha256 = validate_build_output(&validate_artifact_root)?;
                // The receipt is part of the validated staging tree.  Write
                // it before the directory swap so any serialization or I/O
                // failure leaves the last-good promoted build and its
                // authoring identity untouched.
                write_build_provenance_with_authoring(
                    &validate_artifact_root,
                    &build_key_for_publish,
                    &runtime_contract_for_publish,
                    &dependency_snapshot_for_publish,
                    &output_sha256,
                    authoring_contract_sha256.as_deref(),
                )?;
                prune_staging_root_for_publish(&validate_artifact_root)?;
                promote_build_root(&validate_artifact_root, &validate_build_root)?;
                Ok(())
            })
            .await
            .map_err(|error| AppError::Io(format!("build promotion worker failed: {error}")))?
        }
        .await;
        if build_result.is_err() {
            let _ = std::fs::remove_dir_all(&artifact_root);
        }
        if build_result.is_ok() {
            if let Some(candidate) = authoring_candidate {
                let cleanup_result = local_apps::authoring::consume_authoring_candidate_if_matches(
                    layout,
                    &candidate.handle,
                    &candidate.workflow_run_id,
                    &candidate.contract_sha256,
                    candidate.base_contract_sha256.as_deref(),
                );
                build_result = preserve_committed_build_result(
                    build_result,
                    cleanup_result,
                    layout.app_id(),
                    &candidate.handle,
                );
            }
        }
        build_result
    }
}

fn workspace_build_artifact_root(workspace: &Path) -> PathBuf {
    workspace.join(".lingxi-build-state").join("build-output")
}

fn workspace_build_output_rel() -> String {
    format!(".lingxi-build-state/build-output/{VITE_OUTPUT_DIR}")
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>, AppError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::StorageCorrupt(format!("{label} is missing: {}", path.display()))
        } else {
            AppError::Io(format!("inspect {label} {}: {error}", path.display()))
        }
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::StorageCorrupt(format!(
            "{label} must be a regular file: {}",
            path.display()
        )));
    }
    std::fs::read(path).map_err(|error| AppError::Io(format!("read {label}: {error}")))
}

/// Verify the host-owned dependency declaration and proof before any build can
/// restore files or invoke pnpm/Vite. Workspace package/lock edits are drift,
/// not an implicit dependency update, and must go through the receipt flow.
fn validate_dependency_snapshot_files(
    layout: &AppLayout,
    workspace: &Path,
) -> Result<(), AppError> {
    let manifest = local_apps::load_manifest(layout)?;
    let Some(binding) = manifest.runtime_profile.as_ref() else {
        return Ok(());
    };
    let contract = runtime_profile_contract_for_binding(binding)?;
    let snapshot = manifest.dependency_snapshot.as_ref().ok_or_else(|| {
        AppError::StorageCorrupt(format!(
            "app {} is scaffolded but dependencySnapshot is missing",
            layout.app_id()
        ))
    })?;
    if snapshot.toolchain_key != contract.toolchain_key {
        return Err(AppError::StorageCorrupt(format!(
            "runtime_contract_corrupt: dependency snapshot toolchain {} does not match profile {}",
            snapshot.toolchain_key, contract.toolchain_key
        )));
    }

    let requested = read_regular_file(&workspace.join(REQUESTED_FILE_REL), "requested.json")?;
    let effective = read_regular_file(
        &workspace.join(EFFECTIVE_PACKAGE_FILE_REL),
        "effective-package.json",
    )?;
    let lock = read_regular_file(&workspace.join(LOCKFILE_FILE_REL), "dependency lockfile")?;
    let sbom = read_regular_file(&workspace.join(SBOM_FILE_REL), "dependency SBOM")?;
    let snapshot_file = read_regular_file(
        &workspace.join(SNAPSHOT_FILE_REL),
        "dependency snapshot receipt",
    )?;
    let persisted_snapshot: local_apps::AppDependencySnapshot =
        serde_json::from_slice(&snapshot_file).map_err(|error| {
            AppError::StorageCorrupt(format!("parse dependency snapshot receipt: {error}"))
        })?;
    if &persisted_snapshot != snapshot {
        return Err(AppError::StorageCorrupt(
            "dependency snapshot receipt does not match the app manifest".into(),
        ));
    }
    for (label, bytes, expected) in [
        (
            "requested.json",
            requested.as_slice(),
            snapshot.requested_sha256.as_str(),
        ),
        (
            "effective-package.json",
            effective.as_slice(),
            snapshot.package_sha256.as_str(),
        ),
        (
            "pnpm-lock.yaml",
            lock.as_slice(),
            snapshot.lockfile_sha256.as_str(),
        ),
        (
            "sbom.spdx.json",
            sbom.as_slice(),
            snapshot.sbom_sha256.as_str(),
        ),
    ] {
        let actual = hash_bytes(bytes);
        if actual != *expected {
            return Err(AppError::StorageCorrupt(format!(
                "core_dependency_drift: {label} digest {actual} does not match dependency snapshot {expected}"
            )));
        }
    }
    let proof = read_regular_file(
        &workspace.join(TREE_PROOF_FILE_REL),
        "dependency tree proof",
    )?;
    let proof: serde_json::Value = serde_json::from_slice(&proof).map_err(|error| {
        AppError::StorageCorrupt(format!("parse dependency tree proof: {error}"))
    })?;
    if proof.get("treeSha256").and_then(serde_json::Value::as_str)
        != Some(snapshot.dependency_tree_sha256.as_str())
        || proof
            .get("toolchainKey")
            .and_then(serde_json::Value::as_str)
            != Some(snapshot.toolchain_key.as_str())
    {
        return Err(AppError::StorageCorrupt(
            "dependency tree proof does not match dependencySnapshot".into(),
        ));
    }

    let root_package =
        read_regular_file(&workspace.join("package.json"), "workspace package.json")?;
    let root_lock = read_regular_file(
        &workspace.join("pnpm-lock.yaml"),
        "workspace pnpm-lock.yaml",
    )?;
    if root_package != effective || root_lock != lock {
        // WP8: this copy is reachable from the MODEL-callable `LocalAppBuild`
        // (`local_apps_host.rs`'s `build_app` -> `build_workspace` ->
        // `build_workspace_locked` -> here), unlike the two sibling copies.
        //
        // Why the message says "report the drift" and NOT "go change the
        // dependency": reaching here means the workspace package.json /
        // pnpm-lock.yaml have ALREADY diverged from the host-owned snapshot.
        // The supported way to change a dependency is the confirm/update
        // receipt pair, and it starts from a clean baseline —
        // `confirm_dependency_change` mints a receipt against the trusted
        // baseline and `update_dependencies` refuses a receipt whose baseline
        // moved (`dependencies_dirty: dependency confirmation became stale
        // before update`, in `update_dependencies`). From a
        // workspace that is already dirty there is nothing for the agent to
        // retry, so the finding is the terminal move; same wording as
        // `ensure_dependency_install`'s copy.
        //
        // An earlier revision of this comment justified the same wording by
        // asserting that `LocalAppConfirmDependencyChange` /
        // `LocalAppUpdateDependencies` were on NEITHER the builtin tool table
        // nor the MCP surface. Exactly one half of that was false: both DO
        // have rows in `LOCAL_APP_TOOLS` (local_apps_tools.rs:111-116, pinned
        // by `dependency_review_operations_are_wired_as_builtin_tools`), so
        // they are model-callable under their builtin `LocalApp*` names. The
        // MCP half was correct and stays correct — `call_tool` refuses every
        // static host operation before dispatch (local_apps_mcp.rs:3156-3162,
        // pinned by `the_mcp_surface_no_longer_serves_the_static_host_operations`);
        // `host_tool_catalog()` is the shared SCHEMA source the builtins are
        // built from, not a served surface. Either way the reason this message
        // says report-the-drift is the workspace's dirty state, not the tool
        // wiring.
        return Err(AppError::InvalidRequest(
            "dependencies_dirty: workspace package.json or pnpm-lock.yaml differs from the host-owned dependency snapshot; this cannot be repaired by re-editing package.json/lockfile — report the drift to the user/workflow as a finding instead of retrying"
                .into(),
        ));
    }
    Ok(())
}

/// Compute the source build key from the current app-owned bytes.
///
/// Explicit builds are trust-bearing, so this path must not reuse cached
/// content digests from mutable metadata such as size/mtime.
fn workspace_build_key(layout: &AppLayout, workspace: &Path) -> Result<String, AppError> {
    let manifest = local_apps::load_manifest(layout)?;
    let binding = manifest.runtime_profile.as_ref().ok_or_else(|| {
        AppError::StorageCorrupt(format!(
            "app {} has no runtime profile for its build key",
            layout.app_id()
        ))
    })?;
    let contract = runtime_profile_contract_for_binding(binding)?;
    let managed_paths = contract
        .managed_files
        .iter()
        .map(|(path, _)| *path)
        .collect::<BTreeSet<_>>();
    let mut inputs = Vec::new();
    collect_workspace_inputs(workspace, workspace, &mut inputs)?;
    inputs.retain(|path| path != "LINGXI.md" && !managed_paths.contains(path.as_str()));
    inputs.sort();

    let mut hasher = Sha256::new();
    for path in inputs {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(hash_file(&workspace.join(&path))?.as_bytes());
        hasher.update([0]);
    }
    hasher.update(manifest.runtime_contract_hash()?);
    hasher.update([0]);
    hasher.update(manifest.dependency_snapshot_hash()?);
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_workspace_inputs(
    current: &Path,
    workspace: &Path,
    files: &mut Vec<String>,
) -> Result<(), AppError> {
    for entry in std::fs::read_dir(current).map_err(|error| {
        AppError::Io(format!(
            "read build key directory {}: {error}",
            current.display()
        ))
    })? {
        let entry =
            entry.map_err(|error| AppError::Io(format!("read build key entry: {error}")))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `.lingxi` holds the service's OWN documents (`app.json`,
        // `design-spec.json`, `app.manifest.json`) -- application STATE, not
        // generated source. `local-apps/src/checkpoints.rs` already keeps them
        // out of source snapshots with three cooperating rules for exactly that
        // reason; the build key is the fourth place that must agree.
        //
        // It is load-bearing, not tidiness: `MetadataMirror` rewrites
        // `app.json` on every persist and each build mints a fresh
        // `last_build_id`, so leaving `.lingxi` in the key means every build
        // churns one of its own inputs -- `build_cache_hit` can then never hit
        // and every device build is a full Vite rebuild.
        if matches!(
            name.as_ref(),
            ".git" | ".lingxi" | ".lingxi-build-state" | "node_modules" | "dist"
        ) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            AppError::Io(format!(
                "inspect build key input {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::InvalidRequest(format!(
                "build key input must not be a symlink: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_workspace_inputs(&path, workspace, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(workspace)
                .map_err(|error| AppError::Io(format!("derive build key path: {error}")))?;
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, AppError> {
    let bytes = std::fs::read(path).map_err(|error| {
        AppError::Io(format!("read build key input {}: {error}", path.display()))
    })?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Provenance lives beside the promoted build, outside the editable workspace
/// and outside `dist/`, so the preview server never exposes it as an asset.
fn build_provenance_path(build_root: &Path) -> PathBuf {
    build_root.join(BUILD_PROVENANCE_FILE)
}

fn build_cache_hit(
    build_root: &Path,
    build_key: &str,
    runtime_contract_sha256: &str,
    dependency_snapshot_sha256: &str,
) -> Result<bool, AppError> {
    build_cache_hit_with_authoring(
        build_root,
        build_key,
        runtime_contract_sha256,
        dependency_snapshot_sha256,
        None,
    )
}

fn build_cache_hit_with_authoring(
    build_root: &Path,
    build_key: &str,
    runtime_contract_sha256: &str,
    dependency_snapshot_sha256: &str,
    authoring_contract_sha256: Option<&str>,
) -> Result<bool, AppError> {
    let index = build_root.join(VITE_OUTPUT_DIR).join("index.html");
    let index_metadata = match std::fs::symlink_metadata(&index) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(AppError::Io(format!("inspect cached build: {error}"))),
    };
    if !index_metadata.is_file() || index_metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let provenance = match std::fs::read_to_string(build_provenance_path(build_root)) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(AppError::Io(format!("read build provenance: {error}"))),
    };
    let provenance: BuildProvenance = match serde_json::from_str(&provenance) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(error = %error, "ignoring stale local-app build provenance");
            return Ok(false);
        }
    };
    if provenance.version != BUILD_PROVENANCE_VERSION
        || provenance.build_key != build_key
        || provenance.runtime_contract_sha256 != runtime_contract_sha256
        || provenance.dependency_snapshot_sha256 != dependency_snapshot_sha256
        || provenance.authoring_contract_sha256.as_deref() != authoring_contract_sha256
    {
        return Ok(false);
    }
    let output_sha256 = match digest_tree(&build_root.join(VITE_OUTPUT_DIR)) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(output_sha256 == provenance.output_sha256)
}

fn write_build_provenance(
    build_root: &Path,
    build_key: &str,
    runtime_contract_sha256: &str,
    dependency_snapshot_sha256: &str,
    output_sha256: &str,
) -> Result<(), AppError> {
    write_build_provenance_with_authoring(
        build_root,
        build_key,
        runtime_contract_sha256,
        dependency_snapshot_sha256,
        output_sha256,
        None,
    )
}

fn write_build_provenance_with_authoring(
    build_root: &Path,
    build_key: &str,
    runtime_contract_sha256: &str,
    dependency_snapshot_sha256: &str,
    output_sha256: &str,
    authoring_contract_sha256: Option<&str>,
) -> Result<(), AppError> {
    let path = build_provenance_path(build_root);
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Io("build provenance has no parent".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| AppError::Io(format!("create build provenance directory: {error}")))?;
    let temp = parent.join(format!(".{BUILD_PROVENANCE_FILE}.tmp-{}", now_stamp()));
    let body = serde_json::to_vec_pretty(&BuildProvenance {
        version: BUILD_PROVENANCE_VERSION,
        build_id: output_sha256.to_string(),
        build_key: build_key.to_string(),
        runtime_contract_sha256: runtime_contract_sha256.to_string(),
        dependency_snapshot_sha256: dependency_snapshot_sha256.to_string(),
        output_sha256: output_sha256.to_string(),
        authoring_contract_sha256: authoring_contract_sha256.map(ToOwned::to_owned),
    })
    .map_err(|error| AppError::Io(format!("serialize build provenance: {error}")))?;
    std::fs::write(&temp, body)
        .map_err(|error| AppError::Io(format!("write build provenance: {error}")))?;
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        AppError::Io(format!("publish build provenance: {error}"))
    })
}

fn validate_active_build_provenance(provenance: &BuildProvenance) -> Result<(), AppError> {
    if provenance.version != BUILD_PROVENANCE_VERSION {
        return Err(AppError::StorageCorrupt(format!(
            "runtime_contract_corrupt: build receipt version {} is not supported",
            provenance.version
        )));
    }
    if provenance.build_id.is_empty()
        || provenance.build_key.is_empty()
        || provenance.runtime_contract_sha256.is_empty()
        || provenance.dependency_snapshot_sha256.is_empty()
        || provenance.output_sha256.is_empty()
        || provenance
            .authoring_contract_sha256
            .as_deref()
            .is_some_and(|digest| !valid_sha256(digest))
    {
        return Err(AppError::StorageCorrupt(
            "runtime_contract_corrupt: build receipt identity is invalid".into(),
        ));
    }
    Ok(())
}

pub(crate) fn active_build_id(layout: &AppLayout) -> Result<Option<String>, AppError> {
    let build_root = layout.root().join(layout.build_rel(false));
    let provenance_path = build_provenance_path(&build_root);
    let body = match std::fs::read_to_string(&provenance_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Io(format!("read build receipt: {error}")));
        }
    };
    let provenance: BuildProvenance = serde_json::from_str(&body).map_err(|error| {
        AppError::StorageCorrupt(format!(
            "runtime_contract_corrupt: invalid build receipt: {error}"
        ))
    })?;
    validate_active_build_provenance(&provenance)?;
    Ok(Some(provenance.build_id))
}

/// Return the authoring-contract identity selected by the active build
/// provenance.  The build receipt is the selector; Host contract files are
/// immutable content-addressed documents and are never selected by scanning
/// the workspace.
pub(crate) fn active_build_authoring_contract_sha256(
    layout: &AppLayout,
) -> Result<Option<String>, AppError> {
    let build_root = layout.root().join(layout.build_rel(false));
    let provenance_path = build_provenance_path(&build_root);
    let body = match std::fs::read_to_string(&provenance_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::Io(format!("read build receipt: {error}"))),
    };
    let provenance: BuildProvenance = serde_json::from_str(&body).map_err(|error| {
        AppError::StorageCorrupt(format!(
            "runtime_contract_corrupt: invalid build receipt: {error}"
        ))
    })?;
    validate_active_build_provenance(&provenance)?;
    Ok(provenance.authoring_contract_sha256)
}

/// Resolve the effective immutable authoring contract selected by the active
/// build receipt.  The receipt digest is the selector; Host never scans
/// candidate files or picks the newest document.
pub(crate) fn active_authoring_contract(
    layout: &AppLayout,
) -> Result<Option<local_apps::authoring::AppAuthoringContract>, AppError> {
    let Some(digest) = active_build_authoring_contract_sha256(layout)? else {
        return Ok(None);
    };
    local_apps::authoring::load_authoring_contract(layout, &digest)
        .map(Some)
        .map_err(|error| match error {
            AppError::NotFound(_) => AppError::StorageCorrupt(
                "build provenance selects a missing authoring contract".into(),
            ),
            other => other,
        })
}

/// Validate only the immutable launch identity and promoted output. This gate
/// deliberately does not inspect app source, resolve packages, access the
/// network, or migrate a profile; any mismatch requires an explicit rebuild.
pub(crate) fn validate_build_for_launch(layout: &AppLayout) -> Result<(), AppError> {
    let _target = detect_build_target(layout)?;
    let manifest = local_apps::load_manifest(layout)?;
    if manifest.runtime_profile.is_none() || manifest.dependency_snapshot.is_none() {
        return Err(AppError::StorageCorrupt(format!(
            "app {} is scaffolded but has no complete runtime profile and dependency snapshot",
            layout.app_id()
        )));
    }
    let expected_runtime = manifest.runtime_contract_hash()?;
    let expected_dependencies = manifest.dependency_snapshot_hash()?;
    let build_root = layout.root().join(layout.build_rel(false));
    let provenance_path = build_provenance_path(&build_root);
    let body = std::fs::read_to_string(&provenance_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::NotYetAvailable(
                "rebuild_required: the promoted build has no build receipt".into(),
            )
        } else {
            AppError::Io(format!("read build receipt: {error}"))
        }
    })?;
    let provenance: BuildProvenance = serde_json::from_str(&body).map_err(|error| {
        AppError::StorageCorrupt(format!(
            "runtime_contract_corrupt: invalid build receipt: {error}"
        ))
    })?;
    validate_active_build_provenance(&provenance)?;
    if provenance.runtime_contract_sha256 != expected_runtime {
        return Err(AppError::NotYetAvailable(
            "rebuild_required: build receipt runtime contract does not match the app manifest"
                .into(),
        ));
    }
    if provenance.dependency_snapshot_sha256 != expected_dependencies {
        return Err(AppError::NotYetAvailable(
            "rebuild_required: build receipt dependency snapshot does not match the app manifest"
                .into(),
        ));
    }
    let output_root = build_root.join(VITE_OUTPUT_DIR);
    let actual_output = digest_tree(&output_root).map_err(|error| {
        AppError::NotYetAvailable(format!(
            "rebuild_required: promoted build output is missing or invalid: {error}"
        ))
    })?;
    if provenance.output_sha256 != actual_output {
        return Err(AppError::NotYetAvailable(
            "rebuild_required: promoted build output does not match its build receipt".into(),
        ));
    }
    Ok(())
}

/// Derive the persisted runtime-profile health for one app.
///
/// The record's `scaffolded` bit is the first authority: an empty shell has no
/// runtime profile status. For a formed app this function validates the
/// profile binding, dependency receipts, profile-managed files, and promoted
/// build output in that order. It intentionally does not call any repair,
/// dependency-install, or migration path and never walks app-owned source.
/// Every observable failure is represented by a stable status so details/MCP
/// callers do not need to parse build error prose.
pub(crate) fn derive_runtime_profile_status(
    root: &Path,
    record: &AppRecord,
) -> Option<AppRuntimeProfileStatus> {
    derive_runtime_profile_status_with_edges(
        root,
        record,
        local_apps::RUNTIME_PROFILE_MIGRATION_EDGES,
    )
}

fn derive_runtime_profile_status_with_edges(
    root: &Path,
    record: &AppRecord,
    migration_edges: &[local_apps::RuntimeProfileMigrationEdge],
) -> Option<AppRuntimeProfileStatus> {
    let corrupt = || Some(AppRuntimeProfileStatus::RuntimeContractCorrupt);
    let layout = match AppLayout::new(root, record.id.clone()) {
        Ok(layout) => layout,
        Err(_) => return corrupt(),
    };
    let mirror = match load_record_mirror(&layout) {
        Ok(mirror) => mirror,
        Err(_) => return corrupt(),
    };
    if mirror.id != record.id
        || mirror.scaffolded != record.scaffolded
        || mirror.workspace_rel != record.workspace_rel
    {
        return corrupt();
    }

    // A clean shell has no profile metadata yet and therefore no status. A
    // partial shell, however, is a torn state and must remain visible as
    // corruption instead of silently looking like an ordinary draft.
    if !record.scaffolded {
        return match local_apps::load_manifest(&layout) {
            Ok(manifest)
                if manifest.surface.is_none()
                    && manifest.runtime_profile.is_none()
                    && manifest.dependency_snapshot.is_none() =>
            {
                None
            }
            Ok(_) => corrupt(),
            Err(AppError::NotFound(_)) => None,
            Err(_) => corrupt(),
        };
    }

    let manifest = match local_apps::load_manifest(&layout) {
        Ok(manifest) => manifest,
        Err(_) => return corrupt(),
    };
    if manifest.app_id != record.id || manifest.runtime_api_version != local_apps::RUNTIME_API_MAJOR
    {
        return corrupt();
    }

    let Some(surface) = manifest.surface else {
        return corrupt();
    };
    let Some(binding) = manifest.runtime_profile.as_ref() else {
        return corrupt();
    };
    let Some(snapshot) = manifest.dependency_snapshot.as_ref() else {
        return corrupt();
    };
    if binding.family.surface() != surface
        || snapshot.verified_profile_contract_sha256 != binding.contract_sha256
    {
        return corrupt();
    }

    // A known-but-not-shipped family (for example the gated Babylon profile),
    // an unknown family/revision, or a removed historical bundle all mean the
    // exact runtime cannot be materialized on this host. A published bundle
    // with a different digest is corruption, not an upgrade opportunity.
    let contract = match runtime_profile_contract_for_binding(binding) {
        Ok(contract) => contract,
        Err(AppError::StorageCorrupt(_)) => return corrupt(),
        Err(_) => return Some(AppRuntimeProfileStatus::RuntimeBundleMissing),
    };
    if snapshot.toolchain_key != contract.toolchain_key {
        return corrupt();
    }

    let workspace = root.join(layout.workspace_rel());
    let Some(requested) = status_regular_file(&workspace.join(REQUESTED_FILE_REL)) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let Some(effective) = status_regular_file(&workspace.join(EFFECTIVE_PACKAGE_FILE_REL)) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let Some(lock) = status_regular_file(&workspace.join(LOCKFILE_FILE_REL)) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let Some(tree_proof) = status_regular_file(&workspace.join(TREE_PROOF_FILE_REL)) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let Some(sbom) = status_regular_file(&workspace.join(SBOM_FILE_REL)) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let Some(snapshot_file) = status_regular_file(&workspace.join(SNAPSHOT_FILE_REL)) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let persisted_snapshot: local_apps::AppDependencySnapshot =
        match serde_json::from_slice(&snapshot_file) {
            Ok(snapshot) => snapshot,
            Err(_) => return Some(AppRuntimeProfileStatus::CoreDependencyDrift),
        };
    if persisted_snapshot != snapshot.clone() {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    }
    if hash_bytes(&effective) != snapshot.package_sha256
        || hash_bytes(&lock) != snapshot.lockfile_sha256
        || hash_bytes(&sbom) != snapshot.sbom_sha256
    {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    }
    let proof: Value = match serde_json::from_slice(&tree_proof) {
        Ok(proof) => proof,
        Err(_) => return Some(AppRuntimeProfileStatus::CoreDependencyDrift),
    };
    if proof.get("treeSha256").and_then(Value::as_str)
        != Some(snapshot.dependency_tree_sha256.as_str())
        || proof.get("toolchainKey").and_then(Value::as_str)
            != Some(snapshot.toolchain_key.as_str())
    {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    }

    // `requested.json` is the only dependency input the agent is allowed to
    // propose directly. Its digest changing means the app is dirty and needs
    // the explicit dependency confirmation flow; the resolved package, lock,
    // tree proof and SBOM above remain host-owned evidence.
    if hash_bytes(&requested) != snapshot.requested_sha256 {
        return Some(AppRuntimeProfileStatus::DependenciesDirty);
    }
    let Some(root_package) = status_regular_file(&workspace.join("package.json")) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    let Some(root_lock) = status_regular_file(&workspace.join("pnpm-lock.yaml")) else {
        return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
    };
    if root_package != effective || root_lock != lock {
        return Some(AppRuntimeProfileStatus::DependenciesDirty);
    }

    // Package/lock are dependency-managed overrides. Every other catalog
    // managed file must still be byte-identical to the pinned profile bundle;
    // the status path only observes this drift and never restores it.
    for (relative, expected) in contract.managed_files {
        if matches!(*relative, "package.json" | "pnpm-lock.yaml") {
            continue;
        }
        let Some(actual) = status_regular_file(&workspace.join(relative)) else {
            return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
        };
        if actual.as_slice() != *expected {
            return Some(AppRuntimeProfileStatus::CoreDependencyDrift);
        }
    }

    let build_root = root.join(layout.build_rel(false));
    let Some(provenance_bytes) = status_regular_file(&build_provenance_path(&build_root)) else {
        return Some(AppRuntimeProfileStatus::RebuildRequired);
    };
    let provenance: BuildProvenance = match serde_json::from_slice(&provenance_bytes) {
        Ok(provenance) => provenance,
        Err(_) => return Some(AppRuntimeProfileStatus::RebuildRequired),
    };
    let expected_runtime = match manifest.runtime_contract_hash() {
        Ok(hash) => hash,
        Err(_) => return corrupt(),
    };
    let expected_dependencies = match manifest.dependency_snapshot_hash() {
        Ok(hash) => hash,
        Err(_) => return corrupt(),
    };
    if provenance.version != BUILD_PROVENANCE_VERSION
        || provenance.runtime_contract_sha256 != expected_runtime
        || provenance.dependency_snapshot_sha256 != expected_dependencies
    {
        return Some(AppRuntimeProfileStatus::RebuildRequired);
    }
    let output_root = build_root.join(VITE_OUTPUT_DIR);
    let output_sha256 = match digest_tree(&output_root) {
        Ok(hash) => hash,
        Err(_) => return Some(AppRuntimeProfileStatus::RebuildRequired),
    };
    if output_sha256 != provenance.output_sha256 {
        return Some(AppRuntimeProfileStatus::RebuildRequired);
    }

    if migration_edges
        .iter()
        .any(|edge| edge.family == binding.family && edge.from_revision == binding.revision)
    {
        Some(AppRuntimeProfileStatus::MigrationAvailable)
    } else {
        Some(AppRuntimeProfileStatus::Verified)
    }
}

/// Read one status input without following symlinks. A missing, non-regular,
/// unreadable or symlinked file is represented as `None` and classified by the
/// caller according to the owning contract.
fn status_regular_file(path: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    std::fs::read(path).ok()
}

fn now_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
async fn await_workspace_dependencies(
    workspace: &Path,
    timeout_duration: tokio::time::Duration,
) -> Result<(), AppError> {
    let vite = workspace.join("node_modules/vite/bin/vite.js");
    let start = tokio::time::Instant::now();
    loop {
        match std::fs::symlink_metadata(&vite) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Ok(());
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::InvalidRequest(
                    "workspace Vite executable must not be a symlink".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Io(format!(
                    "inspect workspace Vite executable: {error}"
                )));
            }
        }
        if start.elapsed() >= timeout_duration {
            return Err(AppError::NotYetAvailable(format!(
                "app dependencies are not ready yet; waited {} ms for workspace/node_modules/vite/bin/vite.js",
                timeout_duration.as_millis()
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn remove_path_if_exists(path: &Path) -> Result<(), AppError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
                .map_err(|error| AppError::Io(format!("remove stale build output: {error}")))
        }
        Ok(_) => std::fs::remove_file(path)
            .map_err(|error| AppError::Io(format!("remove stale build output file: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Io(format!(
            "inspect stale build output path: {error}"
        ))),
    }
}

fn validate_build_output(staging_root: &Path) -> Result<String, AppError> {
    let output_root = staging_root.join(VITE_OUTPUT_DIR);
    let index = output_root.join("index.html");
    if !index.is_file() {
        return Err(AppError::Io(format!(
            "fixed Vite build produced no {VITE_OUTPUT_DIR}/index.html in {}",
            staging_root.display()
        )));
    }
    digest_tree(&output_root)
}

fn digest_tree(root: &Path) -> Result<String, AppError> {
    let mut files = Vec::new();
    collect_tree_files(root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .map_err(|error| AppError::Io(format!("derive output digest path: {error}")))?;
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(&path).map_err(|error| {
            AppError::Io(format!(
                "read output digest input {}: {error}",
                path.display()
            ))
        })?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_tree_files(current: &Path, files: &mut Vec<PathBuf>) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(current)
        .map_err(|error| AppError::Io(format!("inspect build output: {error}")))?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::InvalidRequest(format!(
            "build output must not contain symlinks: {}",
            current.display()
        )));
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(current)
            .map_err(|error| AppError::Io(format!("read build output: {error}")))?
        {
            let entry =
                entry.map_err(|error| AppError::Io(format!("read build output entry: {error}")))?;
            collect_tree_files(&entry.path(), files)?;
        }
    } else if metadata.is_file() {
        files.push(current.to_path_buf());
    } else {
        return Err(AppError::InvalidRequest(format!(
            "build output must be a regular file or directory: {}",
            current.display()
        )));
    }
    Ok(())
}

/// Recover the only non-atomic window in the directory promotion protocol.
///
/// `promote_build_root` first renames the last-good output to a sibling
/// backup, then renames the validated staging tree into the public `build/`
/// path. If the process dies between those renames, the next build must put
/// the last-good output back before creating another staging tree. Stale
/// staging/backup siblings are private names generated by this module, so
/// sweeping only those prefixes cannot touch app-owned source files.
fn recover_build_promotion(build_root: &Path) -> Result<(), AppError> {
    let Some(parent) = build_root.parent() else {
        return Ok(());
    };
    let Some(file_name) = build_root.file_name().and_then(|name| name.to_str()) else {
        return Err(AppError::Io(format!(
            "build root {} is not valid UTF-8",
            build_root.display()
        )));
    };
    let backup_prefix = format!(".{file_name}.previous-");
    let staging_prefix = format!(".{file_name}.staging-");
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::Io(format!(
                "scan interrupted build promotion: {error}"
            )))
        }
    };
    let mut backups = Vec::new();
    let mut staging = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| AppError::Io(format!("scan build promotion: {error}")))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&backup_prefix) {
            backups.push(entry.path());
        } else if name.starts_with(&staging_prefix) {
            staging.push(entry.path());
        }
    }
    backups.sort();
    let has_build = build_root.exists();
    if !has_build {
        if let Some(last_good) = backups.pop() {
            std::fs::rename(&last_good, build_root).map_err(|error| {
                AppError::Io(format!(
                    "restore interrupted build output {}: {error}",
                    last_good.display()
                ))
            })?;
        }
    }
    for path in backups.into_iter().chain(staging) {
        remove_promotion_artifact(&path)?;
    }
    Ok(())
}

fn remove_promotion_artifact(path: &Path) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::Io(format!("inspect build promotion artifact: {error}")))?;
    let result = if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|error| {
        AppError::Io(format!(
            "remove build promotion artifact {}: {error}",
            path.display()
        ))
    })
}

fn promote_build_root(staging_root: &Path, build_root: &Path) -> Result<(), AppError> {
    let parent = build_root.parent().ok_or_else(|| {
        AppError::Io(format!("build root {} has no parent", build_root.display()))
    })?;
    let file_name = build_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::Io(format!(
                "build root {} is not valid UTF-8",
                build_root.display()
            ))
        })?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let backup = parent.join(format!(".{file_name}.previous-{stamp}"));
    let had_current = build_root.exists();
    if had_current {
        std::fs::rename(build_root, &backup)
            .map_err(|error| AppError::Io(format!("stage previous build output: {error}")))?;
    }
    if let Err(error) = std::fs::rename(staging_root, build_root) {
        if had_current {
            let _ = std::fs::rename(&backup, build_root);
        }
        return Err(AppError::Io(format!("promote build output: {error}")));
    }
    if had_current {
        if let Err(error) = std::fs::remove_dir_all(&backup) {
            tracing::warn!(error = %error, path = %backup.display(), "could not remove previous build backup");
        }
    }
    Ok(())
}

pub(crate) async fn migrate_manifest_with_approval(
    host: &LocalAppsHostBroker,
    layout: &AppLayout,
    manifest: &AppManifest,
) -> Result<(), AppError> {
    let preview = preview_manifest_migration(layout.clone(), manifest.clone()).await?;
    let allow_destructive = if preview.destructive {
        host.approve_destructive_manifest_migration(manifest.app_id.as_str(), &preview)
            .await
            .map_err(AppError::InvalidRequest)?;
        true
    } else {
        false
    };
    apply_manifest_migration(
        layout.clone(),
        manifest.clone(),
        allow_destructive,
        allow_destructive.then_some(preview),
    )
    .await
}

async fn preview_manifest_migration(
    layout: AppLayout,
    manifest: AppManifest,
) -> Result<local_apps::DataMigrationPreview, AppError> {
    tokio::task::spawn_blocking(move || {
        let store = AppDataStore::open(layout)?;
        store.preview_migration(&manifest)
    })
    .await
    .map_err(|error| AppError::Io(format!("manifest migration worker failed: {error}")))?
}

async fn apply_manifest_migration(
    layout: AppLayout,
    manifest: AppManifest,
    allow_destructive: bool,
    approved_preview: Option<local_apps::DataMigrationPreview>,
) -> Result<(), AppError> {
    tokio::task::spawn_blocking(move || {
        let mut store = AppDataStore::open(layout)?;
        if let Some(approved_preview) = approved_preview {
            let current_preview = store.preview_migration(&manifest)?;
            if current_preview != approved_preview {
                return Err(AppError::WorkflowStateInvalid(
                    "destructive data migration changed while waiting for approval; retry generation"
                        .into(),
                ));
            }
        }
        store.migrate_manifest(&manifest, allow_destructive, now_ms())?;
        Ok::<_, AppError>(())
    })
    .await
    .map_err(|error| AppError::Io(format!("manifest migration worker failed: {error}")))?
}

async fn append_build_log(
    layout: &AppLayout,
    full: bool,
    outcome: &str,
    stdout: &str,
    stderr: &str,
) -> Result<(), AppError> {
    let log_dir = layout.root().join(layout.logs_rel());
    tokio::fs::create_dir_all(&log_dir)
        .await
        .map_err(|error| AppError::Io(format!("create build log directory: {error}")))?;
    let log_path = log_dir.join("build.log");
    let channel = if full { "full" } else { "store" };
    // The outcome rides the banner. Without it the file records what the build
    // PRINTED but never whether it succeeded — and the two device builds that
    // prompted this differed only in that stdout stopped early, which is not
    // something a reader can tell apart from a quiet success.
    let body = format!(
        "\n=== {channel} build ({outcome}) ===\nstdout:\n{}\nstderr:\n{}\n",
        bounded_log(stdout),
        bounded_log(stderr)
    );
    // Keep diagnostics useful without allowing repeated builds to grow the
    // app directory forever. The file is host-owned and can be safely
    // truncated between builds; the current invocation is always retained.
    if let Ok(metadata) = tokio::fs::metadata(&log_path).await {
        if metadata.len().saturating_add(body.len() as u64) > MAX_BUILD_LOG_BYTES {
            tokio::fs::write(&log_path, b"build log rotated\n")
                .await
                .map_err(|error| AppError::Io(format!("rotate build log: {error}")))?;
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .map_err(|error| AppError::Io(format!("open build log: {error}")))?;
    file.write_all(body.as_bytes())
        .await
        .map_err(|error| AppError::Io(format!("write build log: {error}")))
}

pub(crate) fn write_file(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    overwrite: bool,
) -> Result<(), AppError> {
    let path = ensure_safe_file_parent(root, relative)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !overwrite && metadata.is_file() && !metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.file_type().is_symlink() {
            std::fs::remove_file(&path).map_err(|error| {
                AppError::Io(format!(
                    "remove symlinked template file {relative}: {error}"
                ))
            })?;
        } else if metadata.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|error| {
                AppError::Io(format!(
                    "remove directory template path {relative}: {error}"
                ))
            })?;
        } else if !metadata.is_file() {
            std::fs::remove_file(&path).map_err(|error| {
                AppError::Io(format!("remove non-file template path {relative}: {error}"))
            })?;
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Io(format!("template path {relative} has no parent")))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::Io(format!("template path {relative} is not valid UTF-8")))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temp_path = parent.join(format!(".{file_name}.tmp-{stamp}"));
    std::fs::write(&temp_path, bytes)
        .map_err(|error| AppError::Io(format!("write temp template file {relative}: {error}")))?;
    std::fs::rename(&temp_path, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        AppError::Io(format!("replace template file {relative}: {error}"))
    })
}

fn ensure_safe_file_parent(root: &Path, relative: &str) -> Result<PathBuf, AppError> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::InvalidRequest(format!(
            "template path must be a normalized relative path: {relative}"
        )));
    }

    let root_metadata = std::fs::symlink_metadata(root)
        .map_err(|error| AppError::Io(format!("inspect template root: {error}")))?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(AppError::InvalidRequest(format!(
            "template root must be a real directory: {}",
            root.display()
        )));
    }

    let mut current = root.to_path_buf();
    if let Some(parent) = relative_path.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                unreachable!("relative path was validated above")
            };
            current.push(name);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    std::fs::remove_file(&current).map_err(|error| {
                        AppError::Io(format!(
                            "remove unsafe template parent {}: {error}",
                            current.display()
                        ))
                    })?;
                    std::fs::create_dir(&current).map_err(|error| {
                        AppError::Io(format!(
                            "create template parent {}: {error}",
                            current.display()
                        ))
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&current).map_err(|error| {
                        AppError::Io(format!(
                            "create template parent {}: {error}",
                            current.display()
                        ))
                    })?;
                }
                Err(error) => {
                    return Err(AppError::Io(format!(
                        "inspect template parent {}: {error}",
                        current.display()
                    )));
                }
            }
        }
    }
    Ok(root.join(relative_path))
}

#[cfg(test)]
fn write_host_managed_build_files(
    workspace: &Path,
    build_root: &Path,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    let managed = build_locked_files(target);
    let manifest = load_workspace_manifest(workspace)?;
    for (relative, bytes) in locked_files(target)
        .iter()
        .filter(|(relative, _)| managed.contains(relative))
    {
        let expected =
            authoritative_locked_file_bytes(workspace, manifest.as_ref(), relative, bytes)?;
        write_file(build_root, relative, &expected, true)?;
    }
    Ok(())
}

#[cfg(test)]
fn replace_build_source(
    workspace: &Path,
    build_root: &Path,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    if build_root.exists() {
        std::fs::remove_dir_all(build_root)
            .map_err(|error| AppError::Io(format!("clear build directory: {error}")))?;
    }
    std::fs::create_dir_all(build_root)
        .map_err(|error| AppError::Io(format!("create build directory: {error}")))?;
    write_host_managed_build_files(workspace, build_root, target)?;
    copy_workspace_tree(workspace, workspace, build_root, target)
}

#[cfg(test)]
fn copy_workspace_tree(
    workspace: &Path,
    current: &Path,
    destination: &Path,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    for entry in std::fs::read_dir(current)
        .map_err(|error| AppError::Io(format!("read generation source: {error}")))?
    {
        let entry = entry.map_err(|error| AppError::Io(format!("read source entry: {error}")))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(workspace)
            .map_err(|_| AppError::InvalidRequest("build source escaped workspace".into()))?;
        let destination_path = destination.join(relative);
        copy_workspace_entry(workspace, &path, &destination_path, relative, target)?;
    }
    Ok(())
}

#[cfg(test)]
fn copy_workspace_contents(
    workspace: &Path,
    current: &Path,
    destination: &Path,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    for entry in std::fs::read_dir(current)
        .map_err(|error| AppError::Io(format!("read generation source: {error}")))?
    {
        let entry = entry.map_err(|error| AppError::Io(format!("read source entry: {error}")))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(workspace)
            .map_err(|_| AppError::InvalidRequest("build source escaped workspace".into()))?;
        let destination_path = destination.join(entry.file_name());
        copy_workspace_entry(workspace, &path, &destination_path, relative, target)?;
    }
    Ok(())
}

#[cfg(test)]
fn copy_workspace_entry(
    workspace: &Path,
    source: &Path,
    destination: &Path,
    relative: &Path,
    target: LocalAppBuildTarget,
) -> Result<(), AppError> {
    if should_skip_workspace_path(relative, target) {
        return Ok(());
    }
    let kind = std::fs::symlink_metadata(source)
        .map_err(|error| AppError::Io(format!("inspect source entry: {error}")))?;
    if kind.file_type().is_symlink() {
        return Err(AppError::InvalidRequest(format!(
            "source symlink is forbidden: {}",
            relative.display()
        )));
    } else if kind.is_dir() {
        std::fs::create_dir_all(destination)
            .map_err(|error| AppError::Io(format!("create build source directory: {error}")))?;
        copy_workspace_contents(workspace, source, destination, target)?;
    } else if kind.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::Io(format!("create build source parent: {error}")))?;
        }
        std::fs::copy(source, destination)
            .map_err(|error| AppError::Io(format!("copy build source: {error}")))?;
    }
    Ok(())
}

#[cfg(test)]
fn should_skip_workspace_path(relative: &Path, target: LocalAppBuildTarget) -> bool {
    if repinned_host_managed_files(target).contains(&relative.to_string_lossy().as_ref()) {
        return true;
    }
    matches!(
        relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str()),
        Some(".git" | ".lingxi" | ".lingxi-build-state" | "dist" | "node_modules")
    ) || relative == Path::new("LINGXI.md")
}

fn prune_staging_root_for_publish(staging_root: &Path) -> Result<(), AppError> {
    for entry in std::fs::read_dir(staging_root)
        .map_err(|error| AppError::Io(format!("read staged build root: {error}")))?
    {
        let entry =
            entry.map_err(|error| AppError::Io(format!("read staged build entry: {error}")))?;
        if entry.file_name() == VITE_OUTPUT_DIR || entry.file_name() == BUILD_PROVENANCE_FILE {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| AppError::Io(format!("inspect staged publish entry: {error}")))?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path)
                .map_err(|error| AppError::Io(format!("prune staged build directory: {error}")))?;
        } else {
            std::fs::remove_file(&path)
                .map_err(|error| AppError::Io(format!("prune staged build file: {error}")))?;
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Keep both ends of a captured build stream, dropping the middle.
///
/// The tail is the bigger half because a build tool states its DIAGNOSIS last —
/// the failing rule, the stack, the reason it stopped — while the head is a
/// banner followed by, for this scaffold, hundreds of non-fatal
/// `[lightningcss minify] 'host-context'` notes from Ionic's shipped CSS and
/// `[EMPTY_IMPORT_META]` notes caused by the locked `format: "iife"`.
///
/// This used to be `value.chars().take(4_000)`, which kept exactly the useless
/// end. Observed on device: a build died with
/// `[MISSING_EXPORT] "useIonRouter" is not exported by "@ionic/react-router"`
/// and that line was never written ANYWHERE — `build.log` held 4 000 characters
/// of `:host-context` notes, the agent read them, and reported a CSS error that
/// did not exist. Truncating a diagnostic stream from the front discards the
/// diagnosis.
fn bounded(value: &str, head_chars: usize, tail_chars: usize) -> String {
    let total = value.chars().count();
    if total <= head_chars + tail_chars {
        return value.to_string();
    }
    // Char offsets, never byte offsets: this stream carries CJK from the
    // agent's own source, and a byte cut would split a codepoint.
    let head_end = value
        .char_indices()
        .nth(head_chars)
        .map_or(value.len(), |(index, _)| index);
    let tail_start = value
        .char_indices()
        .nth(total - tail_chars)
        .map_or(value.len(), |(index, _)| index);
    let elided = total - head_chars - tail_chars;
    format!(
        "{}\n… [{elided} characters elided from the middle] …\n{}",
        &value[..head_end],
        &value[tail_start..]
    )
}

/// Bound for `build.log`. Generous: this is a FILE, rotated whole at
/// [`MAX_BUILD_LOG_BYTES`], and `LocalAppLogs` serves its tail on demand. A
/// warning the app author caused on an otherwise-green build is only
/// recoverable if it was written down in the first place, and Ionic's own notes
/// alone run past 20 kB.
fn bounded_log(value: &str) -> String {
    bounded(value, 2_000, 40_000)
}

/// Bound for the failure message handed to the model. Tight on purpose: this
/// text enters the agent's context on EVERY failed build, and the reason is
/// always at the tail.
fn bounded_message(value: &str) -> String {
    bounded(value, 1_000, 8_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_r4_bindings_route_to_their_exact_build_assets() {
        for (family, expected) in [
            (AppRuntimeProfile::ReactDom, LocalAppBuildTarget::ReactDomR4),
            (AppRuntimeProfile::Canvas2d, LocalAppBuildTarget::Canvas2dR4),
            (AppRuntimeProfile::Three3d, LocalAppBuildTarget::Three3dR4),
            (AppRuntimeProfile::Phaser2d, LocalAppBuildTarget::Phaser2dR4),
        ] {
            let binding =
                crate::mobile::local_app_runtime_profiles::current_binding_for_family(family)
                    .unwrap();
            let contract = runtime_profile_contract_for_binding(&binding).unwrap();
            let target = LocalAppBuildTarget::from_runtime_binding(&binding).unwrap();
            assert_eq!(target, expected);
            assert_eq!(source_files(target), contract.editable_files);
            assert_eq!(locked_files(target), contract.managed_files);
            let root = tempfile::tempdir().unwrap();
            let layout = AppLayout::new(root.path().join("app"), "aabbccdd").unwrap();
            scaffold_workspace(&layout, target).unwrap();
            assert_eq!(detect_build_target(&layout).unwrap(), target);
            let workspace = layout.root().join(layout.workspace_rel());
            for (path, bytes) in contract.managed_files.iter().chain(contract.editable_files) {
                assert_eq!(
                    std::fs::read(workspace.join(path)).unwrap(),
                    *bytes,
                    "{family} {path}"
                );
            }
        }
    }

    fn pinned_locked_bytes(target: LocalAppBuildTarget, relative: &str) -> &'static [u8] {
        locked_files(target)
            .into_iter()
            .find(|(path, _)| *path == relative)
            .map(|(_, bytes)| bytes)
            .expect("pinned file bytes")
    }

    /// A build tool's DIAGNOSIS is the last thing it prints. Keeping the head
    /// threw it away.
    ///
    /// Shaped like the real device failure: Ionic's CSS emits hundreds of
    /// non-fatal `:host-context` notes, then the build states why it stopped.
    /// The old `chars().take(4_000)` kept only the notes, so `build.log` and the
    /// failure message both described a CSS problem that did not exist while the
    /// actual reason was never recorded anywhere.
    #[test]
    fn bounded_log_keeps_the_reason_a_build_prints_last() {
        let noise = "[lightningcss minify] 'host-context' is not recognized\n".repeat(400);
        let reason = "ERROR: the real reason the build stopped";
        let bounded = bounded_message(&format!("{noise}{reason}"));

        assert!(
            bounded.ends_with(reason),
            "the tail carries the diagnosis; got: {:?}",
            &bounded[bounded.len().saturating_sub(120)..]
        );
        assert!(
            bounded.starts_with("[lightningcss minify]"),
            "the head is still kept for context"
        );
        assert!(
            bounded.contains("characters elided from the middle"),
            "an elision has to be visible, not silent"
        );
    }

    /// Short streams are passed through untouched — no marker, no loss.
    #[test]
    fn bounded_log_leaves_a_short_stream_alone() {
        let short = "vite v8.2.1 building...\n\u{2713} built in 521ms";
        assert_eq!(bounded_log(short), short);
        assert_eq!(bounded_message(short), short);
    }

    /// The FILE keeps far more than the model-facing message.
    ///
    /// A stock build of an Ionic app emits >20 kB of stderr that no author can
    /// act on, and an author's OWN warning can sit anywhere inside it. The file
    /// is rotated whole and read on demand, so it can afford to keep that; the
    /// message rides the agent's context on every failure, so it cannot.
    #[test]
    fn the_log_file_keeps_more_than_the_model_facing_message() {
        let stream = "x".repeat(30_000);
        assert_eq!(
            bounded_log(&stream).chars().count(),
            stream.chars().count(),
            "30k characters is well inside the file budget"
        );
        assert!(
            bounded_message(&stream).chars().count() < 10_000,
            "the message stays tight"
        );
    }

    /// CJK reaches this stream from the agent's own source. Cutting on a byte
    /// boundary would split a codepoint and produce invalid UTF-8.
    #[test]
    fn bounded_log_cuts_on_character_boundaries() {
        let wide = "构建失败".repeat(4_000);
        let bounded = bounded_message(&wide);
        assert!(
            bounded.ends_with("构建失败"),
            "tail must land on a boundary"
        );
        assert!(
            bounded.starts_with("构建失败"),
            "head must land on a boundary"
        );
    }
    use async_trait::async_trait;

    use mobile_linux_api::{guest_paths, map_guest_path_to_host};
    use mobile_linux_api::{
        LinuxCommandResult, MobileLinuxCapability, MobileLinuxError, MobileLinuxRuntimeMode,
        MobileLinuxTaskSnapshot, PtyOpenRequest, PtySessionHandle, PtySize, RootfsState,
        RootfsStatus, SandboxBackend,
    };
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    struct RecordingIsolatedRuntime {
        requests: Mutex<Vec<LinuxCommandRequest>>,
        isolated_runs: AtomicUsize,
    }

    impl RecordingIsolatedRuntime {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                isolated_runs: AtomicUsize::new(0),
            })
        }

        async fn recorded_request(&self) -> LinuxCommandRequest {
            self.requests
                .lock()
                .await
                .last()
                .cloned()
                .expect("recorded request")
        }
    }

    #[async_trait]
    impl MobileLinuxRuntime for RecordingIsolatedRuntime {
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::IosIsh
        }

        fn mode(&self) -> MobileLinuxRuntimeMode {
            MobileLinuxRuntimeMode::MobileLinux
        }

        async fn probe_capability(&self) -> MobileLinuxCapability {
            MobileLinuxCapability {
                available: true,
                backend: self.backend(),
                mode: self.mode(),
                reason: None,
                streaming_output: false,
                background_processes: false,
                pty: false,
                bind_mounts: true,
                rootfs_integrity: false,
            }
        }

        async fn boot(&self) -> Result<RootfsStatus, MobileLinuxError> {
            self.rootfs_status().await
        }

        async fn shutdown(&self) -> Result<(), MobileLinuxError> {
            Ok(())
        }

        async fn run(
            &self,
            _request: LinuxCommandRequest,
        ) -> Result<LinuxCommandResult, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn run_isolated(
            &self,
            request: LinuxCommandRequest,
        ) -> Result<LinuxCommandResult, MobileLinuxError> {
            self.isolated_runs.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().await.push(request.clone());
            let mount = request.mounts.first().ok_or_else(|| {
                MobileLinuxError::InvalidRequest("missing LocalAppBuild mount".into())
            })?;
            let out_dir = request
                .args
                .windows(2)
                .find_map(|window| (window[0] == "--outDir").then(|| window[1].clone()))
                .ok_or_else(|| MobileLinuxError::InvalidRequest("missing --outDir".into()))?;
            let guest_output = format!(
                "{}/{}",
                request
                    .cwd
                    .clone()
                    .unwrap_or_else(|| mount.guest_path.clone()),
                out_dir
            );
            let host_output =
                map_guest_path_to_host(&guest_output, &request.mounts).ok_or_else(|| {
                    MobileLinuxError::InvalidRequest(
                        "outDir is outside the mounted workspace".into(),
                    )
                })?;
            fs::create_dir_all(host_output.join("assets")).map_err(|error| {
                MobileLinuxError::Io(format!("create fake build output: {error}"))
            })?;
            fs::write(host_output.join("index.html"), "<html>fresh</html>").map_err(|error| {
                MobileLinuxError::Io(format!("write fake build output: {error}"))
            })?;
            fs::write(host_output.join("assets/app.js"), "console.log('ok');")
                .map_err(|error| MobileLinuxError::Io(format!("write fake asset: {error}")))?;
            Ok(LinuxCommandResult {
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
                cancelled: false,

                enforcement: mobile_linux_api::LinuxEnforcementReceipt {
                    network_policy_enforced: true,
                    memory_limit_enforced: true,
                },
            })
        }

        async fn spawn_background(
            &self,
            _request: LinuxCommandRequest,
        ) -> Result<mobile_linux_api::LinuxProcessHandle, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn kill(
            &self,

            _handle: &mobile_linux_api::LinuxProcessHandle,
        ) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn open_pty(
            &self,
            _request: PtyOpenRequest,
        ) -> Result<PtySessionHandle, MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn write_pty(
            &self,
            _handle: &PtySessionHandle,
            _input: Vec<u8>,
        ) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn resize_pty(
            &self,
            _handle: &PtySessionHandle,
            _size: PtySize,
        ) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn close_pty(&self, _handle: &PtySessionHandle) -> Result<(), MobileLinuxError> {
            Err(MobileLinuxError::Unsupported)
        }

        async fn rootfs_status(&self) -> Result<RootfsStatus, MobileLinuxError> {
            Ok(RootfsStatus {
                state: RootfsState::Ready,
                backend: self.backend(),
                mode: self.mode(),
                platform: "ios".into(),
                abi: "arm64".into(),
                version: None,
                managed_root: None,
                active_root: None,
                staged_root: None,
                archive_sha256: None,
                installed_size_bytes: None,
                writable_guest_paths: guest_paths::writable_roots()
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                last_error: None,
            })
        }

        async fn verify_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
            self.rootfs_status().await
        }

        async fn repair_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
            self.rootfs_status().await
        }

        async fn reset_rootfs(&self) -> Result<RootfsStatus, MobileLinuxError> {
            self.rootfs_status().await
        }

        async fn configure_mounts(
            &self,

            _mounts: Vec<mobile_linux_api::MountSpec>,
        ) -> Result<(), MobileLinuxError> {
            Ok(())
        }

        async fn list_tasks(&self) -> Result<Vec<MobileLinuxTaskSnapshot>, MobileLinuxError> {
            Ok(Vec::new())
        }
    }

    /// Materialize the two persisted facts `detect_build_target` judges, and
    /// nothing else.
    ///
    /// Deliberately NOT built on `scaffold_workspace`: that helper stamps the
    /// record mirror and the manifest CONSISTENTLY, so it cannot express the
    /// two torn rows this suite exists to pin. Written by hand, each half can
    /// disagree with the other.
    fn write_manifest_unchecked(layout: &AppLayout, manifest: &local_apps::AppManifest) {
        let path = layout.root().join(layout.manifest_rel());
        let mut body = serde_json::to_vec_pretty(manifest).expect("serialize corrupt manifest");
        body.push(b'\n');
        fs::write(path, body).expect("write corrupt manifest fixture");
    }

    fn layout_with(
        root: &Path,
        scaffolded: bool,
        surface: Option<local_apps::AppSurface>,
    ) -> AppLayout {
        let layout = AppLayout::new(root, "aaaa1111").expect("layout");
        layout.initialize().expect("initialize");

        let mut record = local_apps::AppState::create_with_git(
            "aaaa1111".to_string(),
            "Fixture".to_string(),
            "a fixture app".to_string(),
            None,
            false,
            1_700_000_000_000,
        )
        .record;
        record.scaffolded = scaffolded;
        save_record_mirror(&layout, &record).expect("record mirror");

        let mut manifest = local_apps::AppManifest::for_new_app("aaaa1111", "Fixture");
        manifest.surface = surface;
        if scaffolded {
            manifest.runtime_profile = surface.and_then(|surface| {
                let family = match surface {
                    local_apps::AppSurface::Dom => AppRuntimeProfile::ReactDom,
                    local_apps::AppSurface::Canvas => AppRuntimeProfile::Canvas2d,
                };
                crate::mobile::local_app_runtime_profiles::current_binding_for_family(family).ok()
            });
            if let Some(binding) = manifest.runtime_profile.as_ref() {
                manifest.template_origin = Some(local_apps::AppTemplateOrigin {
                    plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
                    plugin_version: "builtin".into(),
                    template_id: format!(
                        "{}-r{}",
                        binding.family.as_str().replace('_', "-"),
                        binding.revision
                    ),
                    template_sha256: binding.contract_sha256.clone(),
                });
                manifest.dependency_snapshot = Some(local_apps::AppDependencySnapshot {
                    requested_sha256: "0".repeat(64),
                    package_sha256: "1".repeat(64),
                    lockfile_sha256: "2".repeat(64),
                    dependency_tree_sha256: "3".repeat(64),
                    sbom_sha256: "4".repeat(64),
                    toolchain_key:
                        crate::mobile::local_app_runtime_profiles::RUNTIME_PROFILE_TOOLCHAIN_KEY
                            .to_string(),
                    verified_profile_contract_sha256: binding.contract_sha256.clone(),
                });
            }
        }
        let manifest_path = layout.root().join(layout.manifest_rel());
        if !scaffolded && surface.is_some() {
            let mut body = serde_json::to_vec_pretty(&manifest).expect("serialize torn manifest");
            body.push(b'\n');
            fs::write(&manifest_path, body).expect("write torn manifest");
        } else {
            local_apps::save_manifest(&layout, &manifest).expect("manifest");
        }
        layout
    }

    /// Build the smallest fully verified app state used by the status
    /// classifier tests. `scaffold_workspace` writes the exact profile and
    /// dependency evidence; this helper only adds a promoted output and its
    /// receipt, which are the final inputs needed for `verified`.
    fn verified_status_fixture(root: &Path) -> (AppLayout, local_apps::AppRecord) {
        let layout = AppLayout::new(root, "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold");
        let record = load_record_mirror(&layout).expect("record mirror");
        let manifest = local_apps::load_manifest(&layout).expect("manifest");
        let build_root = layout.root().join(layout.build_rel(false));
        let output = build_root.join(VITE_OUTPUT_DIR);
        fs::create_dir_all(&output).expect("output");
        fs::write(output.join("index.html"), "<html>verified</html>").expect("index");
        let output_sha256 = digest_tree(&output).expect("output digest");
        write_build_provenance(
            &build_root,
            "status-test-build-key",
            &manifest.runtime_contract_hash().expect("runtime hash"),
            &manifest
                .dependency_snapshot_hash()
                .expect("dependency hash"),
            &output_sha256,
        )
        .expect("build receipt");
        (layout, record)
    }

    #[test]
    fn runtime_profile_status_is_absent_for_a_clean_shell() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), false, None);
        let record = load_record_mirror(&layout).expect("record mirror");
        assert_eq!(derive_runtime_profile_status(root.path(), &record), None);
    }

    #[test]
    fn runtime_profile_status_marks_a_partial_shell_corrupt() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), false, Some(local_apps::AppSurface::Dom));
        let record = load_record_mirror(&layout).expect("record mirror");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::RuntimeContractCorrupt)
        );
    }

    #[test]
    fn runtime_profile_status_precedence_reports_verified_and_each_drift_class() {
        let root = tempfile::tempdir().expect("tempdir");
        let (layout, record) = verified_status_fixture(root.path());
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::Verified)
        );

        // Requested declarations are the only dependency input an agent may
        // change directly, so they report a confirmation-required dirty state.
        let workspace = root.path().join(layout.workspace_rel());
        fs::write(
            workspace.join(REQUESTED_FILE_REL),
            br#"{"dependencies":{"date-fns":"4.0.0"}}"#,
        )
        .expect("requested dependency change");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::DependenciesDirty)
        );

        // Resolved dependency evidence is host-owned; changing it is drift,
        // not a user proposal.
        let (layout, record) = verified_status_fixture(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        fs::write(
            workspace.join(EFFECTIVE_PACKAGE_FILE_REL),
            br#"{"name":"tampered"}"#,
        )
        .expect("tamper effective package");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::CoreDependencyDrift)
        );

        // Profile-managed bytes are also host-owned evidence. The classifier
        // observes the drift and never repairs it on the details path.
        let (layout, record) = verified_status_fixture(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        fs::write(
            workspace.join("vite.config.mjs"),
            b"export default {};" as &[u8],
        )
        .expect("tamper profile file");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::CoreDependencyDrift)
        );
    }

    #[test]
    fn runtime_profile_status_distinguishes_bundle_and_metadata_corruption() {
        let root = tempfile::tempdir().expect("tempdir");
        let (layout, record) = verified_status_fixture(root.path());
        let mut manifest = local_apps::load_manifest(&layout).expect("manifest");
        manifest.surface = None;
        manifest.runtime_profile = None;
        manifest.dependency_snapshot = None;
        write_manifest_unchecked(&layout, &manifest);
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::RuntimeContractCorrupt)
        );

        let root = tempfile::tempdir().expect("tempdir");
        let (layout, record) = verified_status_fixture(root.path());
        let mut manifest = local_apps::load_manifest(&layout).expect("manifest");
        let binding = manifest.runtime_profile.as_mut().expect("binding");
        binding.contract_sha256 = "0".repeat(64);
        manifest
            .dependency_snapshot
            .as_mut()
            .expect("snapshot")
            .verified_profile_contract_sha256 = "0".repeat(64);
        // Must bypass `save_manifest`: it now REFUSES to write a manifest whose
        // `templateOrigin.templateSha256` disagrees with `runtimeProfile
        // .contractSha256` (local-apps/src/manifest.rs). This fixture is
        // deliberately manufacturing exactly that on-disk corruption — which
        // still reaches `derive_runtime_profile_status` via external tampering
        // or bit rot — so it writes the bytes directly, the same way the
        // `RuntimeContractCorrupt` fixture above it does.
        write_manifest_unchecked(&layout, &manifest);
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::RuntimeContractCorrupt)
        );

        let root = tempfile::tempdir().expect("tempdir");
        let (layout, record) = verified_status_fixture(root.path());
        let mut manifest = local_apps::load_manifest(&layout).expect("manifest");
        let binding = manifest.runtime_profile.as_mut().expect("binding");
        binding.family = AppRuntimeProfile::Babylon3d;
        binding.contract_sha256 = "a".repeat(64);
        manifest
            .dependency_snapshot
            .as_mut()
            .expect("snapshot")
            .verified_profile_contract_sha256 = "a".repeat(64);
        manifest.surface = Some(local_apps::AppSurface::Canvas);
        // `templateOrigin.templateSha256` must track `contractSha256` (enforced
        // in local-apps/src/manifest.rs), so move it with the binding. This
        // fixture is probing the UNAVAILABLE-PROFILE edge, not contract
        // corruption: the manifest stays internally consistent so the only
        // defect left for the classifier to find is the missing Babylon bundle.
        manifest
            .template_origin
            .as_mut()
            .expect("template origin")
            .template_sha256 = "a".repeat(64);
        local_apps::save_manifest(&layout, &manifest).expect("gate unavailable profile");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::RuntimeBundleMissing)
        );
    }

    #[test]
    fn runtime_profile_status_reports_rebuild_when_output_receipt_is_missing_or_stale() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold");
        let record = load_record_mirror(&layout).expect("record mirror");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::RebuildRequired)
        );

        let (layout, record) = verified_status_fixture(root.path());
        fs::write(
            layout
                .root()
                .join(layout.build_rel(false))
                .join(BUILD_PROVENANCE_FILE),
            b"{}",
        )
        .expect("stale receipt");
        assert_eq!(
            derive_runtime_profile_status(root.path(), &record),
            Some(AppRuntimeProfileStatus::RebuildRequired)
        );
    }

    #[test]
    fn runtime_profile_status_reports_a_valid_migration_edge_only_after_all_gates() {
        let root = tempfile::tempdir().expect("tempdir");
        let (layout, record) = verified_status_fixture(root.path());
        let binding = local_apps::load_manifest(&layout)
            .expect("manifest")
            .runtime_profile
            .expect("binding");
        let edge = local_apps::RuntimeProfileMigrationEdge {
            family: binding.family,
            from_revision: binding.revision,
            to_revision: binding.revision + 1,
            rebuild_compatible: true,
        };
        assert_eq!(
            derive_runtime_profile_status_with_edges(root.path(), &record, &[edge]),
            Some(AppRuntimeProfileStatus::MigrationAvailable)
        );
    }

    /// The `+` button lands exactly this: a record and a workspace, no shape.
    /// The refusal has to name the tool that gives it one, because the reader is
    /// the agent holding the conversation.
    #[test]
    fn an_unscaffolded_shell_says_to_define_the_surface_first() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), false, None);

        let error = detect_build_target(&layout).expect_err("a shell has nothing to build");
        assert!(
            error.to_string().contains("LocalAppScaffold"),
            "the refusal must name the tool that lands a shape: {error:?}"
        );
        assert!(
            matches!(error, AppError::InvalidRequest(_)),
            "an unformed shell is a legitimate state, not corruption: {error:?}"
        );
    }

    /// The ORDERING nail.
    ///
    /// An implementation that returns `from_surface(s)` as soon as `surface` is
    /// `Some(_)` passes every happy-path test and every shell test above, and
    /// goes green on this row too — by building a workspace whose own record
    /// says it was never given a shape. Only the pair judgement catches it.
    #[test]
    fn an_unscaffolded_record_with_a_surface_does_not_bypass_the_shell_gate() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), false, Some(local_apps::AppSurface::Dom));

        let error = detect_build_target(&layout)
            .expect_err("a torn record must not build, whatever its manifest says");
        assert!(
            matches!(error, AppError::StorageCorrupt(_)),
            "no path in this version writes scaffolded=false with a surface: {error:?}"
        );
        assert!(
            error.to_string().contains("invalid app manifest")
                && error.to_string().contains("runtimeProfile"),
            "the refusal must say the store disagrees with itself: {error:?}"
        );
    }

    /// The other torn row. Distinct from the shell above: the workspace HAS a
    /// seeded source tree, so refusing with "call LocalAppScaffold" would send
    /// the agent to wipe the user's app.
    #[test]
    fn a_scaffolded_record_without_a_surface_is_corruption_not_a_shell() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, None);

        let error = detect_build_target(&layout).expect_err("no scaffold can be inferred");
        assert!(
            matches!(error, AppError::StorageCorrupt(_)),
            "no path in this version writes scaffolded=true without a surface: {error:?}"
        );
        assert!(
            !error.to_string().contains("LocalAppScaffold"),
            "this app already has source; it must not be sent back through the scaffold: {error:?}"
        );
    }

    /// The only buildable row, once per surface. Both are asserted because a
    /// `from_surface` that collapsed to one target would otherwise pass.
    #[test]
    fn a_formed_app_resolves_to_the_target_its_surface_names() {
        let dom_root = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            detect_build_target(&layout_with(
                dom_root.path(),
                true,
                Some(local_apps::AppSurface::Dom)
            ))
            .expect("dom surface"),
            LocalAppBuildTarget::ReactDomR4
        );

        let canvas_root = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            detect_build_target(&layout_with(
                canvas_root.path(),
                true,
                Some(local_apps::AppSurface::Canvas)
            ))
            .expect("canvas surface"),
            LocalAppBuildTarget::Canvas2dR4
        );
    }

    /// The RECORD and the MANIFEST decide the scaffold; the files on disk do
    /// not get a vote.
    ///
    /// Sniffing `vite.config.mjs` is what this used to do, and it was circular:
    /// that file is re-pinned from the compiled-in scaffold before every build,
    /// so a mis-detection would write one scaffold's infrastructure and then
    /// read its own output back as proof it guessed right.
    #[test]
    fn a_stray_vite_marker_cannot_talk_the_builder_out_of_the_recorded_surface() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, Some(local_apps::AppSurface::Canvas));
        let workspace = layout.root().join(layout.workspace_rel());

        fs::write(workspace.join("vite.config.mjs"), "export default {};").expect("Vite marker");
        assert_eq!(
            detect_build_target(&layout).expect("canvas surface survives a Vite marker"),
            LocalAppBuildTarget::Canvas2dR4
        );
    }

    /// The legacy Next rejection precedes the combination judgement — on a
    /// buildable app AND on a shell, because "first" is only meaningful if it
    /// beats a row that would otherwise have answered.
    #[test]
    fn the_legacy_next_marker_is_rejected_before_the_combination_is_read() {
        for (scaffolded, surface) in [(true, Some(local_apps::AppSurface::Dom)), (false, None)] {
            let root = tempfile::tempdir().expect("tempdir");
            let layout = layout_with(root.path(), scaffolded, surface);
            let workspace = layout.root().join(layout.workspace_rel());
            fs::write(workspace.join("next.config.mjs"), "export default {};")
                .expect("Next marker");

            let error =
                detect_build_target(&layout).expect_err("legacy Next marker must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("local apps now support Vite only"),
                "scaffolded={scaffolded}: {error:?}"
            );
        }
    }

    /// The mirror is the only place `scaffolded` is readable from a layout. If
    /// it is gone, "is this a shell?" has no answer — and answering "yes"
    /// would offer to re-scaffold over a formed app's source.
    #[test]
    fn a_missing_record_mirror_is_corruption_rather_than_an_assumed_shell() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, Some(local_apps::AppSurface::Dom));
        fs::remove_file(
            layout
                .root()
                .join(local_apps::storage::metadata_rel(layout.app_id())),
        )
        .expect("remove the mirror");

        let error = detect_build_target(&layout).expect_err("no record, no verdict");
        assert!(
            matches!(error, AppError::StorageCorrupt(_)),
            "a missing mirror must not be read as a shell: {error:?}"
        );
    }

    /// `scaffold_workspace` writes BOTH halves of the pair, so the workspace it
    /// materializes is buildable. Stamping only the manifest — which is what it
    /// used to do — now produces the `false + Some(_)` torn row, so this pins
    /// the two writers together rather than restating the detection rule.
    #[test]
    fn scaffold_workspace_stamps_both_halves_of_the_pair() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::Canvas2dR4).expect("scaffold");

        assert!(
            load_record_mirror(&layout)
                .expect("record mirror")
                .scaffolded,
            "the scaffold is the moment the app gets its shape"
        );
        assert_eq!(
            detect_build_target(&layout).expect("a scaffolded workspace is buildable"),
            LocalAppBuildTarget::Canvas2dR4
        );
    }

    /// `scaffold_workspace` amends the record it finds; it does not replace it.
    /// Overwriting would drop the user's name and brief — the two fields the
    /// conversational create flow spends the whole interview collecting.
    #[test]
    fn scaffold_workspace_keeps_the_record_it_finds_and_only_flips_scaffolded() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), false, None);
        let mut before = load_record_mirror(&layout).expect("record mirror");
        before.name = "Tide Clock".to_string();
        before.brief = "shows the local tide".to_string();
        save_record_mirror(&layout, &before).expect("named record");

        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold");

        let after = load_record_mirror(&layout).expect("record mirror");
        assert_eq!(
            after.name, "Tide Clock",
            "the interviewed name must survive"
        );
        assert_eq!(
            after.brief, "shows the local tide",
            "the interviewed brief must survive"
        );
        assert!(after.scaffolded, "and the shape must now be recorded");
    }

    #[test]
    fn build_source_pins_host_managed_files_and_excludes_workspace_metadata() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let output = root.path().join("build");
        fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("node_modules");
        fs::create_dir_all(workspace.join(".lingxi")).expect("lingxi metadata");
        fs::create_dir_all(workspace.join("lib")).expect("lib");
        fs::create_dir_all(workspace.join("app")).expect("app");
        fs::write(
            workspace.join("node_modules/vite/bin/vite.js"),
            "#!/usr/bin/env node\n",
        )
        .expect("vite cli");
        fs::write(workspace.join(".lingxi/source-policy.json"), "{}").expect("metadata");
        fs::write(workspace.join("LINGXI.md"), "workspace context").expect("lingxi context");
        fs::write(workspace.join("lib/lingxi-bridge.js"), "tampered").expect("bridge");
        fs::write(workspace.join("package.json"), "{\"tampered\":true}").expect("package");
        fs::write(workspace.join("app/main.jsx"), "export default 'custom';").expect("source");

        replace_build_source(&workspace, &output, LocalAppBuildTarget::ReactDomR4)
            .expect("copy source");
        assert_eq!(
            fs::read_to_string(output.join("app/main.jsx")).unwrap(),
            "export default 'custom';"
        );
        assert_eq!(
            fs::read(output.join("package.json")).unwrap(),
            pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "package.json")
        );
        assert_eq!(
            fs::read(output.join("lib/lingxi-bridge.js")).unwrap(),
            pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "lib/lingxi-bridge.js")
        );
        assert!(!output.join("node_modules").exists());
        assert!(!output.join(".lingxi").exists());
        assert!(!output.join("LINGXI.md").exists());
    }

    #[test]
    fn build_source_omits_the_generated_vite_output_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let output = root.path().join("build");
        fs::create_dir_all(workspace.join("dist/assets")).expect("current Vite output");
        fs::write(workspace.join("dist/assets/current.js"), "generated")
            .expect("current output asset");
        fs::create_dir_all(workspace.join("src")).expect("source");
        fs::write(workspace.join("src/main.jsx"), "export default null;").expect("source file");

        replace_build_source(&workspace, &output, LocalAppBuildTarget::ReactDomR4)
            .expect("copy source");

        assert!(output.join("src/main.jsx").is_file());
        assert!(!output.join("dist").exists());
    }

    #[tokio::test]
    async fn dependency_wait_reports_not_ready_when_workspace_vite_is_missing() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");

        let error = await_workspace_dependencies(&workspace, std::time::Duration::from_millis(1))
            .await
            .expect_err("missing workspace node_modules must block the build");

        assert!(
            matches!(error, AppError::NotYetAvailable(_)),
            "unexpected error: {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("workspace/node_modules/vite/bin/vite.js"),
            "{error:?}"
        );
    }

    #[test]
    fn successful_build_promotion_replaces_output_without_deleting_stage_first() {
        let root = tempfile::tempdir().expect("tempdir");
        let build_root = root.path().join("store");
        let staging_root = root.path().join(".store.staging-test");
        fs::create_dir_all(build_root.join(VITE_OUTPUT_DIR)).expect("current output");
        fs::write(build_root.join(VITE_OUTPUT_DIR).join("index.html"), "old").expect("old output");
        fs::create_dir_all(staging_root.join(VITE_OUTPUT_DIR)).expect("staged output");
        fs::write(staging_root.join(VITE_OUTPUT_DIR).join("index.html"), "new")
            .expect("new output");

        promote_build_root(&staging_root, &build_root).expect("promote output");

        assert_eq!(
            fs::read_to_string(build_root.join(VITE_OUTPUT_DIR).join("index.html")).unwrap(),
            "new"
        );
        assert!(!staging_root.exists());
    }

    #[test]
    fn committed_build_success_survives_candidate_cleanup_failure() {
        let result = preserve_committed_build_result(
            Ok(()),
            Err(AppError::Io("candidate cleanup failed".into())),
            "aaaa1111",
            "contract_test",
        );
        assert!(result.is_ok(), "post-commit cleanup cannot undo promotion");
    }

    #[test]
    fn build_key_ignores_managed_files_and_invalidates_changed_source() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, Some(local_apps::AppSurface::Dom));
        let workspace = layout.root().join(layout.workspace_rel());
        fs::create_dir_all(workspace.join("app")).expect("workspace");
        fs::write(workspace.join("app/main.jsx"), "export default 'one';").expect("source");
        let first = workspace_build_key(&layout, &workspace).expect("first key");
        let second = workspace_build_key(&layout, &workspace).expect("reused key");
        assert_eq!(first, second, "unchanged inputs should keep the same key");

        fs::write(workspace.join("vite.config.mjs"), "managed drift").expect("managed file");
        fs::write(workspace.join("LINGXI.md"), "informational mirror").expect("contract mirror");
        let managed_only = workspace_build_key(&layout, &workspace).expect("managed-only key");
        assert_eq!(
            first, managed_only,
            "runtime-managed files and LINGXI.md are represented outside the app-source digest"
        );

        fs::write(workspace.join("app/main.jsx"), "export default 'changed';")
            .expect("changed source");
        let third = workspace_build_key(&layout, &workspace).expect("changed key");
        assert_ne!(first, third, "changed source must invalidate the key");
    }

    #[test]
    fn build_key_ignores_a_forged_stale_input_manifest_and_hashes_current_bytes() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, Some(local_apps::AppSurface::Dom));
        let workspace = layout.root().join(layout.workspace_rel());
        let source_path = workspace.join("app/main.jsx");
        let first_source = "export default 'one';";
        let second_source = "export default 'two';";
        assert_eq!(
            first_source.len(),
            second_source.len(),
            "same-size regression required"
        );
        fs::create_dir_all(source_path.parent().expect("parent")).expect("workspace");
        fs::write(&source_path, first_source).expect("source");
        let first = workspace_build_key(&layout, &workspace).expect("first key");

        let legacy_cache = workspace.join(".lingxi-build-state/input-manifest.json");
        fs::create_dir_all(legacy_cache.parent().expect("build-state parent"))
            .expect("build state");
        fs::write(
            &legacy_cache,
            format!(
                r#"{{
  "files": [
    {{
      "path": "app/main.jsx",
      "size": {},
      "modified_ns": 123,
      "content_sha256": "{:x}"
    }}
  ]
}}"#,
                first_source.len(),
                Sha256::digest(first_source.as_bytes())
            ),
        )
        .expect("legacy manifest");

        fs::write(&source_path, second_source).expect("same-size changed source");
        let second = workspace_build_key(&layout, &workspace).expect("second key");
        assert_ne!(
            first, second,
            "explicit builds must hash current bytes even if a stale input-manifest.json claims the old digest"
        );
    }

    #[test]
    fn build_cache_hit_rejects_tampered_output_even_with_the_same_source_key() {
        let root = tempfile::tempdir().expect("tempdir");
        let build_root = root.path().join("store");
        let output = build_root.join(VITE_OUTPUT_DIR);
        fs::create_dir_all(&output).expect("output");
        fs::write(output.join("index.html"), "<html>good</html>").expect("index");
        fs::write(output.join("assets.js"), "console.log('good');").expect("asset");

        let digest = digest_tree(&output).expect("output digest");
        write_build_provenance(
            &build_root,
            "source-key",
            "runtime",
            "dependencies",
            &digest,
        )
        .expect("provenance");
        assert!(
            build_cache_hit(&build_root, "source-key", "runtime", "dependencies")
                .expect("cache check")
        );

        fs::write(output.join("assets.js"), "console.log('tampered');").expect("tamper");
        assert!(
            !build_cache_hit(&build_root, "source-key", "runtime", "dependencies")
                .expect("tampered cache check")
        );

        fs::write(build_root.join(BUILD_PROVENANCE_FILE), "{}").expect("stale provenance");
        assert!(
            !build_cache_hit(&build_root, "source-key", "runtime", "dependencies")
                .expect("stale cache check")
        );
    }

    #[test]
    fn launch_gate_binds_manifest_contracts_and_promoted_output() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, Some(local_apps::AppSurface::Dom));
        let build_root = layout.root().join(layout.build_rel(false));
        let output = build_root.join(VITE_OUTPUT_DIR);
        fs::create_dir_all(&output).expect("output");
        fs::write(output.join("index.html"), "<html>good</html>").expect("index");
        let manifest = local_apps::load_manifest(&layout).expect("manifest");
        let output_sha256 = digest_tree(&output).expect("output digest");
        write_build_provenance(
            &build_root,
            "source-key",
            &manifest.runtime_contract_hash().expect("runtime hash"),
            &manifest
                .dependency_snapshot_hash()
                .expect("dependency hash"),
            &output_sha256,
        )
        .expect("provenance");

        validate_build_for_launch(&layout).expect("matching receipt must launch");

        fs::write(output.join("index.html"), "<html>tampered</html>").expect("tamper");
        let error = validate_build_for_launch(&layout).expect_err("tampered output must block");
        assert!(error.to_string().contains("rebuild_required"), "{error}");
    }

    #[test]
    fn interrupted_build_promotion_restores_last_good_output_and_cleans_artifacts() {
        let root = tempfile::tempdir().expect("tempdir");
        let build_root = root.path().join("store");
        let backup = root.path().join(".store.previous-123");
        let staging = root.path().join(".store.staging-456");
        fs::create_dir_all(backup.join(VITE_OUTPUT_DIR)).expect("backup output");
        fs::write(backup.join(VITE_OUTPUT_DIR).join("index.html"), "last-good")
            .expect("backup html");
        fs::create_dir_all(staging.join(VITE_OUTPUT_DIR)).expect("staging output");
        fs::write(staging.join(VITE_OUTPUT_DIR).join("index.html"), "partial")
            .expect("staging html");

        recover_build_promotion(&build_root).expect("recover interrupted promotion");

        assert_eq!(
            fs::read_to_string(build_root.join(VITE_OUTPUT_DIR).join("index.html")).unwrap(),
            "last-good"
        );
        assert!(!backup.exists());
        assert!(!staging.exists());
    }

    #[test]
    fn vite_build_uses_the_project_local_cli_path() {
        assert_eq!(
            build_tool_executable("/var/lingxi/local-app-build/aaaa1111/store/project"),
            "/var/lingxi/local-app-build/aaaa1111/store/project/node_modules/vite/bin/vite.js"
        );
    }

    #[test]
    fn prune_staging_root_publishes_only_dist() {
        let root = tempfile::tempdir().expect("tempdir");
        let staging = root.path().join("staging");
        fs::create_dir_all(staging.join("dist/assets")).expect("dist");
        fs::create_dir_all(staging.join("node_modules/vite")).expect("deps");
        fs::create_dir_all(staging.join("app")).expect("app");
        fs::write(staging.join("dist/index.html"), "<html/>").expect("index");
        fs::write(staging.join(BUILD_PROVENANCE_FILE), "receipt").expect("receipt");
        fs::write(staging.join("app/main.jsx"), "export default null;").expect("source");

        prune_staging_root_for_publish(&staging).expect("prune staging");

        assert!(staging.join("dist/index.html").is_file());
        assert_eq!(
            fs::read_to_string(staging.join(BUILD_PROVENANCE_FILE)).unwrap(),
            "receipt"
        );
        assert!(!staging.join("node_modules").exists());
        assert!(!staging.join("app").exists());
    }

    #[test]
    fn provenance_write_failure_before_promotion_preserves_last_good_build() {
        let root = tempfile::tempdir().expect("tempdir");
        let build_root = root.path().join("store");
        let staging = root.path().join("staging");
        fs::create_dir_all(build_root.join(VITE_OUTPUT_DIR)).expect("old dist");
        fs::write(build_root.join(VITE_OUTPUT_DIR).join("index.html"), "old").expect("old output");
        let old_contract = "e".repeat(64);
        write_build_provenance_with_authoring(
            &build_root,
            &"1".repeat(64),
            &"2".repeat(64),
            &"3".repeat(64),
            &"4".repeat(64),
            Some(&old_contract),
        )
        .expect("old receipt");
        fs::create_dir_all(staging.join(VITE_OUTPUT_DIR)).expect("new dist");
        fs::write(staging.join(VITE_OUTPUT_DIR).join("index.html"), "new").expect("new output");
        // A directory at the receipt path makes the pre-promotion receipt
        // write fail after staging only; promotion must not be attempted.
        fs::create_dir_all(staging.join(BUILD_PROVENANCE_FILE)).expect("receipt failure");

        let error = write_build_provenance_with_authoring(
            &staging,
            "build-key",
            &"a".repeat(64),
            &"b".repeat(64),
            &"c".repeat(64),
            Some(&"d".repeat(64)),
        )
        .expect_err("receipt write must fail");
        assert!(
            error.to_string().contains("publish build provenance"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(build_root.join(VITE_OUTPUT_DIR).join("index.html")).unwrap(),
            "old"
        );
        // Read the retained receipt directly: this focused promotion fixture
        // uses root/store rather than a full AppLayout.
        let retained: BuildProvenance = serde_json::from_slice(
            &fs::read(build_root.join(BUILD_PROVENANCE_FILE)).expect("retained old receipt"),
        )
        .expect("parse retained receipt");
        assert_eq!(
            retained.authoring_contract_sha256.as_deref(),
            Some(old_contract.as_str())
        );
        assert!(staging.join(VITE_OUTPUT_DIR).join("index.html").is_file());
    }

    #[test]
    fn active_build_contract_digest_missing_document_fails_closed() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        let build_root = layout.root().join(layout.build_rel(false));
        let digest = "d".repeat(64);
        write_build_provenance_with_authoring(
            &build_root,
            "build-key",
            &"a".repeat(64),
            &"b".repeat(64),
            &"c".repeat(64),
            Some(&digest),
        )
        .expect("receipt");

        let error = active_authoring_contract(&layout)
            .expect_err("missing selected contract must not be treated as no contract");
        assert!(
            matches!(error, AppError::StorageCorrupt(ref message) if message.contains("missing authoring contract")),
            "{error:?}"
        );
    }

    #[test]
    fn fixed_vite_build_forces_the_canonical_dist_output() {
        assert_eq!(
            fixed_vite_build_args(
                2_048,
                "/runtime/vite.js".into(),
                ".lingxi-build-state/build-output/dist".into()
            ),
            [
                "--max-old-space-size=1536",
                "/runtime/vite.js",
                "build",
                "--outDir",
                ".lingxi-build-state/build-output/dist",
                "--emptyOutDir",
                // PINNED, not self-resolved: Vite searches `DEFAULT_CONFIG_FILES`
                // and `vite.config.js` sorts ahead of the `.mjs` the host writes,
                // so an unpinned build would execute a shadow config as Node code.
                "--config",
                "vite.config.mjs",
            ]
        );
    }

    #[test]
    fn local_app_build_mount_uses_the_isolated_project_as_its_command_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let staging = root.path().join("build/.store.staging-123");
        let mount = local_app_build_mount("aaaa1111", "store", &staging);

        assert_eq!(mount.host_path, staging);
        assert_eq!(
            mount.guest_path,
            "/var/lingxi/local-app-build/aaaa1111/store/project"
        );
        assert!(!mount.read_only);
        assert_eq!(mount.purpose, MountPurpose::LocalAppBuild);
    }

    #[test]
    fn scaffold_workspace_materializes_the_pinned_template_in_place() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");

        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");

        let workspace = layout.root().join(layout.workspace_rel());
        assert!(workspace.join(".gitignore").is_file());
        assert!(workspace.join("package.json").is_file());
        assert!(workspace.join("pnpm-lock.yaml").is_file());
        assert!(workspace.join("pnpm-workspace.yaml").is_file());
        assert!(!workspace.join("package-lock.json").exists());
        assert!(workspace.join("index.html").is_file());
        assert!(workspace.join("vite.config.mjs").is_file());
        assert!(workspace.join("jsconfig.json").is_file());
        assert!(workspace.join("app/main.jsx").is_file());
        assert!(workspace.join("app/globals.css").is_file());
        assert!(workspace.join("app/providers.jsx").is_file());
        assert!(workspace.join("app/screens/home-screen.jsx").is_file());
        assert!(workspace.join("app/screens/detail-screen.jsx").is_file());
        assert!(workspace.join("styles/foundation.css").is_file());
        assert!(workspace.join(".lingxi/source-policy.json").is_file());
        let gitignore = fs::read_to_string(workspace.join(".gitignore")).expect("gitignore");
        assert!(gitignore.lines().any(|line| line == "node_modules/"));
        assert!(gitignore.lines().any(|line| line == "dist/"));
        assert!(!gitignore.lines().any(|line| line == "package-lock.json"));
    }

    /// A workspace in the state a SHELL app is in when its interview ends: the
    /// layout is initialized, the service has already written the manifest into
    /// `workspace/.lingxi/`, the guided contract is at the workspace root, and
    /// there is no application source.
    fn shell_layout(root: &Path) -> AppLayout {
        let layout = AppLayout::new(root, "aaaa1111").expect("layout");
        let manifest = local_apps::AppManifest::for_new_app(layout.app_id(), layout.app_id());
        local_apps::save_manifest(&layout, &manifest).expect("save manifest");
        let workspace = layout.root().join(layout.workspace_rel());
        fs::write(workspace.join("LINGXI.md"), "# guided contract").expect("guided contract");
        layout
    }

    /// A workspace that has ALREADY been formed: the seed has landed once, so
    /// everything under `app/` and `src/` is the user's app from here on.
    fn formed_layout(root: &Path) -> AppLayout {
        let layout = shell_layout(root);
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("first scaffold");
        layout
    }

    fn write_workspace_file(workspace: &Path, relative: &str, bytes: &[u8]) {
        let path = workspace.join(relative);
        fs::create_dir_all(path.parent().expect("relative path has a parent"))
            .expect("create parent");
        fs::write(&path, bytes).expect("write workspace file");
    }

    /// §C.0.1. Per-path `overwrite = true` is NOT enough, and this test is
    /// shaped around exactly why: Vite's `DEFAULT_EXTENSIONS` resolves `.js`
    /// BEFORE `.jsx`, the template pins no `resolve.extensions` override, and
    /// every import in it is extensionless. A pre-written `app/app.js` therefore
    /// WINS resolution over the seeded `app/app.jsx`, and `copy_workspace_tree`
    /// carries it into the build root. Overwriting the seed's own nine paths
    /// does nothing about a file at a path the seed does not occupy.
    #[test]
    fn a_first_scaffold_wipes_the_editable_surface_before_seeding() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = shell_layout(root.path());
        let workspace = layout.root().join(layout.workspace_rel());
        // Three pre-written files: two same-path different-extension shadows,
        // and one rogue file at a path the seed never occupies.
        write_workspace_file(
            &workspace,
            "app/app.js",
            b"// shadow that would WIN Vite resolution",
        );
        write_workspace_file(
            &workspace,
            "lib/lingxi-provider.js",
            b"// shadow of a host-managed file",
        );
        write_workspace_file(&workspace, "app/screens/rogue.jsx", b"// not in the seed");
        write_workspace_file(&workspace, "node_modules/.keep", b"");

        scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, true)
            .expect("scaffold");

        assert!(
            !workspace.join("app/app.js").exists(),
            "extension shadow must be gone"
        );
        assert!(
            !workspace.join("lib/lingxi-provider.js").exists(),
            "host-managed shadow must be gone"
        );
        assert!(
            !workspace.join("app/screens/rogue.jsx").exists(),
            "rogue source must be gone"
        );
        assert!(workspace.join(".lingxi").is_dir(), ".lingxi is preserved");
        // Not just the directory — `.lingxi/` is on the seed's own path list
        // (`.lingxi/source-policy.json`), so writing the seed would recreate an
        // EMPTY one. The manifest is the thing that must survive: it carries
        // the `surface` stamp every later build reads back.
        assert!(
            local_apps::load_manifest(&layout).is_ok(),
            ".lingxi/ keeps the service's own documents, not just its name"
        );
        assert!(
            workspace.join("node_modules/.keep").exists(),
            "node_modules is preserved"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("LINGXI.md")).expect("contract"),
            "# guided contract",
            "LINGXI.md is preserved"
        );
        assert!(workspace.join("app/app.jsx").is_file(), "the seed landed");
    }

    #[test]
    fn a_first_scaffold_overwrites_a_pre_written_seed_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = shell_layout(root.path());
        let workspace = layout.root().join(layout.workspace_rel());
        write_workspace_file(
            &workspace,
            "app/screens/home-screen.jsx",
            b"// squatted by the agent",
        );

        scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, true)
            .expect("scaffold");

        let landed = fs::read(workspace.join("app/screens/home-screen.jsx")).expect("read");
        assert_ne!(
            landed,
            b"// squatted by the agent".to_vec(),
            "the seed must win"
        );
    }

    /// The boundary that must not move. A formed app's `app/` and `src/` are the
    /// USER'S work; the only reason wiping a shell is safe is that a shell has
    /// no legitimate application source. Anything that re-pins the scaffold over
    /// an app that already has a shape passes `first_scaffold = false`.
    #[test]
    fn a_repin_never_wipes_a_formed_app() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = formed_layout(root.path());
        let workspace = layout.root().join(layout.workspace_rel());
        write_workspace_file(
            &workspace,
            "app/screens/user-written.jsx",
            b"// the user's own code",
        );
        fs::write(
            workspace.join("app/globals.css"),
            "/* the user's own css */",
        )
        .expect("edit a seed path");

        scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, false)
            .expect("repin");

        assert!(
            workspace.join("app/screens/user-written.jsx").exists(),
            "a formed app's source must survive a repin"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("app/globals.css")).expect("globals"),
            "/* the user's own css */",
            "a repin must not revert an edited seed file either"
        );
    }

    /// A checkpoint taken during the interview would carry the very bytes the
    /// wipe exists to destroy, and `LocalAppCheckpointRestore` would put them
    /// back. `.git` is not in `host_owned_relative`, so it is agent-reachable
    /// and goes with the rest of the editable surface.
    #[test]
    fn a_first_scaffold_wipes_pre_confirmation_history_and_build_state() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = shell_layout(root.path());
        let workspace = layout.root().join(layout.workspace_rel());
        write_workspace_file(
            &workspace,
            ".git/objects/pre-confirmation",
            b"squatted history",
        );
        write_workspace_file(
            &workspace,
            ".lingxi-build-state/build-output/dist/index.html",
            b"stale",
        );

        scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, true)
            .expect("scaffold");

        assert!(
            !workspace.join(".git").exists(),
            "a checkpoint history written before confirmation must be gone"
        );
        assert!(
            !workspace.join(".lingxi-build-state").exists(),
            "build state produced from pre-confirmation source must be gone"
        );
    }

    /// Fail safe: the wipe refuses a path that is not an initialized app
    /// workspace instead of emptying whatever it was handed.
    #[test]
    fn a_first_scaffold_refuses_a_path_that_is_not_an_initialized_workspace() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        let workspace = layout.root().join(layout.workspace_rel());
        fs::create_dir_all(&workspace).expect("bare workspace");
        write_workspace_file(&workspace, "keep-me.txt", b"not ours to delete");

        let error = scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, true)
            .expect_err("an uninitialized workspace must not be wiped");

        assert!(
            format!("{error}").contains("initialized app workspace"),
            "the refusal must name what it checked; got {error}"
        );
        assert!(
            workspace.join("keep-me.txt").exists(),
            "nothing may be deleted once the wipe has refused"
        );
    }

    /// The wipe runs BEFORE the first `write_file`, so it cannot lean on
    /// `ensure_safe_file_parent`'s own symlinked-root check: by the time that
    /// runs, a `remove_dir_all` would already have emptied the symlink target.
    #[cfg(unix)]
    #[test]
    fn a_first_scaffold_refuses_a_symlinked_workspace_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        let workspace = layout.root().join(layout.workspace_rel());
        let outside = root.path().join("outside");
        fs::create_dir_all(outside.join(".lingxi")).expect("outside app state");
        fs::write(outside.join("precious.txt"), "someone else's tree").expect("outside file");
        fs::create_dir_all(workspace.parent().expect("app dir")).expect("app dir");
        std::os::unix::fs::symlink(&outside, &workspace).expect("symlink workspace");

        let error = scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, true)
            .expect_err("a symlinked workspace root must be refused");

        assert!(
            outside.join("precious.txt").exists(),
            "the symlink target must not be emptied; got {error}"
        );
    }

    /// A symlink INSIDE the workspace is removed as a link. Deciding
    /// file-vs-directory from `metadata` instead of `symlink_metadata` would
    /// hand a symlinked directory to `remove_dir_all` and delete a tree the
    /// workspace only points at.
    #[cfg(unix)]
    #[test]
    fn a_first_scaffold_unlinks_a_symlinked_entry_without_following_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = shell_layout(root.path());
        let workspace = layout.root().join(layout.workspace_rel());
        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("precious.txt"), "someone else's tree").expect("outside file");
        std::os::unix::fs::symlink(&outside, workspace.join("vendor")).expect("symlink vendor");

        scaffold_workspace_initialized(&layout, LocalAppBuildTarget::ReactDomR4, true)
            .expect("scaffold");

        assert!(
            fs::symlink_metadata(workspace.join("vendor")).is_err(),
            "the symlink itself must be removed"
        );
        assert!(
            outside.join("precious.txt").exists(),
            "the wipe must not follow a symlink out of the workspace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scaffold_workspace_replaces_host_managed_symlinks_with_regular_files() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        layout.initialize().expect("initialize");
        let workspace = layout.root().join(layout.workspace_rel());
        let outside = root.path().join("outside-package.json");
        fs::write(&outside, "{\"outside\":true}").expect("outside file");
        std::os::unix::fs::symlink(&outside, workspace.join("package.json"))
            .expect("symlink package.json");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");

        let metadata = fs::symlink_metadata(workspace.join("package.json")).expect("metadata");
        assert!(metadata.is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(&outside).expect("outside"),
            "{\"outside\":true}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_replaces_matching_host_managed_symlinks_without_following_them() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let pinned_package = pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "package.json");
        let outside = root.path().join("outside-package.json");
        fs::write(&outside, pinned_package).expect("outside file");
        std::os::unix::fs::symlink(&outside, workspace.join("package.json"))
            .expect("symlink package.json");

        restore_host_managed_files(&workspace, LocalAppBuildTarget::ReactDomR4)
            .expect("restore host-managed files");

        let metadata = fs::symlink_metadata(workspace.join("package.json")).expect("metadata");
        assert!(metadata.is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(
            fs::read(workspace.join("package.json")).unwrap(),
            pinned_package
        );
        assert_eq!(fs::read(&outside).unwrap(), pinned_package);
    }

    #[test]
    fn restore_prefers_app_dependency_snapshot_for_package_and_lock() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        layout.initialize().expect("initialize");
        let workspace = layout.root().join(layout.workspace_rel());
        let binding = crate::mobile::local_app_runtime_profiles::current_binding_for_family(
            AppRuntimeProfile::ReactDom,
        )
        .expect("binding");
        let mut manifest = local_apps::AppManifest::for_new_app("aaaa1111", "Fixture");
        manifest.surface = Some(local_apps::AppSurface::Dom);
        manifest.runtime_profile = Some(binding.clone());
        manifest.template_origin = Some(local_apps::AppTemplateOrigin {
            plugin_id: local_apps::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: format!(
                "{}-r{}",
                binding.family.as_str().replace('_', "-"),
                binding.revision
            ),
            template_sha256: binding.contract_sha256.clone(),
        });
        manifest.dependency_snapshot = Some(local_apps::AppDependencySnapshot {
            requested_sha256: "1".repeat(64),
            package_sha256: "2".repeat(64),
            lockfile_sha256: "3".repeat(64),
            dependency_tree_sha256: "4".repeat(64),
            sbom_sha256: "5".repeat(64),
            toolchain_key: "pnpm@12.5.1/node@26.9.0".into(),
            verified_profile_contract_sha256: binding.contract_sha256.clone(),
        });
        local_apps::save_manifest(&layout, &manifest).expect("manifest");
        write_file(
            &workspace,
            EFFECTIVE_PACKAGE_FILE_REL,
            br#"{ "name": "fixture", "dependencies": { "dayjs": "1.11.13" } }"#,
            true,
        )
        .expect("effective package");
        write_file(
            &workspace,
            LOCKFILE_FILE_REL,
            b"lockfileVersion: '9.0'\nsettings:\n  autoInstallPeers: true\n",
            true,
        )
        .expect("lockfile");
        fs::write(workspace.join("package.json"), br#"{"tampered":true}"#).expect("package");
        fs::write(workspace.join("pnpm-lock.yaml"), b"tampered\n").expect("lock");

        restore_host_managed_files(&workspace, LocalAppBuildTarget::ReactDomR4)
            .expect("restore host-managed files");

        assert_eq!(
            fs::read(workspace.join("package.json")).expect("package.json"),
            fs::read(workspace.join(EFFECTIVE_PACKAGE_FILE_REL)).expect("effective package"),
        );
        assert_eq!(
            fs::read(workspace.join("pnpm-lock.yaml")).expect("pnpm-lock.yaml"),
            fs::read(workspace.join(LOCKFILE_FILE_REL)).expect("snapshot lockfile"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_replaces_symlinked_host_managed_parent_without_writing_outside() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("workspace");
        let outside_lib = root.path().join("outside-lib");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&outside_lib).expect("outside lib");
        let pinned_bridge =
            pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "lib/lingxi-bridge.js");
        fs::write(outside_lib.join("lingxi-bridge.js"), pinned_bridge).expect("outside bridge");
        std::os::unix::fs::symlink(&outside_lib, workspace.join("lib"))
            .expect("symlink lib parent");

        restore_host_managed_files(&workspace, LocalAppBuildTarget::ReactDomR4)
            .expect("restore host-managed files");

        let lib_metadata = fs::symlink_metadata(workspace.join("lib")).expect("lib metadata");
        assert!(lib_metadata.is_dir());
        assert!(!lib_metadata.file_type().is_symlink());
        assert_eq!(
            fs::read(workspace.join("lib/lingxi-bridge.js")).unwrap(),
            pinned_bridge
        );
        assert_eq!(
            fs::read(outside_lib.join("lingxi-bridge.js")).unwrap(),
            pinned_bridge,
            "restoring the workspace must not rewrite a symlink target outside it"
        );
    }

    /// The build still promotes only validated output. If the runtime itself is
    /// unavailable, the live build output must survive untouched.
    #[tokio::test]
    async fn an_unavailable_runtime_fails_before_the_build_root_is_destroyed() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        layout.initialize().expect("initialize");
        let workspace = layout.root().join(layout.workspace_rel());
        fs::write(workspace.join("vite.config.mjs"), "export default {};").expect("Vite marker");

        // The output the preview server is serving right now.
        let build_root = layout.root().join(layout.build_rel(false));
        let served_index = build_root.join(VITE_OUTPUT_DIR).join("index.html");
        fs::create_dir_all(served_index.parent().expect("dist dir")).expect("dist dir");
        fs::write(&served_index, "<html>live</html>").expect("served index");

        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::MockSink::arc(),
            None,
            false,
            None,
        );
        let builder = LocalAppBuilder {
            mobile_linux: None,
            host: broker.as_ref(),
        };

        let error = builder
            .build_workspace(&layout)
            .await
            .expect_err("an unavailable runtime must fail the build");
        assert!(
            matches!(error, AppError::NotYetAvailable(_)),
            "unexpected error: {error:?}"
        );
        assert!(
            served_index.is_file(),
            "the live preview output must survive a build that never started"
        );
    }

    /// `build_workspace` is the enforcement point: host-managed infrastructure
    /// is restored while editable app source survives.
    #[tokio::test]
    async fn the_build_re_pins_host_managed_infrastructure_without_touching_app_owned_files() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");
        let workspace = layout.root().join(layout.workspace_rel());
        let pinned_bridge =
            pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "lib/lingxi-bridge.js");
        let pinned_gitignore = pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, ".gitignore");
        let pinned_package = pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "package.json");
        let pinned_index = pinned_locked_bytes(LocalAppBuildTarget::ReactDomR4, "index.html");

        fs::write(
            workspace.join("lib/lingxi-bridge.js"),
            "export const lingxi = { exfiltrate: true };",
        )
        .expect("tampered bridge");
        fs::write(workspace.join(".gitignore"), b"").expect("tampered gitignore");
        fs::write(
            workspace.join("index.html"),
            b"<!doctype html><div>tampered</div>",
        )
        .expect("index.html");
        fs::write(
            workspace.join("app/main.jsx"),
            "export default 'custom main';",
        )
        .expect("main.jsx");
        fs::write(
            workspace.join("app/globals.css"),
            "body { color: hotpink; }",
        )
        .expect("globals.css");
        fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("node_modules");
        fs::write(
            workspace.join("node_modules/vite/bin/vite.js"),
            "#!/usr/bin/env node\n",
        )
        .expect("workspace vite");

        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::MockSink::arc(),
            None,
            false,
            None,
        );
        let runtime: Arc<dyn MobileLinuxRuntime> = Arc::new(
            mobile_linux_api::UnavailableMobileLinuxRuntime::unavailable(
                mobile_linux_api::SandboxBackend::IosIsh,
                mobile_linux_api::MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "arm64",
                "test runtime never executes node",
            ),
        );
        let builder = LocalAppBuilder {
            mobile_linux: Some(runtime),
            host: broker.as_ref(),
        };
        let mut dependency = local_apps::storage::default_dependency_record("aaaa1111", 1);
        dependency.state = local_apps::AppDependencyState::Ready;

        // The preflight passes, so preparation runs; only `node` itself fails.
        builder
            .build_workspace_locked(&layout, &dependency)
            .await
            .expect_err("the stub runtime cannot run the build tool");

        assert_eq!(
            fs::read(workspace.join("lib/lingxi-bridge.js")).expect("bridge"),
            pinned_bridge,
            "a model-rewritten bridge must be restored before the build"
        );
        assert_eq!(
            fs::read(workspace.join(".gitignore")).expect("gitignore"),
            pinned_gitignore,
            "the dependency and build-output exclusions must be restored before the build"
        );
        assert!(workspace.join(".lingxi/source-policy.json").is_file());
        assert_eq!(
            fs::read(workspace.join("package.json")).expect("package.json"),
            pinned_package,
            "host-managed package.json must be restored before the build"
        );
        assert_eq!(
            fs::read(workspace.join("index.html")).expect("index.html"),
            pinned_index,
            "host-managed index.html must be restored before the build"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("app/main.jsx")).expect("main.jsx"),
            "export default 'custom main';"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("app/globals.css")).expect("globals.css"),
            "body { color: hotpink; }"
        );
    }

    /// This is the MODEL-reachable `dependencies_dirty` copy
    /// (`LocalAppBuild` -> `build_workspace` -> `build_workspace_locked` ->
    /// `validate_dependency_snapshot_files`), not the sibling copies in
    /// `local_apps_host.rs`. Before this test, only the sibling copies had a
    /// behavioural assertion; this one was pinned solely by a doc comment.
    /// A regression that dropped the wording entirely, or that reintroduced
    /// the dead `LocalAppConfirmDependencyChange`/`LocalAppUpdateDependencies`
    /// suggestion, would stay green under every other test in this file.
    #[tokio::test]
    async fn build_workspace_locked_refuses_a_tampered_workspace_package_json_without_naming_a_dead_tool(
    ) {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");
        let workspace = layout.root().join(layout.workspace_rel());

        // A model editing the workspace's dependency-owning file directly,
        // instead of going through the dependency-review receipt flow.
        let tampered = br#"{"name":"tampered","dependencies":{"left-pad":"9.9.9"}}"#.to_vec();
        fs::write(workspace.join("package.json"), &tampered).expect("tamper package.json");

        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::MockSink::arc(),
            None,
            false,
            None,
        );
        let runtime: Arc<dyn MobileLinuxRuntime> = Arc::new(
            mobile_linux_api::UnavailableMobileLinuxRuntime::unavailable(
                mobile_linux_api::SandboxBackend::IosIsh,
                mobile_linux_api::MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "arm64",
                "test runtime never executes node",
            ),
        );
        let builder = LocalAppBuilder {
            mobile_linux: Some(runtime),
            host: broker.as_ref(),
        };
        let mut dependency = local_apps::storage::default_dependency_record("aaaa1111", 1);
        dependency.state = local_apps::AppDependencyState::Ready;

        let error = builder
            .build_workspace_locked(&layout, &dependency)
            .await
            .expect_err(
                "a tampered workspace package.json must refuse the build before it can be masked by restore",
            );
        let message = match error {
            AppError::InvalidRequest(message) => message,
            other => panic!("expected InvalidRequest(dependencies_dirty), got {other:?}"),
        };
        assert!(
            message.contains("dependencies_dirty"),
            "the model-reachable copy must still name the drift: {message}"
        );
        assert!(
            !message.contains("LocalAppConfirmDependencyChange")
                && !message.contains("LocalAppUpdateDependencies"),
            "neither dead tool name may be offered as a retry path: {message}"
        );

        // Vacuity guard: the tampered bytes are still exactly what was
        // written, proving the refusal fired before `restore_host_managed_files`
        // ran (which would have overwritten package.json and hidden the drift).
        assert_eq!(
            fs::read(workspace.join("package.json")).expect("package.json"),
            tampered
        );
    }

    #[tokio::test]
    async fn a_failed_build_cleans_up_its_staging_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");
        let workspace = layout.root().join(layout.workspace_rel());
        fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("node_modules");
        fs::write(
            workspace.join("node_modules/vite/bin/vite.js"),
            "#!/usr/bin/env node\n",
        )
        .expect("workspace vite");

        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::MockSink::arc(),
            None,
            false,
            None,
        );
        let runtime: Arc<dyn MobileLinuxRuntime> = Arc::new(
            mobile_linux_api::UnavailableMobileLinuxRuntime::unavailable(
                mobile_linux_api::SandboxBackend::IosIsh,
                mobile_linux_api::MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "arm64",
                "test runtime never executes node",
            ),
        );
        let builder = LocalAppBuilder {
            mobile_linux: Some(runtime),
            host: broker.as_ref(),
        };

        builder
            .build_workspace(&layout)
            .await
            .expect_err("the stub runtime cannot run the build tool");

        assert!(
            !workspace_build_artifact_root(&workspace).exists(),
            "failed builds must not leave private build artifacts behind"
        );
    }

    #[tokio::test]
    async fn build_runs_from_the_workspace_mount_and_promotes_private_output() {
        for target in [LocalAppBuildTarget::ReactDomR4] {
            let root = tempfile::tempdir().expect("tempdir");
            let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
            scaffold_workspace(&layout, target).expect("scaffold workspace");
            let workspace = layout.root().join(layout.workspace_rel());
            fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("node_modules");
            fs::write(
                workspace.join("node_modules/vite/bin/vite.js"),
                "#!/usr/bin/env node\n",
            )
            .expect("workspace vite");

            let build_root = layout.root().join(layout.build_rel(false));
            fs::create_dir_all(build_root.join(VITE_OUTPUT_DIR)).expect("old dist");
            fs::write(
                build_root.join(VITE_OUTPUT_DIR).join("index.html"),
                "<html>old</html>",
            )
            .expect("old output");

            let runtime = RecordingIsolatedRuntime::new();
            let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
                root.path().to_path_buf(),
                client::adapter::MockSink::arc(),
                Some(runtime.clone() as Arc<dyn MobileLinuxRuntime>),
                false,
                None,
            );
            let builder = LocalAppBuilder {
                mobile_linux: Some(runtime.clone() as Arc<dyn MobileLinuxRuntime>),
                host: broker.as_ref(),
            };
            let mut dependency = local_apps::storage::default_dependency_record("aaaa1111", 1);
            dependency.state = local_apps::AppDependencyState::Ready;

            builder
                .build_workspace_locked(&layout, &dependency)
                .await
                .expect("workspace build");

            let request = runtime.recorded_request().await;
            let toolchain = LocalAppsHostBroker::toolchain_for_layout(&layout).unwrap();
            assert_eq!(request.command, toolchain.node_command());
            assert_eq!(request.env["PATH"], toolchain.path());
            assert_eq!(runtime.isolated_runs.load(Ordering::SeqCst), 1);
            assert_eq!(request.mounts.len(), 1);
            assert_eq!(request.mounts[0].host_path, workspace);
            assert_eq!(
                request.mounts[0].guest_path,
                lingxi_core::host::local_app_paths::local_app_build_project("aaaa1111", "store")
            );
            assert_eq!(
                request.cwd.as_deref(),
                Some(request.mounts[0].guest_path.as_str())
            );
            assert!(request.args.windows(2).any(|window| {
                window[0] == "--outDir" && window[1] == ".lingxi-build-state/build-output/dist"
            }));
            assert!(
                fs::read_to_string(build_root.join(VITE_OUTPUT_DIR).join("index.html"))
                    .expect("promoted index")
                    .contains("fresh")
            );
            assert!(
                !workspace_build_artifact_root(&workspace).exists(),
                "private build output should be consumed by promotion"
            );
        }
    }

    #[tokio::test]
    async fn build_fails_fast_when_fresh_lsp_errors_exist_in_app_managed_js() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");
        let workspace = layout.root().join(layout.workspace_rel());
        fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("node_modules");
        fs::write(
            workspace.join("node_modules/vite/bin/vite.js"),
            "#!/usr/bin/env node\n",
        )
        .expect("workspace vite");

        let diagnostics = lsp::LspDiagnosticRegistry::new();
        let host_path = workspace.join("components/generated-card.jsx");
        fs::create_dir_all(host_path.parent().expect("component parent")).expect("components dir");
        fs::write(
            &host_path,
            "// @ts-check\nexport const GeneratedCard = () => oops;\n",
        )
        .expect("generated source");
        let uri = lsp_types::Url::parse("file:///workspace/aaaa1111/components/generated-card.jsx")
            .expect("uri");
        diagnostics
            .record_document_sync(
                &host_path,
                Path::new("/workspace/aaaa1111/components/generated-card.jsx"),
                uri.clone(),
                Some(3),
                true,
            )
            .await;
        diagnostics
            .publish(
                uri,
                lsp::DiagnosticEntry {
                    version: Some(3),
                    diagnostics: vec![lsp_types::Diagnostic {
                        range: lsp_types::Range::new(
                            lsp_types::Position::new(1, 4),
                            lsp_types::Position::new(1, 5),
                        ),
                        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                        code: Some(lsp_types::NumberOrString::Number(2304)),
                        message: "Cannot find name 'oops'.".into(),
                        ..Default::default()
                    }],
                },
            )
            .await;
        let lsp_registry = Arc::new(
            lsp::LspRegistry::new(Arc::new(platform_posix_minimal::PosixLsp::new()))
                .with_diagnostics(diagnostics),
        );

        let runtime = RecordingIsolatedRuntime::new();
        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::MockSink::arc(),
            Some(runtime.clone() as Arc<dyn MobileLinuxRuntime>),
            false,
            None,
        );
        broker
            .attach_lsp_registry(Arc::downgrade(&lsp_registry))
            .expect("attach lsp");
        let builder = LocalAppBuilder {
            mobile_linux: Some(runtime.clone() as Arc<dyn MobileLinuxRuntime>),
            host: broker.as_ref(),
        };
        let mut dependency = local_apps::storage::default_dependency_record("aaaa1111", 1);
        dependency.state = local_apps::AppDependencyState::Ready;

        let error = builder
            .build_workspace_locked(&layout, &dependency)
            .await
            .expect_err("fresh LSP error must block Vite");
        assert!(matches!(&error, AppError::LspDiagnosticsFailed(_)));
        assert!(
            error.to_string().contains("lsp_diagnostics_failed"),
            "unexpected error: {error}"
        );
        assert_eq!(runtime.isolated_runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stale_lsp_versions_only_degrade_and_do_not_block_build_start() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(root.path(), "aaaa1111").expect("layout");
        scaffold_workspace(&layout, LocalAppBuildTarget::ReactDomR4).expect("scaffold workspace");
        let workspace = layout.root().join(layout.workspace_rel());
        fs::create_dir_all(workspace.join("node_modules/vite/bin")).expect("node_modules");
        fs::write(
            workspace.join("node_modules/vite/bin/vite.js"),
            "#!/usr/bin/env node\n",
        )
        .expect("workspace vite");

        let diagnostics = lsp::LspDiagnosticRegistry::new();
        let host_path = workspace.join("app/main.jsx");
        let uri = lsp_types::Url::parse("file:///workspace/aaaa1111/app/main.jsx").expect("uri");
        diagnostics
            .record_document_sync(
                &host_path,
                Path::new("/workspace/aaaa1111/app/main.jsx"),
                uri.clone(),
                Some(3),
                true,
            )
            .await;
        diagnostics
            .publish(
                uri,
                lsp::DiagnosticEntry {
                    version: Some(2),
                    diagnostics: vec![lsp_types::Diagnostic {
                        range: lsp_types::Range::new(
                            lsp_types::Position::new(0, 0),
                            lsp_types::Position::new(0, 1),
                        ),
                        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                        message: "stale".into(),
                        ..Default::default()
                    }],
                },
            )
            .await;
        let lsp_registry = Arc::new(
            lsp::LspRegistry::new(Arc::new(platform_posix_minimal::PosixLsp::new()))
                .with_diagnostics(diagnostics),
        );

        let runtime: Arc<dyn MobileLinuxRuntime> = Arc::new(
            mobile_linux_api::UnavailableMobileLinuxRuntime::unavailable(
                mobile_linux_api::SandboxBackend::IosIsh,
                mobile_linux_api::MobileLinuxRuntimeMode::MobileLinux,
                "ios",
                "arm64",
                "test runtime never executes node",
            ),
        );
        let broker = crate::mobile::local_apps_wire::broker_with_client_sink(
            root.path().to_path_buf(),
            client::adapter::MockSink::arc(),
            Some(runtime.clone()),
            false,
            None,
        );
        broker
            .attach_lsp_registry(Arc::downgrade(&lsp_registry))
            .expect("attach lsp");
        let builder = LocalAppBuilder {
            mobile_linux: Some(runtime),
            host: broker.as_ref(),
        };
        let mut dependency = local_apps::storage::default_dependency_record("aaaa1111", 1);
        dependency.state = local_apps::AppDependencyState::Ready;

        let error = builder
            .build_workspace_locked(&layout, &dependency)
            .await
            .expect_err("runtime stub should still fail once build starts");
        assert!(
            !error.to_string().contains("lsp_diagnostics_failed"),
            "stale diagnostics should degrade, not block: {error}"
        );
    }

    #[test]
    fn the_locked_bridge_exposes_the_native_wire_contract() {
        let bridge = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/react-dom/r4/lib/lingxi-bridge.js"
        ));
        for anchor in [
            "records[].document",
            "export async function upsertRecord",
            "kind: \"upsert\"",
            "recordId",
            "document",
            "export async function deleteRecord",
            "kind: \"delete\"",
            "export async function requestLlmChat",
            "export async function streamLlmChat",
            "export function onLlmStreamFrame",
            "export async function capturePhoto",
            "export async function pickImage",
            "export async function startRecording",
            "export async function stopRecording",
            "export async function getCurrentLocation",
            "export async function transcribeSpeech",
            "export async function postNotification",
            "export async function getClipboardText",
            "export async function setClipboardText",
            "export async function shareContent",
            "export async function synthesizeSpeech",
            "export async function readFile",
            "export async function writeFile",
            "export async function getDeviceStatus",
            "export async function triggerHaptics",
            "export async function openDeepLink",
            "export async function listCalendarEvents",
            "export async function searchContacts",
            "export async function getMedia",
            "export async function postAgentEvent",
            "export async function createAgentSession",
            "export async function sendAgentTurn",
            "export async function streamAgentTurn",
            "export function onAgentStreamFrame",
            "export async function cancelAgentTurn",
            "export async function proposeAgentProfileUpdate",
            "export async function scheduleBackgroundFlow",
            "export async function listBackgroundTasks",
            "export async function getBackgroundTaskStatus",
            "export async function cancelBackgroundTask",
            "export async function retryBackgroundTask",
        ] {
            assert!(bridge.contains(anchor), "bridge missing {anchor}");
        }
    }

    #[test]
    fn platform_adapter_declares_distinct_phone_and_tablet_presentations() {
        let adapter = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/react-dom/r4/lib/platform-adapter.js"
        ));
        let foundation = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/react-dom/r4/styles/foundation.css"
        ));
        let vite_config = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/react-dom/r4/vite.config.mjs"
        ));
        for marker in [
            "ios:iphone",
            "android:phone",
            "ios:tablet",
            "android:tablet",
            "controlDensity: 44",
            "controlDensity: 48",
            "ionicMode",
            "fontFamily",
        ] {
            assert!(adapter.contains(marker), "adapter missing {marker}");
        }
        assert!(
            vite_config.contains("outDir: \"dist\""),
            "the pinned Vite config must use Vite's official dist output"
        );
        assert!(
            !vite_config.contains("/opt/lingxi/local-app-runtime"),
            "the pinned Vite config must not depend on a shared runtime mount"
        );
        assert!(
            !vite_config.contains("NODE_PATH"),
            "the pinned Vite config must not rely on NODE_PATH"
        );
        // iOS forbids JIT, so iSH runs V8 with `--jitless`, and jitless V8 has
        // no WebAssembly. iSH substitutes `/lib/wasm-polyfill.js`, which is a
        // single-purpose llhttp shim installed as the GLOBAL `WebAssembly`:
        // `compile()` discards the bytes and `instantiate()` always hands back
        // llhttp's exports. Vite's `vite:build-import-analysis` runs the
        // bundled es-module-lexer over each output chunk, reads
        // `exports.__heap_base.value`, gets llhttp instead, and every build on
        // device dies with "Cannot read properties of undefined (reading
        // 'value')" -- after transforming every module successfully.
        //
        // That plugin's generateBundle starts with `if (format !== "es")
        // return;`, which returns BEFORE it awaits the lexer's WASM init. A
        // non-ES output format is therefore the one lever in our control that
        // keeps the build off WebAssembly entirely. Verified against the real
        // polyfill: `es` fails, `iife` succeeds.
        assert!(
            vite_config.contains("format: \"iife\""),
            "the pinned Vite config must emit a non-ES bundle; the device's \
             WebAssembly is an llhttp-only shim and an ES build reaches it"
        );
        assert!(
            vite_config.contains("inlineDynamicImports: true"),
            "a single iife bundle cannot code-split, so dynamic imports must be \
             inlined or the build fails on multiple chunks"
        );
        assert!(foundation.contains("@import \"@ionic/react/css/core.css\""));
        assert!(foundation.contains("--ion-safe-area-top: var(--safe-area-top)"));
        assert!(foundation.contains("--safe-area-top: env(safe-area-inset-top"));
        assert!(foundation.contains("prefers-reduced-motion"));
    }

    /// `.lingxi` holds the service's own state, and `MetadataMirror` rewrites
    /// `app.json` on EVERY persist -- each build minting a fresh
    /// `last_build_id`. Left in the key, a build therefore churns one of its
    /// own inputs and `build_cache_hit` can never hit again.
    ///
    /// The failure is silent: builds stay correct, they are just never cached,
    /// so nothing goes red on device -- it only gets slow. Phase 3's own
    /// acceptance test (two no-op builds => `last_build_id` changes,
    /// `last_output_change_id` does not) passes in this broken state too, which
    /// is why this regression has to gate both phases.
    #[test]
    fn build_key_ignores_service_state_written_under_dot_lingxi() {
        let root = tempfile::tempdir().expect("tempdir");
        let layout = layout_with(root.path(), true, Some(local_apps::AppSurface::Dom));
        let workspace = layout.root().join(layout.workspace_rel());
        fs::create_dir_all(workspace.join("app")).expect("workspace");
        fs::write(workspace.join("app/main.jsx"), "export default 'one';").expect("source");
        fs::write(
            workspace.join(".lingxi/app.json"),
            r#"{"last_build_id":"build-1"}"#,
        )
        .expect("record");
        let first = workspace_build_key(&layout, &workspace).expect("first key");

        // Exactly what a build does to its own record on the way out.
        fs::write(
            workspace.join(".lingxi/app.json"),
            r#"{"last_build_id":"build-2"}"#,
        )
        .expect("rewritten record");
        let second = workspace_build_key(&layout, &workspace).expect("second key");
        assert_eq!(
            first, second,
            "a rewritten .lingxi/app.json must not invalidate the build key -- \
             if it does, build_cache_hit never hits and every build is a full rebuild"
        );

        // The skip must be scoped to service state, not a blanket dotfile
        // amnesty: real source still has to invalidate.
        fs::write(workspace.join("app/main.jsx"), "export default 'two';").expect("changed");
        let third = workspace_build_key(&layout, &workspace).expect("third key");
        assert_ne!(first, third, "changed source must still invalidate the key");
    }

    // `local_app_build_workflow_sets_agree` used to pin
    // `tasks::LOCAL_APP_BUILD_WORKFLOWS` and
    // `tool_workflow::LOCAL_APP_BUILD_WORKFLOWS` byte-for-byte equal because
    // the two crates each kept an independent hand-typed copy of "which
    // workflows build a local app" and shared no natural home to hold ONE
    // copy. P-1.9 deleted both arrays, but NOT symmetrically:
    //
    //  * `tasks` genuinely stopped needing a name list -- its lease and
    //    delete guards read a task's typed `scope::LocalAppWorkflowTaskScope`
    //    (`tasks/src/scope.rs`), and this test was the array's only other
    //    reader in that crate.
    //  * `tool_workflow` does not answer the question at all any more. Phase 9
    //    moved the Local App workflows into the plugin bundle;
    //    `tools/workflow/src/builtins.rs` ships `deep-research` alone and its
    //    `BuiltinWorkflowDescriptor` has exactly four fields
    //    (`name`, `description`, `script`, `manual_only`).
    //
    // The only place that still enumerates the Local App workflow basenames is
    // the scanner's own `local_app_workflow_basenames()`
    // (`apps/harness-runtime::mobile/tests/component_literal_scan.rs`), pinned by
    // `scanner_workflow_needles_survive_the_name_list_deletion`.
    //
    // An earlier revision of this comment claimed `tool_workflow` answers from
    // a typed `BuiltinWorkflowDescriptor` field `is_local_app_build` and
    // pointed at a replacement test
    // `local_app_build_is_the_exact_two_build_workflows`. Neither the field nor
    // that test exists anywhere in this tree; `git grep -P` finds both names
    // only inside prose like this one. Grep before restating it.
}
