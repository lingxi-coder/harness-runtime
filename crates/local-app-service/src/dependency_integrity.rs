use crate::runtime_profiles::toolchain_for_binding;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub const DEPENDENCY_SNAPSHOT_VERSION: u8 = 2;

pub const DEPENDENCY_SNAPSHOT_READY_FILE: &str = ".lingxi-dependency-ready";

pub const MAX_DEPENDENCY_SNAPSHOT_READY_BYTES: u64 = 4 * 1024;

pub const DEPENDENCY_SNAPSHOT_INVENTORY_FILE: &str = ".lingxi-dependency-inventory.json";

pub const DEPENDENCY_SNAPSHOT_INVENTORY_SCHEMA_VERSION: u8 = 1;

pub const MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyChangeKind {
    Add,
    Update,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyChange {
    pub kind: DependencyChangeKind,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

pub fn dependency_change_cache_status(kind: &DependencyChangeKind) -> String {
    match kind {
        DependencyChangeKind::Remove => "not_needed".into(),
        DependencyChangeKind::Add | DependencyChangeKind::Update => {
            // A ready app tree says nothing about whether this particular
            // package/version is in the pnpm store.  Do not inspect or mutate
            // that store before approval; expose an honest unknown status.
            "unknown_until_resolution".into()
        }
    }
}

pub struct DependencyInstallCompletion {
    pub lockfile_sha256: String,
    pub toolchain_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledDependencyPackage {
    pub name: String,
    pub version: String,
    pub license: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedDependencyInventory {
    pub schema_version: u8,
    pub toolchain_key: String,
    pub lock_digest: String,
    pub tree_digest: String,
    pub inventory_digest: String,
    pub packages: Vec<InstalledDependencyPackage>,
}

pub fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let ordered = object
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(Map::from_iter(ordered))
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

pub fn dependency_yaml_scalar(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("dependency lock contains an empty YAML scalar".into());
    }
    if value.starts_with('\'') {
        if value.len() < 2 || !value.ends_with('\'') {
            return Err("dependency lock contains an unterminated single-quoted scalar".into());
        }
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value).map_err(|error| {
            format!("dependency lock contains an invalid quoted scalar: {error}")
        });
    }
    Ok(value.to_string())
}

/// Read the exact direct dependency specifiers from pnpm's root importer.
///
/// The host pins pnpm's lockfile format through `PNPM_TOOLCHAIN_KEY`, so this
/// intentionally parses only the small, stable `importers -> . ->
/// dependencies -> <package> -> specifier` surface that must agree with the
/// Host-minted effective package. It does not use package-resolution entries
/// as proof: the same package can occur there transitively without being a
/// requested root dependency.
pub fn pnpm_root_dependency_specifiers(
    lockfile: &[u8],
) -> Result<BTreeMap<String, String>, String> {
    let lockfile = std::str::from_utf8(lockfile)
        .map_err(|error| format!("resolved dependency lockfile is not UTF-8: {error}"))?;
    let mut documents = Vec::new();
    let mut document = String::new();
    for line in lockfile.lines() {
        if line == "---" || line.starts_with("--- #") {
            if !document.trim().is_empty() {
                documents.push(std::mem::take(&mut document));
            }
        } else {
            document.push_str(line);
            document.push('\n');
        }
    }
    if !document.trim().is_empty() {
        documents.push(document);
    }
    let mut dependencies = None;
    let mut found_config = false;
    for document in documents {
        match pnpm_document_dependency_specifiers(&document)? {
            Some(specifiers) => {
                if dependencies.replace(specifiers).is_some() {
                    return Err(
                        "resolved dependency lockfile contains multiple dependency documents"
                            .into(),
                    );
                }
            }
            None => {
                if found_config || dependencies.is_some() {
                    return Err(
                        "resolved dependency lockfile has ambiguous configuration documents".into(),
                    );
                }
                found_config = true;
            }
        }
    }
    dependencies.ok_or_else(|| {
        "resolved dependency lockfile is missing the root dependency importer".into()
    })
}

pub fn pnpm_document_dependency_specifiers(
    lockfile: &str,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let mut in_importers = false;
    let mut in_root_importer = false;
    let mut in_dependencies = false;
    let mut found_importers = false;
    let mut found_root_importer = false;
    let mut found_other_importer = false;
    let mut found_dependencies = false;
    let mut config_sections = HashSet::new();
    let mut unexpected_config_section = false;
    let mut current_package: Option<String> = None;
    let mut dependency_keys = HashSet::new();
    let mut specifiers = BTreeMap::new();

    for line in lockfile.lines() {
        let leading = line.trim_start_matches(' ');
        if leading.starts_with('\t') {
            return Err("resolved dependency lockfile uses tabs for indentation".into());
        }
        let indent = line.len() - leading.len();
        let trimmed = leading.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if indent == 0 {
            if trimmed == "importers:" {
                if found_importers {
                    return Err("resolved dependency lockfile repeats the importers map".into());
                }
                found_importers = true;
                in_importers = true;
                continue;
            }
            in_importers = false;
            in_root_importer = false;
            in_dependencies = false;
            current_package = None;
            continue;
        }
        if !in_importers {
            continue;
        }
        if indent == 2 {
            let importer = trimmed.strip_suffix(':').ok_or_else(|| {
                "resolved dependency lockfile has an invalid importer".to_string()
            })?;
            in_root_importer = dependency_yaml_scalar(importer)? == ".";
            if in_root_importer && found_root_importer {
                return Err("resolved dependency lockfile repeats the root importer".into());
            }
            found_root_importer |= in_root_importer;
            found_other_importer |= !in_root_importer;
            in_dependencies = false;
            current_package = None;
            continue;
        }
        if !in_root_importer {
            continue;
        }
        if indent == 4 {
            unexpected_config_section |= !matches!(
                trimmed.split(':').next(),
                Some("configDependencies" | "packageManagerDependencies")
            );
            if let Some(key) = trimmed
                .split(':')
                .next()
                .filter(|key| matches!(*key, "configDependencies" | "packageManagerDependencies"))
            {
                if !config_sections.insert(key) {
                    return Err(
                        "resolved dependency lockfile repeats a root configuration map".into(),
                    );
                }
            }
            in_dependencies = trimmed == "dependencies:";
            if in_dependencies && found_dependencies {
                return Err(
                    "resolved dependency lockfile repeats the root dependencies map".into(),
                );
            }
            if matches!(trimmed, "devDependencies:" | "optionalDependencies:") {
                return Err(format!(
                    "resolved dependency lockfile contains unexpected root {trimmed}"
                ));
            }
            found_dependencies |= in_dependencies;
            current_package = None;
            continue;
        }
        if !in_dependencies {
            continue;
        }
        if indent == 6 {
            let package = trimmed.strip_suffix(':').ok_or_else(|| {
                "resolved dependency lockfile has an invalid dependency key".to_string()
            })?;
            let package = dependency_yaml_scalar(package)?;
            if !dependency_keys.insert(package.clone()) {
                return Err(format!(
                    "resolved dependency lockfile repeats root dependency {package}"
                ));
            }
            current_package = Some(package);
            continue;
        }
        if indent == 8 {
            let Some(package) = current_package.as_ref() else {
                continue;
            };
            if let Some(specifier) = trimmed.strip_prefix("specifier:") {
                let specifier = dependency_yaml_scalar(specifier)?;
                if specifiers.insert(package.clone(), specifier).is_some() {
                    return Err(format!(
                        "resolved dependency lockfile repeats the specifier for {package}"
                    ));
                }
            }
        }
    }

    if found_root_importer && !found_dependencies && !config_sections.is_empty() {
        if found_other_importer || unexpected_config_section {
            return Err("resolved dependency lockfile configuration document has unexpected importer fields".into());
        }
        return Ok(None);
    }
    if !found_root_importer || !found_dependencies {
        return Err("resolved dependency lockfile is missing the root dependency importer".into());
    }
    if !config_sections.is_empty() {
        return Err(
            "resolved dependency lockfile mixes configuration and application dependencies".into(),
        );
    }
    if dependency_keys.len() != specifiers.len() {
        let missing = dependency_keys
            .iter()
            .find(|package| !specifiers.contains_key(*package))
            .cloned()
            .unwrap_or_else(|| "<unknown>".to_string());
        return Err(format!(
            "resolved dependency lockfile is missing the root specifier for {missing}"
        ));
    }
    Ok(Some(specifiers))
}

pub fn effective_package_dependency_specifiers(
    package_json: &[u8],
) -> Result<BTreeMap<String, String>, String> {
    let package: Value = serde_json::from_slice(package_json)
        .map_err(|error| format!("parse effective dependency package: {error}"))?;
    let dependencies = package
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| "effective dependency package is missing dependencies".to_string())?;
    dependencies
        .iter()
        .map(|(name, version)| {
            let version = version.as_str().ok_or_else(|| {
                format!("effective dependency {name} must use a string specifier")
            })?;
            Ok((name.clone(), version.to_string()))
        })
        .collect()
}

pub fn validate_resolved_dependency_lock(
    package_json: &[u8],
    lockfile: &[u8],
) -> Result<(), String> {
    let expected = effective_package_dependency_specifiers(package_json)?;
    let actual = pnpm_root_dependency_specifiers(lockfile)?;
    if actual == expected {
        return Ok(());
    }
    let mismatch = expected
        .iter()
        .find(|(package, version)| actual.get(*package) != Some(*version))
        .map(|(package, version)| format!("{package}@{version}"))
        .or_else(|| {
            actual
                .keys()
                .find(|package| !expected.contains_key(*package))
                .map(|package| format!("unexpected {package}"))
        })
        .unwrap_or_else(|| "unknown mismatch".to_string());
    Err(format!(
        "resolved dependency lockfile does not match the effective package root importer ({mismatch})"
    ))
}

pub fn installed_package_manifest(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) != Some("package.json") {
        return false;
    }
    let Some(package_dir) = path.parent() else {
        return false;
    };
    let Some(parent) = package_dir.parent() else {
        return false;
    };
    if parent.file_name().and_then(|name| name.to_str()) == Some("node_modules") {
        return true;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    grandparent.file_name().and_then(|name| name.to_str()) == Some("node_modules")
        && parent
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('@'))
}

pub fn package_license_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Object(object) => object
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

pub fn collect_installed_packages(
    root: &Path,
    packages: &mut BTreeMap<(String, String), Option<String>>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("read dependency tree {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!("inspect dependency tree entry {}: {error}", path.display())
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_installed_packages(&path, packages)?;
            continue;
        }
        if !file_type.is_file() || !installed_package_manifest(&path) {
            continue;
        }
        let body = std::fs::read(&path).map_err(|error| {
            format!(
                "read installed package manifest {}: {error}",
                path.display()
            )
        })?;
        let manifest: Value = serde_json::from_slice(&body).map_err(|error| {
            format!(
                "parse installed package manifest {}: {error}",
                path.display()
            )
        })?;
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "installed package manifest {} is missing name",
                    path.display()
                )
            })?
            .to_string();
        let version = manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "installed package manifest {} is missing version",
                    path.display()
                )
            })?
            .to_string();
        let license = manifest.get("license").and_then(package_license_string);
        packages.entry((name, version)).or_insert(license);
    }
    Ok(())
}

pub fn dependency_snapshot_inventory_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join(DEPENDENCY_SNAPSHOT_INVENTORY_FILE)
}

pub fn dependency_inventory_digest(
    inventory: &VerifiedDependencyInventory,
) -> Result<String, String> {
    let mut unsigned = inventory.clone();
    unsigned.inventory_digest.clear();
    let bytes = serde_json::to_vec(&unsigned)
        .map_err(|error| format!("serialize dependency inventory for digest: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn write_verified_dependency_inventory(
    snapshot_root: &Path,
    lock_digest: &str,
    tree_digest: &str,
    packages: &BTreeMap<(String, String), Option<String>>,
    toolchain_key: &str,
) -> Result<(), String> {
    let mut inventory = VerifiedDependencyInventory {
        schema_version: DEPENDENCY_SNAPSHOT_INVENTORY_SCHEMA_VERSION,
        toolchain_key: toolchain_key.to_string(),
        lock_digest: lock_digest.to_string(),
        tree_digest: tree_digest.to_string(),
        inventory_digest: String::new(),
        packages: packages
            .iter()
            .map(|((name, version), license)| InstalledDependencyPackage {
                name: name.clone(),
                version: version.clone(),
                license: license.clone(),
            })
            .collect(),
    };
    inventory.inventory_digest = dependency_inventory_digest(&inventory)?;
    let bytes = serde_json::to_vec_pretty(&inventory)
        .map_err(|error| format!("serialize dependency inventory: {error}"))?;
    if bytes.len() > MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES {
        return Err(format!(
            "dependency inventory exceeds {} bytes",
            MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES
        ));
    }
    let path = dependency_snapshot_inventory_path(snapshot_root);
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| {
        format!(
            "write dependency inventory {}: {error}",
            temporary.display()
        )
    })?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("publish dependency inventory {}: {error}", path.display()))?;
    make_dependency_files_read_only(&path)
        .map_err(|error| format!("protect dependency inventory {}: {error}", path.display()))
}

/// Read only an inventory whose provenance and content digest match the
/// immutable snapshot it sits beside.  A malformed or stale sidecar is a
/// cache miss, never permission to trust a tree or an invented ready state.
pub fn read_verified_dependency_inventory(
    snapshot_root: &Path,
    lock_digest: &str,
    tree_digest: &str,
    toolchain_key: &str,
) -> Result<Option<BTreeMap<(String, String), Option<String>>>, String> {
    let path = dependency_snapshot_inventory_path(snapshot_root);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_DEPENDENCY_SNAPSHOT_INVENTORY_BYTES as u64
    {
        return Ok(None);
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let inventory: VerifiedDependencyInventory = match serde_json::from_slice(&bytes) {
        Ok(inventory) => inventory,
        Err(_) => return Ok(None),
    };
    if inventory.schema_version != DEPENDENCY_SNAPSHOT_INVENTORY_SCHEMA_VERSION
        || inventory.toolchain_key != toolchain_key
        || inventory.lock_digest != lock_digest
        || inventory.tree_digest != tree_digest
        || inventory.inventory_digest.is_empty()
        || dependency_inventory_digest(&inventory)? != inventory.inventory_digest
    {
        return Ok(None);
    }
    let mut packages = BTreeMap::new();
    let mut previous_identity: Option<(&str, &str)> = None;
    for package in &inventory.packages {
        let identity = (package.name.as_str(), package.version.as_str());
        if package.name.trim().is_empty()
            || package.name.trim() != package.name
            || package.version.trim().is_empty()
            || package.version.trim() != package.version
            || package
                .license
                .as_ref()
                .is_some_and(|license| license.trim().is_empty() || license.trim() != license)
            || previous_identity.is_some_and(|previous| previous >= identity)
            || packages
                .insert(
                    (package.name.clone(), package.version.clone()),
                    package.license.clone(),
                )
                .is_some()
        {
            return Ok(None);
        }
        previous_identity = Some(identity);
    }
    Ok(Some(packages))
}

pub fn spdx_ref_for_package(name: &str, version: &str) -> String {
    let normalized = format!("{name}-{version}")
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' => ch,
            _ => '-',
        })
        .collect::<String>();
    let identity = format!("{name}\0{version}");
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    format!("SPDXRef-Package-{normalized}-{digest}")
}

pub fn installed_dependency_sbom(
    node_modules_root: &Path,
    binding: &local_apps::AppRuntimeProfileBinding,
    tree_sha256: &str,
) -> Result<Vec<u8>, String> {
    installed_dependency_sbom_with_inventory(node_modules_root, binding, tree_sha256, None)
}

pub fn installed_dependency_sbom_with_inventory(
    node_modules_root: &Path,
    binding: &local_apps::AppRuntimeProfileBinding,
    tree_sha256: &str,
    snapshot_inventory: Option<(&Path, &str)>,
) -> Result<Vec<u8>, String> {
    let toolchain_key = toolchain_for_binding(binding)
        .map_err(|error| error.to_string())?
        .key();
    let mut packages = BTreeMap::<(String, String), Option<String>>::new();
    let used_inventory = snapshot_inventory
        .and_then(|(snapshot_root, lock_digest)| {
            read_verified_dependency_inventory(
                snapshot_root,
                lock_digest,
                tree_sha256,
                toolchain_key,
            )
            .ok()
            .flatten()
        })
        .filter(|cached| !cached.is_empty())
        .map(|cached| {
            packages = cached;
        })
        .is_some();
    if !used_inventory {
        collect_installed_packages(node_modules_root, &mut packages)?;
    }
    if packages.is_empty() {
        return Err(format!(
            "dependency snapshot cannot be verified because {} contains no installed package manifests",
            node_modules_root.display()
        ));
    }
    let root_id = format!(
        "SPDXRef-LingXiRuntime-{}-r{}",
        binding.family.as_str(),
        binding.revision
    );
    let mut package_values = vec![canonicalize_json(Value::Object(Map::from_iter([
        ("SPDXID".to_string(), Value::String(root_id.clone())),
        (
            "name".to_string(),
            Value::String(format!(
                "LingXi Local App Installed Dependencies {} r{}",
                binding.family.as_str(),
                binding.revision
            )),
        ),
        (
            "versionInfo".to_string(),
            Value::String(format!("{}+{}", binding.contract_sha256, tree_sha256)),
        ),
        (
            "downloadLocation".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
        (
            "licenseConcluded".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
        (
            "licenseDeclared".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
        (
            "copyrightText".to_string(),
            Value::String("NOASSERTION".to_string()),
        ),
    ])))];
    let mut relationships = Vec::new();
    for ((name, version), license) in packages {
        let package_id = spdx_ref_for_package(&name, &version);
        let license = license.unwrap_or_else(|| "NOASSERTION".to_string());
        package_values.push(canonicalize_json(Value::Object(Map::from_iter([
            ("SPDXID".to_string(), Value::String(package_id.clone())),
            ("name".to_string(), Value::String(name)),
            ("versionInfo".to_string(), Value::String(version)),
            (
                "downloadLocation".to_string(),
                Value::String("NOASSERTION".to_string()),
            ),
            (
                "licenseConcluded".to_string(),
                Value::String("NOASSERTION".to_string()),
            ),
            ("licenseDeclared".to_string(), Value::String(license)),
            (
                "copyrightText".to_string(),
                Value::String("NOASSERTION".to_string()),
            ),
        ]))));
        relationships.push(canonicalize_json(Value::Object(Map::from_iter([
            ("spdxElementId".to_string(), Value::String(root_id.clone())),
            (
                "relationshipType".to_string(),
                Value::String("DEPENDS_ON".to_string()),
            ),
            ("relatedSpdxElement".to_string(), Value::String(package_id)),
        ]))));
    }
    let document = canonicalize_json(Value::Object(Map::from_iter([
        (
            "spdxVersion".to_string(),
            Value::String("SPDX-2.3".to_string()),
        ),
        (
            "dataLicense".to_string(),
            Value::String("CC0-1.0".to_string()),
        ),
        (
            "SPDXID".to_string(),
            Value::String("SPDXRef-DOCUMENT".to_string()),
        ),
        (
            "name".to_string(),
            Value::String(format!(
                "LingXi Installed Dependency SBOM {} r{}",
                binding.family.as_str(),
                binding.revision
            )),
        ),
        (
            "documentNamespace".to_string(),
            Value::String(format!(
                "https://lingxi.local/app-dependencies/{}/r{}/{}/{}",
                binding.family.as_str(),
                binding.revision,
                binding.contract_sha256,
                tree_sha256,
            )),
        ),
        (
            "creationInfo".to_string(),
            Value::Object(Map::from_iter([
                (
                    "created".to_string(),
                    Value::String("2026-08-27T00:00:00Z".to_string()),
                ),
                (
                    "creators".to_string(),
                    Value::Array(vec![Value::String(
                        "Tool: lingxi-local-app-installed-dependencies".to_string(),
                    )]),
                ),
            ])),
        ),
        (
            "documentDescribes".to_string(),
            Value::Array(vec![Value::String(root_id.clone())]),
        ),
        ("packages".to_string(), Value::Array(package_values)),
        ("relationships".to_string(), Value::Array(relationships)),
        ("files".to_string(), Value::Array(vec![])),
    ])));
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize dependency SBOM: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn validate_dependency_tree(root: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("inspect dependency tree {}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "dependency tree contains a symlink: {}",
            root.display()
        ));
    }
    if metadata.is_file() {
        return Ok(());
    }
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("resolve dependency tree {}: {error}", root.display()))?;
    validate_dependency_entry(root, &canonical_root)?;
    validate_dependency_lifecycle_scripts(root)
}

pub const FORBIDDEN_DEPENDENCY_LIFECYCLE_SCRIPTS: [&str; 4] =
    ["preinstall", "install", "postinstall", "prepare"];

pub const TRUSTED_TOOLCHAIN_NATIVE_BINDINGS: &[(&str, &str, &str)] = &[
    (
        "@rolldown/binding-linux-arm64-musl",
        "1.2.6",
        "rolldown-binding.linux-arm64-musl.node",
    ),
    (
        "@rolldown/binding-linux-x64-musl",
        "1.2.6",
        "rolldown-binding.linux-x64-musl.node",
    ),
    (
        "@rolldown/binding-linux-arm64-musl",
        "1.2.9",
        "rolldown-binding.linux-arm64-musl.node",
    ),
    (
        "@rolldown/binding-linux-x64-musl",
        "1.2.9",
        "rolldown-binding.linux-x64-musl.node",
    ),
    (
        "@rollup/rollup-linux-arm64-musl",
        "4.44.0",
        "rollup.linux-arm64-musl.node",
    ),
    (
        "@rollup/rollup-linux-x64-musl",
        "4.44.0",
        "rollup.linux-x64-musl.node",
    ),
    (
        "lightningcss-linux-arm64-musl",
        "1.33.0",
        "lightningcss.linux-arm64-musl.node",
    ),
    (
        "lightningcss-linux-x64-musl",
        "1.33.0",
        "lightningcss.linux-x64-musl.node",
    ),
];

pub const TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS: &[(&str, &str, &[&str])] = &[
    ("@modelcontextprotocol/ext-apps", "2.0.0", &["prepare"]),
    ("balanced-match", "4.0.4", &["prepare"]),
    ("brace-expansion", "5.0.9", &["prepare"]),
    ("brace-expansion", "5.0.12", &["prepare"]),
    ("dom-serializer", "2.0.0", &["prepare"]),
    ("domelementtype", "2.3.0", &["prepare"]),
    ("domhandler", "5.0.3", &["prepare"]),
    ("domutils", "3.2.2", &["prepare"]),
    ("entities", "4.5.0", &["prepare"]),
    ("eventsource", "3.0.7", &["prepare"]),
    ("html-dom-parser", "5.1.8", &["prepare"]),
    ("html-react-parser", "5.2.17", &["prepare"]),
    ("htmlparser2", "10.1.0", &["prepare"]),
    ("inline-style-parser", "0.2.7", &["prepare"]),
    ("lightningcss", "1.33.0", &["prepare"]),
    ("minimatch", "10.2.6", &["prepare"]),
    ("style-to-js", "1.1.21", &["prepare"]),
    ("style-to-object", "1.0.14", &["prepare"]),
    ("vite-plugin-singlefile", "2.3.3", &["prepare"]),
];

pub fn trusted_toolchain_lifecycle_scripts(
    package: &str,
    version: &str,
) -> Option<&'static [&'static str]> {
    TRUSTED_TOOLCHAIN_LIFECYCLE_SCRIPTS.iter().find_map(
        |(trusted_package, trusted_version, scripts)| {
            (*trusted_package == package && *trusted_version == version).then_some(*scripts)
        },
    )
}

pub fn dependency_package_path(package: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for part in package.split('/') {
        path.push(part);
    }
    path
}

pub fn validate_trusted_dependency_manifest(
    dependency_root: &Path,
    package: &str,
    version: &str,
) -> Result<(), String> {
    let manifest_path = dependency_root
        .join(dependency_package_path(package))
        .join("package.json");
    let metadata = std::fs::symlink_metadata(&manifest_path).map_err(|error| {
        format!(
            "inspect dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "dependency package manifest must be a regular file: {}",
            manifest_path.display()
        ));
    }
    let bytes = std::fs::read(&manifest_path).map_err(|error| {
        format!(
            "read dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "parse dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    if manifest.get("name").and_then(Value::as_str) != Some(package)
        || manifest.get("version").and_then(Value::as_str) != Some(version)
    {
        return Err(format!(
            "dependency package manifest {} does not match trusted package {}@{}",
            manifest_path.display(),
            package,
            version
        ));
    }
    Ok(())
}

pub fn trusted_dependency_lifecycle_script_path(
    dependency_root: &Path,
    manifest_path: &Path,
    package: &str,
    version: &str,
    script: &str,
) -> Result<bool, String> {
    let canonical_manifest = std::fs::canonicalize(manifest_path).map_err(|error| {
        format!(
            "canonicalize dependency package manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let relative = canonical_manifest
        .strip_prefix(dependency_root)
        .map_err(|_| {
            format!(
                "dependency package manifest {} is outside {}",
                canonical_manifest.display(),
                dependency_root.display()
            )
        })?;
    let expected = dependency_package_path(package).join("package.json");
    if relative != expected {
        return Ok(false);
    }
    validate_trusted_dependency_manifest(dependency_root, package, version)?;
    Ok(trusted_toolchain_lifecycle_scripts(package, version)
        .is_some_and(|allowed| allowed.contains(&script)))
}

pub fn trusted_dependency_native_binding_path(
    path: &Path,
    dependency_root: &Path,
) -> Result<bool, String> {
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "canonicalize dependency tree entry {}: {error}",
            path.display()
        )
    })?;
    let relative = canonical_path.strip_prefix(dependency_root).map_err(|_| {
        format!(
            "dependency tree entry {} is outside {}",
            canonical_path.display(),
            dependency_root.display()
        )
    })?;
    let mut rejected_manifest = None;
    for (package, version, file_name) in TRUSTED_TOOLCHAIN_NATIVE_BINDINGS {
        let expected = dependency_package_path(package).join(file_name);
        if relative == expected {
            match validate_trusted_dependency_manifest(dependency_root, package, version) {
                Ok(()) => return Ok(true),
                Err(error) => rejected_manifest = Some(error),
            }
        }
    }
    match rejected_manifest {
        Some(error) => Err(error),
        None => Ok(false),
    }
}

/// Reject package lifecycle hooks from a resolved dependency tree. The
/// resolver runs with scripts disabled, but retaining a hook in the snapshot
/// would let a later package-manager invocation execute it. Only explicitly
/// reviewed fixed-toolchain metadata is exempted.
pub fn validate_dependency_lifecycle_scripts(root: &Path) -> Result<(), String> {
    let dependency_root = root
        .canonicalize()
        .map_err(|error| format!("canonicalize dependency tree {}: {error}", root.display()))?;
    validate_dependency_lifecycle_scripts_from_root(&dependency_root, &dependency_root)
}

pub fn validate_dependency_lifecycle_scripts_from_root(
    dependency_root: &Path,
    current: &Path,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(current)
        .map_err(|error| format!("inspect dependency tree {}: {error}", current.display()))?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dependency tree entry is not regular: {}",
            current.display()
        ));
    }
    for entry in std::fs::read_dir(current)
        .map_err(|error| format!("read dependency tree {}: {error}", current.display()))?
    {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect dependency tree {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            validate_dependency_lifecycle_scripts_from_root(dependency_root, &path)?;
            continue;
        }
        if !metadata.is_file() || !installed_package_manifest(&path) {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "read dependency package manifest {}: {error}",
                path.display()
            )
        })?;
        let manifest: Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "parse dependency package manifest {}: {error}",
                path.display()
            )
        })?;
        let package = manifest
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("<unnamed>");
        let Some(scripts) = manifest.get("scripts").and_then(Value::as_object) else {
            continue;
        };
        let version = manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for script in FORBIDDEN_DEPENDENCY_LIFECYCLE_SCRIPTS {
            if scripts.contains_key(script) {
                if trusted_dependency_lifecycle_script_path(
                    dependency_root,
                    &path,
                    package,
                    version,
                    script,
                )? {
                    continue;
                }
                return Err(format!(
                    "dependency package {package} declares forbidden lifecycle script {script} in {}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

pub fn validate_dependency_entry(path: &Path, canonical_root: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect dependency tree {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        // A contained shim is legal; anything reaching outside the tree is not.
        // Traversal never descends THROUGH the link, so a link to a directory
        // inside the tree cannot make this recursion unbounded.
        let target = dependency_symlink_target(path, canonical_root)?;
        let path_is_native =
            path.extension().and_then(|extension| extension.to_str()) == Some("node");
        let target_is_native =
            target.extension().and_then(|extension| extension.to_str()) == Some("node");
        if path_is_native || target_is_native {
            return Err(format!(
                "dependency tree contains a native Node addon symlink: {}",
                path.display()
            ));
        }
        return Ok(());
    }
    if metadata.is_file() {
        if path.extension().and_then(|extension| extension.to_str()) == Some("node") {
            if trusted_dependency_native_binding_path(path, canonical_root)? {
                return Ok(());
            }
            return Err(format!(
                "dependency tree contains a native Node addon: {}",
                path.display()
            ));
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dependency tree entry is not regular: {}",
            path.display()
        ));
    }
    for entry in std::fs::read_dir(path)
        .map_err(|error| format!("read dependency tree {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        validate_dependency_entry(&entry.path(), canonical_root)?;
    }
    Ok(())
}

pub fn dependency_attestation(
    lock_digest: &str,
    tree_digest: &str,
    toolchain_key: &str,
) -> String {
    format!("{DEPENDENCY_SNAPSHOT_VERSION}\n{lock_digest}\n{toolchain_key}\n{tree_digest}\n")
}

pub fn dependency_tree_digest_from_marker(marker: &Path) -> Result<Option<String>, String> {
    let contents = match std::fs::read_to_string(marker) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "read dependency marker {}: {error}",
                marker.display()
            ))
        }
    };
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() != 4 || lines[3].is_empty() {
        return Ok(None);
    }
    Ok(Some(lines[3].to_string()))
}

pub fn dependency_tree_digest(root: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_dependency_files(root, Path::new(""), &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    digest.update((files.len() as u64).to_le_bytes());
    for (relative, path) in files {
        let relative = relative.as_bytes();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect dependency tree file {}: {error}", path.display()))?;
        // A shim is digested by its TARGET, tagged so it can never collide with
        // a regular file whose contents happen to be that same path text --
        // otherwise swapping `.bin/vite` between a link and a file would leave
        // the attestation unchanged.
        let (kind, bytes) = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path).map_err(|error| {
                format!("read dependency tree symlink {}: {error}", path.display())
            })?;
            (1u8, target.as_os_str().as_encoded_bytes().to_vec())
        } else {
            let bytes = std::fs::read(&path).map_err(|error| {
                format!("read dependency tree file {}: {error}", path.display())
            })?;
            (0u8, bytes)
        };
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative);
        digest.update([kind]);
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn collect_dependency_files(
    root: &Path,
    relative: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("inspect dependency tree {}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() {
        files.push((relative.to_string_lossy().into_owned(), root.to_path_buf()));
        return Ok(());
    }
    if metadata.is_file() {
        files.push((relative.to_string_lossy().into_owned(), root.to_path_buf()));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "dependency tree entry is not regular: {}",
            root.display()
        ));
    }
    for entry in std::fs::read_dir(root)
        .map_err(|error| format!("read dependency tree {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("read dependency tree entry: {error}"))?;
        let child_relative = if relative.as_os_str().is_empty() {
            PathBuf::from(entry.file_name())
        } else {
            relative.join(entry.file_name())
        };
        collect_dependency_files(&entry.path(), &child_relative, files)?;
    }
    Ok(())
}

pub fn make_dependency_files_read_only(root: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        // Leave the link alone: `set_permissions` FOLLOWS it, so chmod-ing here
        // would re-apply to the target that the walk already visits on its own,
        // and there is no portable `lchmod`. The link node carries no content
        // to protect -- its target is inside the tree and is made read-only in
        // its own right.
        return Ok(());
    }
    if metadata.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() & !0o222);
            std::fs::set_permissions(root, permissions)?;
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        make_dependency_files_read_only(&entry?.path())?;
    }
    Ok(())
}

/// Resolve a symlink and require that it lands inside `canonical_root`.
///
/// The invariant a dependency tree actually needs is that no link reaches
/// outside the tree -- the same rule `stage-local-app-runtime.py`'s
/// `validate_symlinks` already enforces for the staged runtime. Forbidding
/// links outright is stricter than the threat and rejects `node_modules/.bin`,
/// which `pnpm install` writes as relative shims for every package carrying a
/// `bin` field.
///
/// Resolution is strict: a shim whose target does not exist is rejected rather
/// than copied forward as a dangling entry that fails later at `vite` spawn
/// time with an unrelated message.
pub fn dependency_symlink_target(
    path: &Path,
    canonical_root: &Path,
) -> Result<PathBuf, String> {
    let resolved = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "dependency tree symlink does not resolve: {}: {error}",
            path.display()
        )
    })?;
    if !resolved.starts_with(canonical_root) {
        return Err(format!(
            "dependency tree symlink escapes the tree: {} -> {}",
            path.display(),
            resolved.display()
        ));
    }
    Ok(resolved)
}

pub fn clone_or_copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        // The ROOT being a link is still refused: it would make the whole tree
        // an alias for somewhere else, which is the escape this guards.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "dependency source symlink is forbidden: {}",
                source.display()
            ),
        ));
    }
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        return std::fs::copy(source, destination).map(|_| ());
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("dependency source is not regular: {}", source.display()),
        ));
    }
    let canonical_source = std::fs::canonicalize(source)?;
    if try_clone_tree(source, destination).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(destination);
    std::fs::create_dir_all(destination)?;
    copy_dependency_tree(source, destination, &canonical_source)
}

#[cfg(unix)]
pub fn recreate_dependency_symlink(target: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, destination)
}

#[cfg(not(unix))]
pub fn recreate_dependency_symlink(_target: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "dependency tree symlinks are unsupported on this platform",
    ))
}

pub fn copy_dependency_tree(
    source: &Path,
    destination: &Path,
    canonical_source_root: &Path,
) -> io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            // Containment is checked against the ORIGINAL root, not the
            // directory being walked, so `.bin/vite -> ../vite/bin/vite.js`
            // stays legal while `../../../etc/passwd` does not.
            dependency_symlink_target(&source_path, canonical_source_root)
                .map_err(io::Error::other)?;
            let target = std::fs::read_link(&source_path)?;
            if target.is_absolute() {
                // An absolute target resolves inside the tree only for as long
                // as the tree stays at this path; copying it into the snapshot
                // would silently re-point at the source app's workspace.
                return Err(io::Error::other(format!(
                    "dependency tree symlink must be relative: {} -> {}",
                    source_path.display(),
                    target.display()
                )));
            }
            recreate_dependency_symlink(&target, &destination_path)?;
            continue;
        }
        if metadata.is_dir() {
            std::fs::create_dir_all(&destination_path)?;
            copy_dependency_tree(&source_path, &destination_path, canonical_source_root)?;
        } else if metadata.is_file() {
            std::fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "dependency source is not regular: {}",
                    source_path.display()
                ),
            ));
        }
    }
    Ok(())
}

pub fn try_clone_tree(source: &Path, destination: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("cp");
    command.stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(target_os = "macos")]
    command.args(["-R", "-c"]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    command.args(["-R", "--reflink=always"]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "copy-on-write clone is unavailable on this platform",
    ));
    command.arg("--").arg(source).arg(destination);
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("copy-on-write clone exited with {status}"),
        ));
    }
    validate_dependency_tree(destination).map_err(io::Error::other)
}
