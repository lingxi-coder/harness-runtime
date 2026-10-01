use super::dependency_integrity::clone_or_copy_tree;
use super::dependency_integrity::collect_installed_packages;
use super::dependency_integrity::dependency_attestation;
use super::dependency_integrity::dependency_tree_digest;
use super::dependency_integrity::dependency_tree_digest_from_marker;
use super::dependency_integrity::make_dependency_files_read_only;
use super::dependency_integrity::read_verified_dependency_inventory;
use super::dependency_integrity::validate_dependency_tree;
use super::dependency_integrity::write_verified_dependency_inventory;
use super::dependency_integrity::DependencyChange;
use super::dependency_integrity::DependencyChangeKind;
use super::dependency_integrity::DependencyInstallCompletion;
use super::dependency_integrity::DEPENDENCY_SNAPSHOT_READY_FILE;
use super::dependency_integrity::DEPENDENCY_SNAPSHOT_VERSION;
use super::dependency_integrity::MAX_DEPENDENCY_SNAPSHOT_READY_BYTES;
use super::refresh_runtime_profile_snapshot;
use super::required_string;
use super::DependencyBaselineIdentity;
use super::LocalAppPerfDiagnosticTimer;
use super::LocalAppsHostBroker;
use super::RuntimeProfileDependencyAvailability;
use super::BUNDLED_SEED_MANIFEST_FILE;
use super::DEPENDENCY_INSTALL_POLL_INTERVAL;
use super::DEPENDENCY_INSTALL_TIMEOUT;
use super::RUNTIME_SEED_POLL_INTERVAL;
use super::WORKSPACE_DEPENDENCY_ATTESTATION_FILE;
use crate::host::BuildExecutor;
use crate::runtime_profiles::toolchain_for_binding;
use crate::runtime_profiles::RuntimeToolchain;
use local_app_contracts::execution::{
    IsolatedCommand, Mount, MountKind, NetworkPolicy, ResourceLimits,
};
use local_apps::load_manifest;
use local_apps::AppDependencyState;
use local_apps::AppLayout;
use local_apps::AppRuntimeProfile;
use local_apps::AppService;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio::time::Duration;
use tracing::Instrument;

impl LocalAppsHostBroker {
    pub(super) fn app_dependency_marker(layout: &AppLayout) -> PathBuf {
        layout
            .root()
            .join(layout.workspace_rel())
            .join("node_modules/vite/bin/vite.js")
    }
    pub(crate) fn toolchain_for_layout(layout: &AppLayout) -> Result<RuntimeToolchain, String> {
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let binding = manifest.runtime_profile.as_ref().ok_or_else(|| {
            "runtime profile binding is required to select a toolchain".to_string()
        })?;
        toolchain_for_binding(binding).map_err(|error| error.to_string())
    }
    pub(super) fn dependency_store_root(&self, toolchain_key: &str) -> PathBuf {
        let toolchain_key_dir = toolchain_key
            .chars()
            .map(|ch| match ch {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' => ch,
                _ => '_',
            })
            .collect::<String>();
        self.root
            .join("dependency-cache")
            .join("pnpm")
            .join(toolchain_key_dir)
    }
    pub(super) fn dependency_snapshot_root(
        &self,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> PathBuf {
        self.dependency_store_root(toolchain_key)
            .join("snapshots")
            .join(lock_digest)
    }
    pub(super) async fn dependency_snapshot_lock(&self, lock_digest: &str) -> Arc<Mutex<()>> {
        let mut locks = self.dependency_snapshot_locks.lock().await;
        locks
            .entry(lock_digest.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    pub(super) fn workspace_dependencies_ready(layout: &AppLayout) -> Result<bool, String> {
        Self::workspace_dependencies_ready_path(&layout.root().join(layout.workspace_rel()))
    }
    pub(super) fn dependency_lock_digest(layout: &AppLayout) -> Result<String, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let path = workspace.join("pnpm-lock.yaml");
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("read pnpm-lock.yaml {}: {error}", path.display()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
    pub(super) fn dependency_inputs_match(layout: &AppLayout) -> Result<bool, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let expected_files: Vec<(&'static str, Vec<u8>)> = match manifest.runtime_profile.as_ref() {
            Some(binding) => {
                let contract = crate::runtime_profiles::contract_for_binding(binding)
                    .map_err(|error| error.to_string())?;
                let has_snapshot = manifest.dependency_snapshot.is_some();
                contract
                    .managed_files
                    .iter()
                    .chain(
                        contract
                            .editable_files
                            .iter()
                            .filter(|(relative, _)| *relative == "app/mcp-widget/package.json"),
                    )
                    .copied()
                    .filter(|(relative, _)| {
                        matches!(
                            *relative,
                            "package.json"
                                | "pnpm-lock.yaml"
                                | "pnpm-workspace.yaml"
                                | "app/mcp-widget/package.json"
                        )
                    })
                    .map(|(relative, bytes)| {
                        let expected = match (relative, has_snapshot) {
                            ("package.json", true) => std::fs::read(
                                workspace.join(crate::runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL),
                            )
                            .map_err(|error| {
                                format!(
                                    "read {}: {error}",
                                    crate::runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL
                                )
                            })?,
                            ("pnpm-lock.yaml", true) => std::fs::read(
                                workspace.join(crate::runtime_profiles::LOCKFILE_FILE_REL),
                            )
                            .map_err(|error| {
                                format!(
                                    "read {}: {error}",
                                    crate::runtime_profiles::LOCKFILE_FILE_REL
                                )
                            })?,
                            _ => bytes.to_vec(),
                        };
                        Ok((relative, expected))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            }
            None => return Ok(false),
        };
        for (relative, expected) in expected_files {
            let path = workspace.join(relative);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(format!(
                        "inspect dependency input {}: {error}",
                        path.display()
                    ))
                }
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Ok(false);
            }
            if std::fs::read(&path)
                .map_err(|error| format!("read dependency input {}: {error}", path.display()))?
                != expected
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub(super) fn validate_dependency_package_name(package: &str) -> Result<(), String> {
        if package.is_empty() || package.len() > 214 {
            return Err(format!("invalid package name {package:?}"));
        }
        let chars: Vec<char> = package.chars().collect();
        if package.starts_with('@') {
            let slash_count = chars.iter().filter(|&&ch| ch == '/').count();
            if slash_count != 1 {
                return Err(format!(
                    "scoped package name must contain one slash: {package:?}"
                ));
            }
        } else if chars.iter().filter(|&&ch| ch == '/').count() != 0 {
            return Err(format!(
                "unscoped package name must not contain slash: {package:?}"
            ));
        }
        if chars.iter().any(|&ch| {
            !(ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || matches!(ch, '@' | '/' | '.' | '_' | '-'))
        }) {
            return Err(format!(
                "package name {package:?} must use lowercase npm characters only"
            ));
        }
        Ok(())
    }
    pub(super) fn validate_dependency_version(version: &str) -> Result<(), String> {
        let trimmed = version.trim();
        if trimmed.is_empty() {
            return Err("dependency version must not be empty".into());
        }
        let lowered = trimmed.to_ascii_lowercase();
        for forbidden in [
            "file:",
            "link:",
            "portal:",
            "patch:",
            "workspace:",
            "catalog:",
            "catalogs:",
            "npm:",
            "git+",
            "github:",
            "http://",
            "https://",
            "../",
            "./",
            "/",
            "\\",
        ] {
            if lowered.contains(forbidden) {
                return Err(format!(
                    "dependency version {version:?} must resolve from the npm registry only"
                ));
            }
        }
        Ok(())
    }
    pub(super) fn dependency_manifest_bytes(
        contract: &crate::runtime_profiles::RuntimeProfileContract,
    ) -> Result<&'static [u8], String> {
        contract
            .managed_files
            .iter()
            .find(|(relative, _)| *relative == "package.json")
            .map(|(_, bytes)| *bytes)
            .ok_or_else(|| {
                format!(
                    "runtime profile {} r{} is missing package.json",
                    contract.family, contract.revision
                )
            })
    }
    pub(super) fn load_requested_dependency_map(
        workspace: &Path,
    ) -> Result<BTreeMap<String, String>, String> {
        let bytes = Self::read_regular_dependency_input_bytes(
            workspace,
            crate::runtime_profiles::REQUESTED_FILE_REL,
        )?;
        let path = workspace.join(crate::runtime_profiles::REQUESTED_FILE_REL);
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let dependencies = value
            .get("dependencies")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{} must contain an object `dependencies`", path.display()))?;
        let mut map = BTreeMap::new();
        for (package, version) in dependencies {
            let version = version.as_str().ok_or_else(|| {
                format!(
                    "{} dependency {package:?} must map to a string version",
                    path.display()
                )
            })?;
            map.insert(package.clone(), version.to_string());
        }
        Ok(map)
    }
    pub(super) fn read_regular_dependency_input_bytes(
        workspace: &Path,
        relative: &str,
    ) -> Result<Vec<u8>, String> {
        let path = workspace.join(relative);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect {}: {error}", path.display()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(format!(
                "dependencies_dirty: dependency input must be a regular file: {}",
                path.display()
            ));
        }
        std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))
    }
    pub(super) fn load_trusted_dependency_baseline(
        layout: &AppLayout,
        dependency_record: &local_apps::AppDependencyRecord,
    ) -> Result<
        (
            local_apps::AppRuntimeProfileBinding,
            &'static crate::runtime_profiles::RuntimeProfileContract,
            BTreeMap<String, String>,
            DependencyBaselineIdentity,
        ),
        String,
    > {
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        let binding = manifest.runtime_profile.clone().ok_or_else(|| {
            format!(
                "app {} has no runtime profile yet; scaffold it before editing dependencies",
                layout.app_id()
            )
        })?;
        let snapshot = manifest.dependency_snapshot.clone().ok_or_else(|| {
            format!(
                "app {} has no verified dependency snapshot; rerun scaffold or dependency install before editing dependencies",
                layout.app_id()
            )
        })?;
        if snapshot.verified_profile_contract_sha256 != binding.contract_sha256 {
            return Err(format!(
                "runtime_contract_corrupt: app {} dependency snapshot no longer matches runtime profile {}",
                layout.app_id(),
                binding.family
            ));
        }
        if dependency_record.lockfile_sha256.as_deref() != Some(snapshot.lockfile_sha256.as_str())
            || dependency_record.toolchain_key.as_deref() != Some(snapshot.toolchain_key.as_str())
        {
            return Err(format!(
                "dependencies_dirty: app {} dependency record no longer matches the verified snapshot; this cannot be repaired by re-editing package.json/lockfile — report the drift to the user/workflow as a finding instead of retrying",
                layout.app_id()
            ));
        }
        let contract = crate::runtime_profiles::contract_for_binding(&binding)
            .map_err(|error| error.to_string())?;
        let workspace = layout.root().join(layout.workspace_rel());
        let requested_bytes = Self::read_regular_dependency_input_bytes(
            &workspace,
            crate::runtime_profiles::REQUESTED_FILE_REL,
        )?;
        if crate::runtime_profiles::hash_bytes(&requested_bytes) != snapshot.requested_sha256 {
            return Err(format!(
                "dependencies_dirty: app {} requested dependency baseline was modified outside the host-managed dependency flow",
                layout.app_id()
            ));
        }
        let effective_package_bytes = Self::read_regular_dependency_input_bytes(
            &workspace,
            crate::runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
        )?;
        if crate::runtime_profiles::hash_bytes(&effective_package_bytes) != snapshot.package_sha256
        {
            return Err(format!(
                "dependencies_dirty: app {} effective dependency baseline drifted from the verified snapshot",
                layout.app_id()
            ));
        }
        let lockfile_bytes = Self::read_regular_dependency_input_bytes(
            &workspace,
            crate::runtime_profiles::LOCKFILE_FILE_REL,
        )?;
        if crate::runtime_profiles::hash_bytes(&lockfile_bytes) != snapshot.lockfile_sha256 {
            return Err(format!(
                "dependencies_dirty: app {} lockfile baseline drifted from the verified snapshot",
                layout.app_id()
            ));
        }
        let requested_dependencies = Self::load_requested_dependency_map(&workspace)?;
        let expected_package_json =
            Self::build_effective_package_json(contract, &requested_dependencies)?;
        if effective_package_bytes != expected_package_json {
            return Err(format!(
                "dependencies_dirty: app {} package.json no longer matches the committed dependency baseline",
                layout.app_id()
            ));
        }
        let baseline = DependencyBaselineIdentity {
            dependency_snapshot_sha256: manifest
                .dependency_snapshot_hash()
                .map_err(|error| error.to_string())?,
            requested_sha256: snapshot.requested_sha256,
            package_sha256: snapshot.package_sha256,
            lockfile_sha256: snapshot.lockfile_sha256,
            toolchain_key: snapshot.toolchain_key,
            contract_sha256: binding.contract_sha256.clone(),
        };
        Ok((binding, contract, requested_dependencies, baseline))
    }
    pub(super) fn serialize_requested_dependency_map(
        dependencies: &BTreeMap<String, String>,
    ) -> Result<Vec<u8>, String> {
        let dependencies = dependencies
            .iter()
            .map(|(package, version)| (package.clone(), Value::String(version.clone())))
            .collect::<Map<String, Value>>();
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(Map::from_iter([(
            "dependencies".to_string(),
            Value::Object(dependencies),
        )])))
        .map_err(|error| format!("serialize requested dependency manifest: {error}"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
    pub(super) fn build_effective_package_json(
        contract: &crate::runtime_profiles::RuntimeProfileContract,
        requested_dependencies: &BTreeMap<String, String>,
    ) -> Result<Vec<u8>, String> {
        let template = Self::dependency_manifest_bytes(contract)?;
        let mut package_json: Value = serde_json::from_slice(template)
            .map_err(|error| format!("parse runtime profile package.json: {error}"))?;
        let package_object = package_json
            .as_object_mut()
            .ok_or_else(|| "runtime profile package.json must be an object".to_string())?;
        let mut dependencies = contract
            .core_packages
            .iter()
            .map(|(package, version)| {
                (
                    (*package).to_string(),
                    Value::String((*version).to_string()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (package, version) in requested_dependencies {
            dependencies.insert(package.clone(), Value::String(version.clone()));
        }
        package_object.insert(
            "dependencies".to_string(),
            Value::Object(Map::from_iter(dependencies)),
        );
        let mut bytes = serde_json::to_vec_pretty(&package_json)
            .map_err(|error| format!("serialize effective package.json: {error}"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
    pub(super) fn prepare_dependency_change(
        layout: &AppLayout,
        dependency_record: &local_apps::AppDependencyRecord,
        changes_value: &Value,
    ) -> Result<
        (
            local_apps::AppRuntimeProfileBinding,
            DependencyBaselineIdentity,
            Vec<DependencyChange>,
            Vec<u8>,
            Vec<u8>,
        ),
        String,
    > {
        let changes: Vec<DependencyChange> = serde_json::from_value(changes_value.clone())
            .map_err(|error| {
                format!("invalid_argument: changes must be an array of objects: {error}")
            })?;
        if changes.is_empty() {
            return Err("invalid_argument: changes must not be empty".into());
        }
        let (binding, contract, mut requested_dependencies, baseline) =
            Self::load_trusted_dependency_baseline(layout, dependency_record)?;
        let original = requested_dependencies.clone();
        let core_packages = contract
            .core_packages
            .iter()
            .map(|(package, _)| *package)
            .collect::<std::collections::HashSet<_>>();
        for change in &changes {
            Self::validate_dependency_package_name(&change.package)?;
            if core_packages.contains(change.package.as_str()) {
                return Err(format!(
                    "dependency {} is core to runtime profile {} and can only change through runtime profile migration",
                    change.package, binding.family
                ));
            }
            match change.kind {
                DependencyChangeKind::Add | DependencyChangeKind::Update => {
                    let version = change.version.as_deref().ok_or_else(|| {
                        format!(
                            "dependency {} requires a version for {:?}",
                            change.package, change.kind
                        )
                    })?;
                    Self::validate_dependency_version(version)?;
                    requested_dependencies.insert(change.package.clone(), version.to_string());
                }
                DependencyChangeKind::Remove => {
                    if change.version.is_some() {
                        return Err(format!(
                            "dependency {} remove must not include a version",
                            change.package
                        ));
                    }
                    if requested_dependencies.remove(&change.package).is_none() {
                        return Err(format!(
                            "dependency {} is not currently requested by this app",
                            change.package
                        ));
                    }
                }
            }
        }
        if requested_dependencies == original {
            return Err("dependency change makes no observable change".into());
        }
        let requested_json = Self::serialize_requested_dependency_map(&requested_dependencies)?;
        let effective_package_json =
            Self::build_effective_package_json(contract, &requested_dependencies)?;
        Ok((
            binding,
            baseline,
            changes,
            requested_json,
            effective_package_json,
        ))
    }
    pub(super) fn prepare_dependency_staging(layout: &AppLayout) -> Result<PathBuf, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let state_root = workspace.join(".lingxi-build-state");
        let staging = state_root.join("dependency-staging");
        if let Ok(entries) = std::fs::read_dir(&state_root) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("node_modules.previous-")
                {
                    Self::remove_owned_path(&entry.path())?;
                }
            }
        }
        Self::remove_owned_path(&staging)?;
        std::fs::create_dir_all(&staging)
            .map_err(|error| format!("create dependency staging directory: {error}"))?;
        for file in ["package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"] {
            let source = workspace.join(file);
            let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
                format!("inspect dependency input {}: {error}", source.display())
            })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "dependency input must be a regular file: {}",
                    source.display()
                ));
            }
            std::fs::copy(&source, staging.join(file))
                .map_err(|error| format!("stage dependency input {}: {error}", source.display()))?;
        }
        let manifest = load_manifest(layout).map_err(|error| error.to_string())?;
        if let Some(binding) = manifest.runtime_profile.as_ref() {
            let contract = crate::runtime_profiles::contract_for_binding(binding)
                .map_err(|error| error.to_string())?;
            if let Some((relative, bytes)) = contract
                .editable_files
                .iter()
                .find(|(relative, _)| *relative == "app/mcp-widget/package.json")
            {
                // Workspace dependency resolution must see the same widget importer
                // as the frozen lock. Use the pinned profile manifest, never arbitrary
                // editable package metadata as an unapproved dependency request.
                let target = staging.join(relative);
                std::fs::create_dir_all(target.parent().expect("widget manifest parent"))
                    .map_err(|error| format!("create staged widget importer: {error}"))?;
                std::fs::write(target, bytes)
                    .map_err(|error| format!("stage widget importer: {error}"))?;
            }
        }
        Ok(staging)
    }
    pub(super) fn reset_dependency_staging_node_modules(staging: &Path) -> Result<(), String> {
        let node_modules = staging.join("node_modules");
        Self::remove_owned_path(&node_modules)?;
        std::fs::create_dir_all(&node_modules).map_err(|error| {
            format!(
                "recreate dependency staging node_modules {}: {error}",
                node_modules.display()
            )
        })
    }
    pub(super) fn dependency_install_request(
        build_mount: &Mount,
        store_mount: &Mount,
        dependency_staging_guest_path: String,
        build_state_root: &str,
        memory_mb: u32,
        network: NetworkPolicy,
        frozen_lockfile: bool,
        lockfile_only: bool,
        no_runtime: bool,
        toolchain: RuntimeToolchain,
    ) -> IsolatedCommand {
        let mut env = BTreeMap::new();
        env.insert("PATH".into(), toolchain.path().into());
        env.insert("CI".into(), "1".into());
        env.insert("HOME".into(), format!("{build_state_root}/home"));
        env.insert("TMPDIR".into(), format!("{build_state_root}/tmp"));
        env.insert("TMP".into(), format!("{build_state_root}/tmp"));
        env.insert("TEMP".into(), format!("{build_state_root}/tmp"));
        env.insert(
            "XDG_CACHE_HOME".into(),
            format!("{build_state_root}/xdg-cache"),
        );
        env.insert(
            "XDG_CONFIG_HOME".into(),
            format!("{build_state_root}/xdg-config"),
        );
        env.insert(
            "XDG_DATA_HOME".into(),
            format!("{build_state_root}/xdg-data"),
        );
        env.insert("PNPM_HOME".into(), format!("{build_state_root}/pnpm-home"));
        env.insert(
            "COREPACK_HOME".into(),
            format!("{build_state_root}/corepack"),
        );

        let mut args = vec!["install".into()];
        if lockfile_only {
            args.push("--lockfile-only".into());
        }
        args.push(if frozen_lockfile {
            "--frozen-lockfile".into()
        } else {
            "--no-frozen-lockfile".into()
        });
        args.push("--ignore-scripts".into());
        if no_runtime {
            args.push("--no-runtime".into());
        }
        args.extend([
            "--prefer-offline".into(),
            "--store-dir".into(),
            local_app_contracts::guest_paths::LOCAL_APP_DEPENDENCY_STORE.to_string(),
            "--reporter=append-only".into(),
        ]);

        IsolatedCommand {
            command: toolchain.pnpm_command().into(),
            args,
            cwd: Some(dependency_staging_guest_path),
            env,
            timeout_ms: Some(DEPENDENCY_INSTALL_TIMEOUT.as_millis() as u64),
            network,
            limits: ResourceLimits {
                max_memory_mb: Some(memory_mb),
                ..ResourceLimits::default()
            },
            mounts: vec![build_mount.clone(), store_mount.clone()],
        }
    }
    pub(super) async fn run_dependency_install_command(
        runtime: &dyn BuildExecutor,
        request: IsolatedCommand,
    ) -> Result<(), String> {
        let network = request.network;
        let resource_limits = request.limits;
        let frozen_lockfile = request.args.iter().any(|arg| arg == "--frozen-lockfile");
        let install_span = tracing::debug_span!(
            "local_app_dependency_install",
            network = ?network,
            frozen_lockfile = frozen_lockfile,
        );
        let _perf = LocalAppPerfDiagnosticTimer::start(if frozen_lockfile {
            "dependency_pnpm_frozen_install"
        } else {
            "dependency_pnpm_resolve"
        });
        match runtime.run(request).instrument(install_span).await {
            Ok(result) => {
                result
                    .enforcement
                    .ensure_for(network, resource_limits)
                    .map_err(|error| error.to_string())?;
                if result.timed_out || result.cancelled || result.exit_code != 0 {
                    let detail = if !result.stderr.trim().is_empty() {
                        result.stderr
                    } else {
                        result.stdout
                    };
                    Err(format!(
                        "pnpm install failed (exit_code={}, timed_out={}, cancelled={}): {}",
                        result.exit_code,
                        result.timed_out,
                        result.cancelled,
                        detail.chars().take(8_000).collect::<String>()
                    ))
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(format!("dependency install worker failed: {error}")),
        }
    }
    pub(super) fn remove_owned_path(path: &Path) -> Result<(), String> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect owned path {}: {error}", path.display())),
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(path)
                .map_err(|error| format!("remove owned directory {}: {error}", path.display()))
        } else {
            std::fs::remove_file(path)
                .map_err(|error| format!("remove owned file {}: {error}", path.display()))
        }
    }
    pub(super) fn dependency_snapshot_is_ready(
        snapshot_root: &Path,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> Result<bool, String> {
        let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_verify");
        let root_metadata = match std::fs::symlink_metadata(snapshot_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect dependency snapshot {}: {error}",
                    snapshot_root.display()
                ))
            }
        };
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return Ok(false);
        }
        let marker = snapshot_root.join(DEPENDENCY_SNAPSHOT_READY_FILE);
        let marker_metadata = match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect dependency snapshot marker {}: {error}",
                    marker.display()
                ))
            }
        };
        if !marker_metadata.is_file()
            || marker_metadata.file_type().is_symlink()
            || marker_metadata.len() > MAX_DEPENDENCY_SNAPSHOT_READY_BYTES
        {
            return Ok(false);
        }
        let marker_contents = match std::fs::read_to_string(&marker) {
            Ok(contents) => contents,
            Err(_) => return Ok(false),
        };
        let mut marker_lines = marker_contents.lines();
        let expected_version = DEPENDENCY_SNAPSHOT_VERSION.to_string();
        if marker_lines.next() != Some(expected_version.as_str())
            || marker_lines.next() != Some(lock_digest)
            || marker_lines.next() != Some(toolchain_key)
        {
            return Ok(false);
        }
        let Some(expected_tree_digest) = marker_lines.next() else {
            return Ok(false);
        };
        if marker_lines.next().is_some() || expected_tree_digest.is_empty() {
            return Ok(false);
        }
        let node_modules = snapshot_root.join("node_modules");
        let node_modules_metadata = match std::fs::symlink_metadata(&node_modules) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("inspect dependency snapshot node_modules: {error}")),
        };
        if !node_modules_metadata.is_dir() || node_modules_metadata.file_type().is_symlink() {
            return Ok(false);
        }
        // Snapshot directories are read-only by convention, not by an OS
        // sandbox boundary. Re-hash the bytes on every lookup so a tree that
        // changed after an earlier successful lookup cannot inherit stale
        // in-process trust from its marker or path alone.
        if validate_dependency_tree(&node_modules).is_err() {
            return Ok(false);
        }
        let actual_tree_digest = match dependency_tree_digest(&node_modules) {
            Ok(digest) => digest,
            Err(_) => return Ok(false),
        };
        if actual_tree_digest != expected_tree_digest {
            return Ok(false);
        }
        if read_verified_dependency_inventory(
            snapshot_root,
            lock_digest,
            expected_tree_digest,
            toolchain_key,
        )?
        .is_none()
        {
            return Ok(false);
        }
        let vite = node_modules.join("vite/bin/vite.js");
        let vite_metadata = match std::fs::symlink_metadata(&vite) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect dependency snapshot Vite executable: {error}"
                ))
            }
        };
        Ok(vite_metadata.is_file() && !vite_metadata.file_type().is_symlink())
    }
    pub(super) fn workspace_dependencies_match_snapshot(
        workspace: &Path,
        snapshot_root: &Path,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> Result<bool, String> {
        if !Self::workspace_dependencies_ready_path(workspace)?
            || !Self::dependency_snapshot_is_ready(snapshot_root, lock_digest, toolchain_key)?
        {
            return Ok(false);
        }
        let marker = snapshot_root.join(DEPENDENCY_SNAPSHOT_READY_FILE);
        let expected_tree_digest = dependency_tree_digest_from_marker(&marker)?;
        let Some(expected_tree_digest) = expected_tree_digest else {
            return Ok(false);
        };
        let expected_attestation =
            dependency_attestation(lock_digest, &expected_tree_digest, toolchain_key);
        let attestation = workspace.join(WORKSPACE_DEPENDENCY_ATTESTATION_FILE);
        if std::fs::read_to_string(&attestation).ok().as_deref()
            == Some(expected_attestation.as_str())
        {
            return Ok(true);
        }
        let workspace_node_modules = workspace.join("node_modules");
        validate_dependency_tree(&workspace_node_modules)?;
        if dependency_tree_digest(&workspace_node_modules)? != expected_tree_digest {
            return Ok(false);
        }
        crate::app_build::write_file(
            workspace,
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
            expected_attestation.as_bytes(),
            true,
        )
        .map_err(|error| error.to_string())?;
        Ok(true)
    }
    pub(super) fn workspace_dependencies_ready_path(workspace: &Path) -> Result<bool, String> {
        let vite = workspace.join("node_modules/vite/bin/vite.js");
        match std::fs::symlink_metadata(&vite) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "workspace dependency marker is invalid: {} must be a regular file",
                vite.display()
            )),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "inspect workspace dependency marker {}: {error}",
                vite.display()
            )),
        }
    }
    pub(super) fn publish_dependency_snapshot(
        source_node_modules: &Path,
        snapshot_root: &Path,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> Result<(), String> {
        if Self::dependency_snapshot_is_ready(snapshot_root, lock_digest, toolchain_key)? {
            return Ok(());
        }
        match std::fs::symlink_metadata(snapshot_root) {
            Ok(_) => Self::remove_owned_path(snapshot_root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect existing dependency snapshot {}: {error}",
                    snapshot_root.display()
                ))
            }
        }
        let parent = snapshot_root.parent().ok_or_else(|| {
            format!(
                "dependency snapshot has no parent: {}",
                snapshot_root.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create dependency snapshot parent: {error}"))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let staging_root = parent.join(format!(".{lock_digest}.staging-{stamp}"));
        Self::remove_owned_path(&staging_root)?;
        std::fs::create_dir_all(&staging_root)
            .map_err(|error| format!("create dependency snapshot staging: {error}"))?;
        let staged_node_modules = staging_root.join("node_modules");
        let prepare_result = (|| -> Result<(), String> {
            clone_or_copy_tree(source_node_modules, &staged_node_modules)
                .map_err(|error| error.to_string())?;
            if !Self::workspace_dependencies_ready_path(&staging_root)? {
                return Err(format!(
                    "staged Vite executable was not produced at {}",
                    staged_node_modules.join("vite/bin/vite.js").display()
                ));
            }
            validate_dependency_tree(&staged_node_modules)?;
            let tree_digest = dependency_tree_digest(&staged_node_modules)?;
            {
                let _perf =
                    LocalAppPerfDiagnosticTimer::start("dependency_inventory_collect_publish");
                let mut packages = BTreeMap::new();
                collect_installed_packages(&staged_node_modules, &mut packages)?;
                write_verified_dependency_inventory(
                    &staging_root,
                    lock_digest,
                    &tree_digest,
                    &packages,
                    toolchain_key,
                )?;
            }
            std::fs::write(
                staging_root.join(DEPENDENCY_SNAPSHOT_READY_FILE),
                dependency_attestation(lock_digest, &tree_digest, toolchain_key),
            )
            .map_err(|error| format!("write dependency snapshot marker: {error}"))?;
            make_dependency_files_read_only(&staged_node_modules)
                .map_err(|error| format!("protect dependency snapshot: {error}"))
        })();
        if let Err(error) = prepare_result {
            let _ = Self::remove_owned_path(&staging_root);
            return Err(format!("prepare dependency snapshot: {error}"));
        }
        if let Err(error) = std::fs::rename(&staging_root, snapshot_root) {
            let _ = Self::remove_owned_path(&staging_root);
            if snapshot_root.exists()
                && Self::dependency_snapshot_is_ready(snapshot_root, lock_digest, toolchain_key)?
            {
                return Ok(());
            }
            return Err(format!("publish dependency snapshot: {error}"));
        }
        Ok(())
    }
    pub(super) fn materialize_dependency_snapshot(
        snapshot_root: &Path,
        staging_root: &Path,
    ) -> Result<(), String> {
        let destination = staging_root.join("node_modules");
        Self::remove_owned_path(&destination)?;
        clone_or_copy_tree(&snapshot_root.join("node_modules"), &destination)
            .map_err(|error| format!("materialize dependency snapshot: {error}"))
    }
    /// Adopt the read-only dependency tree staged into the app bundle as this
    /// device's snapshot for `lock_digest`.
    ///
    /// `stage-local-app-runtime.py` records `pnpm_lock_sha256` in
    /// `runtime-manifest.json` after validating the tree against the pinned
    /// template lockfile, so the seed carries its own identity and the match is
    /// exact rather than assumed. An app whose lockfile has drifted from the
    /// bundled one gets `false` and falls through to a real install -- the seed
    /// is an accelerator, never an override.
    ///
    /// Publication goes through `publish_dependency_snapshot` rather than
    /// writing an attestation here, so the tree digest and the marker are
    /// produced by the same code that validates every other snapshot.
    pub(super) fn adopt_bundled_dependency_seed(
        runtime_root: &Path,
        lock_digest: &str,
        snapshot_root: &Path,
        toolchain_key: &str,
    ) -> Result<bool, String> {
        if toolchain_key != RuntimeToolchain::Current.key() {
            return Ok(false);
        }
        let manifest_path = runtime_root.join(BUNDLED_SEED_MANIFEST_FILE);
        let manifest = match std::fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            // A build that ships no seed is the ordinary Store configuration,
            // not a fault: fall through to a real install.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "read bundled dependency seed manifest {}: {error}",
                    manifest_path.display()
                ))
            }
        };
        let manifest: Value = serde_json::from_slice(&manifest).map_err(|error| {
            format!(
                "parse bundled dependency seed manifest {}: {error}",
                manifest_path.display()
            )
        })?;
        let seed_digest = manifest
            .get("pnpm_lock_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "bundled dependency seed manifest is missing pnpm_lock_sha256: {}",
                    manifest_path.display()
                )
            })?;
        if seed_digest != lock_digest {
            return Ok(false);
        }
        let seed_node_modules = runtime_root.join("node_modules");
        match std::fs::symlink_metadata(&seed_node_modules) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(format!(
                    "bundled dependency seed is not a directory: {}",
                    seed_node_modules.display()
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect bundled dependency seed {}: {error}",
                    seed_node_modules.display()
                ))
            }
        }
        Self::publish_dependency_snapshot(
            &seed_node_modules,
            snapshot_root,
            lock_digest,
            toolchain_key,
        )?;
        Ok(true)
    }
    pub(super) fn promote_dependency_tree(workspace: &Path, staging: &Path) -> Result<(), String> {
        let staged_node_modules = staging.join("node_modules");
        let marker = staged_node_modules.join("vite/bin/vite.js");
        let marker_metadata = std::fs::symlink_metadata(&marker)
            .map_err(|error| format!("inspect staged Vite executable: {error}"))?;
        if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
            return Err(format!(
                "staged Vite executable is not a regular file: {}",
                marker.display()
            ));
        }
        let current = workspace.join("node_modules");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let backup = workspace
            .join(".lingxi-build-state")
            .join(format!("node_modules.previous-{stamp}"));
        let had_current = std::fs::symlink_metadata(&current).is_ok();
        if had_current {
            let metadata = std::fs::symlink_metadata(&current)
                .map_err(|error| format!("inspect current dependencies: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("workspace node_modules must not be a symlink".into());
            }
            std::fs::rename(&current, &backup)
                .map_err(|error| format!("stage previous dependencies: {error}"))?;
        }
        if let Err(error) = std::fs::rename(&staged_node_modules, &current) {
            if had_current {
                let _ = std::fs::rename(&backup, &current);
            }
            return Err(format!("promote staged dependencies: {error}"));
        }
        // r4-engine-core-05: the rename above already committed the new
        // `node_modules` tree — the install SUCCEEDED. Everything from here
        // is post-commit litter cleanup; propagating either failure via `?`
        // would turn that success into `Err` (and, on the create path, roll
        // back the whole scaffold) while a full, working dependency tree
        // sits in the workspace. Best-effort only: warn and leave the
        // leftovers for a later sweep instead of disowning the promotion.
        if had_current {
            if let Err(error) = Self::remove_owned_path(&backup) {
                tracing::warn!(
                    path = %backup.display(),
                    error = %error,
                    "node_modules backup cleanup deferred after a successful dependency promotion"
                );
            }
        }
        if let Err(error) = Self::remove_owned_path(staging) {
            tracing::warn!(
                path = %staging.display(),
                error = %error,
                "dependency staging cleanup deferred after a successful dependency promotion"
            );
        }
        Ok(())
    }
    pub(super) async fn install_dependencies_value(&self, input: Value) -> Result<Value, String> {
        let app_id = required_string(&input, "app_id")?.to_string();
        let wait = input.get("wait").and_then(Value::as_bool).unwrap_or(false);
        let dependency = self.ensure_dependency_install(&app_id, wait).await?;
        Ok(json!({
            "ok": dependency.state == AppDependencyState::Ready,
            "app_id": app_id,
            "dependencies": dependency,
        }))
    }
    pub(crate) async fn ensure_dependency_install(
        &self,
        app_id: &str,
        wait: bool,
    ) -> Result<local_apps::AppDependencyRecord, String> {
        let service = self.service()?;
        service
            .record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let layout = self.layout(app_id)?;
        let toolchain = Self::toolchain_for_layout(&layout)?;
        let toolchain_key = toolchain.key();
        let workspace = layout.root().join(layout.workspace_rel());
        if !Self::dependency_inputs_match(&layout)? {
            if load_manifest(&layout)
                .map_err(|error| error.to_string())?
                .runtime_profile
                .is_some()
            {
                // The message must not name
                // `LocalAppConfirmDependencyChange`/`LocalAppUpdateDependencies`, but NOT
                // because they are uncallable — an earlier revision of this comment said
                // they "are not in `LOCAL_APP_TOOLS`", which is false: both have rows
                // (`local_apps_tools.rs:112`/`:116`) pinned by
                // `dependency_review_operations_are_wired_as_builtin_tools`. Only their old
                // `mcp__local_apps__*` spelling is refused, and the builtin path is the
                // supported one (`mcp_server.rs`'s
                // `the_mcp_surface_no_longer_serves_the_static_host_operations`).
                //
                // The real reason: neither can repair THIS failure. `update_dependencies`
                // requires a `receipt_id` and claims a host-minted dependency-change
                // receipt (:10087-10107); the drift here is a hand-edited workspace
                // package.json/lockfile that no receipt describes. Naming them would send
                // the agent to a call that cannot fix it, so tell it to report the drift.
                return Err(
                    "dependencies_dirty: workspace package.json or pnpm-lock.yaml differs from the host-owned dependency snapshot; this cannot be repaired by re-editing package.json/lockfile — report the drift to the user/workflow as a finding instead of retrying"
                        .into(),
                );
            } else {
                let target = crate::app_build::detect_build_target(&layout)
                    .map_err(|error| error.to_string())?;
                crate::app_build::restore_host_managed_files(&workspace, target)
                    .map_err(|error| error.to_string())?;
            }
        }
        let dependency = service
            .dependency_record(app_id)
            .await
            .map_err(|error| error.to_string())?;
        let lock_digest = Self::dependency_lock_digest(&layout)?;
        if dependency.state == AppDependencyState::Ready
            && dependency.lockfile_sha256.as_deref() == Some(lock_digest.as_str())
            && dependency.toolchain_key.as_deref() == Some(toolchain_key)
            && Self::workspace_dependencies_match_snapshot(
                &workspace,
                &self.dependency_snapshot_root(&lock_digest, toolchain_key),
                &lock_digest,
                toolchain_key,
            )?
        {
            return Ok(dependency);
        }
        if dependency.state == AppDependencyState::Ready {
            service
                .queue_dependency_install(app_id)
                .await
                .map_err(|error| error.to_string())?;
        }
        let started = match service.start_dependency_install(app_id).await {
            Ok(_) => true,
            Err(local_apps::AppError::InvalidRequest(_)) => false,
            Err(error) => return Err(error.to_string()),
        };
        if started {
            let weak = self.weak_self();
            let app_id = app_id.to_string();
            tokio::spawn(async move {
                let Some(host) = weak.upgrade() else {
                    return;
                };
                host.run_dependency_install(app_id).await;
            });
        }
        if wait {
            return self.wait_for_dependency_install(app_id).await;
        }
        service
            .dependency_record(app_id)
            .await
            .map_err(|error| error.to_string())
    }
    pub(crate) async fn wait_for_dependency_install(
        &self,
        app_id: &str,
    ) -> Result<local_apps::AppDependencyRecord, String> {
        let service = self.service()?;
        timeout(DEPENDENCY_INSTALL_TIMEOUT, async {
            loop {
                let dependency = service
                    .dependency_record(app_id)
                    .await
                    .map_err(|error| error.to_string())?;
                match dependency.state {
                    AppDependencyState::Installing => sleep(DEPENDENCY_INSTALL_POLL_INTERVAL).await,
                    _ => return Ok(dependency),
                }
            }
        })
        .await
        .map_err(|_| {
            format!(
                "workspace dependency installation is still running after {} seconds",
                DEPENDENCY_INSTALL_TIMEOUT.as_secs()
            )
        })?
    }
    pub(super) async fn finalize_dependency_install(
        &self,
        layout: &AppLayout,
        dependency_staging: &Path,
        expected_lock_digest: &str,
    ) -> Result<DependencyInstallCompletion, String> {
        let toolchain = Self::toolchain_for_layout(layout)?;
        let toolchain_key = toolchain.key();
        let workspace = layout.root().join(layout.workspace_rel());
        // The lockfile is host-managed, but re-check it immediately before
        // promotion so a concurrent restore/edit cannot publish a tree built
        // for an older digest into the live workspace.
        let before_promotion = Self::dependency_lock_digest(layout)?;
        if before_promotion != expected_lock_digest {
            return Err("dependency lock changed before promotion".into());
        }
        Self::promote_dependency_tree(&workspace, dependency_staging)?;
        if !Self::workspace_dependencies_ready(layout)? {
            return Err(format!(
                "dependency install finished but {} was not produced",
                Self::app_dependency_marker(layout).display()
            ));
        }
        let actual_lock_digest = Self::dependency_lock_digest(layout)?;
        if actual_lock_digest != expected_lock_digest {
            return Err("dependency lock changed while installation was running".into());
        }
        let snapshot_root = self.dependency_snapshot_root(expected_lock_digest, toolchain_key);
        let snapshot_marker = snapshot_root.join(DEPENDENCY_SNAPSHOT_READY_FILE);
        let tree_digest = dependency_tree_digest_from_marker(&snapshot_marker)?
            .ok_or_else(|| "dependency snapshot marker is malformed".to_string())?;
        let attestation = dependency_attestation(expected_lock_digest, &tree_digest, toolchain_key);
        crate::app_build::write_file(
            &workspace,
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
            attestation.as_bytes(),
            true,
        )
        .map_err(|error| error.to_string())?;
        refresh_runtime_profile_snapshot(
            layout,
            &tree_digest,
            Some((&snapshot_root, expected_lock_digest)),
        )?;
        Ok(DependencyInstallCompletion {
            lockfile_sha256: actual_lock_digest,
            toolchain_key: toolchain_key.to_string(),
        })
    }
    pub(super) async fn dependency_install_once(
        &self,
        layout: &AppLayout,
        app_id: &str,
    ) -> Result<DependencyInstallCompletion, String> {
        let toolchain = Self::toolchain_for_layout(layout)?;
        let toolchain_key = toolchain.key();
        let workspace = layout.root().join(layout.workspace_rel());
        let lock_digest = match Self::dependency_lock_digest(layout) {
            Ok(digest) => digest,
            Err(error) => return Err(error),
        };
        let snapshot_root = self.dependency_snapshot_root(&lock_digest, toolchain_key);
        let snapshot_lock = self.dependency_snapshot_lock(&lock_digest).await;
        let lock_wait_span = tracing::debug_span!(
            "local_app_dependency_snapshot_lock_wait",
            app_id = %app_id,
            lock_digest = %lock_digest,
        );
        let _snapshot_guard = {
            let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_lock_wait");
            snapshot_lock.lock().instrument(lock_wait_span).await
        };
        let dependency_staging = match Self::prepare_dependency_staging(layout) {
            Ok(path) => path,
            Err(error) => return Err(error),
        };
        let mut snapshot_ready =
            match Self::dependency_snapshot_is_ready(&snapshot_root, &lock_digest, toolchain_key) {
                Ok(ready) => ready,
                Err(error) => {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
            };
        if !snapshot_ready {
            // First install on this device: the app bundle already carries a
            // tree resolved from the pinned template lockfile, so adopt it
            // instead of resolving the same 169 packages over the network
            // inside the Linux guest.
            //
            // A seed that cannot be adopted is never fatal. Store builds ship
            // none at all, an app whose lockfile has drifted legitimately needs
            // a real install, and a damaged bundle should degrade to the slow
            // path rather than make app creation impossible -- so failures are
            // recorded and fall through.
            if let Ok(runtime_root) = self.configured_runtime_root() {
                match Self::adopt_bundled_dependency_seed(
                    &runtime_root,
                    &lock_digest,
                    &snapshot_root,
                    toolchain_key,
                ) {
                    Ok(adopted) => snapshot_ready = adopted,
                    Err(error) => {
                        tracing::warn!(app_id = %app_id, error = %error, "bundled dependency seed could not be adopted");
                    }
                }
            }
        }
        if snapshot_ready {
            let snapshot_span = tracing::debug_span!(
                "local_app_dependency_snapshot_materialize",
                app_id = %app_id,
                lock_digest = %lock_digest,
                cache_hit = true,
            );
            let materialize_result = {
                let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_materialize");
                snapshot_span.in_scope(|| {
                    Self::materialize_dependency_snapshot(&snapshot_root, &dependency_staging)
                })
            };
            if let Err(error) = materialize_result {
                let _ = Self::remove_owned_path(&dependency_staging);
                return Err(error);
            }
            return self
                .finalize_dependency_install(layout, &dependency_staging, &lock_digest)
                .await;
        }
        let Some(runtime) = self.build_executor() else {
            let _ = Self::remove_owned_path(&dependency_staging);
            return Err(
                "the mobile Node runtime is unavailable for dependency installation".into(),
            );
        };
        let build_mount = Mount {
            host_path: workspace.clone(),
            guest_path: local_app_contracts::guest_paths::local_app_build_project(app_id, "store"),
            read_only: false,
            kind: MountKind::Project,
        };
        let dependency_store = self.dependency_store_root(toolchain_key);
        if let Err(error) = std::fs::create_dir_all(&dependency_store) {
            let message = format!("create pnpm dependency store: {error}");
            let _ = Self::remove_owned_path(&dependency_staging);
            return Err(message);
        }
        let store_mount = Mount {
            host_path: dependency_store,
            guest_path: local_app_contracts::guest_paths::LOCAL_APP_DEPENDENCY_STORE.to_string(),
            read_only: false,
            kind: MountKind::DependencyStore,
        };
        let project_guest_path = build_mount.guest_path.clone();
        let dependency_staging_guest_path =
            format!("{project_guest_path}/.lingxi-build-state/dependency-staging");
        let build_state_root = format!("{project_guest_path}/.lingxi-build-state");
        let memory_mb = crate::app_build::build_memory_budget_mb(self.physical_memory_bytes());
        let request = Self::dependency_install_request(
            &build_mount,
            &store_mount,
            dependency_staging_guest_path,
            &build_state_root,
            memory_mb,
            NetworkPolicy::Allowed,
            true,
            false,
            true,
            toolchain,
        );
        let outcome = Self::run_dependency_install_command(runtime.as_ref(), request).await;
        match outcome {
            Ok(()) => {
                let snapshot_span = tracing::debug_span!(
                    "local_app_dependency_snapshot_publish",
                    app_id = %app_id,
                    lock_digest = %lock_digest,
                    cache_hit = false,
                );
                let publish_result = {
                    let _perf = LocalAppPerfDiagnosticTimer::start("dependency_snapshot_publish");
                    snapshot_span.in_scope(|| {
                        Self::publish_dependency_snapshot(
                            &dependency_staging.join("node_modules"),
                            &snapshot_root,
                            &lock_digest,
                            toolchain_key,
                        )
                    })
                };
                if let Err(error) = publish_result {
                    let _ = Self::remove_owned_path(&dependency_staging);
                    return Err(error);
                }
                self.finalize_dependency_install(layout, &dependency_staging, &lock_digest)
                    .await
            }
            Err(error) => {
                let _ = Self::remove_owned_path(&dependency_staging);
                Err(error)
            }
        }
    }
    /// Scaffold-landing caller (`land_scaffold`'s commit path): the record's
    /// dependency state is NOT yet `Installing` here, so this wrapper owns
    /// the whole transition itself.
    pub(super) async fn install_scaffold_dependencies(
        &self,
        service: &Arc<AppService>,
        app_id: &str,
        layout: &AppLayout,
    ) -> Result<(), String> {
        service
            .start_dependency_install(app_id)
            .await
            .map_err(|error| error.to_string())?;
        self.run_started_dependency_install(service, app_id, layout)
            .await
    }
    /// Run one dependency-install attempt assuming the record is ALREADY
    /// `Installing` -- e.g. `ensure_dependency_install` transitions it there
    /// itself before spawning `run_dependency_install`. Re-calling
    /// `start_dependency_install` here would always fail (the state is
    /// already `Installing`), leaving the record permanently stuck: this is
    /// the body both callers of the state transition above actually need to
    /// run, split out so each caller owns its own precondition.
    pub(super) async fn run_started_dependency_install(
        &self,
        service: &Arc<AppService>,
        app_id: &str,
        layout: &AppLayout,
    ) -> Result<(), String> {
        match self.dependency_install_once(layout, app_id).await {
            Ok(completion) => service
                .complete_dependency_install_with_metadata(
                    app_id,
                    Some(completion.lockfile_sha256),
                    Some(completion.toolchain_key),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Err(error) => {
                let _ = service.fail_dependency_install(app_id, error.clone()).await;
                Err(error)
            }
        }
    }
    pub(super) async fn run_dependency_install(&self, app_id: String) {
        let service = match self.service() {
            Ok(service) => service,
            Err(error) => {
                tracing::warn!(app_id = %app_id, error = %error, "dependency install lost service");
                return;
            }
        };
        let layout = match self.layout(&app_id) {
            Ok(layout) => layout,
            Err(error) => {
                let _ = service
                    .fail_dependency_install(&app_id, error.clone())
                    .await;
                tracing::warn!(app_id = %app_id, error = %error, "dependency install lost layout");
                return;
            }
        };
        // Serialize dependency mutation with build, checkpoint restore, and
        // physical deletion. The lock is intentionally held across the
        // isolated command so a delete cannot remove the workspace while pnpm
        // is still writing its app-local node_modules tree.
        let _build_lock = match local_apps::storage::lock_app_build(&self.root, &app_id) {
            Ok(lock) => lock,
            Err(error) => {
                let message = error.to_string();
                let _ = service
                    .fail_dependency_install(&app_id, message.clone())
                    .await;
                tracing::warn!(app_id = %app_id, error = %message, "dependency install could not lock app");
                return;
            }
        };
        // `ensure_dependency_install` already transitioned this record to
        // `Installing` before spawning us -- run the install body directly
        // rather than through `install_scaffold_dependencies`, which would
        // try to make that SAME transition again and always fail because the
        // state is already `Installing`, leaving the record stuck forever.
        if let Err(error) = self
            .run_started_dependency_install(&service, &app_id, &layout)
            .await
        {
            // Every other early return out of this worker already lands the
            // record in `Failed` (:4618, :4633). This arm must too: the record
            // is `Installing` when we get here, and the one Err this arm can
            // still receive with the install itself having succeeded is the
            // `complete_dependency_install_with_metadata` persist failure,
            // whose Ok-arm has made no state transition at all. Without this,
            // that window leaves the record terminal `Installing` -- nothing
            // on any boot path sweeps it, so every later `LocalAppBuild`
            // polls the full `DEPENDENCY_INSTALL_TIMEOUT` and fails.
            // Re-failing an already-`Failed` record is an idempotent rewrite
            // (`fail_dependency_install` has no state guard).
            let _ = service
                .fail_dependency_install(&app_id, error.clone())
                .await;
            tracing::warn!(app_id = %app_id, error = %error, "dependency install failed");
        }
    }
    pub(super) fn runtime_seed_ready(root: &Path) -> Result<bool, String> {
        let vite = root.join("node_modules/vite/bin/vite.js");
        let vite_ready = match std::fs::symlink_metadata(&vite) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "verified local-app runtime seed is invalid: {} must be a regular file",
                vite.display()
            )),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "inspect verified local-app runtime seed {}: {error}",
                vite.display()
            )),
        }?;
        if !vite_ready {
            return Ok(false);
        }
        if !Self::runtime_seed_root_is_digest_addressed(root) {
            return Ok(true);
        }
        Self::runtime_seed_ready_marker(root)
    }
    pub(super) fn runtime_seed_root_is_digest_addressed(root: &Path) -> bool {
        root.file_name()
            .and_then(|leaf| leaf.to_str())
            .is_some_and(|leaf| {
                leaf.len() == 64
                    && leaf
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    }
    pub(super) fn runtime_seed_ready_marker(root: &Path) -> Result<bool, String> {
        let Some(digest) = root.file_name().and_then(|leaf| leaf.to_str()) else {
            return Err(format!(
                "verified local-app runtime root {} has no digest leaf",
                root.display()
            ));
        };
        let marker = Self::runtime_seed_marker_path(root, ".ready")?;
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "verified local-app runtime ready marker is invalid: {} must not be a symlink",
                    marker.display()
                ))
            }
            Ok(_) => {
                return Err(format!(
                    "verified local-app runtime ready marker is invalid: {} must be a regular file",
                    marker.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "inspect verified local-app runtime ready marker {}: {error}",
                    marker.display()
                ))
            }
        }
        let mut marker_file = std::fs::File::open(&marker).map_err(|error| {
            format!(
                "open verified local-app runtime ready marker {}: {error}",
                marker.display()
            )
        })?;
        let mut content = Vec::with_capacity(65);
        marker_file
            .by_ref()
            .take(65)
            .read_to_end(&mut content)
            .map_err(|error| {
                format!(
                    "read verified local-app runtime ready marker {}: {error}",
                    marker.display()
                )
            })?;
        if content.len() != digest.len() || content.as_slice() != digest.as_bytes() {
            return Err(format!(
                "verified local-app runtime ready marker is invalid: {} must contain exactly its digest leaf",
                marker.display()
            ));
        }
        Ok(true)
    }
    pub(super) fn runtime_seed_marker_path(root: &Path, suffix: &str) -> Result<PathBuf, String> {
        let digest = root.file_name().ok_or_else(|| {
            format!(
                "verified local-app runtime root {} has no digest leaf",
                root.display()
            )
        })?;
        let parent = root.parent().ok_or_else(|| {
            format!(
                "verified local-app runtime root {} has no parent directory",
                root.display()
            )
        })?;
        let mut marker_name = std::ffi::OsString::from(".");
        marker_name.push(digest);
        marker_name.push(suffix);
        Ok(parent.join(marker_name))
    }
    pub(super) fn runtime_seed_failure_marker(root: &Path) -> Result<Option<PathBuf>, String> {
        let marker = Self::runtime_seed_marker_path(root, ".failed")?;
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                Ok(Some(marker))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "verified local-app runtime failure marker is invalid: {} must not be a symlink",
                marker.display()
            )),
            Ok(_) => Err(format!(
                "verified local-app runtime failure marker is invalid: {} must be a regular file",
                marker.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "inspect verified local-app runtime failure marker {}: {error}",
                marker.display()
            )),
        }
    }
    pub(super) fn read_runtime_seed_failure(root: &Path) -> Result<Option<String>, String> {
        let Some(marker) = Self::runtime_seed_failure_marker(root)? else {
            return Ok(None);
        };
        let reason = std::fs::read_to_string(&marker).map_err(|error| {
            format!(
                "read verified local-app runtime failure marker {}: {error}",
                marker.display()
            )
        })?;
        let detail = reason.trim();
        if detail.is_empty() {
            Ok(Some(format!(
                "verified local-app runtime seed staging failed; see {}",
                marker.display()
            )))
        } else {
            Ok(Some(format!(
                "verified local-app runtime seed staging failed: {detail}"
            )))
        }
    }
    pub async fn await_fixed_runtime_root(
        &self,
        timeout_duration: Duration,
    ) -> Result<PathBuf, String> {
        let root = self.configured_runtime_root()?;
        let start = tokio::time::Instant::now();
        loop {
            match Self::runtime_seed_ready(&root)? {
                true => return Ok(root.clone()),
                false => {
                    if let Some(failure) = Self::read_runtime_seed_failure(&root)? {
                        return Err(failure);
                    }
                }
            }
            if start.elapsed() >= timeout_duration {
                return Err(format!(
                    "local-app runtime root is configured at {}, but the verified runtime seed is not ready yet; waited {} ms for node_modules/vite/bin/vite.js",
                    root.display(),
                    timeout_duration.as_millis()
                ));
            }
            sleep(RUNTIME_SEED_POLL_INTERVAL).await;
        }
    }
    pub(super) fn read_json_object(path: &Path) -> Option<Value> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let value: Value = serde_json::from_slice(bytes.as_slice()).ok()?;
        let _ = value.as_object()?;
        Some(value)
    }
    /// Whether this host already has a verified dependency tree for the exact
    /// profile lock, either in the shared cache or in the configured bundled
    /// runtime seed. Read-only: unlike `adopt_bundled_dependency_seed`, this
    /// helper never publishes a cache entry.
    pub(super) fn runtime_profile_dependency_availability(
        &self,
        family: AppRuntimeProfile,
        revision: u32,
    ) -> RuntimeProfileDependencyAvailability {
        self.runtime_profile_dependency_availability_cached(family, revision, &mut HashMap::new())
    }
    pub(super) fn runtime_profile_dependency_availability_cached(
        &self,
        family: AppRuntimeProfile,
        revision: u32,
        availability_by_lock: &mut HashMap<String, RuntimeProfileDependencyAvailability>,
    ) -> RuntimeProfileDependencyAvailability {
        let Ok(binding) = crate::runtime_profiles::current_binding_for_family(family) else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        if binding.revision != revision {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        }
        let Ok(contract) = crate::runtime_profiles::contract_for_binding(&binding) else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        let toolchain_key = contract.toolchain_key;
        let lock_digest = crate::runtime_profiles::lockfile_sha256(contract);
        if let Some(availability) = availability_by_lock.get(&lock_digest) {
            return *availability;
        }
        let availability =
            self.runtime_profile_dependency_availability_for_lock(&lock_digest, toolchain_key);
        availability_by_lock.insert(lock_digest, availability);
        availability
    }
    pub(super) fn runtime_profile_dependency_availability_for_lock(
        &self,
        lock_digest: &str,
        toolchain_key: &str,
    ) -> RuntimeProfileDependencyAvailability {
        let snapshot_root = self.dependency_snapshot_root(lock_digest, toolchain_key);
        if Self::dependency_snapshot_is_ready(&snapshot_root, lock_digest, toolchain_key)
            .unwrap_or(false)
        {
            return RuntimeProfileDependencyAvailability::Cached;
        }
        let Ok(runtime_root) = self.configured_runtime_root() else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        let Some(manifest) = Self::read_json_object(&runtime_root.join(BUNDLED_SEED_MANIFEST_FILE))
        else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        if manifest.get("pnpm_lock_sha256").and_then(Value::as_str) != Some(lock_digest) {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        }
        let Ok(metadata) = std::fs::symlink_metadata(runtime_root.join("node_modules")) else {
            return RuntimeProfileDependencyAvailability::DownloadRequired;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            RuntimeProfileDependencyAvailability::Bundled
        } else {
            RuntimeProfileDependencyAvailability::DownloadRequired
        }
    }
}
