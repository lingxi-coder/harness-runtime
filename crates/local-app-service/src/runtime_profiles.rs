use local_apps::{
    AppDependencySnapshot, AppError, AppRuntimeProfile, AppRuntimeProfileBinding, AppSurface,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const RUNTIME_PROFILE_TOOLCHAIN_KEY: &str = "pnpm@12.5.1/node@26.9.0";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeToolchain {
    Current,
}

impl RuntimeToolchain {
    pub fn key(self) -> &'static str {
        match self {
            Self::Current => RUNTIME_PROFILE_TOOLCHAIN_KEY,
        }
    }
    pub fn node_command(self) -> &'static str {
        match self {
            Self::Current => "/usr/bin/node",
        }
    }
    pub fn pnpm_command(self) -> &'static str {
        match self {
            Self::Current => "/usr/bin/pnpm",
        }
    }
    pub fn path(self) -> &'static str {
        match self {
            Self::Current => "/usr/bin:/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/sbin",
        }
    }
}

pub fn toolchain_for_binding(
    binding: &AppRuntimeProfileBinding,
) -> Result<RuntimeToolchain, AppError> {
    let contract = contract_for_binding(binding)?;
    match contract.toolchain_key {
        RUNTIME_PROFILE_TOOLCHAIN_KEY => Ok(RuntimeToolchain::Current),
        _ => Err(AppError::StorageCorrupt(
            "unsupported runtime profile toolchain".into(),
        )),
    }
}

pub const REQUESTED_FILE_REL: &str = ".lingxi/dependencies/requested.json";
pub const EFFECTIVE_PACKAGE_FILE_REL: &str = ".lingxi/dependencies/effective-package.json";
pub const LOCKFILE_FILE_REL: &str = ".lingxi/dependencies/pnpm-lock.yaml";
pub const TREE_PROOF_FILE_REL: &str = ".lingxi/dependencies/tree-proof.json";
pub const SBOM_FILE_REL: &str = ".lingxi/dependencies/sbom.spdx.json";
pub const SNAPSHOT_FILE_REL: &str = ".lingxi/dependencies/snapshot.json";
const EMPTY_REQUESTED_JSON: &[u8] = b"{\n  \"dependencies\": {}\n}\n";

macro_rules! profile_file {
    ($family:literal, $path:literal) => {
        (
            $path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../plugins/lingxi-local-app/assets/templates/",
                $family,
                "/r4/",
                $path
            )) as &[u8],
        )
    };
}
macro_rules! widget_file {
    ($path:literal) => {
        (
            concat!("app/mcp-widget/", $path),
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../plugins/lingxi-local-app/assets/templates/shared/mcp-widget/r4/",
                $path
            )) as &[u8],
        )
    };
}
pub const REACT_DOM_R4_MANAGED_FILES: &[(&str, &[u8])] = &[
    profile_file!("react-dom", ".gitignore"),
    profile_file!("react-dom", "package.json"),
    profile_file!("react-dom", "pnpm-lock.yaml"),
    profile_file!("react-dom", "pnpm-workspace.yaml"),
    profile_file!("react-dom", "jsconfig.json"),
    profile_file!("react-dom", "index.html"),
    profile_file!("react-dom", "vite.config.mjs"),
    profile_file!("react-dom", ".lingxi/source-policy.json"),
    profile_file!("react-dom", "lib/lingxi-bridge.js"),
    profile_file!("react-dom", "lib/device-context.js"),
    profile_file!("react-dom", "lib/platform-adapter.js"),
    profile_file!("react-dom", "lib/lingxi-provider.jsx"),
    profile_file!("react-dom", "styles/foundation.css"),
];

pub const REACT_DOM_R4_EDITABLE_FILES: &[(&str, &[u8])] = &[
    profile_file!("react-dom", "app/main.jsx"),
    profile_file!("react-dom", "app/app.jsx"),
    profile_file!("react-dom", "app/providers.jsx"),
    profile_file!("react-dom", "app/error-boundary.jsx"),
    profile_file!("react-dom", "app/globals.css"),
    profile_file!("react-dom", "app/screens/home-screen.jsx"),
    profile_file!("react-dom", "app/screens/detail-screen.jsx"),
    profile_file!("react-dom", "src/stores/app-store.js"),
    profile_file!("react-dom", "public/.gitkeep"),
    widget_file!("package.json"),
    widget_file!("pnpm-lock.yaml"),
    widget_file!("index.html"),
    widget_file!("vite.config.mjs"),
    widget_file!("src/main.jsx"),
    widget_file!("src/widget.jsx"),
];

pub const CANVAS_2D_R4_MANAGED_FILES: &[(&str, &[u8])] = &[
    profile_file!("canvas-2d", ".gitignore"),
    profile_file!("canvas-2d", "package.json"),
    profile_file!("canvas-2d", "pnpm-lock.yaml"),
    profile_file!("canvas-2d", "pnpm-workspace.yaml"),
    profile_file!("canvas-2d", "jsconfig.json"),
    profile_file!("canvas-2d", "index.html"),
    profile_file!("canvas-2d", "vite.config.mjs"),
    profile_file!("canvas-2d", ".lingxi/source-policy.json"),
    profile_file!("canvas-2d", "lib/lingxi-bridge.js"),
    profile_file!("canvas-2d", "lib/device-context.js"),
    profile_file!("canvas-2d", "lib/platform-adapter.js"),
    profile_file!("canvas-2d", "lib/lingxi-provider.jsx"),
    profile_file!("canvas-2d", "lib/frame-loop.js"),
    profile_file!("canvas-2d", "styles/foundation.css"),
];

pub const CANVAS_2D_R4_EDITABLE_FILES: &[(&str, &[u8])] = &[
    profile_file!("canvas-2d", "app/main.jsx"),
    profile_file!("canvas-2d", "app/app.jsx"),
    profile_file!("canvas-2d", "app/providers.jsx"),
    profile_file!("canvas-2d", "app/error-boundary.jsx"),
    profile_file!("canvas-2d", "app/globals.css"),
    profile_file!("canvas-2d", "app/screens/game-screen.jsx"),
    profile_file!("canvas-2d", "src/stores/game-store.js"),
    profile_file!("canvas-2d", "public/.gitkeep"),
    widget_file!("package.json"),
    widget_file!("pnpm-lock.yaml"),
    widget_file!("index.html"),
    widget_file!("vite.config.mjs"),
    widget_file!("src/main.jsx"),
    widget_file!("src/widget.jsx"),
];

pub const THREE_3D_R4_MANAGED_FILES: &[(&str, &[u8])] = &[
    profile_file!("three-3d", ".gitignore"),
    profile_file!("three-3d", "package.json"),
    profile_file!("three-3d", "pnpm-lock.yaml"),
    profile_file!("three-3d", "pnpm-workspace.yaml"),
    profile_file!("three-3d", "jsconfig.json"),
    profile_file!("three-3d", "index.html"),
    profile_file!("three-3d", "vite.config.mjs"),
    profile_file!("three-3d", ".lingxi/source-policy.json"),
    profile_file!("three-3d", "lib/lingxi-bridge.js"),
    profile_file!("three-3d", "lib/device-context.js"),
    profile_file!("three-3d", "lib/platform-adapter.js"),
    profile_file!("three-3d", "lib/lingxi-provider.jsx"),
    profile_file!("three-3d", "lib/frame-loop.js"),
    profile_file!("three-3d", "styles/foundation.css"),
];

pub const THREE_3D_R4_EDITABLE_FILES: &[(&str, &[u8])] = &[
    profile_file!("three-3d", "app/main.jsx"),
    profile_file!("three-3d", "app/app.jsx"),
    profile_file!("three-3d", "app/providers.jsx"),
    profile_file!("three-3d", "app/error-boundary.jsx"),
    profile_file!("three-3d", "app/globals.css"),
    profile_file!("three-3d", "app/screens/game-screen.jsx"),
    profile_file!("three-3d", "src/stores/game-store.js"),
    profile_file!("three-3d", "public/.gitkeep"),
    widget_file!("package.json"),
    widget_file!("pnpm-lock.yaml"),
    widget_file!("index.html"),
    widget_file!("vite.config.mjs"),
    widget_file!("src/main.jsx"),
    widget_file!("src/widget.jsx"),
];

pub const PHASER_2D_R4_MANAGED_FILES: &[(&str, &[u8])] = &[
    profile_file!("phaser-2d", ".gitignore"),
    profile_file!("phaser-2d", "package.json"),
    profile_file!("phaser-2d", "pnpm-lock.yaml"),
    profile_file!("phaser-2d", "pnpm-workspace.yaml"),
    profile_file!("phaser-2d", "jsconfig.json"),
    profile_file!("phaser-2d", "index.html"),
    profile_file!("phaser-2d", "vite.config.mjs"),
    profile_file!("phaser-2d", ".lingxi/source-policy.json"),
    profile_file!("phaser-2d", "lib/lingxi-bridge.js"),
    profile_file!("phaser-2d", "lib/device-context.js"),
    profile_file!("phaser-2d", "lib/platform-adapter.js"),
    profile_file!("phaser-2d", "lib/lingxi-provider.jsx"),
    profile_file!("phaser-2d", "lib/frame-loop.js"),
    profile_file!("phaser-2d", "lib/phaser-runtime.js"),
    profile_file!("phaser-2d", "styles/foundation.css"),
];

pub const PHASER_2D_R4_EDITABLE_FILES: &[(&str, &[u8])] = &[
    profile_file!("phaser-2d", "app/main.jsx"),
    profile_file!("phaser-2d", "app/app.jsx"),
    profile_file!("phaser-2d", "app/providers.jsx"),
    profile_file!("phaser-2d", "app/error-boundary.jsx"),
    profile_file!("phaser-2d", "app/globals.css"),
    profile_file!("phaser-2d", "app/screens/game-screen.jsx"),
    profile_file!("phaser-2d", "src/stores/game-store.js"),
    profile_file!("phaser-2d", "public/.gitkeep"),
    widget_file!("package.json"),
    widget_file!("pnpm-lock.yaml"),
    widget_file!("index.html"),
    widget_file!("vite.config.mjs"),
    widget_file!("src/main.jsx"),
    widget_file!("src/widget.jsx"),
];

pub const BABYLON_3D_R4_MANAGED_FILES: &[(&str, &[u8])] = &[
    profile_file!("babylon-3d", ".gitignore"),
    profile_file!("babylon-3d", "package.json"),
    profile_file!("babylon-3d", "pnpm-lock.yaml"),
    profile_file!("babylon-3d", "pnpm-workspace.yaml"),
    profile_file!("babylon-3d", "jsconfig.json"),
    profile_file!("babylon-3d", "index.html"),
    profile_file!("babylon-3d", "vite.config.mjs"),
    profile_file!("babylon-3d", ".lingxi/source-policy.json"),
    profile_file!("babylon-3d", "lib/lingxi-bridge.js"),
    profile_file!("babylon-3d", "lib/device-context.js"),
    profile_file!("babylon-3d", "lib/platform-adapter.js"),
    profile_file!("babylon-3d", "lib/lingxi-provider.jsx"),
    profile_file!("babylon-3d", "lib/frame-loop.js"),
    profile_file!("babylon-3d", "lib/babylon-runtime.js"),
    profile_file!("babylon-3d", "styles/foundation.css"),
];

pub const BABYLON_3D_R4_EDITABLE_FILES: &[(&str, &[u8])] = &[
    profile_file!("babylon-3d", "app/main.jsx"),
    profile_file!("babylon-3d", "app/app.jsx"),
    profile_file!("babylon-3d", "app/providers.jsx"),
    profile_file!("babylon-3d", "app/error-boundary.jsx"),
    profile_file!("babylon-3d", "app/globals.css"),
    profile_file!("babylon-3d", "app/screens/game-screen.jsx"),
    profile_file!("babylon-3d", "src/stores/game-store.js"),
    profile_file!("babylon-3d", "public/.gitkeep"),
    widget_file!("package.json"),
    widget_file!("pnpm-lock.yaml"),
    widget_file!("index.html"),
    widget_file!("vite.config.mjs"),
    widget_file!("src/main.jsx"),
    widget_file!("src/widget.jsx"),
];

#[derive(Debug, Clone, Copy)]
pub struct RuntimeProfileContract {
    pub family: AppRuntimeProfile,
    pub revision: u32,
    pub surface: AppSurface,
    pub source_seed_id: &'static str,
    pub toolchain_key: &'static str,
    pub core_packages: &'static [(&'static str, &'static str)],
    pub managed_files: &'static [(&'static str, &'static [u8])],
    pub editable_files: &'static [(&'static str, &'static [u8])],
}

#[derive(Clone, Copy)]
struct RuntimeProfileUnavailable {
    contract: RuntimeProfileContract,
    reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProfileCatalogEntry {
    pub family: AppRuntimeProfile,
    pub revision: u32,
    pub surface: AppSurface,
    pub toolchain_key: &'static str,
    pub core_packages: BTreeMap<&'static str, &'static str>,
    pub contract_sha256: String,
    pub available: bool,
    pub availability_reason: Option<&'static str>,
}

pub struct RuntimeProfileScaffoldArtifacts {
    pub binding: AppRuntimeProfileBinding,
    pub files: Vec<(&'static str, Vec<u8>)>,
}

pub struct RuntimeProfileSnapshotArtifacts {
    pub snapshot: AppDependencySnapshot,
    pub files: Vec<(&'static str, Vec<u8>)>,
}

const REACT_DOM_R4_PACKAGES: &[(&str, &str)] = &[
    ("@ionic/react", "9.0.4"),
    ("@ionic/react-router", "9.0.4"),
    ("@vitejs/plugin-react", "6.1.1"),
    ("react", "19.3.0"),
    ("react-dom", "19.3.0"),
    ("react-router", "6.30.6"),
    ("react-router-dom", "6.30.6"),
    ("vite", "8.3.0"),
    ("zod", "4.6.5"),
    ("zustand", "5.0.15"),
];

const REACT_DOM_R4: RuntimeProfileContract = RuntimeProfileContract {
    family: AppRuntimeProfile::ReactDom,
    revision: 4,
    surface: AppSurface::Dom,
    source_seed_id: "react-dom/r4",
    toolchain_key: RUNTIME_PROFILE_TOOLCHAIN_KEY,
    core_packages: REACT_DOM_R4_PACKAGES,
    managed_files: REACT_DOM_R4_MANAGED_FILES,
    editable_files: REACT_DOM_R4_EDITABLE_FILES,
};

const CANVAS_2D_R4_PACKAGES: &[(&str, &str)] = &[
    ("@ionic/react", "9.0.4"),
    ("@ionic/react-router", "9.0.4"),
    ("@vitejs/plugin-react", "6.1.1"),
    ("react", "19.3.0"),
    ("react-dom", "19.3.0"),
    ("react-router", "6.30.6"),
    ("react-router-dom", "6.30.6"),
    ("vite", "8.3.0"),
    ("zod", "4.6.5"),
    ("zustand", "5.0.15"),
];

const CANVAS_2D_R4: RuntimeProfileContract = RuntimeProfileContract {
    family: AppRuntimeProfile::Canvas2d,
    revision: 4,
    surface: AppSurface::Canvas,
    source_seed_id: "canvas-2d/r4",
    toolchain_key: RUNTIME_PROFILE_TOOLCHAIN_KEY,
    core_packages: CANVAS_2D_R4_PACKAGES,
    managed_files: CANVAS_2D_R4_MANAGED_FILES,
    editable_files: CANVAS_2D_R4_EDITABLE_FILES,
};

const THREE_3D_R4_PACKAGES: &[(&str, &str)] = &[
    ("@ionic/react", "9.0.4"),
    ("@ionic/react-router", "9.0.4"),
    ("@vitejs/plugin-react", "6.1.1"),
    ("react", "19.3.0"),
    ("react-dom", "19.3.0"),
    ("react-router", "6.30.6"),
    ("react-router-dom", "6.30.6"),
    ("three", "0.186.0"),
    ("vite", "8.3.0"),
    ("zod", "4.6.5"),
    ("zustand", "5.0.15"),
];

const THREE_3D_R4: RuntimeProfileContract = RuntimeProfileContract {
    family: AppRuntimeProfile::Three3d,
    revision: 4,
    surface: AppSurface::Canvas,
    source_seed_id: "three-3d/r4",
    toolchain_key: RUNTIME_PROFILE_TOOLCHAIN_KEY,
    core_packages: THREE_3D_R4_PACKAGES,
    managed_files: THREE_3D_R4_MANAGED_FILES,
    editable_files: THREE_3D_R4_EDITABLE_FILES,
};

const PHASER_2D_R4_PACKAGES: &[(&str, &str)] = &[
    ("@ionic/react", "9.0.4"),
    ("@ionic/react-router", "9.0.4"),
    ("@vitejs/plugin-react", "6.1.1"),
    ("phaser", "4.2.1"),
    ("react", "19.3.0"),
    ("react-dom", "19.3.0"),
    ("react-router", "6.30.6"),
    ("react-router-dom", "6.30.6"),
    ("vite", "8.3.0"),
    ("zod", "4.6.5"),
    ("zustand", "5.0.15"),
];

const PHASER_2D_R4: RuntimeProfileContract = RuntimeProfileContract {
    family: AppRuntimeProfile::Phaser2d,
    revision: 4,
    surface: AppSurface::Canvas,
    source_seed_id: "phaser-2d/r4",
    toolchain_key: RUNTIME_PROFILE_TOOLCHAIN_KEY,
    core_packages: PHASER_2D_R4_PACKAGES,
    managed_files: PHASER_2D_R4_MANAGED_FILES,
    editable_files: PHASER_2D_R4_EDITABLE_FILES,
};

const BABYLON_3D_R4_PACKAGES: &[(&str, &str)] = &[
    ("@babylonjs/core", "9.27.1"),
    ("@babylonjs/havok", "1.3.14"),
    ("@babylonjs/loaders", "9.27.1"),
    ("@ionic/react", "9.0.4"),
    ("@ionic/react-router", "9.0.4"),
    ("@vitejs/plugin-react", "6.1.1"),
    ("react", "19.3.0"),
    ("react-dom", "19.3.0"),
    ("react-router", "6.30.6"),
    ("react-router-dom", "6.30.6"),
    ("vite", "8.3.0"),
    ("zod", "4.6.5"),
    ("zustand", "5.0.15"),
];

const BABYLON_3D_R4: RuntimeProfileContract = RuntimeProfileContract {
    family: AppRuntimeProfile::Babylon3d,
    revision: 4,
    surface: AppSurface::Canvas,
    source_seed_id: "babylon-3d/r4",
    toolchain_key: RUNTIME_PROFILE_TOOLCHAIN_KEY,
    core_packages: BABYLON_3D_R4_PACKAGES,
    managed_files: BABYLON_3D_R4_MANAGED_FILES,
    editable_files: BABYLON_3D_R4_EDITABLE_FILES,
};

const UNAVAILABLE_PROFILES: &[RuntimeProfileUnavailable] = &[RuntimeProfileUnavailable {
    contract: BABYLON_3D_R4,
    reason:
        "babylon_3d remains gated pending iOS/Android Babylon + glTF + Havok real-device validation",
}];

fn published_available_contracts() -> &'static [RuntimeProfileContract] {
    &[REACT_DOM_R4, CANVAS_2D_R4, THREE_3D_R4, PHASER_2D_R4]
}

fn current_catalog_contracts() -> &'static [RuntimeProfileContract] {
    &[REACT_DOM_R4, CANVAS_2D_R4, THREE_3D_R4, PHASER_2D_R4]
}

pub fn contract_for_binding(
    binding: &AppRuntimeProfileBinding,
) -> Result<&'static RuntimeProfileContract, AppError> {
    let contract = published_available_contracts()
        .iter()
        .find(|contract| contract.family == binding.family && contract.revision == binding.revision)
        .ok_or_else(|| {
            AppError::NotYetAvailable(format!(
                "runtime profile {} r{} is not published in this host build",
                binding.family, binding.revision
            ))
        })?;
    let expected = contract_sha256(contract)?;
    if binding.contract_sha256 != expected {
        return Err(AppError::StorageCorrupt(format!(
            "runtime profile {} r{} expects contract {}, found {}",
            binding.family, binding.revision, expected, binding.contract_sha256
        )));
    }
    Ok(contract)
}

pub fn current_binding_for_family(
    family: AppRuntimeProfile,
) -> Result<AppRuntimeProfileBinding, AppError> {
    let contract = published_available_contracts()
        .iter()
        .filter(|contract| contract.family == family)
        .max_by_key(|contract| contract.revision)
        .ok_or_else(|| {
            AppError::NotYetAvailable(format!(
                "runtime profile {} is not scaffoldable in this host build",
                family
            ))
        })?;
    Ok(AppRuntimeProfileBinding {
        family: contract.family,
        revision: contract.revision,
        contract_sha256: contract_sha256(contract)?,
    })
}

/// The binding a published, available contract carries for `family` at
/// `revision`, or `None` when no such contract is published. Fixtures that need
/// a workspace stamped the way creation stamps it use this instead of going
/// through a scaffold.
pub fn binding_for_family_revision(
    family: AppRuntimeProfile,
    revision: u32,
) -> Option<AppRuntimeProfileBinding> {
    published_available_contracts()
        .iter()
        .find(|contract| contract.family == family && contract.revision == revision)
        .map(|contract| AppRuntimeProfileBinding {
            family,
            revision,
            contract_sha256: contract_sha256(contract).expect("published contract digest"),
        })
}

pub fn list_runtime_profiles() -> Vec<RuntimeProfileCatalogEntry> {
    let mut entries = Vec::new();
    for contract in current_catalog_contracts() {
        entries.push(RuntimeProfileCatalogEntry {
            family: contract.family,
            revision: contract.revision,
            surface: contract.surface,
            toolchain_key: contract.toolchain_key,
            core_packages: contract.core_packages.iter().copied().collect(),
            contract_sha256: contract_sha256(contract).expect("published contract digest"),
            available: true,
            availability_reason: None,
        });
    }
    for unavailable in UNAVAILABLE_PROFILES {
        entries.push(RuntimeProfileCatalogEntry {
            family: unavailable.contract.family,
            revision: unavailable.contract.revision,
            surface: unavailable.contract.surface,
            toolchain_key: unavailable.contract.toolchain_key,
            core_packages: unavailable.contract.core_packages.iter().copied().collect(),
            contract_sha256: contract_sha256(&unavailable.contract)
                .expect("unavailable contract digest"),
            available: false,
            availability_reason: Some(unavailable.reason),
        });
    }
    entries.sort_by(|left, right| left.family.as_str().cmp(right.family.as_str()));
    entries
}

pub fn contract_sha256(contract: &RuntimeProfileContract) -> Result<String, AppError> {
    let descriptor = contract_descriptor(
        contract.family,
        contract.revision,
        contract.surface,
        contract.toolchain_key,
        contract.core_packages,
        contract.managed_files,
        contract.editable_files,
        contract.source_seed_id,
        lockfile_sha256(contract),
    );
    let bytes = serde_json::to_vec(&descriptor)
        .map_err(|error| AppError::Io(format!("serialize runtime profile descriptor: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn file_digest_map(files: &[(&str, &[u8])]) -> Map<String, Value> {
    let mut digests = Map::new();
    for (path, bytes) in files
        .iter()
        .map(|(path, bytes)| (*path, *bytes))
        .collect::<BTreeMap<_, _>>()
    {
        digests.insert(
            path.to_string(),
            Value::String(format!("{:x}", Sha256::digest(bytes))),
        );
    }
    digests
}

fn editable_seed_sha256(files: &[(&str, &[u8])]) -> String {
    let bytes = serde_json::to_vec(&canonicalize(Value::Object(file_digest_map(files))))
        .expect("serialize editable seed digest map");
    format!("{:x}", Sha256::digest(bytes))
}

fn contract_descriptor(
    family: AppRuntimeProfile,
    revision: u32,
    surface: AppSurface,
    toolchain_key: &str,
    core_packages: &[(&str, &str)],
    managed_files: &[(&str, &[u8])],
    editable_files: &[(&str, &[u8])],
    source_seed_id: &str,
    base_lockfile_sha256: String,
) -> Value {
    let mut core_packages_json = Map::new();
    for (name, version) in core_packages.iter().copied().collect::<BTreeMap<_, _>>() {
        core_packages_json.insert(name.to_string(), Value::String(version.to_string()));
    }
    canonicalize(Value::Object(Map::from_iter([
        (
            "family".to_string(),
            Value::String(family.as_str().to_string()),
        ),
        (
            "revision".to_string(),
            Value::Number(serde_json::Number::from(revision)),
        ),
        (
            "surface".to_string(),
            Value::String(surface.as_str().to_string()),
        ),
        (
            "toolchainKey".to_string(),
            Value::String(toolchain_key.to_string()),
        ),
        (
            "corePackages".to_string(),
            Value::Object(core_packages_json),
        ),
        (
            "managedFiles".to_string(),
            Value::Object(file_digest_map(managed_files)),
        ),
        (
            "editableFiles".to_string(),
            Value::Object(file_digest_map(editable_files)),
        ),
        (
            "sourceSeedSha256".to_string(),
            Value::String(editable_seed_sha256(editable_files)),
        ),
        (
            "baseLockfileSha256".to_string(),
            Value::String(base_lockfile_sha256),
        ),
        (
            "sourceSeedId".to_string(),
            Value::String(source_seed_id.to_string()),
        ),
    ])))
}

pub fn lockfile_sha256(contract: &RuntimeProfileContract) -> String {
    let lockfile = contract
        .managed_files
        .iter()
        .find(|(path, _)| *path == "pnpm-lock.yaml")
        .map(|(_, bytes)| *bytes)
        .expect("runtime profile contract includes pnpm-lock.yaml");
    format!("{:x}", Sha256::digest(lockfile))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn managed_file_bytes(
    contract: &RuntimeProfileContract,
    path: &str,
) -> Result<&'static [u8], AppError> {
    contract
        .managed_files
        .iter()
        .find(|(managed_path, _)| *managed_path == path)
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| {
            AppError::StorageCorrupt(format!(
                "runtime profile {} r{} is missing managed file {path}",
                contract.family, contract.revision
            ))
        })
}

#[cfg(test)]
fn package_dependencies(
    contract: &RuntimeProfileContract,
) -> Result<BTreeMap<String, String>, AppError> {
    let package_json = managed_file_bytes(contract, "package.json")?;
    let body: Value = serde_json::from_slice(package_json)
        .map_err(|error| AppError::Io(format!("parse runtime profile package.json: {error}")))?;
    let dependencies = body
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            AppError::StorageCorrupt("runtime profile package.json is missing dependencies".into())
        })?;
    dependencies
        .iter()
        .map(|(name, version)| {
            version
                .as_str()
                .map(|version| (name.clone(), version.to_string()))
                .ok_or_else(|| {
                    AppError::StorageCorrupt(format!(
                        "runtime profile package.json dependency {name} is not a string"
                    ))
                })
        })
        .collect()
}

fn snapshot_json(snapshot: &AppDependencySnapshot) -> Result<Vec<u8>, AppError> {
    let mut bytes = serde_json::to_vec_pretty(snapshot)
        .map_err(|error| AppError::Io(format!("serialize dependency snapshot: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn tree_proof_json(tree_sha256: &str, toolchain_key: &str) -> Result<Vec<u8>, AppError> {
    let body = Value::Object(Map::from_iter([
        (
            "schemaVersion".to_string(),
            Value::Number(serde_json::Number::from(1_u32)),
        ),
        (
            "treeSha256".to_string(),
            Value::String(tree_sha256.to_string()),
        ),
        (
            "toolchainKey".to_string(),
            Value::String(toolchain_key.to_string()),
        ),
    ]));
    let mut bytes = serde_json::to_vec_pretty(&body)
        .map_err(|error| AppError::Io(format!("serialize dependency tree proof: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn scaffold_artifacts_for_binding(
    binding: &AppRuntimeProfileBinding,
) -> Result<RuntimeProfileScaffoldArtifacts, AppError> {
    let contract = contract_for_binding(binding)?;
    Ok(RuntimeProfileScaffoldArtifacts {
        binding: binding.clone(),
        files: vec![
            (REQUESTED_FILE_REL, EMPTY_REQUESTED_JSON.to_vec()),
            (
                EFFECTIVE_PACKAGE_FILE_REL,
                managed_file_bytes(contract, "package.json")?.to_vec(),
            ),
            (
                LOCKFILE_FILE_REL,
                managed_file_bytes(contract, "pnpm-lock.yaml")?.to_vec(),
            ),
        ],
    })
}

pub fn snapshot_artifacts_for_binding(
    binding: &AppRuntimeProfileBinding,
    requested_sha256: String,
    package_sha256: String,
    lockfile_sha256: String,
    tree_sha256: String,
    sbom_bytes: &[u8],
) -> Result<RuntimeProfileSnapshotArtifacts, AppError> {
    let contract = contract_for_binding(binding)?;
    let snapshot = AppDependencySnapshot {
        requested_sha256,
        package_sha256,
        lockfile_sha256,
        dependency_tree_sha256: tree_sha256.clone(),
        sbom_sha256: hash_bytes(sbom_bytes),
        toolchain_key: contract.toolchain_key.to_string(),
        verified_profile_contract_sha256: binding.contract_sha256.clone(),
    };
    Ok(RuntimeProfileSnapshotArtifacts {
        files: vec![
            (SBOM_FILE_REL, sbom_bytes.to_vec()),
            (
                TREE_PROOF_FILE_REL,
                tree_proof_json(&tree_sha256, contract.toolchain_key)?,
            ),
            (SNAPSHOT_FILE_REL, snapshot_json(&snapshot)?),
        ],
        snapshot,
    })
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered = object
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(Map::from_iter(ordered))
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_contracts() -> [RuntimeProfileContract; 5] {
        [
            REACT_DOM_R4,
            CANVAS_2D_R4,
            THREE_3D_R4,
            PHASER_2D_R4,
            BABYLON_3D_R4,
        ]
    }

    fn validate_catalog_contracts(catalog: &Value) -> Result<(), Vec<String>> {
        let Some(templates) = catalog["templates"].as_array() else {
            return Err(vec!["catalog templates must be an array".to_string()]);
        };
        let mut mismatches = Vec::new();
        for contract in catalog_contracts() {
            let Some(published) = templates.iter().find(|entry| {
                entry["family"].as_str() == Some(contract.family.as_str())
                    && entry["revision"].as_u64() == Some(contract.revision.into())
            }) else {
                mismatches.push(format!(
                    "catalog entry missing for {} r{}",
                    contract.family, contract.revision
                ));
                continue;
            };
            if published["revision"].as_u64() != Some(contract.revision.into()) {
                mismatches.push(format!(
                    "{} revision: catalog={:?}, production={}",
                    contract.family, published["revision"], contract.revision
                ));
            }
            let actual = contract_sha256(&contract).unwrap();
            if published["contractSha256"].as_str() != Some(actual.as_str()) {
                mismatches.push(format!(
                    "{} contractSha256: catalog={:?}, production={actual}",
                    contract.family, published["contractSha256"]
                ));
            }
        }
        if mismatches.is_empty() {
            Ok(())
        } else {
            Err(mismatches)
        }
    }

    #[test]
    fn current_widget_starter_has_a_locked_workspace() {
        for contract in catalog_contracts() {
            assert_eq!(contract.toolchain_key, RUNTIME_PROFILE_TOOLCHAIN_KEY);
            let workspace =
                std::str::from_utf8(managed_file_bytes(&contract, "pnpm-workspace.yaml").unwrap())
                    .unwrap();
            assert!(workspace.contains("app/mcp-widget"));
            let lockfile =
                std::str::from_utf8(managed_file_bytes(&contract, "pnpm-lock.yaml").unwrap())
                    .unwrap();
            assert!(lockfile.contains("app/mcp-widget:"));
            for path in [
                "package.json",
                "pnpm-lock.yaml",
                "index.html",
                "vite.config.mjs",
                "src/main.jsx",
                "src/widget.jsx",
            ] {
                assert!(contract
                    .editable_files
                    .iter()
                    .any(|(candidate, _)| *candidate == format!("app/mcp-widget/{path}")));
            }
        }
    }

    #[test]
    #[ignore = "prints reviewed current profile contracts for catalog regeneration"]
    fn print_current_profile_contracts() {
        for contract in catalog_contracts() {
            println!(
                "{}",
                serde_json::json!({
                    "family": contract.family.as_str(), "revision": contract.revision,
                    "contractSha256": contract_sha256(&contract).unwrap(),
                    "managedFiles": file_digest_map(contract.managed_files),
                    "editableFiles": file_digest_map(contract.editable_files),
                })
            );
        }
    }

    #[test]
    fn plugin_catalog_contract_digests_match_production_contracts() {
        let catalog: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/catalog.json"
        )))
        .expect("Plugin runtime profile catalog must be valid JSON");
        if let Err(mismatches) = validate_catalog_contracts(&catalog) {
            panic!(
                "Plugin catalog differs from production contracts:\n{}",
                mismatches.join("\n")
            );
        }
    }

    #[test]
    fn every_catalog_contract_digest_is_part_of_the_gate() {
        let catalog: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/catalog.json"
        )))
        .expect("Plugin runtime profile catalog must be valid JSON");
        for contract in catalog_contracts() {
            let mut tampered = catalog.clone();
            let entry = tampered["templates"]
                .as_array_mut()
                .expect("templates")
                .iter_mut()
                .find(|entry| {
                    entry["family"].as_str() == Some(contract.family.as_str())
                        && entry["revision"].as_u64() == Some(contract.revision.into())
                })
                .expect("family entry");
            entry["contractSha256"] = Value::String("0".repeat(64));
            let errors = validate_catalog_contracts(&tampered)
                .expect_err("tampering any family digest must fail the production gate");
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(contract.family.as_str())),
                "failure must name the tampered family: {errors:?}"
            );
        }
    }

    #[test]
    fn published_bindings_round_trip_exactly() {
        for family in [
            AppRuntimeProfile::ReactDom,
            AppRuntimeProfile::Canvas2d,
            AppRuntimeProfile::Three3d,
            AppRuntimeProfile::Phaser2d,
        ] {
            let binding = current_binding_for_family(family).expect("binding");
            let contract = contract_for_binding(&binding).expect("exact contract");
            assert_eq!(contract.family, family);
            assert_eq!(contract.revision, 4, "{family} should default to r4");
        }
    }

    #[test]
    fn unpublished_profiles_are_listed_but_not_scaffoldable() {
        {
            let family = AppRuntimeProfile::Babylon3d;
            assert!(current_binding_for_family(family).is_err());
        }
        let listed = list_runtime_profiles();
        assert!(listed.iter().any(|entry| {
            entry.family == AppRuntimeProfile::Phaser2d
                && entry.available
                && !entry.contract_sha256.is_empty()
        }));
        assert!(listed.iter().any(|entry| {
            entry.family == AppRuntimeProfile::Babylon3d
                && !entry.available
                && !entry.contract_sha256.is_empty()
                && entry.availability_reason.is_some()
        }));
    }

    #[test]
    fn current_profiles_own_all_managed_and_editable_template_bytes() {
        for contract in catalog_contracts() {
            assert_eq!(contract.revision, 4);
            let family = contract.family.as_str().replace('_', "-");
            let template = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../plugins/lingxi-local-app/assets/templates")
                .join(family)
                .join("r4");
            for (path, bytes) in contract.managed_files.iter().chain(contract.editable_files) {
                let source = if let Some(widget_path) = path.strip_prefix("app/mcp-widget/") {
                    template
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("shared/mcp-widget/r4")
                        .join(widget_path)
                } else {
                    template.join(path)
                };
                assert_eq!(
                    std::fs::read(source).unwrap(),
                    *bytes,
                    "{} {path}",
                    contract.family
                );
            }
        }
    }

    #[test]
    fn retired_runtime_revisions_are_not_supported() {
        for family in [
            AppRuntimeProfile::ReactDom,
            AppRuntimeProfile::Canvas2d,
            AppRuntimeProfile::Three3d,
            AppRuntimeProfile::Phaser2d,
        ] {
            let mut binding = current_binding_for_family(family).unwrap();
            for revision in 1..4 {
                binding.revision = revision;
                assert!(matches!(
                    contract_for_binding(&binding),
                    Err(AppError::NotYetAvailable(_))
                ));
            }
        }
    }

    #[test]
    fn tampered_contract_hash_fails_closed() {
        let mut binding = current_binding_for_family(AppRuntimeProfile::ReactDom).unwrap();
        binding.contract_sha256 = "0".repeat(64);
        let error = contract_for_binding(&binding).expect_err("tampered hash");
        assert!(matches!(error, AppError::StorageCorrupt(_)), "{error:?}");
    }

    #[test]
    fn scaffold_artifacts_do_not_claim_a_verified_sbom() {
        let binding = current_binding_for_family(AppRuntimeProfile::ReactDom).unwrap();
        let scaffold = scaffold_artifacts_for_binding(&binding).unwrap();
        assert!(!scaffold
            .files
            .iter()
            .any(|(path, _)| *path == SBOM_FILE_REL));
    }

    #[test]
    fn snapshot_artifacts_embed_the_host_supplied_sbom() {
        let react = current_binding_for_family(AppRuntimeProfile::ReactDom).unwrap();
        let sbom = br#"{"spdxVersion":"SPDX-2.3"}"#;
        let snapshot = snapshot_artifacts_for_binding(
            &react,
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            sbom,
        )
        .unwrap();
        assert_eq!(snapshot.snapshot.package_sha256, "b".repeat(64));
        assert_eq!(snapshot.snapshot.lockfile_sha256, "c".repeat(64));
        assert_eq!(snapshot.snapshot.sbom_sha256, hash_bytes(sbom));
        assert!(snapshot
            .files
            .iter()
            .any(|(path, bytes)| *path == SBOM_FILE_REL && bytes.as_slice() == sbom));
    }

    #[test]
    fn catalog_core_packages_match_the_checked_in_package_json() {
        for contract in catalog_contracts() {
            let expected = contract
                .core_packages
                .iter()
                .map(|(name, version)| ((*name).to_string(), (*version).to_string()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(package_dependencies(&contract).unwrap(), expected);
        }
    }

    #[test]
    fn engine_templates_import_only_their_declared_runtime() {
        let phaser_source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/phaser-2d/r4/app/screens/game-screen.jsx"
        ));
        let phaser_runtime = String::from_utf8_lossy(
            managed_file_bytes(&PHASER_2D_R4, "lib/phaser-runtime.js")
                .expect("Phaser runtime adapter is managed"),
        );
        let phaser_packages = package_dependencies(&PHASER_2D_R4).expect("Phaser package.json");
        assert_eq!(
            phaser_packages.get("phaser").map(String::as_str),
            Some("4.2.1")
        );
        assert!(!phaser_packages.contains_key("three"));
        assert!(!phaser_packages
            .keys()
            .any(|name| name.starts_with("@babylonjs/")));
        assert!(PHASER_2D_R4_MANAGED_FILES
            .iter()
            .any(|(path, _)| *path == "lib/phaser-runtime.js"));
        assert!(PHASER_2D_R4_MANAGED_FILES
            .iter()
            .any(|(path, _)| *path == "lib/frame-loop.js"));
        assert!(!PHASER_2D_R4_EDITABLE_FILES
            .iter()
            .any(|(path, _)| *path == "src/game/frame-loop.js"));
        assert!(phaser_source.contains("from \"@/lib/phaser-runtime\""));
        assert!(phaser_runtime.contains("import Phaser from \"phaser\";"));
        assert!(phaser_runtime.contains("new Phaser.Game"));
        assert!(phaser_runtime.contains("from \"@/lib/frame-loop\""));
        assert!(!phaser_source.contains("three"));
        assert!(!phaser_source.contains("phaser\""));
        assert!(!phaser_source.contains("@babylonjs/"));
        assert!(!phaser_runtime.contains("three"));
        assert!(!phaser_runtime.contains("@babylonjs/"));

        let babylon_source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/lingxi-local-app/assets/templates/babylon-3d/r4/app/screens/game-screen.jsx"
        ));
        let babylon_runtime = String::from_utf8_lossy(
            managed_file_bytes(&BABYLON_3D_R4, "lib/babylon-runtime.js")
                .expect("Babylon runtime adapter is managed"),
        );
        let babylon_packages = package_dependencies(&BABYLON_3D_R4).expect("Babylon package.json");
        assert_eq!(
            babylon_packages.get("@babylonjs/core").map(String::as_str),
            Some("9.27.1")
        );
        assert_eq!(
            babylon_packages
                .get("@babylonjs/loaders")
                .map(String::as_str),
            Some("9.27.1")
        );
        assert_eq!(
            babylon_packages.get("@babylonjs/havok").map(String::as_str),
            Some("1.3.14")
        );
        assert!(!babylon_packages.contains_key("three"));
        assert!(!babylon_packages.contains_key("phaser"));
        assert!(BABYLON_3D_R4_MANAGED_FILES
            .iter()
            .any(|(path, _)| *path == "lib/babylon-runtime.js"));
        assert!(BABYLON_3D_R4_MANAGED_FILES
            .iter()
            .any(|(path, _)| *path == "lib/frame-loop.js"));
        assert!(!BABYLON_3D_R4_EDITABLE_FILES
            .iter()
            .any(|(path, _)| *path == "src/game/frame-loop.js"));
        assert!(babylon_source.contains("from \"@/lib/babylon-runtime\""));
        assert!(babylon_runtime.contains("from \"@babylonjs/core\";"));
        assert!(babylon_runtime.contains("from \"@babylonjs/havok\";"));
        assert!(babylon_runtime.contains("import \"@babylonjs/loaders\";"));
        assert!(babylon_runtime.contains("new HavokPlugin"));
        assert!(babylon_runtime.contains("scene.enablePhysics"));
        assert!(babylon_runtime.contains("from \"@/lib/frame-loop\""));
        assert!(!babylon_source.contains("three"));
        assert!(!babylon_source.contains("phaser"));
        assert!(!babylon_source.contains("@babylonjs/"));
        assert!(!babylon_runtime.contains("three"));
        assert!(!babylon_runtime.contains("phaser\""));
    }

    #[test]
    fn source_policy_covers_profile_managed_runtime_helpers() {
        for contract in catalog_contracts() {
            let policy_bytes = managed_file_bytes(&contract, ".lingxi/source-policy.json")
                .expect("source policy is managed");
            let policy: Value =
                serde_json::from_slice(policy_bytes).expect("source policy json parses");
            let host_managed_paths = policy
                .get("host_managed_paths")
                .and_then(Value::as_array)
                .expect("source policy host_managed_paths");
            let host_managed_paths = host_managed_paths
                .iter()
                .filter_map(Value::as_str)
                .collect::<std::collections::HashSet<_>>();
            for (path, _) in contract.managed_files {
                if path.starts_with("lib/") || path.starts_with("styles/") {
                    assert!(
                        host_managed_paths.contains(path),
                        "{} source policy must reserve managed helper {}",
                        contract.family,
                        path
                    );
                }
            }
            let workspace_settings = managed_file_bytes(&contract, "pnpm-workspace.yaml")
                .expect("pnpm workspace settings are managed");
            let workspace_text =
                std::str::from_utf8(workspace_settings).expect("workspace settings are utf-8");
            assert!(
                workspace_text.contains("nodeLinker: hoisted"),
                "{} must keep the hoisted nodeLinker contract",
                contract.family
            );
        }
    }

    #[test]
    fn editable_seed_files_are_part_of_the_contract_digest() {
        for contract in catalog_contracts() {
            let original = contract_sha256(&contract).expect("original digest");
            let (path, _) = contract
                .editable_files
                .first()
                .expect("editable seed file exists");
            let mutated_files = [(*path, b"mutated seed bytes" as &[u8])];
            assert_ne!(
                format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&contract_descriptor(
                            contract.family,
                            contract.revision,
                            contract.surface,
                            contract.toolchain_key,
                            contract.core_packages,
                            contract.managed_files,
                            &mutated_files,
                            contract.source_seed_id,
                            lockfile_sha256(&contract),
                        ))
                        .expect("serialize mutated descriptor")
                    )
                ),
                original,
                "{} editable seed must change the published contract digest",
                contract.family
            );
        }
    }
}
