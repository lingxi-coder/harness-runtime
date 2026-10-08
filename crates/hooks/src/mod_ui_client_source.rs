//! Host-side discovery and preparation of Desktop Client surface modules.
//!
//! The plugin entry is read as source only. Its static import graph is parsed
//! without evaluating plugin code, Client module literals are normalized
//! against the plugin root, and all source files needed by the resulting
//! surface graphs are transformed in one Bun process. The worker receives
//! these immutable source snapshots and never resolves a plugin filesystem
//! path itself.

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const SURFACE_RUNTIME_KEY: &str = "claude:surface-runtime";
const HOOKS_TYPES_KEY: &str = "claude:hooks-types";
const SURFACE_PREFIX: &str = "surface:///";
const UNLINKED_PREFIX: &str = "surface-unlinked:///";
const MAX_SOURCE_FILES: usize = 256;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const SOURCE_COMPILER_BUDGET: Duration = Duration::from_secs(10);

const BUN_TRANSPILE_PROGRAM: &str = r#"
const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString('utf8'));
const jsxHeader = '/** @jsxRuntime classic */\n/** @jsx h */\n/** @jsxFrag Fragment */\n';
const output = [];
for (const file of request.files) {
  let source = file.source;
  if (file.loader === 'jsx' || file.loader === 'tsx') source = jsxHeader + source;
  if (file.loader !== 'js') {
    source = new Bun.Transpiler({ loader: file.loader, macro: false }).transformSync(source);
  }
  output.push({ path: file.path, source });
}
process.stdout.write(JSON.stringify({ version: Bun.version, files: output }));
"#;

/// Runtime helper and hooks-type module sources supplied by the host. Their
/// exact bytes are included in the manifest hash; a functional host port does
/// not imply byte identity with Native's embedded assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientSurfaceRuntimeSources {
    pub(super) surface_runtime: String,
    pub(super) hooks_types: String,
}

/// Hook module files transformed from the same load-time source snapshot used
/// by Client discovery. Paths remain plugin-local identities; imports preserve
/// their original specifiers and are resolved through the explicit link rows.
/// The `claude:hooks-types` builtin is included as an explicit linked source
/// whenever the plugin graph imports `claude-code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct HookSourceFile {
    pub(super) file: String,
    pub(super) source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct PreparedHookSourceGraph {
    pub(super) entry: String,
    pub(super) files: Vec<HookSourceFile>,
    pub(super) links: Vec<ClientSourceLink>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSourceGraphResponse<'a> {
    entry: &'a str,
    files: &'a [HookSourceFile],
    links: &'a [ClientSourceLink],
    compiler_version: &'a str,
}

/// A single plugin-load source snapshot. Hook source is always present, even
/// for plugins that do not reference a Client surface module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparedPluginSources {
    pub(super) hooks: PreparedHookSourceGraph,
    pub(super) clients: Option<PreparedClientModules>,
    pub(super) bun_version: String,
}

impl PreparedPluginSources {
    pub(super) fn hook_worker_graph(&self) -> Result<serde_json::Value, ClientSourceError> {
        serde_json::to_value(HookSourceGraphResponse {
            entry: &self.hooks.entry,
            files: &self.hooks.files,
            links: &self.hooks.links,
            compiler_version: &self.bun_version,
        })
        .map_err(|error| ClientSourceError::Manifest(error.to_string()))
    }
}

/// A local static import resolved from one file to another in the plugin tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ClientSourceLink {
    pub(super) from: String,
    pub(super) spelled: String,
    pub(super) file: String,
}

/// One non-entry source file linked into a surface module graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ClientLinkedSource {
    pub(super) file: String,
    pub(super) source: String,
}

/// Native WEt/jas row with transformed source snapshots for the worker VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClientSurfaceModule {
    pub(super) module: String,
    pub(super) module_path: String,
    pub(super) component: String,
    pub(super) source: String,
    pub(super) linked: Vec<ClientLinkedSource>,
    pub(super) links: Vec<ClientSourceLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ClientManifestModule {
    pub(super) module: String,
    pub(super) entry: String,
    pub(super) component: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ClientManifestFile {
    pub(super) key: String,
    pub(super) source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClientModuleLimits {
    pub(super) nodes: u64,
    pub(super) depth: u64,
    pub(super) chars: u64,
    pub(super) values: u64,
    pub(super) data_depth: u64,
}

impl Default for ClientModuleLimits {
    fn default() -> Self {
        Self {
            nodes: 20_000,
            depth: 32,
            chars: 100_000,
            values: 20_000,
            data_depth: 32,
        }
    }
}

/// The content-addressed result returned by Native `EO` (plugin is attached
/// by `for_plugin` after preparation, and is intentionally outside the hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ClientModuleManifest {
    pub(super) hash: String,
    pub(super) modules: Vec<ClientManifestModule>,
    pub(super) runtime: String,
    pub(super) limits: ClientModuleLimits,
    pub(super) files: Vec<ClientManifestFile>,
}

#[derive(Serialize)]
struct ClientModuleManifestResponse<'a> {
    plugin: &'a str,
    hash: &'a str,
    modules: &'a [ClientManifestModule],
    runtime: &'a str,
    limits: &'a ClientModuleLimits,
    files: &'a [ClientManifestFile],
}

#[derive(Serialize)]
struct ClientModuleGraphResponse<'a> {
    modules: &'a [ClientSurfaceModule],
    manifest: &'a ClientModuleManifest,
}

#[derive(Serialize)]
struct ManifestPreimage<'a> {
    files: &'a [ClientManifestFile],
    modules: &'a [ClientManifestModule],
    limits: &'a ClientModuleLimits,
}

/// Prepared immutable snapshots for the native `surfaceModules` list and the
/// `clientModule` manifest endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparedClientModules {
    pub(super) surface_modules: Vec<ClientSurfaceModule>,
    pub(super) manifest: ClientModuleManifest,
}

impl PreparedClientModules {
    /// Build the Native manifest envelope for one loaded plugin identity.
    pub(super) fn for_plugin(&self, plugin: &str) -> Result<serde_json::Value, ClientSourceError> {
        serde_json::to_value(ClientModuleManifestResponse {
            plugin,
            hash: &self.manifest.hash,
            modules: &self.manifest.modules,
            runtime: &self.manifest.runtime,
            limits: &self.manifest.limits,
            files: &self.manifest.files,
        })
        .map_err(|error| ClientSourceError::Manifest(error.to_string()))
    }

    /// Build the worker VM input. Module source bodies and manifest files come
    /// from the same prepared snapshot, so the VM cannot observe a later disk
    /// edit between discovery and mount.
    pub(super) fn worker_module_graph(&self) -> Result<serde_json::Value, ClientSourceError> {
        serde_json::to_value(ClientModuleGraphResponse {
            modules: &self.surface_modules,
            manifest: &self.manifest,
        })
        .map_err(|error| ClientSourceError::Manifest(error.to_string()))
    }
}

#[derive(Debug, Error)]
pub(super) enum ClientSourceError {
    #[error("Client source graph: {0}")]
    Source(String),
    #[error("Client source transform failed: {0}")]
    Transform(String),
    #[error("Client source manifest could not be encoded: {0}")]
    Manifest(String),
}

#[derive(Debug, Clone)]
struct ResolvedSourceFile {
    /// Lexical path under the caller-visible plugin root; this is the graph key.
    file: PathBuf,
    /// Canonical path used only for root-boundary and file identity checks.
    real: PathBuf,
}

#[derive(Debug, Clone)]
struct RawSourceFile {
    file: PathBuf,
    source: String,
}

#[derive(Debug, Clone)]
struct RawModuleGraph {
    entry: PathBuf,
    files: Vec<RawSourceFile>,
    links: Vec<ClientSourceLink>,
}

#[derive(Debug, Clone)]
struct ClientModuleLiteral {
    importer: PathBuf,
    spelled: String,
    start_byte: usize,
    end_byte: usize,
}

#[derive(Debug, Clone, Serialize)]
struct BunSourceInput {
    path: String,
    loader: &'static str,
    source: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct BunSourceOutput {
    path: String,
    source: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct BunTransformOutput {
    version: String,
    files: Vec<BunSourceOutput>,
}

struct TransformedSourceGraph {
    version: String,
    sources: HashMap<PathBuf, String>,
}

/// Read the plugin hook and static import graphs, discover literal Client
/// entries, transform the complete snapshot in one Bun process, and calculate
/// the Client manifest hash over insertion-ordered JSON.
pub(super) async fn prepare_plugin_sources(
    plugin_root: &Path,
    plugin_entry: &Path,
    bun_executable: Option<&Path>,
    runtime_sources: &ClientSurfaceRuntimeSources,
) -> Result<PreparedPluginSources, ClientSourceError> {
    let lexical_root = absolute_normalized(plugin_root)?;
    let real_root = fs::canonicalize(plugin_root).map_err(|error| {
        ClientSourceError::Source(format!(
            "could not resolve plugin root {}: {error}",
            plugin_root.display()
        ))
    })?;
    let entry = resolve_entry_path(&lexical_root, &real_root, plugin_entry)?;
    let mut main_graph = read_module_graph(&lexical_root, &real_root, entry, true)?;

    let mut replacements_by_importer = HashMap::<PathBuf, Vec<(usize, usize, String)>>::new();
    let mut client_entries = Vec::new();
    for file in &main_graph.files {
        for literal in scan_client_literals(&file.file, &file.source)? {
            let normalized = normalize_client_module(
                &lexical_root,
                &real_root,
                &literal.importer,
                &literal.spelled,
            )?;
            let replacement = serde_json::to_string(&normalized.0)
                .map_err(|error| ClientSourceError::Source(error.to_string()))?;
            replacements_by_importer
                .entry(literal.importer.clone())
                .or_default()
                .push((literal.start_byte, literal.end_byte, replacement));
            if !client_entries
                .iter()
                .any(|(module, _): &(String, ResolvedSourceFile)| module == &normalized.0)
            {
                client_entries.push(normalized);
            }
        }
    }
    for file in &mut main_graph.files {
        if let Some(replacements) = replacements_by_importer.remove(&file.file) {
            file.source = apply_source_replacements(&file.source, replacements)?;
        }
    }
    client_entries.sort_by(|left, right| js_string_cmp(&left.0, &right.0));
    let mut surface_graphs = Vec::with_capacity(client_entries.len());
    for (_, entry) in &client_entries {
        surface_graphs.push(read_module_graph(
            &lexical_root,
            &real_root,
            entry.clone(),
            false,
        )?);
    }

    let all_graphs = std::iter::once(&main_graph).chain(surface_graphs.iter());
    let mut input_by_file = HashMap::<PathBuf, BunSourceInput>::new();
    let mut source_bytes = 0usize;
    for graph in all_graphs {
        for file in &graph.files {
            if input_by_file.contains_key(&file.file) {
                continue;
            }
            source_bytes = source_bytes.saturating_add(file.source.len());
            if source_bytes > MAX_SOURCE_BYTES {
                return Err(ClientSourceError::Source(format!(
                    "source graphs exceed {MAX_SOURCE_BYTES} bytes"
                )));
            }
            let path = path_string(&file.file)?;
            input_by_file.insert(
                file.file.clone(),
                BunSourceInput {
                    path,
                    loader: source_loader(&file.file)?,
                    source: file.source.clone(),
                },
            );
        }
    }
    if input_by_file.len() > MAX_SOURCE_FILES {
        return Err(ClientSourceError::Source(format!(
            "source graphs exceed {MAX_SOURCE_FILES} files"
        )));
    }
    let mut compile_inputs = input_by_file.into_values().collect::<Vec<_>>();
    compile_inputs.sort_by(|left, right| js_string_cmp(&left.path, &right.path));
    let transformed = transform_source_graph(bun_executable, &compile_inputs).await?;

    let mut hook_files = main_graph
        .files
        .iter()
        .map(|file| {
            Ok(HookSourceFile {
                file: path_string(&file.file)?,
                source: transformed_source(&transformed.sources, &file.file)?.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, ClientSourceError>>()?;
    if main_graph
        .links
        .iter()
        .any(|link| link.file == HOOKS_TYPES_KEY)
        && !hook_files.iter().any(|file| file.file == HOOKS_TYPES_KEY)
    {
        hook_files.push(HookSourceFile {
            file: HOOKS_TYPES_KEY.into(),
            source: runtime_sources.hooks_types.clone(),
        });
    }
    let hook_graph_bytes = hook_files
        .iter()
        .map(|file| file.source.len())
        .sum::<usize>();
    if hook_files.len() > MAX_SOURCE_FILES || hook_graph_bytes > MAX_SOURCE_BYTES {
        return Err(ClientSourceError::Source(
            "prepared hook source graph exceeds its file or byte limit".into(),
        ));
    }
    let hooks = PreparedHookSourceGraph {
        entry: path_string(&main_graph.entry)?,
        files: hook_files,
        links: main_graph.links.clone(),
    };

    let mut surface_modules = Vec::with_capacity(surface_graphs.len());
    for ((module, module_entry), graph) in client_entries.iter().zip(surface_graphs.iter()) {
        let entry_source = transformed_source(&transformed.sources, &graph.entry)?;
        let component = select_component(entry_source)?;
        let mut linked = Vec::new();
        for file in graph.files.iter().filter(|file| file.file != graph.entry) {
            linked.push(ClientLinkedSource {
                file: path_string(&file.file)?,
                source: transformed_source(&transformed.sources, &file.file)?.to_owned(),
            });
        }
        let links = graph.links.clone();
        surface_modules.push(ClientSurfaceModule {
            module: module.clone(),
            module_path: path_string(&module_entry.file)?,
            component,
            source: entry_source.to_owned(),
            linked,
            links,
        });
    }

    let clients = if surface_modules.is_empty() {
        None
    } else {
        let manifest = make_manifest(&lexical_root, &surface_modules, runtime_sources)?;
        Some(PreparedClientModules {
            surface_modules,
            manifest,
        })
    };
    Ok(PreparedPluginSources {
        hooks,
        clients,
        bun_version: transformed.version,
    })
}

fn absolute_normalized(path: &Path) -> Result<PathBuf, ClientSourceError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| ClientSourceError::Source(error.to_string()))?
            .join(path)
    };
    Ok(normalize_path(&absolute))
}

fn resolve_entry_path(
    lexical_root: &Path,
    real_root: &Path,
    entry: &Path,
) -> Result<ResolvedSourceFile, ClientSourceError> {
    let lexical = if entry.is_absolute() {
        let entry = normalize_path(entry);
        if let Ok(relative) = entry.strip_prefix(lexical_root) {
            normalize_path(&lexical_root.join(relative))
        } else if let Ok(relative) = entry.strip_prefix(real_root) {
            normalize_path(&lexical_root.join(relative))
        } else {
            return Err(ClientSourceError::Source(format!(
                "plugin entry leaves plugin root: {}",
                entry.display()
            )));
        }
    } else {
        normalize_path(&lexical_root.join(entry))
    };
    ensure_lexically_inside(lexical_root, &lexical)?;
    resolve_real_file(lexical_root, real_root, lexical)
}

fn read_module_graph(
    lexical_root: &Path,
    real_root: &Path,
    entry: ResolvedSourceFile,
    allow_external_imports: bool,
) -> Result<RawModuleGraph, ClientSourceError> {
    ensure_lexically_inside(lexical_root, &entry.file)?;
    ensure_real_inside(real_root, &entry.real)?;

    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut pending = VecDeque::from([entry.clone()]);
    let mut real_to_graph_file = HashMap::<PathBuf, PathBuf>::new();
    real_to_graph_file.insert(entry.real.clone(), entry.file.clone());
    let mut visited = HashSet::<PathBuf>::new();
    let mut total_bytes = 0usize;

    while let Some(resolved) = pending.pop_front() {
        if !visited.insert(resolved.real.clone()) {
            continue;
        }
        if files.len() >= MAX_SOURCE_FILES {
            return Err(ClientSourceError::Source(format!(
                "module graph exceeds {MAX_SOURCE_FILES} files"
            )));
        }
        let source = fs::read_to_string(&resolved.real).map_err(|error| {
            ClientSourceError::Source(format!(
                "could not read source {}: {error}",
                resolved.file.display()
            ))
        })?;
        total_bytes = total_bytes.saturating_add(source.len());
        if total_bytes > MAX_SOURCE_BYTES {
            return Err(ClientSourceError::Source(format!(
                "module graph exceeds {MAX_SOURCE_BYTES} bytes"
            )));
        }

        let imports = source_imports(&source)?;
        for spelled in &imports {
            if is_surface_builtin(spelled) {
                links.push(ClientSourceLink {
                    from: path_string(&resolved.file)?,
                    spelled: spelled.clone(),
                    file: builtin_module_key(spelled).to_owned(),
                });
                continue;
            }
            if !is_relative_specifier(spelled) {
                if allow_external_imports {
                    links.push(ClientSourceLink {
                        from: path_string(&resolved.file)?,
                        spelled: spelled.clone(),
                        file: spelled.clone(),
                    });
                    continue;
                }
                return Err(ClientSourceError::Source(format!(
                    "only relative imports are supported in Client modules (got {spelled:?})"
                )));
            }
            let target = resolve_import(lexical_root, real_root, &resolved.file, spelled)?;
            let graph_file = real_to_graph_file
                .entry(target.real.clone())
                .or_insert_with(|| target.file.clone())
                .clone();
            links.push(ClientSourceLink {
                from: path_string(&resolved.file)?,
                spelled: spelled.clone(),
                file: path_string(&graph_file)?,
            });
            if !visited.contains(&target.real) {
                pending.push_back(ResolvedSourceFile {
                    file: graph_file,
                    real: target.real,
                });
            }
        }
        files.push(RawSourceFile {
            file: resolved.file,
            source,
        });
    }

    Ok(RawModuleGraph {
        entry: entry.file,
        files,
        links,
    })
}

fn resolve_import(
    lexical_root: &Path,
    real_root: &Path,
    importer: &Path,
    specifier: &str,
) -> Result<ResolvedSourceFile, ClientSourceError> {
    if !is_relative_specifier(specifier) {
        return Err(ClientSourceError::Source(format!(
            "only relative imports are supported in Client modules (got {specifier:?})"
        )));
    }
    let parent = importer
        .parent()
        .ok_or_else(|| ClientSourceError::Source("importer has no parent directory".into()))?;
    let mut base = normalize_path(&parent.join(specifier));
    if matches!(specifier, "." | ".." | "./" | "../") {
        base = base.join("index");
    }
    ensure_lexically_inside(lexical_root, &base)?;
    for candidate in import_candidates(&base) {
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                let resolved = resolve_real_file(lexical_root, real_root, candidate)?;
                return Ok(resolved);
            }
            Ok(_) => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(ClientSourceError::Source(format!(
                    "could not stat import {specifier:?} from {}: {error}",
                    importer.display()
                )));
            }
        }
    }
    Err(ClientSourceError::Source(format!(
        "cannot resolve import {specifier:?} from {}",
        importer.display()
    )))
}

fn import_candidates(base: &Path) -> Vec<PathBuf> {
    // This order mirrors Native 2.1.291's rr(): keep the explicit specifier first,
    // try source replacements for known JS-family suffixes, then append each
    // supported suffix and its index form in extension order.
    let replacement_extensions = match base.extension().and_then(OsStr::to_str) {
        Some("js") => [Some("ts"), Some("tsx"), None],
        Some("jsx") => [Some("tsx"), None, None],
        Some("mjs") => [Some("mts"), None, None],
        Some("cjs") => [Some("cts"), None, None],
        _ => [None, None, None],
    };
    let extensions = ["ts", "tsx", "jsx", "js", "mjs", "cjs", "mts", "cts"];
    let mut candidates = Vec::with_capacity(1 + 3 + extensions.len() * 2);
    candidates.push(base.to_path_buf());
    for extension in replacement_extensions.into_iter().flatten() {
        candidates.push(base.with_extension(extension));
    }
    for extension in extensions {
        let mut appended = base.as_os_str().to_os_string();
        appended.push(format!(".{extension}"));
        candidates.push(PathBuf::from(appended));
        candidates.push(base.join(format!("index.{extension}")));
    }
    candidates
}

fn is_relative_specifier(specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
}

fn is_surface_builtin(specifier: &str) -> bool {
    matches!(
        specifier,
        SURFACE_RUNTIME_KEY | HOOKS_TYPES_KEY | "claude-code"
    )
}

fn builtin_module_key(specifier: &str) -> &str {
    if specifier == "claude-code" {
        HOOKS_TYPES_KEY
    } else {
        specifier
    }
}

fn resolve_real_file(
    lexical_root: &Path,
    real_root: &Path,
    lexical: PathBuf,
) -> Result<ResolvedSourceFile, ClientSourceError> {
    ensure_lexically_inside(lexical_root, &lexical)?;
    let real = fs::canonicalize(&lexical).map_err(|error| {
        ClientSourceError::Source(format!(
            "could not resolve source {}: {error}",
            lexical.display()
        ))
    })?;
    ensure_real_inside(real_root, &real)?;
    if !real.is_file() {
        return Err(ClientSourceError::Source(format!(
            "source is not a file: {}",
            lexical.display()
        )));
    }
    Ok(ResolvedSourceFile {
        file: lexical,
        real,
    })
}

fn ensure_lexically_inside(root: &Path, path: &Path) -> Result<(), ClientSourceError> {
    if path.strip_prefix(root).is_err() {
        return Err(ClientSourceError::Source(format!(
            "source path leaves the plugin root: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_real_inside(root: &Path, real: &Path) -> Result<(), ClientSourceError> {
    if real.strip_prefix(root).is_err() {
        return Err(ClientSourceError::Source(format!(
            "resolved source leaves the plugin root: {}",
            real.display()
        )));
    }
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn normalize_client_module(
    lexical_root: &Path,
    real_root: &Path,
    importer: &Path,
    spelled: &str,
) -> Result<(String, ResolvedSourceFile), ClientSourceError> {
    if !is_relative_specifier(spelled) {
        return Err(ClientSourceError::Source(format!(
            "Client module {spelled:?} is not relative; spell the surface module path from this file"
        )));
    }
    let extension = Path::new(spelled).extension().and_then(OsStr::to_str);
    if !matches!(extension, Some("js" | "jsx" | "ts" | "tsx")) {
        return Err(ClientSourceError::Source(format!(
            "Client module {spelled:?} must end in .js, .jsx, .ts, or .tsx"
        )));
    }
    let parent = importer
        .parent()
        .ok_or_else(|| ClientSourceError::Source("Client source has no parent directory".into()))?;
    let lexical = normalize_path(&parent.join(spelled));
    ensure_lexically_inside(lexical_root, &lexical)?;
    let relative = lexical
        .strip_prefix(lexical_root)
        .map_err(|_| ClientSourceError::Source("Client module leaves plugin root".into()))?;
    let module = relative_path_key(relative)?;
    let resolved = resolve_real_file(lexical_root, real_root, lexical)?;
    Ok((module, resolved))
}

fn relative_path_key(path: &Path) -> Result<String, ClientSourceError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(ClientSourceError::Source(
                "normalized plugin-relative module path is not normal".into(),
            ));
        };
        let part = part
            .to_str()
            .ok_or_else(|| ClientSourceError::Source("module path is not UTF-8".into()))?;
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn path_string(path: &Path) -> Result<String, ClientSourceError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ClientSourceError::Source("source path is not UTF-8".into()))
}

fn source_loader(path: &Path) -> Result<&'static str, ClientSourceError> {
    match path.extension().and_then(OsStr::to_str) {
        Some("ts" | "mts" | "cts") => Ok("ts"),
        Some("tsx") => Ok("tsx"),
        Some("jsx") => Ok("jsx"),
        Some("js" | "mjs" | "cjs") => Ok("js"),
        Some(other) => Err(ClientSourceError::Source(format!(
            "unsupported source extension .{other}"
        ))),
        None => Err(ClientSourceError::Source(
            "source file has no extension".into(),
        )),
    }
}

async fn transform_source_graph(
    bun_executable: Option<&Path>,
    files: &[BunSourceInput],
) -> Result<TransformedSourceGraph, ClientSourceError> {
    if files.is_empty() {
        return Ok(TransformedSourceGraph {
            version: "not-run".into(),
            sources: HashMap::new(),
        });
    }
    let configured = std::env::var_os("LINGXI_MOD_BUN_EXECUTABLE");
    let executable = bun_executable
        .or_else(|| configured.as_deref().map(Path::new))
        .unwrap_or_else(|| Path::new("bun"));
    let input = serde_json::to_vec(&BunTranspileRequest { files })
        .map_err(|error| ClientSourceError::Transform(error.to_string()))?;
    let started = Instant::now();
    let mut child = Command::new(executable)
        .arg("-e")
        .arg(BUN_TRANSPILE_PROGRAM)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            ClientSourceError::Transform(format!(
                "could not start Bun source compiler {}: {error}",
                executable.display()
            ))
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        ClientSourceError::Transform("Bun source compiler stdin was unavailable".into())
    })?;
    tokio::time::timeout(SOURCE_COMPILER_BUDGET, stdin.write_all(&input))
        .await
        .map_err(|_| {
            ClientSourceError::Transform(format!(
                "Bun source transform exceeded {} ms while sending input",
                SOURCE_COMPILER_BUDGET.as_millis()
            ))
        })?
        .map_err(|error| ClientSourceError::Transform(error.to_string()))?;
    drop(stdin);
    let remaining = SOURCE_COMPILER_BUDGET.saturating_sub(started.elapsed());
    let output = tokio::time::timeout(remaining, child.wait_with_output())
        .await
        .map_err(|_| {
            ClientSourceError::Transform(format!(
                "Bun source transform exceeded {} ms",
                SOURCE_COMPILER_BUDGET.as_millis()
            ))
        })?
        .map_err(|error| ClientSourceError::Transform(error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ClientSourceError::Transform(format!(
            "Bun source transform exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    let output: BunTransformOutput = serde_json::from_slice(&output.stdout)
        .map_err(|error| ClientSourceError::Transform(format!("invalid Bun output: {error}")))?;
    if output.files.len() != files.len() {
        return Err(ClientSourceError::Transform(format!(
            "Bun transformed {} files, expected {}",
            output.files.len(),
            files.len()
        )));
    }
    if output.version.is_empty() {
        return Err(ClientSourceError::Transform(
            "Bun source transform did not report a compiler version".into(),
        ));
    }
    let expected_paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<HashSet<_>>();
    let mut transformed = HashMap::with_capacity(output.files.len());
    for output in output.files {
        if !expected_paths.contains(output.path.as_str())
            || transformed
                .insert(PathBuf::from(&output.path), output.source)
                .is_some()
        {
            return Err(ClientSourceError::Transform(format!(
                "Bun source transform returned an unexpected or duplicate path: {}",
                output.path
            )));
        }
    }
    Ok(TransformedSourceGraph {
        version: output.version,
        sources: transformed,
    })
}

#[derive(Serialize)]
struct BunTranspileRequest<'a> {
    files: &'a [BunSourceInput],
}

fn transformed_source<'a>(
    transformed: &'a HashMap<PathBuf, String>,
    path: &Path,
) -> Result<&'a str, ClientSourceError> {
    transformed.get(path).map(String::as_str).ok_or_else(|| {
        ClientSourceError::Transform(format!(
            "transformed source snapshot missing {}",
            path.display()
        ))
    })
}

fn source_imports(source: &str) -> Result<Vec<String>, ClientSourceError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| ClientSourceError::Source(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ClientSourceError::Source("source parser returned no tree".into()))?;
    let bytes = source.as_bytes();
    let mut imports = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "import_statement" | "export_statement") {
            if let Some(import) = node.child_by_field_name("source") {
                if let Some(value) = js_string_value(import, bytes)? {
                    imports.push(value);
                }
            }
        } else if is_dynamic_import(node) {
            if let Some(argument) = node
                .child_by_field_name("arguments")
                .and_then(|arguments| arguments.named_child(0))
            {
                if let Some(value) = js_string_value(argument, bytes)? {
                    imports.push(value);
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    imports.reverse();
    Ok(imports)
}

fn scan_client_literals(
    importer: &Path,
    source: &str,
) -> Result<Vec<ClientModuleLiteral>, ClientSourceError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| ClientSourceError::Source(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ClientSourceError::Source("source parser returned no tree".into()))?;
    let bytes = source.as_bytes();
    let mut clients = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if node
                    .child_by_field_name("name")
                    .is_some_and(|name| source_text(name, bytes) == "Client")
                {
                    let (spelled, start_byte, end_byte) = jsx_client_module(node, bytes)?;
                    clients.push(ClientModuleLiteral {
                        importer: importer.to_path_buf(),
                        spelled,
                        start_byte,
                        end_byte,
                    });
                }
            }
            "call_expression" => {
                if let Some(props) = client_call_props(node, bytes) {
                    let (spelled, start_byte, end_byte) = object_client_module(props, bytes)?;
                    clients.push(ClientModuleLiteral {
                        importer: importer.to_path_buf(),
                        spelled,
                        start_byte,
                        end_byte,
                    });
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    Ok(clients)
}

fn is_dynamic_import(node: tree_sitter::Node<'_>) -> bool {
    node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| function.kind() == "import")
}

fn client_call_props<'tree>(
    call: tree_sitter::Node<'tree>,
    source: &[u8],
) -> Option<tree_sitter::Node<'tree>> {
    let function = call.child_by_field_name("function")?;
    let arguments = call.child_by_field_name("arguments")?;
    let named = named_children(arguments);
    if node_is_client_reference(function, source) {
        return Some(*named.first()?);
    }
    if function.kind() == "identifier" && source_text(function, source) == "h" {
        let first = *named.first()?;
        if first.kind() != "spread_element" && node_is_client_reference(first, source) {
            return Some(*named.get(1)?);
        }
    }
    None
}

fn node_is_client_reference(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "identifier" => source_text(node, source) == "Client",
        "string" => js_string_value(node, source)
            .ok()
            .flatten()
            .is_some_and(|value| value == "Client"),
        "member_expression" => node
            .child_by_field_name("property")
            .is_some_and(|property| {
                !node.child_by_field_name("optional_chain").is_some()
                    && property.kind() == "property_identifier"
                    && source_text(property, source) == "Client"
            }),
        _ => false,
    }
}

fn jsx_client_module(
    element: tree_sitter::Node<'_>,
    source: &[u8],
) -> Result<(String, usize, usize), ClientSourceError> {
    let mut module = None;
    for attribute in children_of_kind(element, "jsx_attribute") {
        let children = named_children(attribute);
        let Some(name) = children.first().copied() else {
            continue;
        };
        if source_text(name, source) != "module" {
            continue;
        }
        let Some(value) = children.get(1).copied() else {
            return Err(ClientSourceError::Source(
                "Client module must be a string literal".into(),
            ));
        };
        let literal = jsx_attribute_string(value, source).ok_or_else(|| {
            ClientSourceError::Source("Client module must be a string literal".into())
        })?;
        module = Some(literal);
    }
    module.ok_or_else(|| {
        ClientSourceError::Source(
            "Client names no module: give it module, the surface module's path as a string literal"
                .into(),
        )
    })
}

fn jsx_attribute_string(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<(String, usize, usize)> {
    if node.kind() == "string" || node.kind() == "template_string" {
        return Some((
            js_string_value(node, source).ok().flatten()?,
            node.start_byte(),
            node.end_byte(),
        ));
    }
    if node.kind() == "jsx_expression" {
        let value = named_children(node).into_iter().next()?;
        return Some((
            js_string_value(value, source).ok().flatten()?,
            value.start_byte(),
            value.end_byte(),
        ));
    }
    None
}

fn object_client_module(
    props: tree_sitter::Node<'_>,
    source: &[u8],
) -> Result<(String, usize, usize), ClientSourceError> {
    if props.kind() != "object" {
        return Err(ClientSourceError::Source(format!(
            "Client takes its props as an object literal (got {})",
            props.kind()
        )));
    }
    let mut module: Option<(tree_sitter::Node<'_>, bool)> = None;
    for property in named_children(props) {
        if property.kind() == "spread_element" {
            if let Some((node, _)) = module {
                module = Some((node, true));
            }
            continue;
        }
        if property.kind() != "pair" {
            continue;
        }
        let Some(key) = property.child_by_field_name("key") else {
            continue;
        };
        if property_has_computed_key(property, source)
            || !js_property_name_is(key, "module", source)
        {
            continue;
        }
        let Some(value) = property.child_by_field_name("value") else {
            continue;
        };
        module = Some((value, false));
    }
    let Some((value, spread_after)) = module else {
        return Err(ClientSourceError::Source(
            "Client names no module: give it module, the surface module's path as a string literal"
                .into(),
        ));
    };
    if spread_after {
        return Err(ClientSourceError::Source(
            "a spread after module in Client(...) could replace it: put module after the spread"
                .into(),
        ));
    }
    let spelled = js_string_value(value, source)?.ok_or_else(|| {
        let location = value.start_position().row + 1;
        ClientSourceError::Source(format!(
            "Client module must be a string literal on line {location}"
        ))
    })?;
    Ok((spelled, value.start_byte(), value.end_byte()))
}

fn apply_source_replacements(
    source: &str,
    mut replacements: Vec<(usize, usize, String)>,
) -> Result<String, ClientSourceError> {
    replacements.sort_by_key(|(start, end, _)| (*start, *end));
    let mut previous_end = 0;
    for (start, end, _) in &replacements {
        if start < &previous_end
            || start > end
            || *end > source.len()
            || !source.is_char_boundary(*start)
            || !source.is_char_boundary(*end)
        {
            return Err(ClientSourceError::Source(
                "overlapping or invalid Client module source ranges".into(),
            ));
        }
        previous_end = *end;
    }
    let mut rewritten = source.to_owned();
    for (start, end, replacement) in replacements.into_iter().rev() {
        rewritten.replace_range(start..end, &replacement);
    }
    Ok(rewritten)
}

fn property_has_computed_key(property: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    source_text(property, source).trim_start().starts_with('[')
}

fn js_property_name_is(node: tree_sitter::Node<'_>, expected: &str, source: &[u8]) -> bool {
    match node.kind() {
        "property_identifier" => source_text(node, source) == expected,
        "string" => js_string_value(node, source)
            .ok()
            .flatten()
            .is_some_and(|value| value == expected),
        _ => false,
    }
}

fn js_string_value(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Result<Option<String>, ClientSourceError> {
    match node.kind() {
        "string" => decode_js_quoted(source_text(node, source)).map(Some),
        "template_string" => {
            let text = source_text(node, source);
            if text.contains("${") {
                return Ok(None);
            }
            decode_js_quoted(text).map(Some)
        }
        _ => Ok(None),
    }
}

fn decode_js_quoted(raw: &str) -> Result<String, ClientSourceError> {
    let Some(quote) = raw.chars().next() else {
        return Err(ClientSourceError::Source("empty string literal".into()));
    };
    if !matches!(quote, '\'' | '"' | '`') || !raw.ends_with(quote) {
        return Err(ClientSourceError::Source("invalid string literal".into()));
    }
    let body = &raw[quote.len_utf8()..raw.len() - quote.len_utf8()];
    let mut chars = body.chars().peekable();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let escaped = chars
            .next()
            .ok_or_else(|| ClientSourceError::Source("unterminated string escape".into()))?;
        match escaped {
            '\n' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            '\'' => output.push('\''),
            '"' => output.push('"'),
            '`' => output.push('`'),
            '\\' => output.push('\\'),
            '/' => output.push('/'),
            'b' => output.push('\u{0008}'),
            'f' => output.push('\u{000c}'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            'v' => output.push('\u{000b}'),
            '0' => output.push('\0'),
            'x' => output.push(decode_hex_scalar(&mut chars, 2)?),
            'u' => {
                let first = if chars.peek() == Some(&'{') {
                    chars.next();
                    let digits = chars
                        .by_ref()
                        .take_while(|ch| *ch != '}')
                        .collect::<String>();
                    u32::from_str_radix(&digits, 16).map_err(|_| {
                        ClientSourceError::Source("invalid Unicode escape in string".into())
                    })?
                } else {
                    let mut digits = String::with_capacity(4);
                    for _ in 0..4 {
                        digits.push(chars.next().ok_or_else(|| {
                            ClientSourceError::Source("short Unicode escape in string".into())
                        })?);
                    }
                    u32::from_str_radix(&digits, 16).map_err(|_| {
                        ClientSourceError::Source("invalid Unicode escape in string".into())
                    })?
                };
                if (0xD800..=0xDBFF).contains(&first) {
                    let mut lookahead = chars.clone();
                    if lookahead.next() == Some('\\') && lookahead.next() == Some('u') {
                        chars.next();
                        chars.next();
                        let mut digits = String::with_capacity(4);
                        for _ in 0..4 {
                            digits.push(chars.next().ok_or_else(|| {
                                ClientSourceError::Source("short Unicode escape in string".into())
                            })?);
                        }
                        let second = u32::from_str_radix(&digits, 16).map_err(|_| {
                            ClientSourceError::Source("invalid Unicode escape in string".into())
                        })?;
                        if (0xDC00..=0xDFFF).contains(&second) {
                            let scalar = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                            output.push(char::from_u32(scalar).ok_or_else(|| {
                                ClientSourceError::Source("invalid Unicode scalar in string".into())
                            })?);
                            continue;
                        }
                        return Err(ClientSourceError::Source(
                            "unpaired Unicode surrogate in Client module path".into(),
                        ));
                    }
                    return Err(ClientSourceError::Source(
                        "unpaired Unicode surrogate in Client module path".into(),
                    ));
                }
                if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(ClientSourceError::Source(
                        "unpaired Unicode surrogate in Client module path".into(),
                    ));
                }
                output.push(char::from_u32(first).ok_or_else(|| {
                    ClientSourceError::Source("invalid Unicode scalar in string".into())
                })?);
            }
            other => output.push(other),
        }
    }
    Ok(output)
}

fn decode_hex_scalar(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    count: usize,
) -> Result<char, ClientSourceError> {
    let mut digits = String::with_capacity(count);
    for _ in 0..count {
        digits.push(
            chars
                .next()
                .ok_or_else(|| ClientSourceError::Source("short hex escape in string".into()))?,
        );
    }
    let scalar = u32::from_str_radix(&digits, 16)
        .map_err(|_| ClientSourceError::Source("invalid hex escape in string".into()))?;
    char::from_u32(scalar)
        .ok_or_else(|| ClientSourceError::Source("invalid scalar in hex escape".into()))
}

fn make_manifest(
    root: &Path,
    surface_modules: &[ClientSurfaceModule],
    runtime_sources: &ClientSurfaceRuntimeSources,
) -> Result<ClientModuleManifest, ClientSourceError> {
    let mut files = vec![
        ClientManifestFile {
            key: SURFACE_RUNTIME_KEY.into(),
            source: runtime_sources.surface_runtime.clone(),
        },
        ClientManifestFile {
            key: HOOKS_TYPES_KEY.into(),
            source: runtime_sources.hooks_types.clone(),
        },
    ];
    let mut files_by_key =
        HashSet::from([SURFACE_RUNTIME_KEY.to_owned(), HOOKS_TYPES_KEY.to_owned()]);
    let mut modules = Vec::with_capacity(surface_modules.len());
    for module in surface_modules {
        let entry_path = PathBuf::from(&module.module_path);
        let entry_key = surface_file_key(root, &entry_path)?;
        modules.push(ClientManifestModule {
            module: module.module.clone(),
            entry: entry_key.clone(),
            component: module.component.clone(),
        });
        let mut source_files = Vec::with_capacity(module.linked.len() + 1);
        source_files.push((entry_path, module.source.as_str()));
        for linked in &module.linked {
            source_files.push((PathBuf::from(&linked.file), linked.source.as_str()));
        }
        for (path, source) in source_files {
            let key = surface_file_key(root, &path)?;
            if files_by_key.insert(key.clone()) {
                let rewritten = rewrite_module_imports(source, &path, root, &module.links)?;
                files.push(ClientManifestFile {
                    key,
                    source: rewritten,
                });
            }
        }
    }
    let limits = ClientModuleLimits::default();
    let preimage = serde_json::to_vec(&ManifestPreimage {
        files: &files,
        modules: &modules,
        limits: &limits,
    })
    .map_err(|error| ClientSourceError::Manifest(error.to_string()))?;
    let hash = format!("{:x}", Sha256::digest(preimage));
    Ok(ClientModuleManifest {
        hash,
        modules,
        runtime: SURFACE_RUNTIME_KEY.into(),
        limits,
        files,
    })
}

fn surface_file_key(root: &Path, path: &Path) -> Result<String, ClientSourceError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ClientSourceError::Source("surface file leaves plugin root".into()))?;
    let mut key = String::from(SURFACE_PREFIX);
    let mut first = true;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(ClientSourceError::Source(
                "surface file path is not normalized".into(),
            ));
        };
        if !first {
            key.push('/');
        }
        first = false;
        key.push_str(&encode_uri_component(part.to_str().ok_or_else(|| {
            ClientSourceError::Source("surface path is not UTF-8".into())
        })?));
    }
    Ok(key)
}

fn encode_uri_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(byte) {
            output.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

fn rewrite_module_imports(
    source: &str,
    importer: &Path,
    root: &Path,
    links: &[ClientSourceLink],
) -> Result<String, ClientSourceError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| ClientSourceError::Source(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ClientSourceError::Source("source parser returned no tree".into()))?;
    let bytes = source.as_bytes();
    let importer = path_string(importer)?;
    let mut replacements = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "import_statement" | "export_statement") {
            if let Some(specifier_node) = node.child_by_field_name("source") {
                if let Some(specifier) = js_string_value(specifier_node, bytes)? {
                    replacements.push((
                        specifier_node.start_byte(),
                        specifier_node.end_byte(),
                        encoded_import_specifier(&specifier, &importer, root, links)?,
                    ));
                }
            }
        } else if is_dynamic_import(node) {
            if let Some(argument) = node
                .child_by_field_name("arguments")
                .and_then(|arguments| arguments.named_child(0))
            {
                if let Some(specifier) = js_string_value(argument, bytes)? {
                    replacements.push((
                        argument.start_byte(),
                        argument.end_byte(),
                        encoded_import_specifier(&specifier, &importer, root, links)?,
                    ));
                } else {
                    let expression = source_text(argument, bytes);
                    let prefix = serde_json::to_string(&format!("{UNLINKED_PREFIX}computed#"))
                        .map_err(|error| ClientSourceError::Source(error.to_string()))?;
                    replacements.push((
                        argument.start_byte(),
                        argument.end_byte(),
                        format!("({prefix} + String({expression}))"),
                    ));
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    let mut rewritten = source.to_owned();
    for (start, end, text) in replacements.into_iter().rev() {
        rewritten.replace_range(start..end, &text);
    }
    Ok(rewritten)
}

fn encoded_import_specifier(
    specifier: &str,
    importer: &str,
    root: &Path,
    links: &[ClientSourceLink],
) -> Result<String, ClientSourceError> {
    let destination = if is_surface_builtin(specifier) {
        builtin_module_key(specifier).to_owned()
    } else {
        let link = links
            .iter()
            .find(|link| link.from == importer && link.spelled == specifier)
            .ok_or_else(|| {
                ClientSourceError::Source(format!(
                    "surface import {specifier:?} from {importer} was not linked"
                ))
            })?;
        surface_file_key(root, Path::new(&link.file))?
    };
    serde_json::to_string(&destination)
        .map_err(|error| ClientSourceError::Source(error.to_string()))
}

fn select_component(source: &str) -> Result<String, ClientSourceError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .map_err(|error| ClientSourceError::Source(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ClientSourceError::Source("module export parser returned no tree".into()))?;
    let bytes = source.as_bytes();
    let mut exports = Vec::new();
    let mut cursor = tree.root_node().walk();
    for statement in tree.root_node().named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        if source_text(statement, bytes)
            .trim_start()
            .starts_with("export default")
        {
            exports.push("default".to_owned());
            continue;
        }
        if let Some(declaration) = statement.child_by_field_name("declaration") {
            add_exported_declaration(declaration, bytes, &mut exports);
        }
        if let Some(clause) = statement
            .named_children(&mut statement.walk())
            .find(|child| child.kind() == "export_clause")
        {
            for specifier in children_of_kind(clause, "export_specifier") {
                let exported = specifier
                    .child_by_field_name("alias")
                    .or_else(|| specifier.child_by_field_name("name"));
                if let Some(exported) = exported {
                    if let Some(name) = js_export_name(exported, bytes) {
                        exports.push(name);
                    }
                }
            }
        }
        if let Some(namespace) = statement
            .named_children(&mut statement.walk())
            .find(|child| child.kind() == "namespace_export")
        {
            let child = named_children(namespace).into_iter().last();
            if let Some(child) = child {
                if let Some(name) = js_export_name(child, bytes) {
                    exports.push(name);
                }
            }
        }
    }
    if exports.iter().any(|name| name == "default") {
        return Ok("default".into());
    }
    let mut components = exports
        .into_iter()
        .filter(|name| is_component_export(name))
        .collect::<Vec<_>>();
    components.sort_by(|left, right| js_string_cmp(left, right));
    components.dedup();
    match components.as_slice() {
        [component] => Ok(component.clone()),
        [] => Err(ClientSourceError::Source(
            "surface module exports no component: give it a default export or a single capitalized one"
                .into(),
        )),
        components => Err(ClientSourceError::Source(format!(
            "surface module exports multiple components: {}; give it a default export or a single capitalized one",
            components.join(", ")
        ))),
    }
}

fn add_exported_declaration(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    exports: &mut Vec<String>,
) {
    if matches!(
        declaration.kind(),
        "function_declaration" | "class_declaration"
    ) {
        if let Some(name) = declaration.child_by_field_name("name") {
            exports.push(source_text(name, source).to_owned());
        }
        return;
    }
    if matches!(
        declaration.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        for declarator in children_of_kind(declaration, "variable_declarator") {
            if let Some(name) = declarator.child_by_field_name("name") {
                if name.kind() == "identifier" {
                    exports.push(source_text(name, source).to_owned());
                }
            }
        }
    }
}

fn js_export_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "property_identifier" => Some(source_text(node, source).to_owned()),
        "string" => decode_js_quoted(source_text(node, source)).ok(),
        _ => None,
    }
}

fn is_component_export(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_uppercase)
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric())
        && bytes.iter().any(u8::is_ascii_lowercase)
}

fn js_string_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn source_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> &'a str {
    std::str::from_utf8(&source[node.byte_range()]).unwrap_or("")
}

fn named_children(node: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn children_of_kind<'tree>(
    node: tree_sitter::Node<'tree>,
    kind: &str,
) -> Vec<tree_sitter::Node<'tree>> {
    named_children(node)
        .into_iter()
        .filter(|child| child.kind() == kind)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn test_runtime_sources() -> ClientSurfaceRuntimeSources {
        ClientSurfaceRuntimeSources {
            surface_runtime: "export const runtime = true;".into(),
            hooks_types: "export const HookTypes = {};".into(),
        }
    }

    #[test]
    fn import_candidates_match_native_extension_and_index_order() {
        let base = PathBuf::from("/plugin/components/card.js");
        let mut expected = vec![
            base.clone(),
            PathBuf::from("/plugin/components/card.ts"),
            PathBuf::from("/plugin/components/card.tsx"),
        ];
        for extension in ["ts", "tsx", "jsx", "js", "mjs", "cjs", "mts", "cts"] {
            let mut appended = base.as_os_str().to_os_string();
            appended.push(format!(".{extension}"));
            expected.push(PathBuf::from(appended));
            expected.push(base.join(format!("index.{extension}")));
        }
        assert_eq!(import_candidates(&base), expected);

        let base = PathBuf::from("/plugin/components/card");
        let mut expected = vec![base.clone()];
        for extension in ["ts", "tsx", "jsx", "js", "mjs", "cjs", "mts", "cts"] {
            let mut appended = base.as_os_str().to_os_string();
            appended.push(format!(".{extension}"));
            expected.push(PathBuf::from(appended));
            expected.push(base.join(format!("index.{extension}")));
        }
        assert_eq!(import_candidates(&base), expected);
    }

    #[test]
    fn relative_import_resolution_uses_native_js_and_index_priority() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let lexical_root = absolute_normalized(root).unwrap();
        let real_root = fs::canonicalize(root).unwrap();
        let entry_path = root.join("entry.ts");
        fs::write(&entry_path, "import './card.js'; import './folder';").unwrap();

        let exact_js = root.join("card.js");
        let js_replacement = root.join("card.ts");
        let appended_js = root.join("card.js.ts");
        fs::write(&exact_js, "export const source = 'exact-js';").unwrap();
        fs::write(&js_replacement, "export const source = 'replacement';").unwrap();
        fs::write(&appended_js, "export const source = 'appended';").unwrap();

        let extensionless_tsx = root.join("folder.tsx");
        let extensionless_index = root.join("folder/index.ts");
        fs::create_dir_all(extensionless_index.parent().unwrap()).unwrap();
        fs::write(&extensionless_tsx, "export const source = 'tsx';").unwrap();
        fs::write(&extensionless_index, "export const source = 'index-ts';").unwrap();

        let entry = resolve_entry_path(&lexical_root, &real_root, Path::new("entry.ts")).unwrap();
        let graph = read_module_graph(&lexical_root, &real_root, entry, true).unwrap();
        assert_eq!(
            graph.links[0].file,
            path_string(&lexical_root.join("card.js")).unwrap(),
            "an existing explicit .js candidate precedes every source replacement"
        );
        assert_eq!(
            graph.links[1].file,
            path_string(&lexical_root.join("folder/index.ts")).unwrap(),
            "Native interleaves each appended extension with its index candidate"
        );
    }

    #[test]
    fn dot_relative_imports_resolve_from_native_index_base() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let lexical_root = absolute_normalized(root).unwrap();
        let real_root = fs::canonicalize(root).unwrap();
        let nested = root.join("dir/child");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.join("dir.ts"),
            "export const source = 'wrong-sibling';",
        )
        .unwrap();
        fs::write(
            root.join("dir/index.ts"),
            "export const source = 'native-index';",
        )
        .unwrap();
        fs::write(
            root.join("dir/entry.ts"),
            "import './child/entry.ts'; import '.'; import './';",
        )
        .unwrap();
        fs::write(nested.join("entry.ts"), "import '..'; import '../';").unwrap();

        let entry =
            resolve_entry_path(&lexical_root, &real_root, Path::new("dir/entry.ts")).unwrap();
        let graph = read_module_graph(&lexical_root, &real_root, entry, true).unwrap();
        let expected = path_string(&lexical_root.join("dir/index.ts")).unwrap();
        assert_eq!(graph.links[1].file, expected);
        assert_eq!(graph.links[2].file, expected);
        assert_eq!(graph.links[3].file, expected);
        assert_eq!(graph.links[4].file, expected);
    }

    #[test]
    fn relative_js_import_resolves_appended_js_ts_before_js_index_ts() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let lexical_root = absolute_normalized(root).unwrap();
        let real_root = fs::canonicalize(root).unwrap();
        fs::write(root.join("entry.ts"), "import './card.js';").unwrap();
        fs::create_dir_all(root.join("card.js")).unwrap();
        fs::write(root.join("card.js.ts"), "export const source = 'appended';").unwrap();
        fs::write(
            root.join("card.js/index.ts"),
            "export const source = 'index';",
        )
        .unwrap();

        let entry = resolve_entry_path(&lexical_root, &real_root, Path::new("entry.ts")).unwrap();
        let graph = read_module_graph(&lexical_root, &real_root, entry, true).unwrap();
        assert_eq!(
            graph.links[0].file,
            path_string(&lexical_root.join("card.js.ts")).unwrap()
        );
    }

    #[tokio::test]
    async fn prepares_client_graph_from_tsx_and_static_imports() {
        let directory = tempdir().unwrap();
        let hooks = directory.path().join("hooks");
        let ui = hooks.join("ui");
        fs::create_dir_all(&ui).unwrap();
        fs::write(
            hooks.join("register.tsx"),
            r#"import { helper } from './helper.js';
               export function register($) {
                 const { Client } = $.ui.resolve({ surface: 'desktop', component: 'Panel' });
                 return <Client module="./ui/board.tsx" key="board" data={{ title: helper() }} />;
               }"#,
        )
        .unwrap();
        fs::write(
            hooks.join("helper.js"),
            "export const helper = () => 'ready';",
        )
        .unwrap();
        fs::write(
            ui.join("board.tsx"),
            r#"import { label } from './label.ts';
               export default function Board(props, surface) {
                 return <surface.elements.Text>{label(props)}</surface.elements.Text>;
               }"#,
        )
        .unwrap();
        fs::write(
            ui.join("label.ts"),
            "export const label = props => props.title;",
        )
        .unwrap();

        let prepared = prepare_plugin_sources(
            directory.path(),
            Path::new("hooks/register.tsx"),
            Some(Path::new("bun")),
            &test_runtime_sources(),
        )
        .await
        .unwrap();
        assert_eq!(prepared.hooks.files.len(), 2);
        assert_eq!(prepared.hooks.links.len(), 1);
        assert!(prepared.hooks.files[0].source.contains("h("));
        assert!(prepared.hooks.files[0]
            .source
            .contains("hooks/ui/board.tsx"));
        assert!(!prepared.hooks.files[0].source.contains("./ui/board.tsx"));
        let clients = prepared.clients.as_ref().unwrap();
        assert_eq!(clients.surface_modules.len(), 1);
        let module = &clients.surface_modules[0];
        assert_eq!(module.module, "hooks/ui/board.tsx");
        assert_eq!(module.component, "default");
        assert_eq!(module.linked.len(), 1);
        assert_eq!(module.links.len(), 1);
        assert!(module.source.contains("surface.elements.Text"));
        assert!(module.source.contains("h("));
        assert!(clients.manifest.files.iter().any(|file| {
            file.key == "surface:///hooks/ui/board.tsx"
                && file.source.contains("surface:///hooks/ui/label.ts")
        }));
        assert_eq!(clients.manifest.runtime, SURFACE_RUNTIME_KEY);
        let envelope = clients.for_plugin("test-plugin").unwrap();
        assert_eq!(envelope["plugin"], "test-plugin");
        assert_eq!(envelope["hash"], clients.manifest.hash);
        let graph = clients.worker_module_graph().unwrap();
        assert!(graph["modules"][0]["linked"][0]["source"].is_string());
        assert_eq!(graph["manifest"]["hash"], clients.manifest.hash);
        let hooks = prepared.hook_worker_graph().unwrap();
        assert_eq!(hooks["entry"].as_str(), Some(prepared.hooks.entry.as_str()));
        assert_eq!(
            hooks["files"][0]["file"].as_str(),
            Some(prepared.hooks.files[0].file.as_str())
        );
        assert_eq!(
            hooks["compilerVersion"].as_str(),
            Some(prepared.bun_version.as_str())
        );
    }

    #[tokio::test]
    async fn rewrites_client_literals_per_importer_before_manifest_and_hook_compile() {
        let directory = tempdir().unwrap();
        let hooks = directory.path().join("hooks");
        for folder in ["one", "two"] {
            let view = hooks.join("views").join(folder);
            fs::create_dir_all(&view).unwrap();
            fs::write(
                view.join("view.jsx"),
                r#"export const View = () => <Client module="./card.tsx" key="view" />;"#,
            )
            .unwrap();
            fs::write(
                view.join("card.tsx"),
                "export default function Card() { return null; }",
            )
            .unwrap();
        }
        fs::create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join("register.tsx"),
            "import { View as One } from './views/one/view.jsx'; import { View as Two } from './views/two/view.jsx'; export function register() { return [One(), Two()]; }",
        )
        .unwrap();

        let prepared = prepare_plugin_sources(
            directory.path(),
            Path::new("hooks/register.tsx"),
            Some(Path::new("bun")),
            &test_runtime_sources(),
        )
        .await
        .unwrap();

        let one = prepared
            .hooks
            .files
            .iter()
            .find(|file| file.file.ends_with("hooks/views/one/view.jsx"))
            .unwrap();
        let two = prepared
            .hooks
            .files
            .iter()
            .find(|file| file.file.ends_with("hooks/views/two/view.jsx"))
            .unwrap();
        assert!(one.source.contains("hooks/views/one/card.tsx"));
        assert!(!one.source.contains("./card.tsx"));
        assert!(two.source.contains("hooks/views/two/card.tsx"));
        assert!(!two.source.contains("./card.tsx"));

        let clients = prepared.clients.as_ref().unwrap();
        let manifest_modules = clients
            .manifest
            .modules
            .iter()
            .map(|module| module.module.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            manifest_modules,
            ["hooks/views/one/card.tsx", "hooks/views/two/card.tsx"]
        );
        let manifest_entries = clients
            .manifest
            .modules
            .iter()
            .map(|module| module.entry.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            manifest_entries,
            [
                "surface:///hooks/views/one/card.tsx",
                "surface:///hooks/views/two/card.tsx"
            ]
        );
    }

    #[tokio::test]
    async fn packaged_bun_compiler_path_works_without_path_lookup() {
        const CHILD: &str = "LINGXI_TEST_PACKAGED_BUN_COMPILER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let directory = tempdir().unwrap();
            fs::write(
                directory.path().join("register.tsx"),
                "import { value } from './shared.ts'; export function register() { return <Box>{value}</Box>; }",
            )
            .unwrap();
            fs::write(
                directory.path().join("shared.ts"),
                "export const value: string = 'packaged';",
            )
            .unwrap();
            let prepared = prepare_plugin_sources(
                directory.path(),
                Path::new("register.tsx"),
                None,
                &test_runtime_sources(),
            )
            .await
            .unwrap();
            assert!(prepared.clients.is_none());
            assert_ne!(prepared.bun_version, "not-run");
            assert!(prepared.hooks.files[0].source.contains("h("));
            let shared = prepared
                .hooks
                .files
                .iter()
                .find(|file| file.file.ends_with("shared.ts"))
                .unwrap();
            assert!(shared.source.contains("packaged"));
            assert!(!shared.source.contains("value: string"));
            return;
        }

        let compiler = Command::new("bun")
            .args(["--print", "process.execPath"])
            .output()
            .await
            .unwrap();
        assert!(compiler.status.success());
        let compiler = PathBuf::from(String::from_utf8(compiler.stdout).unwrap().trim());
        assert!(compiler.is_absolute());
        let directory = tempdir().unwrap();
        let test_name = format!(
            "{}::packaged_bun_compiler_path_works_without_path_lookup",
            module_path!().split_once("::").unwrap().1
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD, "1")
            .env("LINGXI_MOD_BUN_EXECUTABLE", compiler)
            .env("PATH", directory.path().join("no-system-tools"))
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .expect("isolated compiler-path regression exceeded its budget")
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "the isolated process must execute the exact regression"
        );
    }

    #[tokio::test]
    async fn prepares_and_transpiles_hook_graph_without_client_references() {
        let directory = tempdir().unwrap();
        fs::create_dir_all(directory.path().join("hooks")).unwrap();
        fs::write(
            directory.path().join("hooks/register.tsx"),
            "import { value } from './shared.ts'; import type { HookApi } from 'claude-code'; export function register($: HookApi) { return <Box>{value}</Box>; }",
        )
        .unwrap();
        fs::write(
            directory.path().join("hooks/shared.ts"),
            "export const value: string = 'ok';",
        )
        .unwrap();
        let prepared = prepare_plugin_sources(
            directory.path(),
            Path::new("hooks/register.tsx"),
            Some(Path::new("bun")),
            &test_runtime_sources(),
        )
        .await
        .unwrap();
        assert!(prepared.clients.is_none());
        assert_eq!(prepared.hooks.files.len(), 3);
        assert_eq!(prepared.hooks.links.len(), 2);
        assert!(prepared.hooks.files[0].source.contains("h("));
        let shared = prepared
            .hooks
            .files
            .iter()
            .find(|file| file.file.ends_with("hooks/shared.ts"))
            .unwrap();
        assert!(shared.source.contains("ok"));
        assert!(!shared.source.contains("value: string"));
        assert_eq!(prepared.hooks.files[2].file, HOOKS_TYPES_KEY);
        assert_eq!(
            prepared.hooks.files[2].source,
            test_runtime_sources().hooks_types
        );
        assert!(!prepared.hooks.files[0].source.contains("claude-code"));
        assert!(prepared
            .hooks
            .links
            .iter()
            .any(|link| { link.spelled == "claude-code" && link.file == HOOKS_TYPES_KEY }));
        assert_ne!(prepared.bun_version, "not-run");
    }

    #[test]
    fn export_selection_prefers_default_and_accepts_only_one_capitalized_export() {
        assert_eq!(
            select_component("export const Board = () => null; export default function Other(){}")
                .unwrap(),
            "default"
        );
        assert_eq!(
            select_component("export function Board(){}").unwrap(),
            "Board"
        );
        assert!(
            select_component("export function Board(){} export const Panel=()=>null;")
                .unwrap_err()
                .to_string()
                .contains("multiple components")
        );
        assert!(select_component("export const BOARD=1;")
            .unwrap_err()
            .to_string()
            .contains("no component"));
    }

    #[test]
    fn module_key_normalization_enforces_relative_supported_plugin_paths() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        let importer = root.join("hooks/register.ts");
        let ui = root.join("ui");
        fs::create_dir_all(&ui).unwrap();
        fs::write(ui.join("board.tsx"), "export default () => null;").unwrap();
        let real_root = fs::canonicalize(root).unwrap();
        let (module, resolved) =
            normalize_client_module(root, &real_root, &importer, "../ui/board.tsx").unwrap();
        assert_eq!(module, "ui/board.tsx");
        assert_eq!(resolved.file, ui.join("board.tsx"));
        assert!(
            normalize_client_module(root, &real_root, &importer, "file:///tmp/board.tsx").is_err()
        );
        assert!(normalize_client_module(root, &real_root, &importer, "../../outside.tsx").is_err());
        assert!(normalize_client_module(root, &real_root, &importer, "./board.mjs").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepts_symlinked_plugin_root_without_allowing_escaped_sources() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let actual = directory.path().join("actual-plugin");
        let alias = directory.path().join("linked-plugin");
        fs::create_dir_all(&actual).unwrap();
        symlink(&actual, &alias).unwrap();
        fs::write(actual.join("register.js"), "export function register() {}").unwrap();
        fs::write(
            actual.join("client.jsx"),
            "export default function View() { return null; }",
        )
        .unwrap();
        let real_root = fs::canonicalize(&alias).unwrap();
        let prepared = prepare_plugin_sources(
            &alias,
            Path::new("register.js"),
            Some(Path::new("bun")),
            &test_runtime_sources(),
        )
        .await
        .unwrap();
        assert!(prepared.clients.is_none());

        let entry = resolve_entry_path(&alias, &real_root, Path::new("register.js")).unwrap();
        let resolved =
            normalize_client_module(&alias, &real_root, &entry.file, "./client.jsx").unwrap();
        assert_eq!(resolved.0, "client.jsx");
        assert!(
            normalize_client_module(&alias, &real_root, &entry.file, "../../outside.jsx",).is_err()
        );
    }

    #[test]
    fn manifest_preimage_preserves_native_field_and_file_order() {
        let files = vec![
            ClientManifestFile {
                key: SURFACE_RUNTIME_KEY.into(),
                source: "runtime".into(),
            },
            ClientManifestFile {
                key: HOOKS_TYPES_KEY.into(),
                source: "types".into(),
            },
        ];
        let modules = vec![ClientManifestModule {
            module: "ui/Board.tsx".into(),
            entry: "surface:///ui/Board.tsx".into(),
            component: "default".into(),
        }];
        let limits = ClientModuleLimits::default();
        let bytes = serde_json::to_vec(&ManifestPreimage {
            files: &files,
            modules: &modules,
            limits: &limits,
        })
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            json!({
                "files":[
                    {"key":"claude:surface-runtime","source":"runtime"},
                    {"key":"claude:hooks-types","source":"types"}
                ],
                "modules":[{"module":"ui/Board.tsx","entry":"surface:///ui/Board.tsx","component":"default"}],
                "limits":{"nodes":20000,"depth":32,"chars":100000,"values":20000,"dataDepth":32}
            })
        );
        assert!(String::from_utf8(bytes).unwrap().starts_with("{\"files\":"));
    }
}
