//! `plugin install` / `plugin uninstall` — materialize a plugin from a
//! configured marketplace and toggle the on-disk state, 1:1 with claude-code
//! 2.1.201 (directory-source marketplaces; verified end-to-end).
//!
//! install (`<plugin>[@<market>]`):
//! 1. resolve the marketplace from the `known_marketplaces.json` registry
//!    (an explicit `@market`, else the first registry whose marketplace file
//!    lists a plugin of that name);
//! 2. copy the plugin tree `<installLocation>/<entry.source>` → the versioned
//!    cache `cache/{market}/{plugin}/{version}/` (version from the plugin
//!    manifest, or the marketplace entry when that optional manifest is absent);
//! 3. write the v2 `installed_plugins.json` record
//!    (`plugins["<plugin>@<market>"] = [{scope, installPath, version,
//!    installedAt, lastUpdated}]`) and set `enabledPlugins[id]=true` at scope.
//!
//! uninstall drops the installed record, DELETES the `enabledPlugins[id]` key
//! (note: NOT set to `false` — that is what `disable` does), and ORPHANS the
//! cache (writes a `.orphaned_at` marker rather than deleting immediately).
//!
//! `--config key=value` persists NON-SENSITIVE userConfig values (validated
//! against the plugin's manifest schema, byte-faithful errors) to settings
//! `pluginConfigs[<name@marketplace>].options` at the chosen scope — the map
//! the composition-root loader reads back; uninstall clears that entry
//! (`deletePluginOptions` settings half).
//!
//! Sensitive `--config` values are handled by the async CLI wrapper and never
//! written to settings. The synchronous core remains available for deterministic
//! filesystem tests.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use migrations::settings_update::{
    SettingsUpdateReceipt, read_settings_source, rollback_settings_update,
    transform_settings_source_with_receipt,
};
use serde_json::{Map, Value};
use telemetry::{AnalyticsBus, AnalyticsValue, LogEventMetadata};

use crate::plugin_policy;
use crate::plugin_settings::{parse_scope_str, scope_label, scope_path};
use lingxi_core::types::WritableScope;

fn registry_error_kind(error: &str) -> &'static str {
    if error.contains("expected value")
        || error.contains("EOF while parsing")
        || error.contains("trailing characters")
        || error.contains("key must be a string")
    {
        "parse"
    } else if error.contains("lock") {
        "lock"
    } else {
        "io"
    }
}

/// Now as ISO-8601 with milliseconds + `Z`.
fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread tokio runtime")
}

/// `<plugins>/installed_plugins.json` path.
#[cfg(test)]
fn installed_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir.join("installed_plugins.json")
}

/// Load the v2 installed DB (`{version:2, plugins:{...}}`); missing/malformed ⇒
/// a fresh empty v2 doc.
pub fn load_installed(plugins_dir: &Path) -> Value {
    plugin::installed::load_normalized(plugins_dir)
        .unwrap_or_else(|| serde_json::json!({"version": 2, "plugins": {}}))
}

/// Write the installed DB (pretty, no trailing newline).
pub fn write_installed(plugins_dir: &Path, doc: &Value) -> Result<(), String> {
    std::fs::create_dir_all(plugins_dir).map_err(|error| error.to_string())?;
    let serialized = serde_json::to_string_pretty(doc).map_err(|error| error.to_string())?;
    lingxi_core::host::rooted_fs::atomic_write(
        plugins_dir,
        Path::new("installed_plugins.json"),
        serialized.as_bytes(),
        lingxi_core::host::AtomicWriteOptions::default(),
    )
    .map_err(|error| error.to_string())
}

/// The resolved-marketplaces registry (`name → {source, installLocation, …}`).
fn load_registry(plugins_dir: &Path) -> Map<String, Value> {
    std::fs::read_to_string(plugins_dir.join("known_marketplaces.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// A marketplace's on-disk root (directory source → `installLocation`).
fn install_location(entry: &Value) -> Option<PathBuf> {
    entry
        .get("installLocation")
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

/// Split `name@market` → `(name, Some(market))`; bare → `(name, None)`
/// (only the first `@` separates, mirroring `parsePluginIdentifier`).
fn split_id(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('@') {
        Some((n, rest)) => (n, Some(rest.split('@').next().unwrap_or(rest))),
        None => (arg, None),
    }
}

/// The pre-`@` display name.
fn name_of(id: &str) -> &str {
    id.split('@').next().unwrap_or(id)
}

/// The marketplace segment of a `name@marketplace` id, if present.
fn marketplace_of(id: &str) -> Option<&str> {
    split_id(id).1
}

/// Resolve a catalog entry that is materialized inside its marketplace root.
///
/// The shared typed resolver rejects absolute paths and `..`; the canonical
/// containment check additionally closes symlink escapes at the selected root.
/// External Git/URL/npm sources are materialized separately before reaching
/// this path.
fn marketplace_entry_source_path(
    market_root: &Path,
    marketplace: &str,
    name: &str,
    plugins_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    current_thread_runtime().block_on(marketplace_entry_source_path_with_bus(
        market_root,
        marketplace,
        name,
        plugins_dir,
        None,
        false,
    ))
}

async fn marketplace_entry_source_path_with_bus(
    market_root: &Path,
    marketplace: &str,
    name: &str,
    plugins_dir: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<Option<PathBuf>, String> {
    use plugin::marketplace::{MarketplacePluginExternalSource, MarketplacePluginSource};

    let Some(raw_entry) = marketplace_entry(market_root, name) else {
        return Ok(None);
    };
    let entry = serde_json::from_value::<plugin::marketplace::MarketplacePluginEntry>(raw_entry)
        .map_err(|error| format!("Invalid marketplace entry for \"{name}\": {error}"))?;
    // §8: the catalog's own declared entry name is third-party-controlled the
    // moment the marketplace is; install is one of the paths the finding
    // calls out as never calling the name gate at all.
    plugin::validate_plugin_name(&entry.name)
        .map_err(|reason| format!("Invalid marketplace entry for \"{name}\": {reason}"))?;
    if let Some(MarketplacePluginSource::Structured(source)) = entry.source.as_ref() {
        match source {
            MarketplacePluginExternalSource::Github { .. }
            | MarketplacePluginExternalSource::Url { .. }
            | MarketplacePluginExternalSource::GitSubdir { .. }
            | MarketplacePluginExternalSource::Archive { .. }
            | MarketplacePluginExternalSource::Npm { .. }
            | MarketplacePluginExternalSource::Command { .. } => {
                return materialize_external_plugin_source_with_bus(
                    plugins_dir,
                    marketplace,
                    name,
                    source,
                    analytics_bus,
                    yes,
                )
                .await
                .map(Some);
            }
            MarketplacePluginExternalSource::Unsupported { error } => {
                return Err(format!(
                    "This plugin's marketplace entry is invalid: '{name}'{}",
                    error
                        .as_deref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ));
            }
        }
    }
    let candidate = match plugin::MarketplaceManager::plugin_dir_in_clone(market_root, &entry) {
        Ok(candidate) => candidate,
        Err(_) => return Ok(None),
    };
    let Ok(canonical_root) = std::fs::canonicalize(market_root) else {
        return Ok(None);
    };
    match std::fs::canonicalize(&candidate) {
        Ok(canonical_candidate) => Ok((canonical_candidate.starts_with(&canonical_root)
            && canonical_candidate.is_dir())
        .then_some(canonical_candidate)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Preserve update's distinction between "listed but not installed"
            // and "not found" even when the listed source has not been checked
            // out yet. `plugin_dir_in_clone` already rejected traversal.
            let relative = candidate.strip_prefix(market_root).ok();
            Ok(relative.map(|relative| canonical_root.join(relative)))
        }
        Err(_) => Ok(None),
    }
}

fn external_source_cache_key(
    source: &plugin::marketplace::MarketplacePluginExternalSource,
) -> String {
    let encoded = serde_json::to_vec(source).unwrap_or_default();
    plugin::plugin_source_sha256(&encoded)[..20].to_string()
}

fn confined_source_subdir(root: &Path, relative: Option<&str>) -> Result<PathBuf, String> {
    let relative = relative.filter(|path| !path.is_empty()).unwrap_or(".");
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "plugin source path escapes its repository: {relative}"
        ));
    }
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("failed to resolve plugin source: {error}"))?;
    let candidate = std::fs::canonicalize(root.join(relative_path))
        .map_err(|error| format!("failed to resolve plugin source path {relative}: {error}"))?;
    if !candidate.starts_with(&canonical_root) || !candidate.is_dir() {
        return Err(format!(
            "plugin source path escapes its repository: {relative}"
        ));
    }
    Ok(candidate)
}

/// Materialize the oracle `archive` plugin-entry source: an HTTPS zip,
/// optionally pinned by `sha256` (verified against every download; the
/// install is refused on mismatch).
fn download_external_plugin_archive(
    url: &str,
    sha256: Option<&str>,
    root: &Path,
) -> Result<PathBuf, String> {
    let url = url.to_string();
    let sha256 = sha256.map(ToOwned::to_owned);
    let dest = root.join("archive");
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("failed to initialize plugin download: {error}"))?;
        runtime.block_on(crate::plugin_download::download_plugin_archive(
            &url,
            sha256.as_deref(),
            &dest,
        ))
    })
    .join()
    .map_err(|_| "plugin download worker panicked".to_string())?
}

/// Verify a `sha`-pinned checkout's resolved HEAD commit against the entry's
/// declared pin (oracle: *"SHA pin verification failed: expected HEAD to be
/// … Refusing to install."*). `None` (no pin declared) always succeeds.
fn verify_sha_pin(expected: Option<&str>, actual_head: &str) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if expected.eq_ignore_ascii_case(actual_head) {
        Ok(())
    } else {
        Err(format!(
            "SHA pin verification failed: expected HEAD to be {expected}, got {actual_head}. \
             The pinned commit may have been removed upstream, or a ref with the same name \
             exists. Refusing to install."
        ))
    }
}

fn materialize_npm_source(
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    root: &Path,
) -> Result<PathBuf, String> {
    crate::plugin_npm::materialize(package, version, registry, root)
}

/// CLI `-y` (oracle `jl({yes})`) or an explicit env grant.
///
/// `LINGXI_PLUGIN_COMMAND_SOURCE_CONSENT=1` accepts any command; a value equal
/// to the exact command string accepts only that command.
fn command_source_is_consented(yes: bool, command: &str) -> bool {
    yes || std::env::var("LINGXI_PLUGIN_COMMAND_SOURCE_CONSENT")
        .ok()
        .is_some_and(|value| value == "1" || value == command)
}

fn remote_fetch_source_kind(
    source: &plugin::marketplace::MarketplacePluginExternalSource,
) -> Option<&'static str> {
    use plugin::marketplace::MarketplacePluginExternalSource;

    match source {
        MarketplacePluginExternalSource::Github { .. } => Some("github"),
        MarketplacePluginExternalSource::Url { .. } => Some("url"),
        MarketplacePluginExternalSource::GitSubdir { .. } => Some("git-subdir"),
        MarketplacePluginExternalSource::Archive { .. } => Some("archive"),
        MarketplacePluginExternalSource::Npm { .. } => Some("npm"),
        MarketplacePluginExternalSource::Command { .. } => Some("command"),
        MarketplacePluginExternalSource::Unsupported { .. } => None,
    }
}

fn remote_fetch_host(source: &plugin::marketplace::MarketplacePluginExternalSource) -> String {
    use plugin::marketplace::MarketplacePluginExternalSource;

    let url_like = match source {
        MarketplacePluginExternalSource::Github { .. } => return "github.com".to_string(),
        MarketplacePluginExternalSource::Url { url, .. }
        | MarketplacePluginExternalSource::GitSubdir { url, .. }
        | MarketplacePluginExternalSource::Archive { url, .. } => url.as_str(),
        MarketplacePluginExternalSource::Npm { registry, .. } => {
            return registry
                .as_deref()
                .and_then(|registry| registry.split("://").nth(1).or(Some(registry)))
                .and_then(|value| value.split('/').next())
                .unwrap_or("registry.npmjs.org")
                .trim_start_matches("git@")
                .trim_end_matches(':')
                .to_string();
        }
        MarketplacePluginExternalSource::Command { .. }
        | MarketplacePluginExternalSource::Unsupported { .. } => return String::new(),
    };

    if url_like.starts_with("file://") {
        return "file".to_string();
    }
    if let Some(rest) = url_like.strip_prefix("git@") {
        return rest.split(':').next().unwrap_or("ssh").to_string();
    }
    url_like
        .split("://")
        .nth(1)
        .unwrap_or(url_like)
        .split('/')
        .next()
        .unwrap_or(url_like)
        .to_string()
}

fn remote_fetch_error_kind(error: &str) -> &'static str {
    if error.contains("SHA pin verification failed") {
        "sha_pin"
    } else if error.contains("Failed to checkout commit") {
        "git_checkout"
    } else if error.contains("failed to initialize plugin download") {
        "download_init"
    } else if error.contains("failed to install npm plugin source") {
        "npm_install"
    } else if error.contains("failed to clone") {
        "git_clone"
    } else if error.contains("failed to download") {
        "download"
    } else {
        "other"
    }
}

fn remote_fetch_metadata(
    source: &plugin::marketplace::MarketplacePluginExternalSource,
    started: Instant,
    result: &Result<PathBuf, String>,
) -> Option<LogEventMetadata> {
    let source_kind = remote_fetch_source_kind(source)?;
    let mut metadata = LogEventMetadata::new();
    metadata.insert(
        "source".into(),
        AnalyticsValue::String(source_kind.to_string()),
    );
    metadata.insert(
        "host".into(),
        AnalyticsValue::String(remote_fetch_host(source)),
    );
    metadata.insert(
        "outcome".into(),
        AnalyticsValue::String(if result.is_ok() { "success" } else { "failure" }.to_string()),
    );
    metadata.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)),
    );
    metadata.insert(
        "error_kind".into(),
        AnalyticsValue::String(
            result
                .as_ref()
                .err()
                .map(|error| remote_fetch_error_kind(error))
                .unwrap_or("")
                .to_string(),
        ),
    );
    Some(metadata)
}

async fn emit_remote_fetch(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    source: &plugin::marketplace::MarketplacePluginExternalSource,
    started: Instant,
    result: &Result<PathBuf, String>,
) {
    let Some(bus) = analytics_bus else {
        return;
    };
    let Some(metadata) = remote_fetch_metadata(source, started, result) else {
        return;
    };
    bus.log_event(telemetry::tengu::plugin::REMOTE_FETCH, metadata)
        .await;
}

fn official_marketplace_source(entry: &Value) -> bool {
    match plugin_policy::MarketplaceSourceIdentity::from_value(entry) {
        Some(plugin_policy::MarketplaceSourceIdentity::Github { repo, .. }) => {
            repo.eq_ignore_ascii_case("anthropics/claude-plugins-official")
        }
        Some(plugin_policy::MarketplaceSourceIdentity::Git { url, .. })
        | Some(plugin_policy::MarketplaceSourceIdentity::Url { url }) => {
            let lowered = url.to_ascii_lowercase();
            lowered.contains("github.com/anthropics/claude-plugins-official")
                || lowered.contains("github.com:anthropics/claude-plugins-official")
        }
        Some(
            plugin_policy::MarketplaceSourceIdentity::Npm { .. }
            | plugin_policy::MarketplaceSourceIdentity::File { .. }
            | plugin_policy::MarketplaceSourceIdentity::Directory { .. },
        )
        | None => false,
    }
}

const PLUGIN_ID_HASH_SALT: &str = "claude-plugin-telemetry-v1";

struct InstalledEventContext {
    name: String,
    marketplace_name: String,
    is_official: bool,
    version: Option<String>,
}

fn telemetry_plugin_id_hash(name: &str, marketplace: Option<&str>) -> String {
    let key = marketplace
        .map(|marketplace| format!("{name}@{}", marketplace.to_ascii_lowercase()))
        .unwrap_or_else(|| name.to_string());
    plugin::plugin_source_sha256(format!("{key}{PLUGIN_ID_HASH_SALT}").as_bytes())[..16].to_string()
}

async fn installed_event_context(arg: &str, plugins_dir: &Path) -> Option<InstalledEventContext> {
    let (name, requested_marketplace) = split_id(arg);
    let registry = load_registry(plugins_dir);
    let (marketplace_name, entry, raw_entry) = match requested_marketplace {
        Some(marketplace) => {
            let entry = registry.get(marketplace)?;
            let root = install_location(entry)?;
            Some((marketplace, entry, marketplace_entry(&root, name)?))
        }
        None => registry.iter().find_map(|(marketplace, entry)| {
            let root = install_location(entry)?;
            marketplace_entry(&root, name).map(|raw_entry| (marketplace.as_str(), entry, raw_entry))
        }),
    }?;
    Some(InstalledEventContext {
        name: name.to_string(),
        marketplace_name: marketplace_name.to_string(),
        is_official: official_marketplace_source(entry),
        version: raw_entry
            .get("version")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
    })
}

fn installed_event_metadata(context: &InstalledEventContext) -> LogEventMetadata {
    let mut metadata = LogEventMetadata::new();
    metadata.insert(
        "_PROTO_plugin_name".into(),
        AnalyticsValue::String(context.name.clone()),
    );
    metadata.insert(
        "_PROTO_marketplace_name".into(),
        AnalyticsValue::String(context.marketplace_name.clone()),
    );
    metadata.insert(
        "plugin_id_hash".into(),
        AnalyticsValue::String(telemetry_plugin_id_hash(
            &context.name,
            Some(&context.marketplace_name),
        )),
    );
    metadata.insert(
        "plugin_scope".into(),
        AnalyticsValue::String(
            if context.is_official {
                "official"
            } else {
                "user-local"
            }
            .to_string(),
        ),
    );
    metadata.insert(
        "plugin_name_redacted".into(),
        AnalyticsValue::String(if context.is_official {
            context.name.clone()
        } else {
            "third-party".into()
        }),
    );
    metadata.insert(
        "marketplace_name_redacted".into(),
        AnalyticsValue::String(if context.is_official {
            context.marketplace_name.clone()
        } else {
            "third-party".into()
        }),
    );
    metadata.insert(
        "is_official_plugin".into(),
        AnalyticsValue::Bool(context.is_official),
    );
    metadata.insert(
        "plugin_id".into(),
        AnalyticsValue::String(if context.is_official {
            format!("{}@{}", context.name, context.marketplace_name)
        } else {
            "third-party".into()
        }),
    );
    metadata.insert(
        "trigger".into(),
        AnalyticsValue::String("cli-explicit".into()),
    );
    metadata.insert(
        "install_source".into(),
        AnalyticsValue::String("cli-explicit".into()),
    );
    if let Some(version) = context.version.as_ref() {
        metadata.insert("version".into(), AnalyticsValue::String(version.clone()));
    }
    metadata
}

async fn emit_installed_event(bus: Arc<AnalyticsBus>, arg: &str, plugins_dir: &Path) {
    let Some(context) = installed_event_context(arg, plugins_dir).await else {
        return;
    };
    bus.log_event(
        telemetry::tengu::plugin::INSTALLED,
        installed_event_metadata(&context),
    )
    .await;
}

/// Fetch/clone one external plugin-entry `source` into a scratch subdirectory
/// of `work` and return the resolved plugin-root directory to copy from.
fn resolve_external_plugin_source(
    source: &plugin::marketplace::MarketplacePluginExternalSource,
    work: &Path,
) -> Result<PathBuf, String> {
    current_thread_runtime().block_on(resolve_external_plugin_source_with_bus(
        source, work, None, false,
    ))
}

async fn resolve_external_plugin_source_with_bus(
    source: &plugin::marketplace::MarketplacePluginExternalSource,
    work: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<PathBuf, String> {
    use plugin::marketplace::MarketplacePluginExternalSource;

    let started = Instant::now();
    let result = (|| -> Result<PathBuf, String> {
        match source {
            MarketplacePluginExternalSource::Github { repo, git_ref, sha } => {
                let checkout = work.join("checkout");
                let head = plugin::clone_plugin_git_pinned(
                    &format!("https://github.com/{repo}.git"),
                    git_ref.as_deref().unwrap_or_default(),
                    sha.as_deref(),
                    &checkout,
                )?;
                verify_sha_pin(sha.as_deref(), &head)?;
                Ok(checkout)
            }
            // Oracle: `source:"url"` on a plugin entry names a GIT REPOSITORY
            // ("Full git repository URL (https:// or git@)"), not an archive —
            // that is the separate `archive` arm below. The whole checkout is
            // the plugin root (this arm has no `path`).
            MarketplacePluginExternalSource::Url { url, git_ref, sha } => {
                let checkout = work.join("checkout");
                let head = plugin::clone_plugin_git_pinned(
                    url,
                    git_ref.as_deref().unwrap_or_default(),
                    sha.as_deref(),
                    &checkout,
                )?;
                verify_sha_pin(sha.as_deref(), &head)?;
                Ok(checkout)
            }
            // A subdirectory of a larger repository (monorepo). The oracle
            // partial-clones (`--filter=tree:0`); this port does a full clone
            // and confines to `path` (same result, more bandwidth — see the
            // `GitSubdir` doc comment).
            MarketplacePluginExternalSource::GitSubdir {
                url,
                path,
                git_ref,
                sha,
            } => {
                let checkout = work.join("checkout");
                let head = plugin::clone_plugin_git_pinned(
                    url,
                    git_ref.as_deref().unwrap_or_default(),
                    sha.as_deref(),
                    &checkout,
                )?;
                verify_sha_pin(sha.as_deref(), &head)?;
                confined_source_subdir(&checkout, Some(path.as_str()))
            }
            MarketplacePluginExternalSource::Archive { url, sha256 } => {
                download_external_plugin_archive(url, sha256.as_deref(), work)
            }
            MarketplacePluginExternalSource::Npm {
                package,
                version,
                registry,
            } => materialize_npm_source(package, version.as_deref(), registry.as_deref(), work),
            MarketplacePluginExternalSource::Command { command, .. } => {
                let consented = command_source_is_consented(yes, command);
                plugin::materialize_command_plugin_source(source, work, consented)
            }
            MarketplacePluginExternalSource::Unsupported { error } => Err(format!(
                "plugin source type unsupported{}",
                error
                    .as_deref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            )),
        }
    })();
    emit_remote_fetch(analytics_bus, source, started, &result).await;
    result
}

fn materialize_external_plugin_source(
    plugins_dir: &Path,
    marketplace: &str,
    name: &str,
    source: &plugin::marketplace::MarketplacePluginExternalSource,
) -> Result<PathBuf, String> {
    current_thread_runtime().block_on(materialize_external_plugin_source_with_bus(
        plugins_dir,
        marketplace,
        name,
        source,
        None,
        false,
    ))
}

async fn materialize_external_plugin_source_with_bus(
    plugins_dir: &Path,
    marketplace: &str,
    name: &str,
    source: &plugin::marketplace::MarketplacePluginExternalSource,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<PathBuf, String> {
    let cache_key = external_source_cache_key(source);
    let destination = plugins_dir
        .join("source-cache")
        .join(sanitize(marketplace, false))
        .join(sanitize(name, false))
        .join(cache_key);
    if destination.is_dir() {
        plugin::ensure_plugin_manifest(&destination)?;
        return std::fs::canonicalize(&destination)
            .map_err(|error| format!("failed to resolve cached plugin source: {error}"));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "invalid plugin source cache destination".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create plugin source cache: {error}"))?;
    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let staging = parent.join(format!(".source-{}-{nonce}", std::process::id()));
    let payload = staging.join("payload");
    let work = staging.join("work");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&work)
        .map_err(|error| format!("failed to create plugin source staging: {error}"))?;

    let resolved = resolve_external_plugin_source_with_bus(source, &work, analytics_bus, yes).await;
    let result = (|| -> Result<(), String> {
        let resolved = resolved?;
        copy_dir(&resolved, &payload).map_err(|error| error.to_string())?;
        plugin::ensure_plugin_manifest(&payload)?;
        if let Err(error) = std::fs::rename(&payload, &destination) {
            // Another installer may have won the deterministic cache race.
            // Accept only a complete, validated winner.
            if !destination.is_dir() || plugin::ensure_plugin_manifest(&destination).is_err() {
                return Err(format!("failed to publish plugin source cache: {error}"));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result?;
    std::fs::canonicalize(&destination)
        .map_err(|error| format!("failed to resolve materialized plugin source: {error}"))
}

fn marketplace_entry(market_root: &Path, name: &str) -> Option<Value> {
    let manifest = branding::marketplace_manifest_path(market_root).ok()?;
    let raw = std::fs::read_to_string(manifest).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let plugins = value.get("plugins").and_then(Value::as_array)?;
    plugins
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .cloned()
}

pub(crate) fn marketplace_entry_for_plugin_id(
    plugins_dir: &Path,
    plugin_id: &str,
) -> Option<Value> {
    let (name, marketplace) = split_id(plugin_id);
    let marketplace = marketplace?;
    let root = load_registry(plugins_dir)
        .get(marketplace)
        .and_then(install_location)?;
    marketplace_entry(&root, name)
}

fn resolve_plugin_source(
    arg: &str,
    plugins_dir: &Path,
) -> Result<Option<(String, String, PathBuf)>, String> {
    current_thread_runtime().block_on(resolve_plugin_source_with_bus(
        arg,
        plugins_dir,
        None,
        false,
    ))
}

async fn resolve_plugin_source_with_bus(
    arg: &str,
    plugins_dir: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<Option<(String, String, PathBuf)>, String> {
    let (name, requested_marketplace) = split_id(arg);
    let registry = load_registry(plugins_dir);
    match requested_marketplace {
        Some(marketplace) => {
            let Some(entry) = registry.get(marketplace) else {
                return Ok(None);
            };
            let identity = plugin_policy::MarketplaceSourceIdentity::from_value(entry);
            plugin_policy::ensure_marketplace_source_allowed(Some(marketplace), identity.as_ref())?;
            let Some(root) = install_location(entry) else {
                return Ok(None);
            };
            let Some(source) = marketplace_entry_source_path_with_bus(
                &root,
                marketplace,
                name,
                plugins_dir,
                analytics_bus,
                yes,
            )
            .await?
            else {
                return Ok(None);
            };
            Ok(Some((
                format!("{name}@{marketplace}"),
                marketplace.to_string(),
                source,
            )))
        }
        None => {
            for (marketplace, entry) in &registry {
                let Some(root) = install_location(entry) else {
                    continue;
                };
                if marketplace_entry(&root, name).is_none() {
                    continue;
                }
                let identity = plugin_policy::MarketplaceSourceIdentity::from_value(entry);
                plugin_policy::ensure_marketplace_source_allowed(
                    Some(marketplace),
                    identity.as_ref(),
                )?;
                if let Some(source) = marketplace_entry_source_path_with_bus(
                    &root,
                    marketplace,
                    name,
                    plugins_dir,
                    analytics_bus,
                    yes,
                )
                .await?
                {
                    return Ok(Some((
                        format!("{name}@{marketplace}"),
                        marketplace.clone(),
                        source,
                    )));
                }
            }
            Ok(None)
        }
    }
}

/// Marketplace-level `defaultEnabled` override for one plugin entry.
///
/// Claude Code gives this value precedence over the plugin manifest. Missing
/// or non-boolean values leave the manifest fallback in control; the
/// marketplace validator is responsible for surfacing schema type errors.
fn marketplace_entry_default_enabled(market_root: &Path, name: &str) -> Option<bool> {
    let manifest = branding::marketplace_manifest_path(market_root).ok()?;
    let raw = std::fs::read_to_string(manifest).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("plugins")
        .and_then(Value::as_array)?
        .iter()
        .find(|plugin| plugin.get("name").and_then(Value::as_str) == Some(name))?
        .get("defaultEnabled")
        .and_then(Value::as_bool)
}

/// Manifest-level `defaultEnabled`, whose public-schema default is true.
fn plugin_default_enabled(plugin_root: &Path) -> bool {
    branding::plugin_manifest_path(plugin_root)
        .ok()
        .and_then(|manifest| std::fs::read_to_string(manifest).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("defaultEnabled").and_then(Value::as_bool))
        .unwrap_or(true)
}

/// Recursive directory copy (sync).
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = std::fs::symlink_metadata(&from)?.file_type();
        if kind.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("plugin source contains a symbolic link: {}", from.display()),
            ));
        }
        if kind.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Sanitize a path segment for the cache layout (mirrors the plugin crate's
/// `getVersionedCachePath` sanitizer: keep `[A-Za-z0-9-_]`, plus `.` for the
/// version segment; empty/`.`/`..` collapse to `-`).
fn sanitize(segment: &str, allow_dot: bool) -> String {
    let mapped: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || (allow_dot && c == '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if mapped.is_empty() || mapped == "." || mapped == ".." {
        "-".to_string()
    } else {
        mapped
    }
}

/// Parse the install-family `--scope` (default `user`); its invalid-scope
/// wording differs from enable/disable and marketplace.
fn parse_scope(scope: Option<&str>) -> Result<WritableScope, String> {
    match scope {
        None => Ok(WritableScope::User),
        Some(s) => parse_scope_str(s)
            .ok_or_else(|| format!("Invalid scope: {s}. Must be one of: user, project, local.")),
    }
}

fn empty_source_object() -> Utf16JsonProjection {
    Utf16JsonProjection::object(Vec::new()).expect("empty source object is valid")
}

fn source_bool(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("boolean JSON value is valid")
}

fn source_object_field(source: &Utf16JsonProjection, field: &str) -> Utf16JsonProjection {
    let field_units: Vec<u16> = field.encode_utf16().collect();
    source
        .subprojection_property_units(&field_units)
        .ok()
        .filter(|value| value.object_entries().is_some())
        .unwrap_or_else(empty_source_object)
}

#[derive(Default)]
struct SettingsRollbackJournal {
    receipts: Vec<SettingsUpdateReceipt>,
}

impl SettingsRollbackJournal {
    fn record(&mut self, receipt: Option<SettingsUpdateReceipt>) {
        if let Some(receipt) = receipt {
            self.receipts.push(receipt);
        }
    }

    fn rollback(&self) {
        for receipt in self.receipts.iter().rev() {
            if let Err(error) = rollback_settings_update(receipt) {
                tracing::warn!(%error, "failed to roll back plugin settings delta");
            }
        }
    }
}

/// Read a scope's `enabledPlugins` map.
fn read_enabled(scope: WritableScope, home: &Path, cwd: &Path) -> Utf16JsonProjection {
    let settings = read_settings_source(&scope_path(scope, home, cwd))
        .unwrap_or_else(|_| empty_source_object());
    source_object_field(&settings, "enabledPlugins")
}

fn enabled_plugin_value_is_array(scope: WritableScope, home: &Path, cwd: &Path, id: &str) -> bool {
    read_enabled(scope, home, cwd)
        .get_property(id)
        .is_some_and(|value| value.array_len().is_some())
}

/// Preserve Native array entries across enabledPlugins writes for the root
/// plugin and dependency rows in the install closure.
fn edit_enabled_after_install_with_journal(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    id: &str,
    value: Option<bool>,
    journal: &mut SettingsRollbackJournal,
) -> Result<(), String> {
    update_enabled_entry_with_journal(scope, home, cwd, id, value, true, Some(journal))
}

fn update_enabled_entry_with_journal(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    id: &str,
    value: Option<bool>,
    preserve_array: bool,
    journal: Option<&mut SettingsRollbackJournal>,
) -> Result<(), String> {
    let path = scope_path(scope, home, cwd);
    let receipt = transform_settings_source_with_receipt(&path, |settings| {
        let mut enabled = source_object_field(settings, "enabledPlugins");
        if preserve_array
            && enabled
                .get_property(id)
                .is_some_and(|value| value.array_len().is_some())
        {
            return Ok(());
        }
        match value {
            Some(enabled_value) => enabled
                .set_field(id, source_bool(enabled_value))
                .map_err(|error| error.to_string())?,
            None => {
                enabled
                    .remove_field(id)
                    .map_err(|error| error.to_string())?;
            }
        }
        settings
            .set_field("enabledPlugins", enabled)
            .map_err(|error| error.to_string())
    })?;
    if let Some(journal) = journal {
        journal.record(receipt);
    }
    Ok(())
}

/// `✘ Failed to <verb> plugin "<arg>": <reason>`.
fn fail(verb: &str, arg: &str, reason: &str) -> String {
    format!("✘ Failed to {verb} plugin \"{arg}\": {reason}")
}

/// Read a plugin's declared `userConfig` schema from its selected root manifest.
/// The marketplace entry supplies it only when the root `plugin.json` is
/// absent. A present malformed manifest never falls back to the entry.
fn read_user_config_schema(
    plugin_root: &Path,
    marketplace_entry: Option<&Value>,
) -> Map<String, Value> {
    let Ok(manifest_path) = branding::plugin_manifest_path(plugin_root) else {
        return Map::new();
    };
    match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let Ok(canonical_root) = std::fs::canonicalize(plugin_root) else {
                return Map::new();
            };
            let Ok(canonical_manifest) = std::fs::canonicalize(&manifest_path) else {
                return Map::new();
            };
            if !canonical_manifest.starts_with(canonical_root) {
                return Map::new();
            }
            std::fs::read_to_string(canonical_manifest)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|value| value.get("userConfig").and_then(Value::as_object).cloned())
                .unwrap_or_default()
        }
        Ok(_) => Map::new(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => marketplace_entry
            .and_then(|entry| entry.get("userConfig"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        Err(_) => Map::new(),
    }
}

/// Is a declared userConfig field sensitive (routed to secure storage)?
fn field_is_sensitive(schema: &Map<String, Value>, key: &str) -> bool {
    schema
        .get(key)
        .and_then(|f| f.get("sensitive"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Parse + validate the repeatable `--config key=value` flags against a plugin's
/// `userConfig` `schema`, byte-faithful with claude-code 2.1.207's parser: each
/// arg must contain `=` at a non-zero index (`indexOf("=") > 0`), the key must be
/// declared, and the value (the first line after `=`, trimmed) must be non-empty.
/// Returns the accepted `(key, value)` pairs (both sensitive + non-sensitive; the
/// caller routes them). Type coercion (`number`/`boolean`) is a follow-up — values
/// are carried as strings, which the `${user_config.*}` substitution accepts.
#[derive(Debug, Clone)]
struct ConfigPair {
    key: String,
    value: Value,
    raw_value: String,
}

fn parse_config_pairs(
    config: &[String],
    schema: &Map<String, Value>,
) -> Result<Vec<ConfigPair>, String> {
    let mut out = Vec::with_capacity(config.len());
    for raw in config {
        // `indexOf("=")` with the `s <= 0` guard: no `=`, or `=` at index 0.
        let Some(eq) = raw.find('=').filter(|&i| i > 0) else {
            return Err(format!(
                "--config expects KEY=VALUE, got \"{raw}\". Use --config key=value (repeatable)."
            ));
        };
        let key = &raw[..eq];
        // Value = first line after `=`, trimmed (CC `slice(s+1).split(/\r\n|\r|\n/,1)[0].trim()`).
        let value = raw[eq + 1..]
            .split(['\r', '\n'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !schema.contains_key(key) {
            let known: Vec<&str> = schema.keys().map(String::as_str).collect();
            let suffix = if known.is_empty() {
                String::new()
            } else {
                format!(" Known keys: {}.", known.join(", "))
            };
            return Err(format!(
                "--config key \"{key}\" isn't declared in this plugin's userConfig.{suffix}"
            ));
        }
        if value.is_empty() {
            return Err(format!(
                "--config {key}: value is empty. Omit the flag to leave \"{key}\" unset."
            ));
        }
        let field_type = schema
            .get(key)
            .and_then(|field| field.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("string");
        let typed = match field_type {
            "string" | "directory" | "file" => Value::String(value.clone()),
            "boolean" => match value.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => {
                    return Err(format!(
                        "--config {key}: expected a boolean (true or false), got \"{value}\"."
                    ));
                }
            },
            "number" => {
                let number = value
                    .parse::<f64>()
                    .map_err(|_| format!("--config {key}: expected a number, got \"{value}\"."))?;
                let number = serde_json::Number::from_f64(number).ok_or_else(|| {
                    format!("--config {key}: expected a finite number, got \"{value}\".")
                })?;
                Value::Number(number)
            }
            other => {
                return Err(format!(
                    "--config {key}: unsupported userConfig type \"{other}\"."
                ));
            }
        };
        out.push(ConfigPair {
            key: key.to_string(),
            value: typed,
            raw_value: value,
        });
    }
    Ok(out)
}

/// Persist the non-sensitive `--config` values to settings
/// `pluginConfigs[<plugin_key>].options` at `scope` — the exact map the
/// composition-root loader reads back (`PluginManager::load_plugin` looks it up
/// by the installed `name@marketplace` id for cache-installed plugins, else the
/// bare local-plugin name).
/// Existing `options` / `mcpServers` are preserved; a value now declared
/// SENSITIVE is scrubbed from plaintext `options` (claude-code's `{...n, ...u}`
/// stale-key scrub) and instead routed to secure storage — the keychain write is
/// a documented follow-up, so a sensitive `--config` value is validated here but
/// not persisted to plaintext settings.
fn persist_plugin_options(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    plugin_key: &str,
    schema: &Map<String, Value>,
    pairs: &[ConfigPair],
) -> Result<(), String> {
    persist_plugin_options_with_journal(scope, home, cwd, plugin_key, schema, pairs, None)
}

fn persist_plugin_options_with_journal(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    plugin_key: &str,
    schema: &Map<String, Value>,
    pairs: &[ConfigPair],
    journal: Option<&mut SettingsRollbackJournal>,
) -> Result<(), String> {
    let path = scope_path(scope, home, cwd);
    let receipt = transform_settings_source_with_receipt(&path, |settings| {
        let mut plugin_configs = source_object_field(settings, "pluginConfigs");
        let mut entry = source_object_field(&plugin_configs, plugin_key);
        let mut options = source_object_field(&entry, "options");
        for pair in pairs {
            if field_is_sensitive(schema, &pair.key) {
                // Never write a secret to plaintext settings; drop any stale copy.
                options
                    .remove_field(&pair.key)
                    .map_err(|error| error.to_string())?;
            } else {
                let value = Utf16JsonProjection::from_serde_value(&pair.value)
                    .map_err(|error| error.to_string())?;
                options
                    .set_field(&pair.key, value)
                    .map_err(|error| error.to_string())?;
            }
        }
        entry
            .set_field("options", options)
            .map_err(|error| error.to_string())?;
        plugin_configs
            .set_field(plugin_key, entry)
            .map_err(|error| error.to_string())?;
        settings
            .set_field("pluginConfigs", plugin_configs)
            .map_err(|error| error.to_string())
    })?;
    if let Some(journal) = journal {
        journal.record(receipt);
    }
    Ok(())
}

/// The `projectPath` an install record carries at this scope: the realpath of
/// `cwd` for `project`/`local`, `None` for `user` (which is cwd-independent).
/// Records are keyed per (scope, projectPath), so this identifies the slot.
fn project_path(scope: WritableScope, cwd: &Path) -> Option<String> {
    match scope {
        WritableScope::User => None,
        WritableScope::Project | WritableScope::Local => Some(
            std::fs::canonicalize(cwd)
                .unwrap_or_else(|_| cwd.to_path_buf())
                .display()
                .to_string(),
        ),
    }
}

/// Does an installed record occupy the (scope, projectPath) slot? For `user` the
/// scope match suffices; for project/local the record's `projectPath` must match
/// the current one too (distinct projects install the same plugin independently).
fn record_matches(rec: &Value, scope: WritableScope, proj: &Option<String>) -> bool {
    if rec.get("scope").and_then(Value::as_str) != Some(scope_label(scope)) {
        return false;
    }
    match proj {
        None => true,
        Some(p) => rec.get("projectPath").and_then(Value::as_str) == Some(p.as_str()),
    }
}

const SENSITIVE_CONFIG_KEYS_FIELD: &str = "sensitiveConfigKeys";

fn sensitive_config_keys_from_schema(schema: &Map<String, Value>) -> BTreeSet<String> {
    schema
        .iter()
        .filter_map(|(key, field)| {
            field
                .get("sensitive")
                .and_then(Value::as_bool)
                .filter(|sensitive| *sensitive)
                .map(|_| key.clone())
        })
        .collect()
}

fn stored_sensitive_config_keys(record: &Value) -> BTreeSet<String> {
    record
        .get(SENSITIVE_CONFIG_KEYS_FIELD)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Keep the union of sensitive field names on every scope record for a plugin.
/// Plugin secrets are namespaced by plugin id, not scope, so removing one
/// scope must leave enough metadata on the surviving records to clean up the
/// shared credentials when the last scope is removed.
fn remember_sensitive_config_keys(
    installed: &mut Value,
    plugin_id: &str,
    additional: &BTreeSet<String>,
) -> bool {
    let Some(records) = installed
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .and_then(|plugins| plugins.get_mut(plugin_id))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };

    let mut keys = additional.clone();
    for record in records.iter() {
        keys.extend(stored_sensitive_config_keys(record));
    }
    if keys.is_empty() {
        return false;
    }
    let encoded = Value::Array(keys.into_iter().map(Value::String).collect());
    let mut changed = false;
    for record in records {
        let Some(record) = record.as_object_mut() else {
            continue;
        };
        if record.get(SENSITIVE_CONFIG_KEYS_FIELD) != Some(&encoded) {
            record.insert(SENSITIVE_CONFIG_KEYS_FIELD.to_string(), encoded.clone());
            changed = true;
        }
    }
    changed
}

fn sensitive_config_keys_for_plugin_records(
    plugins_dir: &Path,
    installed: &Value,
    plugin_id: &str,
    marketplace_entry: Option<&Value>,
) -> BTreeSet<String> {
    let Some(records) = installed
        .get("plugins")
        .and_then(Value::as_object)
        .and_then(|plugins| plugins.get(plugin_id))
        .and_then(Value::as_array)
    else {
        return BTreeSet::new();
    };

    let mut keys = BTreeSet::new();
    for record in records {
        keys.extend(stored_sensitive_config_keys(record));
        let Some(install_path) = record.get("installPath").and_then(Value::as_str) else {
            continue;
        };
        let Some(install_path) = confined_cache_record_path(plugins_dir, Path::new(install_path))
        else {
            continue;
        };
        keys.extend(sensitive_config_keys_from_schema(&read_user_config_schema(
            &install_path,
            marketplace_entry,
        )));
    }
    keys
}

/// `plugin install <plugin[@market]> [--scope] [--config]`.
pub fn run_install(
    arg: &str,
    scope: Option<&str>,
    config: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    current_thread_runtime().block_on(run_install_async(
        arg,
        scope,
        config,
        plugins_dir,
        home,
        cwd,
        None,
        false,
    ))
}

pub fn run_install_with_bus(
    arg: &str,
    scope: Option<&str>,
    config: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    current_thread_runtime().block_on(run_install_async(
        arg,
        scope,
        config,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
        false,
    ))
}

async fn run_install_async(
    arg: &str,
    scope: Option<&str>,
    config: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<String, String> {
    run_install_async_with_credential_updates(
        arg,
        scope,
        config,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
        yes,
        None,
    )
    .await
}

async fn run_install_async_with_credential_updates(
    arg: &str,
    scope: Option<&str>,
    config: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
    credential_update: Option<(&secret::CredentialManager, &str, &[(String, String)])>,
) -> Result<String, String> {
    let parsed_scope = parse_scope(scope)?;
    let telemetry_scope = crate::plugin_telemetry::telemetry_scope(Some(scope_label(parsed_scope)));
    let mut installed_tx = match plugin::installed::InstalledRegistryTransaction::begin(plugins_dir)
    {
        Ok(tx) => tx,
        Err(error) => {
            crate::plugin_telemetry::emit_plugin_state_file_error(
                analytics_bus,
                "install",
                "transaction_begin",
                registry_error_kind(&error),
            )
            .await;
            return Err(format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &error)
            ));
        }
    };
    let previous_db = installed_tx.document().clone();
    let mut created_paths = Vec::new();
    let mut settings_journal = SettingsRollbackJournal::default();
    let mut previous_secrets = Vec::new();
    if let Some((credentials, plugin_key, updates)) = credential_update {
        for (key, _) in updates {
            let previous = credentials
                .get_plugin_secret(plugin_key, key)
                .await
                .map_err(|error| format!("Failed to read plugin secret {key}: {error}"))?
                .map(|secret| secret.expose_secret().clone());
            previous_secrets.push((key.clone(), previous));
        }
        for (index, (key, value)) in updates.iter().enumerate() {
            if let Err(error) = credentials.set_plugin_secret(plugin_key, key, value).await {
                rollback_plugin_secrets(credentials, plugin_key, &previous_secrets[..=index]).await;
                return Err(format!("Failed to store plugin secret {key}: {error}"));
            }
        }
    }
    let result = run_install_inner(
        arg,
        scope,
        config,
        plugins_dir,
        home,
        cwd,
        &mut installed_tx,
        &mut settings_journal,
        false,
        None,
        &mut Vec::new(),
        &mut created_paths,
        analytics_bus,
        yes,
    )
    .await;
    if result.is_err() {
        if let Some((credentials, plugin_key, _)) = credential_update {
            rollback_plugin_secrets(credentials, plugin_key, &previous_secrets).await;
        }
        rollback_install_transaction(
            plugins_dir,
            &settings_journal,
            &installed_tx,
            &previous_db,
            &created_paths,
        );
    } else {
        crate::plugin_telemetry::emit_plugin_cli_result(
            analytics_bus,
            telemetry::tengu::plugin::INSTALLED_CLI,
            crate::plugin_telemetry::PluginCommandOutcome::Success,
            telemetry_scope,
            1,
        )
        .await;
    }
    result
}

pub fn restore_file(path: &Path, previous: Option<&[u8]>) {
    match previous {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = std::fs::write(path, bytes) {
                tracing::warn!(path = %path.display(), %error, "failed to roll back plugin transaction file");
            }
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn install_paths(db: &Value) -> std::collections::HashSet<PathBuf> {
    db.get("plugins")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|plugins| plugins.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|record| record.get("installPath").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect()
}

fn confined_existing_child(root: &Path, candidate: &Path) -> Option<PathBuf> {
    if std::fs::symlink_metadata(root)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let relative = candidate.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let canonical_candidate = std::fs::canonicalize(candidate).ok()?;
    (canonical_candidate != canonical_root
        && canonical_candidate.starts_with(&canonical_root)
        && canonical_candidate.is_dir())
    .then_some(canonical_candidate)
}

/// Validate an `installPath` loaded from the mutable installed registry before
/// any marker write or recursive deletion.
pub fn confined_cache_record_path(plugins_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    confined_existing_child(&plugins_dir.join("cache"), candidate)
}

/// Resolve one plugin data directory without letting an installed-registry id
/// become an absolute path or a multi-component traversal.
pub fn confined_plugin_data_path(plugins_dir: &Path, id: &str) -> Option<PathBuf> {
    let id_path = Path::new(id);
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || id_path.is_absolute()
        || id_path.components().count() != 1
    {
        return None;
    }
    let data_key = sanitize(id, false);
    confined_existing_child(
        &plugins_dir.join("data"),
        &plugins_dir.join("data").join(data_key),
    )
}

pub fn rollback_installed_registry(installed_tx: &plugin::installed::InstalledRegistryTransaction) {
    if let Err(error) = installed_tx.restore_previous() {
        tracing::warn!(%error, "failed to restore installed plugin registry");
    }
}

pub fn edit_enabled_strict(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    id: &str,
    value: Option<bool>,
) -> Result<(), String> {
    edit_enabled_strict_inner(scope, home, cwd, id, value, None)
}

fn edit_enabled_strict_with_journal(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    id: &str,
    value: Option<bool>,
    journal: &mut SettingsRollbackJournal,
) -> Result<(), String> {
    edit_enabled_strict_inner(scope, home, cwd, id, value, Some(journal))
}

fn edit_enabled_strict_inner(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    id: &str,
    value: Option<bool>,
    journal: Option<&mut SettingsRollbackJournal>,
) -> Result<(), String> {
    update_enabled_entry_with_journal(scope, home, cwd, id, value, false, journal)
}

fn clear_plugin_config_strict(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    plugin_key: &str,
) -> Result<(), String> {
    clear_plugin_config_strict_inner(scope, home, cwd, plugin_key, None)
}

fn clear_plugin_config_strict_with_journal(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    plugin_key: &str,
    journal: &mut SettingsRollbackJournal,
) -> Result<(), String> {
    clear_plugin_config_strict_inner(scope, home, cwd, plugin_key, Some(journal))
}

fn clear_plugin_config_strict_inner(
    scope: WritableScope,
    home: &Path,
    cwd: &Path,
    plugin_key: &str,
    journal: Option<&mut SettingsRollbackJournal>,
) -> Result<(), String> {
    let path = scope_path(scope, home, cwd);
    if !path.exists() {
        return Ok(());
    }
    let receipt = transform_settings_source_with_receipt(&path, |settings| {
        let mut plugin_configs = source_object_field(settings, "pluginConfigs");
        if plugin_configs.get_property(plugin_key).is_none() {
            return Ok(());
        }
        plugin_configs
            .remove_field(plugin_key)
            .map_err(|error| error.to_string())?;
        settings
            .set_field("pluginConfigs", plugin_configs)
            .map_err(|error| error.to_string())
    })?;
    if let Some(journal) = journal {
        journal.record(receipt);
    }
    Ok(())
}

fn canonical_record_install_path(plugins_dir: &Path, record: &Value) -> Option<PathBuf> {
    record
        .get("installPath")
        .and_then(Value::as_str)
        .and_then(|path| confined_cache_record_path(plugins_dir, Path::new(path)))
}

pub fn document_references_install_path(doc: &Value, plugins_dir: &Path, candidate: &Path) -> bool {
    doc.get("plugins")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|plugins| plugins.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|record| canonical_record_install_path(plugins_dir, record))
        .any(|path| path == candidate)
}

pub fn plugin_has_records(doc: &Value, id: &str) -> bool {
    doc.get("plugins")
        .and_then(|plugins| plugins.get(id))
        .and_then(Value::as_array)
        .is_some_and(|records| !records.is_empty())
}

pub fn unreferenced_removed_record_paths(
    plugins_dir: &Path,
    doc: &Value,
    removed_records: &[Value],
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in removed_records
        .iter()
        .filter_map(|record| canonical_record_install_path(plugins_dir, record))
    {
        if !document_references_install_path(doc, plugins_dir, &path) && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn mark_orphaned_cache_paths(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::write(path.join(".orphaned_at"), iso_now());
    }
}

fn ensure_confined_cache_parent(root: &Path, parent: &Path) -> Result<PathBuf, String> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| "Refusing to publish a plugin outside its cache root".to_string())?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("Refusing to publish a plugin outside its cache root".to_string());
    }
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err("Refusing to use a symlinked plugin cache root".to_string());
            }
            if !metadata.file_type().is_dir() {
                return Err("Plugin cache root is not a directory".to_string());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(root).map_err(|create_error| {
                format!("Failed to create plugin cache root: {create_error}")
            })?;
        }
        Err(error) => {
            return Err(format!("Failed to inspect plugin cache root: {error}"));
        }
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if let Ok(metadata) = std::fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err("Refusing to use a symlinked plugin cache path".to_string());
            }
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "Plugin cache path component is not a directory: {}",
                    current.display()
                ));
            }
        }
    }
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create plugin cache path: {error}"))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("Failed to resolve plugin cache root: {error}"))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|error| format!("Failed to resolve plugin cache path: {error}"))?;
    if canonical_parent == canonical_root || canonical_parent.starts_with(&canonical_root) {
        Ok(canonical_parent)
    } else {
        Err("Refusing to publish a plugin outside its cache root".to_string())
    }
}

struct PublishedCacheDir {
    destination: PathBuf,
    backup: Option<PathBuf>,
}

impl PublishedCacheDir {
    fn rollback(self) {
        if let Some(backup) = self.backup {
            let _ = std::fs::remove_dir_all(&self.destination);
            let _ = std::fs::rename(backup, self.destination);
        } else {
            let _ = std::fs::remove_dir_all(self.destination);
        }
    }

    fn finalize(self) {
        if let Some(backup) = self.backup {
            let _ = std::fs::remove_dir_all(backup);
        }
    }
}

fn stage_and_publish_cache_dir(
    plugins_dir: &Path,
    source: &Path,
    destination: &Path,
) -> Result<PublishedCacheDir, String> {
    let cache_root = plugins_dir.join("cache");
    let parent = destination
        .parent()
        .ok_or_else(|| "invalid plugin cache destination".to_string())?;
    let canonical_parent = ensure_confined_cache_parent(&cache_root, parent)?;
    if destination.exists() && confined_cache_record_path(plugins_dir, destination).is_none() {
        return Err("Refusing to replace a plugin cache outside its root".to_string());
    }
    let staging = parent.join(format!(
        ".staged-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    if let Err(error) = copy_dir(source, &staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error.to_string());
    }
    // The caller validated the selected metadata file before staging. A
    // missing plugin.json is valid for marketplace plugins whose entry
    // supplies the manifest fields; `ensure_plugin_manifest` would mistake
    // that optional-manifest tree for an MCPB bundle and require root
    // `manifest.json`.
    let canonical_staged = std::fs::canonicalize(&staging)
        .map_err(|error| format!("Failed to resolve staged plugin cache: {error}"))?;
    if canonical_staged.parent() != Some(canonical_parent.as_path()) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("Refusing to publish a plugin outside its cache root".to_string());
    }
    if canonical_staged == destination {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("Plugin staging path collides with its destination".to_string());
    }
    let backup = destination.exists().then(|| {
        parent.join(format!(
            ".backup-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    });
    if let Some(backup_path) = &backup {
        std::fs::rename(destination, backup_path)
            .map_err(|error| format!("Failed to stage existing plugin cache: {error}"))?;
    }
    if let Err(error) = std::fs::rename(&canonical_staged, destination) {
        if let Some(backup_path) = &backup {
            let _ = std::fs::rename(backup_path, destination);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("Failed to publish plugin cache: {error}"));
    }
    Ok(PublishedCacheDir {
        destination: destination.to_path_buf(),
        backup,
    })
}

fn rollback_install_transaction(
    plugins_dir: &Path,
    settings_journal: &SettingsRollbackJournal,
    installed_tx: &plugin::installed::InstalledRegistryTransaction,
    previous_db: &Value,
    created_paths: &[PathBuf],
) {
    for path in created_paths.iter().rev() {
        if let Some(path) = confined_cache_record_path(plugins_dir, path) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    let previous_paths = install_paths(previous_db);
    for path in install_paths(installed_tx.document()) {
        if !previous_paths.contains(&path) {
            if let Some(path) = confined_cache_record_path(plugins_dir, &path) {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
    settings_journal.rollback();
    rollback_installed_registry(installed_tx);
}

#[allow(clippy::too_many_arguments)]
async fn run_install_inner(
    arg: &str,
    scope: Option<&str>,
    config: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    installed_tx: &mut plugin::installed::InstalledRegistryTransaction,
    settings_journal: &mut SettingsRollbackJournal,
    auto_installed: bool,
    required_by: Option<&str>,
    dependency_stack: &mut Vec<String>,
    created_paths: &mut Vec<PathBuf>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<String, String> {
    // WritableScope is validated BEFORE the "Installing plugin …" progress prefix — the
    // binary emits the bare `Invalid scope: …` line with no prefix.
    let scope = parse_scope(scope)?;

    if !config.is_empty() && !matches!(scope, WritableScope::User) {
        return Err(format!(
            "Installing plugin \"{}\"...{}",
            arg,
            fail("install", arg, "--config can only be used with user scope.")
        ));
    }

    let (name, market) = split_id(arg);
    let registry = load_registry(plugins_dir);

    // Resolve only after the marketplace policy gate. External typed sources
    // are materialized into a confined source cache by the same resolver.
    let resolved = resolve_plugin_source_with_bus(arg, plugins_dir, analytics_bus, yes)
        .await
        .map_err(|reason| {
            format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &reason)
            )
        })?;
    let Some((_, market_name, plugin_src)) = resolved else {
        let reason = match market {
            Some(market) => format!(
                "Plugin \"{name}\" not found in marketplace \"{market}\". Your local copy may be out of date — try `lingxi-cli plugin marketplace update {market}`."
            ),
            None => format!("Plugin \"{name}\" not found in any configured marketplace"),
        };
        return Err(format!(
            "Installing plugin \"{arg}\"...{}",
            fail("install", arg, &reason)
        ));
    };

    // Use the runtime's typed manifest parser before deriving cache identity
    // or installing user configuration. A present malformed native manifest
    // must fail here rather than becoming `unknown` or falling back to the
    // other metadata format.
    let manifest_version = plugin::discovery::plugin_manifest_version(&plugin_src)
        .await
        .map_err(|reason| {
            format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &reason)
            )
        })?;
    let marketplace_catalog_entry = registry
        .get(&market_name)
        .and_then(install_location)
        .and_then(|root| marketplace_entry(&root, name));

    let full_id = format!("{name}@{market_name}");
    if dependency_stack.contains(&full_id) {
        let mut cycle = dependency_stack.clone();
        cycle.push(full_id.clone());
        return Err(format!(
            "Plugin dependency cycle detected: {}",
            cycle.join(" -> ")
        ));
    }
    let default_enabled = registry
        .get(&market_name)
        .and_then(install_location)
        .and_then(|root| marketplace_entry_default_enabled(&root, name))
        .unwrap_or_else(|| plugin_default_enabled(&plugin_src));
    let policy_source = registry
        .get(&market_name)
        .and_then(plugin_policy::MarketplaceSourceIdentity::from_value);
    plugin_policy::ensure_marketplace_source_allowed(Some(&market_name), policy_source.as_ref())
        .map_err(|reason| {
            format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &reason)
            )
        })?;

    dependency_stack.push(full_id.clone());
    let dependency_result = install_declared_dependencies(
        &full_id,
        &market_name,
        &plugin_src,
        Some(scope_label(scope)),
        plugins_dir,
        home,
        cwd,
        installed_tx,
        settings_journal,
        dependency_stack,
        created_paths,
        analytics_bus,
        yes,
    )
    .await;
    dependency_stack.pop();
    dependency_result?;

    // `--config key=value` userConfig persistence. Parse + validate against the
    // plugin's declared schema (byte-faithful errors, no "Installing…" prefix —
    // like the scope error), then persist the NON-SENSITIVE values to settings
    // `pluginConfigs[<name@marketplace>].options` at the chosen scope. That is
    // the exact map the composition-root loader reads back for installed
    // marketplace/cache plugins.
    // Done BEFORE materialisation so a bad `--config` aborts without a half
    // install; a no-op when no `--config` was passed (byte-identical to before).
    let schema = read_user_config_schema(&plugin_src, marketplace_catalog_entry.as_ref());
    let sensitive_config_keys = sensitive_config_keys_from_schema(&schema);
    let config_pairs = parse_config_pairs(config, &schema)?;
    if !config_pairs.is_empty() {
        persist_plugin_options_with_journal(
            scope,
            home,
            cwd,
            &full_id,
            &schema,
            &config_pairs,
            Some(settings_journal),
        )?;
    }

    // Version from the selected, typed plugin manifest.
    let version = manifest_version
        .or_else(|| {
            marketplace_catalog_entry
                .as_ref()
                .and_then(|entry| entry.get("version"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_string());

    let proj = project_path(scope, cwd);
    // Already installed AT THIS (scope, projectPath) slot? A record at a
    // different scope does NOT block — install appends a second per-scope record.
    if installed_tx
        .document()
        .get("plugins")
        .and_then(|p| p.get(&full_id))
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|r| record_matches(r, scope, &proj)))
    {
        let sensitive_keys_changed = remember_sensitive_config_keys(
            installed_tx.document_mut(),
            &full_id,
            &sensitive_config_keys,
        );
        if auto_installed {
            add_required_by_metadata(
                installed_tx.document_mut(),
                &full_id,
                scope,
                &proj,
                required_by,
            );
        }
        if auto_installed || sensitive_keys_changed {
            installed_tx.persist()?;
        }
        if auto_installed {
            edit_enabled_after_install_with_journal(
                scope,
                home,
                cwd,
                &full_id,
                Some(true),
                settings_journal,
            )?;
        }
        return Ok(format!(
            "Installing plugin \"{arg}\"...✔ Plugin \"{full_id}\" is already installed (scope: {})",
            scope_label(scope)
        ));
    }

    // Materialize into the versioned cache.
    let dest = plugins_dir
        .join("cache")
        .join(sanitize(&market_name, false))
        .join(sanitize(name, false))
        .join(sanitize(&version, true));
    if !dest.exists() {
        let parent = dest.parent().ok_or_else(|| {
            format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, "invalid plugin cache destination")
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &error.to_string())
            )
        })?;
        let staging = parent.join(format!(
            ".{}.install-{}-{}",
            sanitize(name, false),
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&staging);
        if let Err(error) = copy_dir(&plugin_src, &staging) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &error.to_string())
            ));
        }
        match std::fs::rename(&staging, &dest) {
            Ok(()) => created_paths.push(dest.clone()),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                if !dest.exists() {
                    return Err(format!(
                        "Installing plugin \"{arg}\"...{}",
                        fail("install", arg, &error.to_string())
                    ));
                }
            }
        }
    }

    // Record (v2) — `projectPath` is the LAST field, present only for
    // project/local scope (matching the binary's on-disk shape).
    let now = iso_now();
    let mut record = serde_json::Map::new();
    record.insert(
        "scope".to_string(),
        Value::String(scope_label(scope).to_string()),
    );
    record.insert(
        "installPath".to_string(),
        Value::String(dest.display().to_string()),
    );
    record.insert("version".to_string(), Value::String(version.clone()));
    record.insert("installedAt".to_string(), Value::String(now.clone()));
    record.insert("lastUpdated".to_string(), Value::String(now));
    if auto_installed {
        record.insert("auto".to_string(), Value::Bool(true));
    }
    if let Some(required_by) = required_by {
        record.insert(
            "requiredBy".to_string(),
            Value::Array(vec![Value::String(required_by.to_string())]),
        );
    }
    if let Some(p) = &proj {
        record.insert("projectPath".to_string(), Value::String(p.clone()));
    }
    // Append to the plugin's record array (create it if this is the first scope).
    if let Some(plugins) = installed_tx
        .document_mut()
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
    {
        let arr = plugins
            .entry(full_id.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(a) = arr.as_array_mut() {
            a.push(Value::Object(record));
        }
    }
    remember_sensitive_config_keys(
        installed_tx.document_mut(),
        &full_id,
        &sensitive_config_keys,
    );
    installed_tx
        .persist()
        .map_err(|e| format!("Installing plugin \"{arg}\"...{}", fail("install", arg, &e)))?;
    edit_enabled_after_install_with_journal(
        scope,
        home,
        cwd,
        &full_id,
        Some(if auto_installed {
            true
        } else {
            default_enabled
        }),
        settings_journal,
    )
    .map_err(|e| format!("Installing plugin \"{arg}\"...{}", fail("install", arg, &e)))?;

    Ok(format!(
        "Installing plugin \"{arg}\"...✔ Successfully installed plugin: {full_id} (scope: {})",
        scope_label(scope)
    ))
}

#[allow(clippy::too_many_arguments)]
async fn install_declared_dependencies(
    owner_id: &str,
    owner_marketplace: &str,
    plugin_source: &Path,
    scope: Option<&str>,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    installed_tx: &mut plugin::installed::InstalledRegistryTransaction,
    settings_journal: &mut SettingsRollbackJournal,
    dependency_stack: &mut Vec<String>,
    created_paths: &mut Vec<PathBuf>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    yes: bool,
) -> Result<(), String> {
    let registry = load_registry(plugins_dir);
    let market_root = registry
        .get(owner_marketplace)
        .and_then(install_location)
        .ok_or_else(|| format!("Marketplace \"{owner_marketplace}\" is not available"))?;
    let owner_name = name_of(owner_id);
    let manifest = branding::plugin_manifest_path(plugin_source)
        .ok()
        .and_then(|manifest| std::fs::read_to_string(manifest).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let entry = marketplace_entry(&market_root, owner_name);
    let manifest_dependencies = plugin::parse_dependencies(
        manifest
            .as_ref()
            .and_then(|manifest| manifest.get("dependencies")),
    )?;
    let marketplace_dependencies =
        plugin::parse_dependencies(entry.as_ref().and_then(|entry| entry.get("dependencies")))?;
    let dependencies = plugin::merge_dependency_requirements(
        manifest_dependencies
            .into_iter()
            .chain(marketplace_dependencies),
    );
    if dependencies.is_empty() {
        return Ok(());
    }
    let cross_market_allow = branding::marketplace_manifest_path(&market_root)
        .ok()
        .and_then(|manifest| std::fs::read_to_string(manifest).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("allowCrossMarketplaceDependenciesOn")
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default();

    let enabled_scope = parse_scope(scope)?;
    for (dependency, requirements) in dependencies {
        let dependency_id = dependency.resolved_id(owner_marketplace);
        // Native preserves dependency `sha` metadata but its current dependency
        // selector reads the version constraint only. Git checkout pins remain
        // attached to the dependency's resolved plugin source, not this field.
        let dependency_marketplace = marketplace_of(&dependency_id).unwrap_or(owner_marketplace);
        if dependency_marketplace != owner_marketplace
            && !cross_market_allow
                .iter()
                .any(|allowed| allowed.as_str() == Some(dependency_marketplace))
        {
            return Err(format!(
                "Plugin \"{owner_id}\" cannot install cross-marketplace dependency \"{dependency_id}\""
            ));
        }
        let (_, _, dependency_source) =
            resolve_plugin_source_with_bus(&dependency_id, plugins_dir, analytics_bus, yes)
                .await?
                .ok_or_else(|| format!("Plugin dependency \"{dependency_id}\" was not found"))?;
        let available = plugin::discovery::plugin_manifest_version(&dependency_source)
            .await
            .map_err(|error| {
                format!("Plugin dependency \"{dependency_id}\" has an invalid manifest: {error}")
            })?
            .or_else(|| {
                marketplace_entry_for_plugin_id(plugins_dir, &dependency_id).and_then(|entry| {
                    entry
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            })
            .unwrap_or_else(|| "unknown".to_string());
        let array_enabled = enabled_plugin_value_is_array(enabled_scope, home, cwd, &dependency_id);
        let dependency_project = project_path(enabled_scope, cwd);
        let installed_dependency = installed_tx
            .document()
            .get("plugins")
            .and_then(|plugins| plugins.get(&dependency_id))
            .and_then(Value::as_array)
            .and_then(|records| {
                records
                    .iter()
                    .find(|record| record_matches(record, enabled_scope, &dependency_project))
            });
        // Native's array branch suppresses a forced update for an existing
        // target-scope dependency. It does not erase version constraints:
        // compare that installed version, while fresh dependencies still use
        // the available version from their source.
        let version_to_check = if array_enabled && installed_dependency.is_some() {
            installed_dependency
                .and_then(|record| record.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        } else {
            &available
        };
        if !plugin::version_satisfies_all(version_to_check, &requirements)? {
            return Err(format!(
                "Plugin dependency \"{dependency_id}\" version {version_to_check} does not satisfy {}",
                requirements.join(", ")
            ));
        }
        Box::pin(run_install_inner(
            &dependency_id,
            scope,
            &[],
            plugins_dir,
            home,
            cwd,
            installed_tx,
            settings_journal,
            true,
            Some(owner_id),
            dependency_stack,
            created_paths,
            analytics_bus,
            yes,
        ))
        .await?;
    }
    Ok(())
}

fn add_required_by_metadata(
    installed: &mut Value,
    plugin_id: &str,
    scope: WritableScope,
    project: &Option<String>,
    required_by: Option<&str>,
) {
    let Some(required_by) = required_by else {
        return;
    };
    let Some(records) = installed
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .and_then(|plugins| plugins.get_mut(plugin_id))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let Some(record) = records
        .iter_mut()
        .find(|record| record_matches(record, scope, project))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let required = record
        .entry("requiredBy")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(required) = required.as_array_mut() else {
        return;
    };
    if !required
        .iter()
        .any(|value| value.as_str() == Some(required_by))
    {
        required.push(Value::String(required_by.to_string()));
    }
}

/// Async production wrapper that commits sensitive `--config` fields to the
/// platform credential store and restores their previous values if the install
/// transaction fails.
pub async fn run_install_with_credential_factory<F, Fut>(
    arg: &str,
    scope: Option<&str>,
    yes: bool,
    config: &[String],
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<Arc<AnalyticsBus>>,
    credential_factory: F,
) -> Result<String, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Arc<secret::CredentialManager>, String>>,
{
    let Some((plugin_key, _, plugin_source)) =
        resolve_plugin_source_with_bus(arg, plugins_dir, analytics_bus.as_ref(), yes).await?
    else {
        let result = run_install_async(
            arg,
            scope,
            config,
            plugins_dir,
            home,
            cwd,
            analytics_bus.as_ref(),
            yes,
        )
        .await;
        if result.is_ok() {
            if let Some(bus) = analytics_bus.clone() {
                emit_installed_event(bus, arg, plugins_dir).await;
            }
        }
        return result;
    };
    plugin::discovery::plugin_manifest_version(&plugin_source)
        .await
        .map_err(|reason| {
            format!(
                "Installing plugin \"{arg}\"...{}",
                fail("install", arg, &reason)
            )
        })?;
    let marketplace_entry = marketplace_entry_for_plugin_id(plugins_dir, &plugin_key);
    let schema = read_user_config_schema(&plugin_source, marketplace_entry.as_ref());
    let pairs = parse_config_pairs(config, &schema)?;
    let sensitive: Vec<&ConfigPair> = pairs
        .iter()
        .filter(|pair| field_is_sensitive(&schema, &pair.key))
        .collect();
    if sensitive.is_empty() {
        let result = run_install_async(
            arg,
            scope,
            config,
            plugins_dir,
            home,
            cwd,
            analytics_bus.as_ref(),
            yes,
        )
        .await;
        if result.is_ok() {
            if let Some(bus) = analytics_bus.clone() {
                emit_installed_event(bus, arg, plugins_dir).await;
            }
        }
        return result;
    }

    let credentials = credential_factory().await?;
    let updates: Vec<(String, String)> = sensitive
        .iter()
        .map(|pair| (pair.key.clone(), pair.raw_value.clone()))
        .collect();
    // Secret writes happen only after the install wrapper has acquired the
    // installed-registry transaction lock. Final uninstall uses that same
    // lock while deleting the shared secret namespace and removing records.
    let result = run_install_async_with_credential_updates(
        arg,
        scope,
        config,
        plugins_dir,
        home,
        cwd,
        analytics_bus.as_ref(),
        yes,
        Some((&credentials, &plugin_key, &updates)),
    )
    .await;
    if result.is_ok() {
        if let Some(bus) = analytics_bus {
            emit_installed_event(bus, arg, plugins_dir).await;
        }
    }
    result
}

async fn rollback_plugin_secrets(
    credentials: &secret::CredentialManager,
    plugin: &str,
    previous: &[(String, Option<String>)],
) {
    for (key, value) in previous.iter().rev() {
        let result = match value {
            Some(value) => credentials.set_plugin_secret(plugin, key, value).await,
            None => credentials.delete_plugin_secret(plugin, key).await,
        };
        if let Err(error) = result {
            tracing::warn!(plugin, key, %error, "failed to roll back plugin secret");
        }
    }
}

/// `plugin uninstall <plugin> [--keep-data] [--prune] [-y] [--scope]`.
pub fn run_uninstall(
    arg: &str,
    scope: Option<&str>,
    keep_data: bool,
    prune: bool,
    yes: bool,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    current_thread_runtime().block_on(run_uninstall_with_bus(
        arg,
        scope,
        keep_data,
        prune,
        yes,
        plugins_dir,
        home,
        cwd,
        None,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginUninstallOutcome {
    pub message: String,
    pub plugin_id: String,
    pub last_scope_removed: bool,
}

async fn begin_uninstall_transaction(
    plugins_dir: &Path,
    arg: &str,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<plugin::installed::InstalledRegistryTransaction, String> {
    match plugin::installed::InstalledRegistryTransaction::begin(plugins_dir) {
        Ok(tx) => Ok(tx),
        Err(error) => {
            crate::plugin_telemetry::emit_plugin_state_file_error(
                analytics_bus,
                "uninstall",
                "transaction_begin",
                registry_error_kind(&error),
            )
            .await;
            Err(fail("uninstall", arg, &error))
        }
    }
}

pub async fn run_uninstall_with_bus(
    arg: &str,
    scope: Option<&str>,
    keep_data: bool,
    _prune: bool,
    _yes: bool,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    run_uninstall_with_bus_outcome(
        arg,
        scope,
        keep_data,
        _prune,
        _yes,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
    )
    .await
    .map(|outcome| outcome.message)
}

pub(crate) async fn run_uninstall_with_bus_outcome(
    arg: &str,
    scope: Option<&str>,
    keep_data: bool,
    prune: bool,
    yes: bool,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<PluginUninstallOutcome, String> {
    let scope_value = parse_scope(scope)?;
    let mut installed_tx = begin_uninstall_transaction(plugins_dir, arg, analytics_bus).await?;
    let installed = installed_tx.document().clone();
    let plugin_key = resolve_uninstall_plugin_key(&installed, arg);
    let marketplace_entry = marketplace_entry_for_plugin_id(plugins_dir, &plugin_key);
    let sensitive_keys = sensitive_config_keys_for_plugin_records(
        plugins_dir,
        &installed,
        &plugin_key,
        marketplace_entry.as_ref(),
    );
    let mut outcome = run_uninstall_with_sensitive_config_keys(
        arg,
        scope,
        keep_data,
        prune,
        yes,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
        &mut installed_tx,
        &sensitive_keys,
    )
    .await?;
    drop(installed_tx);
    append_uninstall_suffix(
        &mut outcome,
        prune,
        yes,
        scope_value,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
    )
    .await?;
    Ok(outcome)
}

fn resolve_uninstall_plugin_key(installed: &Value, arg: &str) -> String {
    let (name, marketplace) = split_id(arg);
    marketplace.map_or_else(
        || {
            installed
                .get("plugins")
                .and_then(Value::as_object)
                .and_then(|plugins| plugins.keys().find(|id| name_of(id) == name).cloned())
                .unwrap_or_else(|| name.to_string())
        },
        |marketplace| format!("{name}@{marketplace}"),
    )
}

async fn append_uninstall_suffix(
    outcome: &mut PluginUninstallOutcome,
    prune: bool,
    yes: bool,
    scope: WritableScope,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<(), String> {
    if prune {
        let prune_message = crate::plugin_prune::run_prune_with_bus(
            false,
            yes,
            scope_label(scope),
            plugins_dir,
            home,
            cwd,
            analytics_bus,
        )
        .await?;
        outcome.message.push('\n');
        outcome.message.push_str(&prune_message);
    } else {
        let project = project_path(scope, cwd);
        outcome
            .message
            .push_str(&uninstall_orphan_suffix(plugins_dir, scope, &project));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_uninstall_with_sensitive_config_keys(
    arg: &str,
    scope: Option<&str>,
    keep_data: bool,
    _prune: bool,
    _yes: bool,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    installed_tx: &mut plugin::installed::InstalledRegistryTransaction,
    additional_sensitive_keys: &BTreeSet<String>,
) -> Result<PluginUninstallOutcome, String> {
    let scope = parse_scope(scope)?;
    let telemetry_scope = crate::plugin_telemetry::telemetry_scope(Some(scope_label(scope)));
    let proj = project_path(scope, cwd);
    let mut settings_journal = SettingsRollbackJournal::default();
    let (name, market) = split_id(arg);
    let result = async {
        // Resolve the full id: explicit `@market`, else the first installed key
        // whose name-part matches.
        let full_id = match market {
            Some(market) => format!("{name}@{market}"),
            None => installed_tx
                .document()
                .get("plugins")
                .and_then(Value::as_object)
                .and_then(|p| p.keys().find(|k| name_of(k) == name).cloned())
                .unwrap_or_else(|| name.to_string()),
        };

        let records = installed_tx
            .document()
            .get("plugins")
            .and_then(|p| p.get(&full_id))
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty());
        let Some(records) = records else {
            return Err(fail(
                "uninstall",
                arg,
                &format!("Plugin \"{full_id}\" not found in installed plugins"),
            ));
        };
        let mut sensitive_keys = additional_sensitive_keys.clone();
        for record in records {
            sensitive_keys.extend(stored_sensitive_config_keys(record));
        }

        // Records at the requested (scope, projectPath) slot. If none, the plugin is
        // installed at some OTHER scope — name it, matching the binary.
        let matching: Vec<Value> = records
            .iter()
            .filter(|r| record_matches(r, scope, &proj))
            .cloned()
            .collect();
        if matching.is_empty() {
            let mut other: Vec<&str> = Vec::new();
            for s in records
                .iter()
                .filter_map(|r| r.get("scope").and_then(Value::as_str))
            {
                if !other.contains(&s) {
                    other.push(s);
                }
            }
            let installed_in = other.join(", ");
            let first = other.first().copied().unwrap_or("user");
            return Err(fail(
                "uninstall",
                arg,
                &format!(
                    "Plugin \"{full_id}\" is installed in {installed_in} scope, not {}. \
                     Use --scope {first} to uninstall.",
                    scope_label(scope)
                ),
            ));
        }

        // Drop only the matched records; remove the key once the array is empty.
        if let Some(plugins) = installed_tx
            .document_mut()
            .get_mut("plugins")
            .and_then(Value::as_object_mut)
        {
            if let Some(arr) = plugins.get_mut(&full_id).and_then(Value::as_array_mut) {
                arr.retain(|r| !record_matches(r, scope, &proj));
                if arr.is_empty() {
                    plugins.remove(&full_id);
                }
            }
        }
        remember_sensitive_config_keys(installed_tx.document_mut(), &full_id, &sensitive_keys);

        installed_tx
            .persist()
            .map_err(|error| fail("uninstall", arg, &error))?;
        edit_enabled_strict_with_journal(scope, home, cwd, &full_id, None, &mut settings_journal)
            .map_err(|error| fail("uninstall", arg, &error))?;
        clear_plugin_config_strict_with_journal(scope, home, cwd, &full_id, &mut settings_journal)
            .map_err(|error| fail("uninstall", arg, &error))?;

        mark_orphaned_cache_paths(&unreferenced_removed_record_paths(
            plugins_dir,
            installed_tx.document(),
            &matching,
        ));
        let last_scope_removed = !plugin_has_records(installed_tx.document(), &full_id);
        if !keep_data && last_scope_removed {
            if let Some(path) = confined_plugin_data_path(plugins_dir, &full_id) {
                let _ = std::fs::remove_dir_all(path);
            }
        }

        crate::plugin_telemetry::emit_plugin_cli_result(
            analytics_bus,
            telemetry::tengu::plugin::UNINSTALLED_CLI,
            crate::plugin_telemetry::PluginCommandOutcome::Success,
            telemetry_scope,
            1,
        )
        .await;

        Ok(PluginUninstallOutcome {
            message: format!(
                "✔ Successfully uninstalled plugin: {} (scope: {})",
                name_of(&full_id),
                scope_label(scope)
            ),
            plugin_id: full_id,
            last_scope_removed,
        })
    }
    .await;
    if result.is_err() {
        settings_journal.rollback();
        rollback_installed_registry(&installed_tx);
    }
    result
}

/// Async production uninstall wrapper. Credential cleanup is independent from
/// `--keep-data`; that flag only preserves `${CLAUDE_PLUGIN_DATA}`.
pub async fn run_uninstall_with_credential_factory<F, Fut>(
    arg: &str,
    scope: Option<&str>,
    keep_data: bool,
    prune: bool,
    yes: bool,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    credential_factory: F,
) -> Result<String, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Arc<secret::CredentialManager>, String>>,
{
    run_uninstall_with_credential_factory_outcome(
        arg,
        scope,
        keep_data,
        prune,
        yes,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
        credential_factory,
    )
    .await
    .map(|outcome| outcome.message)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_uninstall_with_credential_factory_outcome<F, Fut>(
    arg: &str,
    scope: Option<&str>,
    keep_data: bool,
    prune: bool,
    yes: bool,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    credential_factory: F,
) -> Result<PluginUninstallOutcome, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Arc<secret::CredentialManager>, String>>,
{
    let scope_value = parse_scope(scope)?;
    let project = project_path(scope_value, cwd);
    // Hold the installed-registry lock through the last-scope check, secret
    // deletion, and registry mutation. Otherwise a concurrent install could
    // add a new scope after this snapshot and have its shared credentials
    // deleted by the in-flight uninstall.
    let mut installed_tx = begin_uninstall_transaction(plugins_dir, arg, analytics_bus).await?;
    let installed = installed_tx.document().clone();
    let plugin_key = resolve_uninstall_plugin_key(&installed, arg);
    let marketplace_entry = marketplace_entry_for_plugin_id(plugins_dir, &plugin_key);
    let sensitive_keys = sensitive_config_keys_for_plugin_records(
        plugins_dir,
        &installed,
        &plugin_key,
        marketplace_entry.as_ref(),
    );
    let records = installed
        .get("plugins")
        .and_then(|plugins| plugins.get(&plugin_key))
        .and_then(Value::as_array);
    let removes_last_record = records.is_some_and(|records| {
        records
            .iter()
            .any(|record| record_matches(record, scope_value, &project))
            && !records
                .iter()
                .any(|record| !record_matches(record, scope_value, &project))
    });
    // Credential keys share the plugin identity across user/project/local
    // records. Keep them while any scope still installs that plugin; only the
    // final uninstall clears the shared secret namespace.
    let credential_stack = if !removes_last_record || sensitive_keys.is_empty() {
        None
    } else {
        Some(credential_factory().await?)
    };
    let mut previous_secrets = Vec::with_capacity(sensitive_keys.len());
    if let Some(stack) = &credential_stack {
        for key in &sensitive_keys {
            let previous = stack
                .get_plugin_secret(&plugin_key, key)
                .await
                .map_err(|error| format!("Failed to read plugin secret {key}: {error}"))?
                .map(|secret| secret.expose_secret().clone());
            previous_secrets.push((key.clone(), previous));
        }
        for key in &sensitive_keys {
            if let Err(error) = stack.delete_plugin_secret(&plugin_key, key).await {
                rollback_plugin_secrets(stack, &plugin_key, &previous_secrets).await;
                return Err(format!("Failed to delete plugin secret {key}: {error}"));
            }
        }
    }
    let mut outcome = match run_uninstall_with_sensitive_config_keys(
        arg,
        scope,
        keep_data,
        prune,
        yes,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
        &mut installed_tx,
        &sensitive_keys,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(stack) = &credential_stack {
                rollback_plugin_secrets(stack, &plugin_key, &previous_secrets).await;
            }
            return Err(error);
        }
    };
    drop(installed_tx);
    append_uninstall_suffix(
        &mut outcome,
        prune,
        yes,
        scope_value,
        plugins_dir,
        home,
        cwd,
        analytics_bus,
    )
    .await?;
    Ok(outcome)
}

/// The trailing text a non-`--prune` uninstall appends to the message.
fn uninstall_orphan_suffix(
    plugins_dir: &Path,
    scope: WritableScope,
    project: &Option<String>,
) -> String {
    let db = load_installed(plugins_dir);
    let orphans = crate::plugin_prune::scan_orphans(&db, scope, project);
    crate::plugin_prune::orphan_notice(&orphans, scope_label(scope))
}

/// Validate a `plugin update` `--scope`. Unlike the install family, update's
/// valid set INCLUDES `managed` (a read-only enterprise scope that update may
/// name but never has an editable record at), and its invalid-scope wording is
/// distinct: `Invalid scope "<s>". Valid scopes: user, project, local, managed`
/// (no ✘ prefix; emitted before the `Checking for updates…` line).
fn parse_update_scope(scope: &str) -> Result<&str, String> {
    match scope {
        "user" | "project" | "local" | "managed" => Ok(scope),
        _ => Err(format!(
            "Invalid scope \"{scope}\". Valid scopes: user, project, local, managed"
        )),
    }
}

/// The project-root path recorded for a scope (`project`/`local` → cwd; `user`/
/// `managed` → none), used both to disambiguate multi-install records and to
/// render the `not installed at scope <scope> (<path>)` suffix.
fn scope_project_path(scope: &str, cwd: &Path) -> Option<PathBuf> {
    if scope == "project" || scope == "local" {
        Some(cwd.to_path_buf())
    } else {
        None
    }
}
/// Resolve a full `name@market` id to its marketplace + on-disk plugin source
/// dir (mirrors `iP`): a bare id (no `@`) or a market/plugin the registry can't
/// resolve ⇒ `None` (⇒ the caller's `Plugin "<name>" not found`).
fn resolve_source(plugins_dir: &Path, id: &str) -> Result<Option<(String, PathBuf)>, String> {
    current_thread_runtime().block_on(resolve_source_with_bus(plugins_dir, id, None))
}

async fn resolve_source_with_bus(
    plugins_dir: &Path,
    id: &str,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<Option<(String, PathBuf)>, String> {
    let (name, market) = split_id(id);
    let Some(market) = market else {
        return Ok(None);
    };
    let registry = load_registry(plugins_dir);
    let Some(entry) = registry.get(market) else {
        return Ok(None);
    };
    let identity = plugin_policy::MarketplaceSourceIdentity::from_value(entry);
    plugin_policy::ensure_marketplace_source_allowed(Some(market), identity.as_ref())?;
    let Some(root) = install_location(entry) else {
        return Ok(None);
    };
    Ok(marketplace_entry_source_path_with_bus(
        &root,
        market,
        name,
        plugins_dir,
        analytics_bus,
        false,
    )
    .await?
    .map(|source| (market.to_string(), source)))
}

/// `plugin update <plugin> [--scope]` — re-materialize an installed plugin from
/// its marketplace and bump the on-disk record. 1:1 with claude-code 2.1.201
/// (`ukc`/`Rvt`/`Prf`, directory sources; probed against the real binary).
///
/// Output is two lines: a `Checking for updates for plugin "<arg>" at <scope>
/// scope…` header (always, once the scope parses) followed by the result:
///
/// * up-to-date → `✔ <name> is already at the latest version (<version>).`
/// * a newer marketplace version → re-copy the plugin tree into the versioned
///   cache `cache/<market>/<name>/<version>/`, point the record's `installPath`
///   at it, set `version` + bump `lastUpdated` (keeping `installedAt`), orphan
///   the previous cache dir (a `.orphaned_at` marker, when unreferenced) →
///   `✔ Plugin "<name>" updated from <old> to <new> for scope <scope>. Restart
///   to apply changes.` (`enabledPlugins` is NOT touched.)
///
/// Errors (each after the header, `✘ Failed to update plugin "<arg>": …`):
/// a plugin not resolvable in any marketplace → `Plugin "<name>" not found`;
/// resolvable but never installed → `Plugin "<name>" is not installed`; installed
/// but not at the requested scope → `Plugin "<name>" is not installed at scope
/// <scope>` (with ` (<cwd>)` for project/local). An unknown `--scope` errors
/// (no header) with the update-family `Invalid scope …` wording.
pub fn run_update(
    arg: &str,
    scope: &str,
    plugins_dir: &Path,
    _home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    current_thread_runtime().block_on(run_update_async(arg, scope, plugins_dir, _home, cwd, None))
}

pub fn run_update_with_bus(
    arg: &str,
    scope: &str,
    plugins_dir: &Path,
    _home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    current_thread_runtime().block_on(run_update_async(
        arg,
        scope,
        plugins_dir,
        _home,
        cwd,
        analytics_bus,
    ))
}

pub async fn run_update_async(
    arg: &str,
    scope: &str,
    plugins_dir: &Path,
    _home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    let scope = parse_update_scope(scope)?;
    let telemetry_scope = crate::plugin_telemetry::telemetry_scope(Some(scope));
    let header = format!("Checking for updates for plugin \"{arg}\" at {scope} scope\u{2026}\n");
    match update_inner(arg, scope, plugins_dir, cwd, analytics_bus).await {
        Ok(msg) => {
            if !msg.contains("already at the latest version") {
                crate::plugin_telemetry::emit_plugin_cli_result(
                    analytics_bus,
                    telemetry::tengu::plugin::UPDATED_CLI,
                    crate::plugin_telemetry::PluginCommandOutcome::Success,
                    telemetry_scope,
                    1,
                )
                .await;
            }
            Ok(format!("{header}\u{2714} {msg}"))
        }
        Err(reason) => Err(format!("{header}{}", fail("update", arg, &reason))),
    }
}

/// The core update resolution + materialization (sans the header/`✔`/`✘`
/// framing). `Ok` carries the bare success sentence; `Err` the bare reason.
async fn update_inner(
    arg: &str,
    scope: &str,
    plugins_dir: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    // Display name for every message = the ORIGINAL arg's name-part (matches the
    // binary's `n` from `zo(e)`, which case-resolution does not rewrite).
    let (name, market) = split_id(arg);

    // Resolve the id against the installed keys (exact, then case-insensitive —
    // `Loe`); a bare name that matches no key stays bare (⇒ "not found").
    let mut installed_tx = match plugin::installed::InstalledRegistryTransaction::begin(plugins_dir)
    {
        Ok(tx) => tx,
        Err(error) => {
            crate::plugin_telemetry::emit_plugin_state_file_error(
                analytics_bus,
                "update",
                "transaction_begin",
                registry_error_kind(&error),
            )
            .await;
            return Err(error);
        }
    };
    let mut committed = false;
    let result = async {
        let base = match market {
            Some(m) => format!("{name}@{m}"),
            None => arg.to_string(),
        };
        let id = installed_tx
            .document()
            .get("plugins")
            .and_then(Value::as_object)
            .and_then(|p| {
                p.keys()
                    .find(|k| k.as_str() == base)
                    .cloned()
                    .or_else(|| p.keys().find(|k| k.eq_ignore_ascii_case(&base)).cloned())
            })
            .unwrap_or(base);

        if let Some(marketplace) = marketplace_of(&id) {
            let registry = load_registry(plugins_dir);
            let source = registry
                .get(marketplace)
                .and_then(plugin_policy::MarketplaceSourceIdentity::from_value);
            plugin_policy::ensure_marketplace_source_allowed(Some(marketplace), source.as_ref())?;
        }

        let Some((market_name, plugin_src)) =
            resolve_source_with_bus(plugins_dir, &id, analytics_bus).await?
        else {
            return Err(format!("Plugin \"{name}\" not found"));
        };

        let has_records = installed_tx
            .document()
            .get("plugins")
            .and_then(|p| p.get(&id))
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        if !has_records {
            return Err(format!("Plugin \"{name}\" is not installed"));
        }

        let project_path = scope_project_path(scope, cwd);
        let want_pp = project_path.as_ref().map(|p| p.display().to_string());
        let (idx, old_version, old_path) = {
            let records = installed_tx
                .document()
                .get("plugins")
                .and_then(|p| p.get(&id))
                .and_then(Value::as_array)
                .unwrap();
            let scoped: Vec<usize> = records
                .iter()
                .enumerate()
                .filter(|(_, r)| r.get("scope").and_then(Value::as_str) == Some(scope))
                .map(|(i, _)| i)
                .collect();
            if scoped.is_empty() {
                let disp = match &want_pp {
                    Some(p) => format!("{scope} ({p})"),
                    None => scope.to_string(),
                };
                return Err(format!(
                    "Plugin \"{name}\" is not installed at scope {disp}"
                ));
            }
            let idx = scoped
                .iter()
                .copied()
                .find(|&i| {
                    let rp = records[i].get("projectPath").and_then(Value::as_str);
                    match &want_pp {
                        Some(w) => rp == Some(w.as_str()),
                        None => rp.is_none(),
                    }
                })
                .unwrap_or(scoped[0]);
            let ov = records[idx]
                .get("version")
                .and_then(Value::as_str)
                .map(String::from);
            let op = records[idx]
                .get("installPath")
                .and_then(Value::as_str)
                .map(String::from);
            (idx, ov, op)
        };

        let manifest_version = plugin::discovery::plugin_manifest_version(&plugin_src)
            .await
            .map_err(|error| format!("Plugin manifest is invalid: {error}"))?;
        let marketplace_entry = load_registry(plugins_dir)
            .get(&market_name)
            .and_then(install_location)
            .and_then(|market_root| marketplace_entry(&market_root, split_id(&id).0));
        let marketplace_version = marketplace_entry
            .as_ref()
            .and_then(|entry| entry.get("version").and_then(Value::as_str).map(str::to_owned));
        let mut updated_sensitive_keys = sensitive_config_keys_from_schema(&read_user_config_schema(
            &plugin_src,
            marketplace_entry.as_ref(),
        ));
        // Read the previous installed cache before replacing its installPath.
        // Older registry records may predate `sensitiveConfigKeys`, so the
        // old manifest is the last durable source for renamed/deleted fields.
        updated_sensitive_keys.extend(sensitive_config_keys_for_plugin_records(
            plugins_dir,
            installed_tx.document(),
            &id,
            marketplace_entry.as_ref(),
        ));
        let sensitive_keys_changed = remember_sensitive_config_keys(
            installed_tx.document_mut(),
            &id,
            &updated_sensitive_keys,
        );
        let new_version = manifest_version
            .or(marketplace_version)
            .unwrap_or_else(|| "unknown".to_string());
        let id_name = split_id(&id).0;
        let dest = plugins_dir
            .join("cache")
            .join(sanitize(&market_name, false))
            .join(sanitize(id_name, false))
            .join(sanitize(&new_version, true));
        let dest_str = dest.display().to_string();

        if new_version != "unknown"
            && (old_version.as_deref() == Some(new_version.as_str())
                || old_path.as_deref() == Some(dest_str.as_str()))
        {
            if sensitive_keys_changed {
                installed_tx.persist()?;
                committed = true;
            }
            return Ok(format!(
                "{name} is already at the latest version ({new_version})."
            ));
        }

        let published = stage_and_publish_cache_dir(plugins_dir, &plugin_src, &dest)?;
        let now = iso_now();
        if let Some(rec) = installed_tx
            .document_mut()
            .get_mut("plugins")
            .and_then(Value::as_object_mut)
            .and_then(|p| p.get_mut(&id))
            .and_then(Value::as_array_mut)
            .and_then(|a| a.get_mut(idx))
            .and_then(Value::as_object_mut)
        {
            rec.insert("installPath".to_string(), Value::String(dest_str.clone()));
            rec.insert("version".to_string(), Value::String(new_version.clone()));
            rec.insert("lastUpdated".to_string(), Value::String(now));
        }
        if let Err(error) = installed_tx.persist() {
            published.rollback();
            return Err(error);
        }
        committed = true;
        published.finalize();

        if let Some(old) = old_path.as_deref() {
            if old != dest_str {
                if let Some(path) = confined_cache_record_path(plugins_dir, Path::new(old)) {
                    if !document_references_install_path(installed_tx.document(), plugins_dir, &path)
                    {
                        mark_orphaned_cache_paths(&[path]);
                    }
                }
            }
        }

        let scope_disp = match &want_pp {
            Some(p) => format!("{scope} ({p})"),
            None => scope.to_string(),
        };
        let old_disp = old_version.as_deref().unwrap_or("unknown");
        Ok(format!(
            "Plugin \"{name}\" updated from {old_disp} to {new_version} for scope {scope_disp}. Restart to apply changes."
        ))
    }
    .await;
    if result.is_err() && !committed {
        rollback_installed_registry(&installed_tx);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_telemetry::telemetry_test_support::{capture_events, event};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use telemetry::{AnalyticsValue, InMemorySink};

    struct Env {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
        plugins: PathBuf,
        market: PathBuf,
    }

    /// A home/cwd/plugins env with a registered directory marketplace `mymkt`
    /// carrying a plugin `hello` v1.2.3 (a `commands/hi.md` component).
    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("proj");
        let plugins = home.join("plugins");
        let market = tmp.path().join("mymkt");
        for d in [&home, &cwd, &plugins] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mdir = market.join(branding::PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&mdir).unwrap();
        std::fs::write(
            mdir.join("marketplace.json"),
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[{"name":"hello","source":"./plugins/hello"}]}"#,
        )
        .unwrap();
        let pdir = market.join("plugins").join("hello");
        std::fs::create_dir_all(pdir.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::create_dir_all(pdir.join("commands")).unwrap();
        std::fs::write(
            pdir.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),
            r#"{"name":"hello","version":"1.2.3"}"#,
        )
        .unwrap();
        std::fs::write(pdir.join("commands").join("hi.md"), "# hi").unwrap();
        // Register the marketplace (directory source).
        let abs = std::fs::canonicalize(&market)
            .unwrap()
            .display()
            .to_string();
        std::fs::write(
            plugins.join("known_marketplaces.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "mymkt": {"source": {"source": "directory", "path": abs}, "installLocation": abs}
            }))
            .unwrap(),
        )
        .unwrap();
        Env {
            _tmp: tmp,
            home,
            cwd,
            plugins,
            market,
        }
    }

    fn claude_sensitive_env() -> Env {
        let e = env();
        std::fs::remove_dir_all(e.market.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        let plugin_root = e.market.join("plugins/hello");
        std::fs::remove_dir_all(plugin_root.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        write_claude_sensitive_schema(&e, "1.2.3", &["API_TOKEN"]);
        e
    }

    fn write_claude_sensitive_schema(e: &Env, version: &str, keys: &[&str]) {
        let user_config: serde_json::Map<String, Value> = keys
            .iter()
            .map(|key| {
                (
                    (*key).to_string(),
                    serde_json::json!({
                        "type": "string",
                        "title": key,
                        "description": "credential",
                        "sensitive": true
                    }),
                )
            })
            .collect();
        let plugin_manifest_dir = e
            .market
            .join("plugins/hello")
            .join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&plugin_manifest_dir).unwrap();
        std::fs::write(
            plugin_manifest_dir.join("plugin.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "hello",
                "version": version,
                "userConfig": user_config.clone()
            }))
            .unwrap(),
        )
        .unwrap();

        let marketplace_manifest_dir = e.market.join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&marketplace_manifest_dir).unwrap();
        std::fs::write(
            marketplace_manifest_dir.join("marketplace.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "mymkt",
                "owner": {"name": "me"},
                "plugins": [{
                    "name": "hello",
                    "version": version,
                    "source": "./plugins/hello",
                    "userConfig": user_config
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    struct CredentialTestClock;

    impl lingxi_core::host::Clock for CredentialTestClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
    }

    struct UnusedCredentialHttp;

    impl lingxi_core::host::HttpTransport for UnusedCredentialHttp {
        fn request<'life0, 'async_trait>(
            &'life0 self,
            _request: lingxi_core::types::HttpRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            lingxi_core::types::HttpResponse,
                            lingxi_core::host::HttpError,
                        >,
                    > + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async {
                Err(lingxi_core::host::HttpError::InvalidRequest(
                    "credential tests do not use HTTP".to_string(),
                ))
            })
        }

        fn stream_sse<'life0, 'async_trait>(
            &'life0 self,
            _request: lingxi_core::types::HttpRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            lingxi_core::host::http::SseStream,
                            lingxi_core::host::HttpError,
                        >,
                    > + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async {
                Err(lingxi_core::host::HttpError::InvalidRequest(
                    "credential tests do not use HTTP".to_string(),
                ))
            })
        }
    }

    fn in_memory_credentials() -> Arc<secret::CredentialManager> {
        Arc::new(secret::CredentialManager::new(
            Arc::new(lingxi_core::host::InMemorySecureStorage::new()),
            Arc::new(CredentialTestClock),
            Arc::new(UnusedCredentialHttp),
        ))
    }

    fn credential_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn uninstall_clears_sensitive_keys_after_schema_change_and_marketplace_removal() {
        let e = claude_sensitive_env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let credentials = in_memory_credentials();
        let runtime = credential_test_runtime();

        // Simulate a record written before sensitive-key metadata existed.
        let mut installed = installed_db(&e);
        installed["plugins"]["hello@mymkt"][0]
            .as_object_mut()
            .unwrap()
            .remove(SENSITIVE_CONFIG_KEYS_FIELD);
        write_current_installed(&e, &installed);
        runtime
            .block_on(credentials.set_plugin_secret("hello@mymkt", "API_TOKEN", "old-secret"))
            .unwrap();

        // Reconfiguring an already-installed legacy record learns the current
        // schema's key name transactionally, without putting the value in the
        // installed registry or settings file.
        let old_config = vec!["API_TOKEN=rotated-secret".to_string()];
        let install_credentials = credentials.clone();
        runtime
            .block_on(run_install_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                true,
                &old_config,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move { Ok(install_credentials) },
            ))
            .unwrap();
        let installed = installed_db(&e);
        assert_eq!(
            installed["plugins"]["hello@mymkt"][0][SENSITIVE_CONFIG_KEYS_FIELD],
            serde_json::json!(["API_TOKEN"])
        );
        assert!(
            !std::fs::read_to_string(installed_path(&e.plugins))
                .unwrap()
                .contains("rotated-secret")
        );

        // The catalog changes the sensitive field name. Update retains the
        // old key name alongside the new schema so both can be cleaned later.
        write_claude_sensitive_schema(&e, "2.0.0", &["NEW_TOKEN"]);
        run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();
        let installed = installed_db(&e);
        assert_eq!(
            installed["plugins"]["hello@mymkt"][0][SENSITIVE_CONFIG_KEYS_FIELD],
            serde_json::json!(["API_TOKEN", "NEW_TOKEN"])
        );

        let new_config = vec!["NEW_TOKEN=new-secret".to_string()];
        let install_credentials = credentials.clone();
        runtime
            .block_on(run_install_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                true,
                &new_config,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move { Ok(install_credentials) },
            ))
            .unwrap();

        // Once the marketplace is gone, uninstall must rely on the durable
        // names and the credential namespace must still be fully cleared.
        std::fs::remove_dir_all(&e.market).unwrap();
        let uninstall_credentials = credentials.clone();
        runtime
            .block_on(run_uninstall_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                false,
                false,
                true,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move { Ok(uninstall_credentials) },
            ))
            .unwrap();
        assert!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "API_TOKEN"))
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "NEW_TOKEN"))
                .unwrap()
                .is_none()
        );
        assert!(installed_db(&e)["plugins"].get("hello@mymkt").is_none());
    }

    #[test]
    fn update_retains_sensitive_keys_from_the_old_cache_schema() {
        let e = claude_sensitive_env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let mut installed = installed_db(&e);
        installed["plugins"]["hello@mymkt"][0]
            .as_object_mut()
            .unwrap()
            .remove(SENSITIVE_CONFIG_KEYS_FIELD);
        write_current_installed(&e, &installed);

        let credentials = in_memory_credentials();
        let runtime = credential_test_runtime();
        runtime
            .block_on(credentials.set_plugin_secret("hello@mymkt", "API_TOKEN", "old-secret"))
            .unwrap();

        // This legacy record has no key-name metadata. Once update changes the
        // selected source schema, only its old cache manifest can identify the
        // stale API_TOKEN secret.
        write_claude_sensitive_schema(&e, "2.0.0", &["NEW_TOKEN"]);
        run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            installed_db(&e)["plugins"]["hello@mymkt"][0][SENSITIVE_CONFIG_KEYS_FIELD],
            serde_json::json!(["API_TOKEN", "NEW_TOKEN"])
        );
        runtime
            .block_on(credentials.set_plugin_secret("hello@mymkt", "NEW_TOKEN", "new-secret"))
            .unwrap();

        std::fs::remove_dir_all(&e.market).unwrap();
        let uninstall_credentials = credentials.clone();
        runtime
            .block_on(run_uninstall_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                false,
                false,
                true,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move { Ok(uninstall_credentials) },
            ))
            .unwrap();
        assert!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "API_TOKEN"))
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "NEW_TOKEN"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn uninstall_keeps_shared_sensitive_secret_until_last_scope_is_removed() {
        let e = claude_sensitive_env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let credentials = in_memory_credentials();
        let runtime = credential_test_runtime();
        runtime
            .block_on(credentials.set_plugin_secret("hello@mymkt", "API_TOKEN", "shared"))
            .unwrap();

        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_by_factory = called.clone();
        let credentials_for_factory = credentials.clone();
        runtime
            .block_on(run_uninstall_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                false,
                false,
                true,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                move || async move {
                    called_by_factory.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(credentials_for_factory)
                },
            ))
            .unwrap();
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "API_TOKEN"))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            installed_db(&e)["plugins"]["hello@mymkt"][0][SENSITIVE_CONFIG_KEYS_FIELD],
            serde_json::json!(["API_TOKEN"])
        );

        let uninstall_credentials = credentials.clone();
        runtime
            .block_on(run_uninstall_with_credential_factory(
                "hello@mymkt",
                Some("project"),
                false,
                false,
                true,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move { Ok(uninstall_credentials) },
            ))
            .unwrap();
        assert!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "API_TOKEN"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn failed_sensitive_uninstall_restores_registry_and_credential() {
        let e = claude_sensitive_env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let credentials = in_memory_credentials();
        let runtime = credential_test_runtime();
        runtime
            .block_on(credentials.set_plugin_secret("hello@mymkt", "API_TOKEN", "recover"))
            .unwrap();
        let before_registry = std::fs::read(installed_path(&e.plugins)).unwrap();

        std::fs::remove_file(e.home.join("settings.json")).unwrap();
        std::fs::create_dir_all(e.home.join("settings.json")).unwrap();
        let uninstall_credentials = credentials.clone();
        let error = runtime
            .block_on(run_uninstall_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                false,
                false,
                true,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move { Ok(uninstall_credentials) },
            ))
            .unwrap_err();
        assert!(error.contains("Failed to uninstall plugin"), "{error}");
        assert_eq!(
            std::fs::read(installed_path(&e.plugins)).unwrap(),
            before_registry,
            "failed settings update must restore exact installed-registry bytes"
        );
        assert_eq!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "API_TOKEN"))
                .unwrap()
                .unwrap()
                .expose_secret(),
            "recover"
        );
    }

    #[test]
    fn failed_sensitive_install_restores_previous_credential() {
        let e = claude_sensitive_env();
        let credentials = in_memory_credentials();
        let runtime = credential_test_runtime();
        runtime
            .block_on(credentials.set_plugin_secret("hello@mymkt", "API_TOKEN", "previous"))
            .unwrap();
        let plugin_manifest = e
            .market
            .join("plugins/hello")
            .join(branding::CLAUDE_PLUGIN_MANIFEST_DIR)
            .join("plugin.json");

        let config = vec!["API_TOKEN=replacement".to_string()];
        let install_credentials = credentials.clone();
        let manifest_for_factory = plugin_manifest.clone();
        let error = runtime
            .block_on(run_install_with_credential_factory(
                "hello@mymkt",
                Some("user"),
                true,
                &config,
                &e.plugins,
                &e.home,
                &e.cwd,
                None,
                || async move {
                    // The wrapper has already parsed the valid schema before
                    // initializing credentials. Make its second manifest read
                    // fail so secret storage is exercised before install rollback.
                    std::fs::write(manifest_for_factory, b"{invalid json").unwrap();
                    Ok(install_credentials)
                },
            ))
            .unwrap_err();
        assert!(error.contains("invalid plugin manifest"), "{error}");
        assert!(!installed_path(&e.plugins).exists());
        assert!(!e.home.join("settings.json").exists());
        assert_eq!(
            runtime
                .block_on(credentials.get_plugin_secret("hello@mymkt", "API_TOKEN"))
                .unwrap()
                .unwrap()
                .expose_secret(),
            "previous"
        );
    }

    #[test]
    fn failed_install_waiting_for_registry_lock_preserves_newer_settings() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();

        // Hold the registry lock while a settings writer publishes a newer
        // valid document. The failing install starts during that interval and
        // must not overwrite the settings writer's committed change.
        let lock = plugin::installed::InstalledRegistryTransaction::begin(&e.plugins).unwrap();
        let plugins = e.plugins.clone();
        let home = e.home.clone();
        let cwd = e.cwd.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            started_tx.send(()).unwrap();
            runtime.block_on(run_install_async(
                "hello@mymkt",
                Some("user"),
                &["UNKNOWN=value".to_string()],
                &plugins,
                &home,
                &cwd,
                None,
                false,
            ))
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("install worker did not start");
        thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !worker.is_finished(),
            "install worker should still be waiting for the held registry lock"
        );

        let settings_path = e.home.join("settings.json");
        let mut settings = user_settings(&e);
        settings["enabledPlugins"]["hello@mymkt"] = Value::Bool(false);
        let latest_settings = serde_json::to_vec(&settings).unwrap();
        std::fs::write(&settings_path, &latest_settings).unwrap();
        drop(lock);

        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("isn't declared"), "{error}");
        assert_eq!(
            std::fs::read(settings_path).unwrap(),
            latest_settings,
            "failed install rollback must preserve settings written while it waited for the registry lock"
        );
    }

    fn user_settings(e: &Env) -> Value {
        serde_json::from_str(&std::fs::read_to_string(e.home.join("settings.json")).unwrap())
            .unwrap()
    }

    fn installed_db(e: &Env) -> Value {
        serde_json::from_str(&std::fs::read_to_string(installed_path(&e.plugins)).unwrap()).unwrap()
    }

    fn write_current_installed(e: &Env, doc: &Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(doc).unwrap();
        std::fs::write(installed_path(&e.plugins), &bytes).unwrap();
        bytes
    }

    fn string_field<'a>(metadata: &'a telemetry::LogEventMetadata, key: &str) -> &'a str {
        match metadata.get(key) {
            Some(AnalyticsValue::String(value)) => value.as_str(),
            other => panic!("missing string field {key}: {other:?}"),
        }
    }

    fn bool_field(metadata: &telemetry::LogEventMetadata, key: &str) -> bool {
        match metadata.get(key) {
            Some(AnalyticsValue::Bool(value)) => *value,
            other => panic!("missing bool field {key}: {other:?}"),
        }
    }

    fn int_field(metadata: &telemetry::LogEventMetadata, key: &str) -> i64 {
        match metadata.get(key) {
            Some(AnalyticsValue::Int(value)) => *value,
            other => panic!("missing int field {key}: {other:?}"),
        }
    }

    fn runtime_bus() -> (
        tokio::runtime::Runtime,
        Arc<AnalyticsBus>,
        Arc<InMemorySink>,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        runtime.block_on(bus.attach_sink(sink.clone() as Arc<dyn telemetry::AnalyticsSink>));
        (runtime, bus, sink)
    }

    /// Tiny recursive file walk (test-only) yielding every file path under
    /// `root` as a String.
    fn walkdir(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.to_string_lossy().into_owned());
                }
            }
        }
        out
    }

    #[test]
    fn install_materializes_records_and_enables() {
        let e = env();
        let msg = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Installing plugin \"hello@mymkt\"...✔ Successfully installed plugin: hello@mymkt (scope: user)"
        );
        // Cache materialized with the component.
        let cached = e.plugins.join("cache/mymkt/hello/1.2.3");
        assert!(cached.join(".lingxi-plugin/plugin.json").exists());
        assert!(cached.join("commands/hi.md").exists());
        // v2 record.
        let db = installed_db(&e);
        let rec = &db["plugins"]["hello@mymkt"][0];
        assert_eq!(rec["scope"], "user");
        assert_eq!(rec["version"], "1.2.3");
        assert_eq!(rec["installPath"], cached.display().to_string());
        assert!(rec["installedAt"].is_string());
        // Enabled.
        assert_eq!(
            user_settings(&e)["enabledPlugins"]["hello@mymkt"],
            Value::Bool(true)
        );
    }

    #[test]
    fn install_and_runtime_discovery_preserve_current_claude_plugin_tree() {
        let e = env();
        std::fs::remove_dir_all(e.market.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        let plugin_manifest_dir = e.market.join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&plugin_manifest_dir).unwrap();
        std::fs::create_dir_all(e.market.join("skills/webmcp")).unwrap();
        std::fs::write(
            plugin_manifest_dir.join("marketplace.json"),
            r#"{"name":"ora","owner":{"name":"ora"},"plugins":[{"name":"webmcp","version":"0.10.0","source":"./"}]}"#,
        )
        .unwrap();
        let plugin_json = br#"{"name":"webmcp","version":"0.10.0","skills":"./skills"}"#;
        std::fs::write(plugin_manifest_dir.join("plugin.json"), plugin_json).unwrap();
        let skill_body = "# Upstream WebMCP\n\nUse ${CLAUDE_PLUGIN_ROOT}/resources as shipped.\n";
        let skill_path = e.market.join("skills/webmcp/SKILL.md");
        std::fs::write(&skill_path, skill_body).unwrap();
        let install_location = std::fs::canonicalize(&e.market)
            .unwrap()
            .display()
            .to_string();
        std::fs::write(
            e.plugins.join("known_marketplaces.json"),
            serde_json::to_vec(&serde_json::json!({
                "ora": {
                    "source": {"source": "directory", "path": install_location.clone()},
                    "installLocation": install_location
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let message = run_install("webmcp@ora", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect("temporary Claude-format marketplace installs");
        assert!(message.contains("Successfully installed plugin: webmcp@ora"));

        let cache = e.plugins.join("cache/ora/webmcp/0.10.0");
        assert_eq!(
            std::fs::read(
                cache
                    .join(branding::CLAUDE_PLUGIN_MANIFEST_DIR)
                    .join("plugin.json")
            )
            .unwrap(),
            plugin_json,
            "the source manifest is copied byte-for-byte"
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("skills/webmcp/SKILL.md")).unwrap(),
            skill_body
        );
        assert!(!cache.join(branding::PLUGIN_MANIFEST_DIR).exists());
        assert_eq!(
            installed_db(&e)["plugins"]["webmcp@ora"][0]["version"],
            "0.10.0"
        );

        let enabled = std::collections::BTreeMap::from([("webmcp@ora".to_string(), true)]);
        let context = plugin::PluginDiscoveryContext::new(
            e.cwd.clone(),
            e.cwd.clone(),
            Arc::new(plugin::PluginArchiveSession::new()),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let discovered = runtime.block_on(plugin::discover_enabled_plugins(
            &e.plugins, &enabled, &context,
        ));
        assert_eq!(discovered.len(), 1);
        let (_id, manifest, install_path) = &discovered[0];
        assert_eq!(manifest.name, "webmcp");
        assert_eq!(manifest.version, "0.10.0");
        assert_eq!(install_path, &cache);
        assert!(matches!(
            &manifest.source,
            plugin::PluginSource::LocalPath { path } if path == &cache
        ));
        assert_eq!(std::fs::read_to_string(&skill_path).unwrap(), skill_body);
    }

    #[test]
    fn install_uses_marketplace_manifest_when_claude_plugin_json_is_absent() {
        let e = env();
        std::fs::remove_dir_all(e.market.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        let manifest_dir = e.market.join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::create_dir_all(e.market.join("skills/webmcp")).unwrap();
        std::fs::write(
            manifest_dir.join("marketplace.json"),
            r#"{"name":"ora","owner":{"name":"ora"},"plugins":[{"name":"webmcp","version":"0.10.0","source":"./","skills":"./skills","userConfig":{"tone":{"type":"string","title":"Tone","description":"Response style"}}}]}"#,
        )
        .unwrap();
        let skill_body = "# WebMCP\n\nMarketplace entry supplied this plugin manifest.\n";
        let skill_path = e.market.join("skills/webmcp/SKILL.md");
        std::fs::write(&skill_path, skill_body).unwrap();
        let install_location = std::fs::canonicalize(&e.market)
            .unwrap()
            .display()
            .to_string();
        std::fs::write(
            e.plugins.join("known_marketplaces.json"),
            serde_json::to_vec(&serde_json::json!({
                "ora": {
                    "source": {"source": "directory", "path": install_location.clone()},
                    "installLocation": install_location
                }
            }))
            .unwrap(),
        )
        .unwrap();

        run_install(
            "webmcp@ora",
            None,
            &["tone=concise".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .expect("the marketplace entry supplies the missing manifest");

        let cache = e.plugins.join("cache/ora/webmcp/0.10.0");
        assert!(!cache
            .join(branding::CLAUDE_PLUGIN_MANIFEST_DIR)
            .join("plugin.json")
            .exists());
        assert_eq!(
            std::fs::read_to_string(cache.join("skills/webmcp/SKILL.md")).unwrap(),
            skill_body
        );
        assert_eq!(
            user_settings(&e)["pluginConfigs"]["webmcp@ora"]["options"]["tone"],
            "concise"
        );
        assert_eq!(
            installed_db(&e)["plugins"]["webmcp@ora"][0]["version"],
            "0.10.0"
        );

        let enabled = std::collections::BTreeMap::from([("webmcp@ora".to_string(), true)]);
        let context = plugin::PluginDiscoveryContext::new(
            e.cwd.clone(),
            e.cwd.clone(),
            Arc::new(plugin::PluginArchiveSession::new()),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let discovered = runtime.block_on(plugin::discover_enabled_plugins(
            &e.plugins, &enabled, &context,
        ));
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].1.name, "webmcp");
        assert_eq!(discovered[0].1.version, "0.10.0");
        assert_eq!(discovered[0].1.components.skills.len(), 1);
        assert_eq!(std::fs::read_to_string(skill_path).unwrap(), skill_body);
    }

    #[test]
    fn update_uses_marketplace_version_when_claude_plugin_json_is_absent() {
        let e = env();
        std::fs::remove_dir_all(e.market.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::remove_dir_all(
            e.market
                .join("plugins/hello")
                .join(branding::PLUGIN_MANIFEST_DIR),
        )
        .unwrap();
        let manifest_dir = e.market.join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let marketplace_path = manifest_dir.join("marketplace.json");
        let marketplace = |version: &str| {
            serde_json::json!({
                "name": "mymkt",
                "owner": {"name": "me"},
                "plugins": [{
                    "name": "hello",
                    "version": version,
                    "source": "./plugins/hello",
                    "commands": "./commands"
                }]
            })
        };
        std::fs::write(
            &marketplace_path,
            serde_json::to_vec(&marketplace("1.0.0")).unwrap(),
        )
        .unwrap();

        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect("marketplace entry can install a plugin without plugin.json");
        assert_eq!(
            installed_db(&e)["plugins"]["hello@mymkt"][0]["version"],
            "1.0.0"
        );

        std::fs::write(
            &marketplace_path,
            serde_json::to_vec(&marketplace("2.0.0")).unwrap(),
        )
        .unwrap();
        let message = run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd)
            .expect("update falls back to the marketplace entry version");
        assert!(message.contains("updated from 1.0.0 to 2.0.0"));
        assert_eq!(
            installed_db(&e)["plugins"]["hello@mymkt"][0]["version"],
            "2.0.0"
        );
        let updated_cache = e.plugins.join("cache/mymkt/hello/2.0.0");
        assert!(updated_cache.join("commands/hi.md").exists());
        assert!(!updated_cache
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json")
            .exists());
        assert!(!updated_cache
            .join(branding::CLAUDE_PLUGIN_MANIFEST_DIR)
            .join("plugin.json")
            .exists());
    }

    #[test]
    fn install_failure_restores_current_registry_bytes() {
        let e = env();
        let weather_cache = e.plugins.join("cache/oldmkt/weather/1.0.0");
        std::fs::create_dir_all(weather_cache.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            weather_cache
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"weather","version":"1.0.0"}"#,
        )
        .unwrap();
        let current = serde_json::json!({
            "version": 2,
            "plugins": {
                "weather@oldmkt": [{
                    "scope": "user",
                    "installPath": weather_cache.display().to_string(),
                    "version": "1.0.0",
                    "installedAt": "2026-08-31T00:00:00.000Z",
                    "lastUpdated": "2026-08-31T00:00:00.000Z"
                }]
            }
        });
        let before = write_current_installed(&e, &current);
        std::fs::create_dir_all(e.home.join("settings.json")).unwrap();

        let err = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("settings write failure must roll back the current registry");

        assert!(
            err.contains("Failed to install plugin")
                || err.contains("Is a directory")
                || err.contains("Not a directory"),
            "got: {err}"
        );
        assert_eq!(
            std::fs::read(installed_path(&e.plugins)).unwrap(),
            before,
            "failed install must restore the exact current registry bytes"
        );
        assert!(installed_db(&e)["plugins"].get("weather@oldmkt").is_some());
        assert!(installed_db(&e)["plugins"].get("hello@mymkt").is_none());
        assert!(!e.plugins.join("cache/mymkt/hello/1.2.3").exists());
        assert!(weather_cache.join(branding::PLUGIN_MANIFEST_DIR).exists());
    }

    #[test]
    fn install_begin_io_failure_emits_state_file_error() {
        let e = env();
        std::fs::remove_dir_all(&e.plugins).unwrap();
        std::fs::write(&e.plugins, "not-a-directory").unwrap();

        let (result, events) =
            capture_events(|| run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd));

        assert!(result.is_err());
        let record = event(&events, telemetry::tengu::plugin::STATE_FILE_ERROR);
        assert_eq!(record["outcome"], "failure");
        assert_eq!(record["command"], "install");
        assert_eq!(record["operation"], "transaction_begin");
        assert_eq!(record["error_kind"], "io");
    }

    #[test]
    fn install_secure_emits_installed_to_analytics_bus() {
        let e = env();
        let (runtime, bus, sink) = runtime_bus();

        let result = runtime.block_on(run_install_with_credential_factory(
            "hello@mymkt",
            None,
            false,
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
            Some(bus),
            || async {
                Err("unexpected credential initialization for non-sensitive plugin".to_string())
            },
        ));

        assert!(result.is_ok(), "install should succeed: {result:?}");
        let events = runtime.block_on(sink.events());
        let installed = events
            .iter()
            .find(|event| event.name == telemetry::tengu::plugin::INSTALLED)
            .expect("installed event");
        assert_eq!(
            string_field(&installed.metadata, "_PROTO_plugin_name"),
            "hello"
        );
        assert_eq!(
            string_field(&installed.metadata, "_PROTO_marketplace_name"),
            "mymkt"
        );
        assert_eq!(
            string_field(&installed.metadata, "plugin_id_hash"),
            telemetry_plugin_id_hash("hello", Some("mymkt"))
        );
        assert_eq!(
            string_field(&installed.metadata, "plugin_scope"),
            "user-local"
        );
        assert_eq!(
            string_field(&installed.metadata, "plugin_name_redacted"),
            "third-party"
        );
        assert_eq!(
            string_field(&installed.metadata, "marketplace_name_redacted"),
            "third-party"
        );
        assert!(!bool_field(&installed.metadata, "is_official_plugin"));
        assert_eq!(
            string_field(&installed.metadata, "plugin_id"),
            "third-party"
        );
        assert_eq!(string_field(&installed.metadata, "trigger"), "cli-explicit");
        assert_eq!(
            string_field(&installed.metadata, "install_source"),
            "cli-explicit"
        );
        assert!(!installed.metadata.contains_key("version"));
        assert!(!installed.metadata.contains_key("marketplace.is_official"));
        assert!(!installed.metadata.contains_key("install.trigger"));
        assert!(!installed
            .metadata
            .contains_key("install.disabled_by_default"));
    }

    #[test]
    fn concurrent_install_and_discovery_preserve_both_records() {
        let e = env();
        let weather_cache = e.plugins.join("cache/oldmkt/weather/1.0.0");
        std::fs::create_dir_all(weather_cache.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            weather_cache
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"weather","version":"1.0.0"}"#,
        )
        .unwrap();
        let _ = write_current_installed(
            &e,
            &serde_json::json!({
                "version": 2,
                "plugins": {
                    "weather@oldmkt": [{
                        "scope": "user",
                        "installPath": weather_cache.display().to_string(),
                        "version": "1.0.0",
                        "installedAt": "2026-08-31T00:00:00.000Z",
                        "lastUpdated": "2026-08-31T00:00:00.000Z"
                    }]
                }
            }),
        );

        let barrier = Arc::new(Barrier::new(2));
        let discover_plugins = e.plugins.clone();
        let discover_barrier = barrier.clone();
        let archive_session = Arc::new(plugin::PluginArchiveSession::new());
        let discover_archive_session = archive_session.clone();
        let discover = thread::spawn(move || {
            discover_barrier.wait();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(plugin::discover_recorded_plugins(
                    &discover_plugins,
                    &discover_archive_session,
                ))
        });

        let install_plugins = e.plugins.clone();
        let install_home = e.home.clone();
        let install_cwd = e.cwd.clone();
        let install_barrier = barrier;
        let install = thread::spawn(move || {
            install_barrier.wait();
            run_install(
                "hello@mymkt",
                None,
                &[],
                &install_plugins,
                &install_home,
                &install_cwd,
            )
        });

        let discovered = discover.join().unwrap();
        let install_result = install.join().unwrap().unwrap();

        assert!(
            discovered
                .iter()
                .any(|(_, manifest, _)| manifest.name == "weather"),
            "discovered plugin set must retain the current registry record"
        );
        assert_eq!(
            install_result,
            "Installing plugin \"hello@mymkt\"...✔ Successfully installed plugin: hello@mymkt (scope: user)"
        );
        assert!(installed_db(&e)["plugins"].get("weather@oldmkt").is_some());
        assert!(installed_db(&e)["plugins"].get("hello@mymkt").is_some());
    }

    #[test]
    fn install_accepts_relative_source_inside_catalog() {
        let e = env();
        std::fs::write(
            e.market
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("marketplace.json"),
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[{"name":"hello","source":"./plugins/hello"}]}"#,
        )
        .unwrap();

        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();

        assert!(e
            .plugins
            .join("cache/mymkt/hello/1.2.3/.lingxi-plugin/plugin.json")
            .exists());
    }

    /// Migrated from `plugin/tests/materialize.rs`'s
    /// `install_marketplace_arm_rejects_symlink_escape` (spec §25d): that
    /// test drove a lexically-safe-but-symlinked marketplace catalog entry
    /// through `PluginManager::install`'s now-deleted marketplace arm.
    /// Production's own `marketplace_entry_source_path` carries an
    /// equivalent canonicalize + `starts_with` containment check (see its
    /// doc comment) — this pins THAT check down directly, since nothing
    /// exercised it before.
    ///
    /// A symlinked entry canonicalizes outside the marketplace root, so
    /// `marketplace_entry_source_path` returns `Ok(None)` (fails closed,
    /// same as a plugin that was never listed) rather than the deleted
    /// arm's "resolves to a path outside the cache directory" message —
    /// the shape of the failure differs, but the security property (never
    /// copy the escaped directory) is the same.
    #[cfg(unix)]
    #[test]
    fn install_rejects_a_marketplace_entry_whose_path_symlinks_outside_the_root() {
        let e = env();
        // The exfiltration target OUTSIDE the marketplace repo (stands in
        // for `~/.ssh`).
        let outside = e._tmp.path().join("outside");
        std::fs::create_dir_all(outside.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            outside
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"evil","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(outside.join("id_rsa"), "PRIVATE KEY").unwrap();

        // A malicious catalog entry: "link" is a single Normal path component
        // (passes the lexical `..`/absolute guard in `plugin_dir_in_clone`)
        // but is a symlink pointing OUT of the marketplace root.
        std::fs::write(
            e.market
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("marketplace.json"),
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[{"name":"evil","source":"link"}]}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, e.market.join("link")).unwrap();

        let err = run_install("evil@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("a symlinked catalog entry must be rejected, not followed");
        assert!(err.contains("not found in marketplace"), "got: {err}");
        // Nothing was exfiltrated into the cache or source-cache.
        for root in [e.plugins.join("cache"), e.plugins.join("source-cache")] {
            if !root.exists() {
                continue;
            }
            let leaked = walkdir(&root).iter().any(|p| p.ends_with("id_rsa"));
            assert!(
                !leaked,
                "the symlink target's files must NOT be copied into {root:?}"
            );
        }
    }

    /// Migrated from `plugin/tests/materialize.rs`'s
    /// `install_git_arm_malicious_version_cannot_escape_cache` (spec §25d):
    /// that test drove a malicious `plugin.json` `"version":".."` through
    /// `PluginManager::install`'s now-deleted git arm's `copy_into_cache`
    /// (whose `sanitize_segment` is byte-identical to this crate's own
    /// `sanitize`, still live at every versioned-cache-path call site in
    /// this file). Pin the guard down directly at its real, still-used
    /// entry point instead of through the deleted duplicate.
    #[test]
    fn sanitize_neutralizes_dot_and_dotdot_segments() {
        for segment in ["..", ".", ""] {
            assert_eq!(
                sanitize(segment, true),
                "-",
                "segment {segment:?} must collapse to a safe token, not resolve to a parent/current dir"
            );
        }
        // A real version string is untouched (dots preserved only when allowed).
        assert_eq!(sanitize("1.2.3", true), "1.2.3");
        // A non-empty segment that merely CONTAINS ".." is not collapsed (only
        // an ENTIRE segment equal to ".." is); its slash becomes "-" and its
        // dots are preserved (allow_dot=true), same as any other character map.
        assert_eq!(sanitize("../1.2.3", true), "..-1.2.3");
    }

    #[cfg(unix)]
    #[test]
    fn external_git_source_materializes_a_confined_subdirectory() {
        let e = env();
        let repository = e._tmp.path().join("external-source");
        let plugin_root = repository.join("nested/plugin");
        std::fs::create_dir_all(plugin_root.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            plugin_root
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"external","version":"1.0.0"}"#,
        )
        .unwrap();
        for args in [
            vec!["init", repository.to_str().unwrap()],
            vec!["-C", repository.to_str().unwrap(), "add", "."],
            vec![
                "-C",
                repository.to_str().unwrap(),
                "-c",
                "user.name=LingXi Test",
                "-c",
                "user.email=lingxi@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .expect("git is required by marketplace installation");
            assert!(
                output.status.success(),
                "git fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let source = plugin::marketplace::MarketplacePluginExternalSource::GitSubdir {
            url: format!("file://{}", repository.display()),
            git_ref: None,
            path: "nested/plugin".to_string(),
            sha: None,
        };

        let materialized =
            materialize_external_plugin_source(&e.plugins, "mymkt", "external", &source).unwrap();

        assert!(materialized
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json")
            .is_file());
        assert!(materialized
            .starts_with(std::fs::canonicalize(e.plugins.join("source-cache")).unwrap()));
    }

    /// A bare `git init`+commit fixture whose root IS the plugin (no subdir),
    /// for the `url`-as-git-repo tests below. Returns `(repo path, HEAD sha)`.
    #[cfg(unix)]
    fn init_git_plugin_fixture(root: &Path) -> (PathBuf, String) {
        std::fs::create_dir_all(root.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            root.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),
            r#"{"name":"url-repo","version":"1.0.0"}"#,
        )
        .unwrap();
        for args in [
            vec!["init", root.to_str().unwrap()],
            vec!["-C", root.to_str().unwrap(), "add", "."],
            vec![
                "-C",
                root.to_str().unwrap(),
                "-c",
                "user.name=LingXi Test",
                "-c",
                "user.email=lingxi@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .expect("git is required by marketplace installation");
            assert!(
                output.status.success(),
                "git fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let head = std::process::Command::new("git")
            .args(["-C", root.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert!(head.status.success());
        (
            root.to_path_buf(),
            String::from_utf8_lossy(&head.stdout).trim().to_string(),
        )
    }

    /// Oracle: `source:"url"` on a plugin entry names a GIT REPOSITORY, not an
    /// archive — a prior port version conflated the two under the same `url`
    /// tag and tried to download-and-unpack an archive here, which would fail
    /// (or silently mis-handle) a real oracle `source:"url"` entry.
    #[cfg(unix)]
    #[test]
    fn external_url_source_materializes_via_git_clone_not_archive_download() {
        let e = env();
        let repository = e._tmp.path().join("url-source");
        let (repository, _head) = init_git_plugin_fixture(&repository);
        let (runtime, bus, sink) = runtime_bus();

        let source = plugin::marketplace::MarketplacePluginExternalSource::Url {
            url: format!("file://{}", repository.display()),
            git_ref: None,
            sha: None,
        };

        let materialized = runtime
            .block_on(materialize_external_plugin_source_with_bus(
                &e.plugins,
                "mymkt",
                "urlrepo",
                &source,
                Some(&bus),
                false,
            ))
            .unwrap();
        let events = runtime.block_on(sink.events());

        assert!(materialized
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json")
            .is_file());
        let record = events
            .iter()
            .find(|event| event.name == telemetry::tengu::plugin::REMOTE_FETCH)
            .expect("remote fetch event");
        assert_eq!(string_field(&record.metadata, "source"), "url");
        assert_eq!(string_field(&record.metadata, "host"), "file");
        assert_eq!(string_field(&record.metadata, "outcome"), "success");
        assert_eq!(string_field(&record.metadata, "error_kind"), "");
        assert!(int_field(&record.metadata, "duration_ms") >= 0);
        for value in record.metadata.values() {
            if let AnalyticsValue::String(value) = value {
                assert!(
                    !value.contains(&repository.display().to_string()),
                    "remote fetch metadata must not leak repository path: {value}"
                );
            }
        }
    }

    /// Oracle `ohr`: a `sha` that is not in the repository at all reaches
    /// `git checkout <sha>` (the `--unshallow` fallback fetch succeeds) and
    /// fails there — *"Failed to checkout commit …"*. Either way the install
    /// is refused; what must NOT happen is the pin being silently ignored.
    #[cfg(unix)]
    #[test]
    fn external_url_source_rejects_a_sha_pin_that_is_not_in_the_repository() {
        let e = env();
        let repository = e._tmp.path().join("url-source-pinned");
        let (repository, head) = init_git_plugin_fixture(&repository);
        let (runtime, bus, sink) = runtime_bus();
        let wrong_sha = if head.starts_with('f') {
            "0".repeat(40)
        } else {
            "f".repeat(40)
        };

        let source = plugin::marketplace::MarketplacePluginExternalSource::Url {
            url: format!("file://{}", repository.display()),
            git_ref: None,
            sha: Some(wrong_sha),
        };

        let error = runtime
            .block_on(materialize_external_plugin_source_with_bus(
                &e.plugins,
                "mymkt",
                "urlrepo-pinned",
                &source,
                Some(&bus),
                false,
            ))
            .expect_err("a mismatched sha pin must refuse the install");
        let events = runtime.block_on(sink.events());
        assert!(
            error.contains("Failed to checkout commit"),
            "expected the pinned checkout to fail, got: {error}"
        );
        let record = events
            .iter()
            .find(|event| event.name == telemetry::tengu::plugin::REMOTE_FETCH)
            .expect("remote fetch event");
        assert_eq!(string_field(&record.metadata, "source"), "url");
        assert_eq!(string_field(&record.metadata, "host"), "file");
        assert_eq!(string_field(&record.metadata, "outcome"), "failure");
        assert_eq!(string_field(&record.metadata, "error_kind"), "git_checkout");
        assert!(int_field(&record.metadata, "duration_ms") >= 0);
    }

    #[test]
    fn unsupported_entry_source_does_not_emit_remote_fetch() {
        let e = env();
        let (runtime, bus, sink) = runtime_bus();
        let source = plugin::marketplace::MarketplacePluginExternalSource::Unsupported {
            error: Some("registration source used as a plugin entry".to_string()),
        };

        let error = runtime
            .block_on(materialize_external_plugin_source_with_bus(
                &e.plugins,
                "mymkt",
                "hello",
                &source,
                Some(&bus),
                false,
            ))
            .expect_err("unsupported entry source should not route through remote fetch");
        assert!(error.contains("plugin source type unsupported"));
        assert!(
            runtime.block_on(sink.events()).is_empty(),
            "local source must not emit remote fetch"
        );
    }

    /// The tamper check itself (oracle `KHt`): once the pinned commit IS
    /// checked out, a resolved HEAD that still disagrees with the pin refuses
    /// the install with this byte-exact copy.
    #[test]
    fn verify_sha_pin_refuses_a_head_that_does_not_match_the_pin() {
        let error = verify_sha_pin(Some("a".repeat(40).as_str()), &"b".repeat(40)).unwrap_err();
        assert_eq!(
            error,
            format!(
                "SHA pin verification failed: expected HEAD to be {} , got {}. \
                 The pinned commit may have been removed upstream, or a ref with the same name \
                 exists. Refusing to install.",
                "a".repeat(40),
                "b".repeat(40)
            )
            .replace(" ,", ",")
        );
        assert!(verify_sha_pin(None, &"b".repeat(40)).is_ok());
    }

    /// Oracle `ohr`: a `sha` names the commit to CHECK OUT (`--no-checkout`,
    /// `fetch origin <sha>`, `checkout <sha>`), and the `rev-parse HEAD`
    /// verification is the tamper check that runs AFTER it. A pin to anything
    /// but the current tip — the only reason anyone pins — must therefore
    /// install, not refuse. A port that only compares the pin against the tip
    /// of the cloned ref inverts the feature: every genuine pin fails.
    #[cfg(unix)]
    #[test]
    fn external_url_source_installs_a_pin_to_a_non_tip_commit() {
        let e = env();
        let repository = e._tmp.path().join("url-source-nontip");
        let (repository, first) = init_git_plugin_fixture(&repository);

        // A second commit moves the tip away from the pinned commit and
        // changes the manifest, so the checked-out content is identifiable.
        std::fs::write(
            repository
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"url-repo","version":"2.0.0"}"#,
        )
        .unwrap();
        for args in [
            vec!["-C", repository.to_str().unwrap(), "add", "."],
            vec![
                "-C",
                repository.to_str().unwrap(),
                "-c",
                "user.name=LingXi Test",
                "-c",
                "user.email=lingxi@example.invalid",
                "commit",
                "-m",
                "second",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .expect("git is required by marketplace installation");
            assert!(output.status.success());
        }
        let tip = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C", repository.to_str().unwrap(), "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert_ne!(first, tip, "the fixture must have moved the tip");

        let source = plugin::marketplace::MarketplacePluginExternalSource::Url {
            url: format!("file://{}", repository.display()),
            git_ref: None,
            sha: Some(first.clone()),
        };

        let materialized =
            materialize_external_plugin_source(&e.plugins, "mymkt", "urlrepo-nontip", &source)
                .expect("a pin to a non-tip commit must install");
        let manifest = std::fs::read_to_string(
            materialized
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
        )
        .unwrap();
        assert!(
            manifest.contains("1.0.0"),
            "the PINNED commit's tree must be checked out, got: {manifest}"
        );
    }

    /// Oracle `ohr`'s first line: `Invalid sha "…": cannot start with "-"`.
    #[test]
    fn a_sha_pin_that_looks_like_a_git_option_is_refused() {
        let error = plugin::clone_plugin_git_pinned(
            "https://example.invalid/x.git",
            "",
            Some("--upload-pack=touch /tmp/pwn"),
            Path::new("/tmp/never-created-by-this-test"),
        )
        .expect_err("a sha starting with `-` must be refused before any git call");
        assert!(
            error.contains(r#"Invalid sha "--upload-pack=touch /tmp/pwn": cannot start with "-""#),
            "{error}"
        );
    }

    #[test]
    fn install_rejects_marketplace_source_outside_catalog_root() {
        let e = env();
        let outside = e._tmp.path().join("outside-plugin");
        std::fs::create_dir_all(outside.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            outside
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"escaped","version":"9.9.9"}"#,
        )
        .unwrap();
        std::fs::write(
            e.market
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("marketplace.json"),
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[{"name":"escaped","source":"../outside-plugin"}]}"#,
        )
        .unwrap();

        let error = run_install("escaped@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("catalog traversal must fail closed");

        assert!(error.contains("not found in marketplace"), "{error}");
        assert!(!e.plugins.join("cache/mymkt/escaped/9.9.9").exists());
    }

    #[test]
    fn install_rollback_removes_cache_created_before_registry_write_failure() {
        let e = env();
        let registry_path = installed_path(&e.plugins);
        std::fs::create_dir_all(&registry_path).unwrap();

        let error = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("a directory at installed_plugins.json must make the write fail");

        assert!(error.contains("Failed to install plugin"), "{error}");
        assert!(!e.plugins.join("cache/mymkt/hello/1.2.3").exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_never_writes_through_a_registry_symlink() {
        use std::os::unix::fs::symlink;

        let e = env();
        let outside = e._tmp.path().join("outside-registry.json");
        std::fs::write(&outside, r#"{"sentinel":true}"#).unwrap();
        symlink(&outside, installed_path(&e.plugins)).unwrap();

        let error = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("root-confined registry write must reject symlinks");

        assert!(error.contains("Failed to install plugin"), "{error}");
        assert_eq!(
            std::fs::read_to_string(outside).unwrap(),
            r#"{"sentinel":true}"#
        );
        assert!(!e.plugins.join("cache/mymkt/hello/1.2.3").exists());
    }

    #[test]
    fn plugin_data_paths_reject_forged_identifiers() {
        let e = env();
        for id in [
            "../escape",
            "/tmp/escape",
            "nested/plugin",
            "nested\\plugin",
        ] {
            assert!(confined_plugin_data_path(&e.plugins, id).is_none());
        }
    }

    #[test]
    fn plugin_data_path_uses_the_materialized_sanitized_identity() {
        let e = env();
        let expected = e.plugins.join("data/hello-mymkt");
        std::fs::create_dir_all(&expected).unwrap();

        assert_eq!(
            confined_plugin_data_path(&e.plugins, "hello@mymkt"),
            Some(expected.canonicalize().unwrap())
        );
    }

    #[test]
    fn install_respects_manifest_default_disabled_state() {
        let e = env();
        let manifest = e
            .market
            .join("plugins/hello")
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json");
        std::fs::write(
            manifest,
            r#"{"name":"hello","version":"1.2.3","defaultEnabled":false}"#,
        )
        .unwrap();

        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();

        assert_eq!(
            user_settings(&e)["enabledPlugins"]["hello@mymkt"],
            Value::Bool(false)
        );
    }

    #[test]
    fn marketplace_default_enabled_overrides_plugin_manifest() {
        let e = env();
        let marketplace = e
            .market
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("marketplace.json");
        std::fs::write(
            marketplace,
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[{"name":"hello","source":"./plugins/hello","defaultEnabled":false}]}"#,
        )
        .unwrap();

        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();

        assert_eq!(
            user_settings(&e)["enabledPlugins"]["hello@mymkt"],
            Value::Bool(false)
        );
    }

    /// Overwrite the marketplace `hello` plugin's manifest with a valid
    /// `userConfig` schema for `--config` behavior tests.
    fn set_user_config(e: &Env, user_config_json: &str) {
        let p = e
            .market
            .join("plugins")
            .join("hello")
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json");
        std::fs::write(
            &p,
            format!(r#"{{"name":"hello","version":"1.2.3","userConfig":{user_config_json}}}"#),
        )
        .unwrap();
    }

    fn add_market_plugin(e: &Env, name: &str, version: &str, dependencies: Value) {
        let marketplace_path = e
            .market
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("marketplace.json");
        let mut marketplace: Value =
            serde_json::from_str(&std::fs::read_to_string(&marketplace_path).unwrap()).unwrap();
        marketplace["plugins"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": name,
                "source": format!("./plugins/{name}")
            }));
        std::fs::write(
            marketplace_path,
            serde_json::to_string_pretty(&marketplace).unwrap(),
        )
        .unwrap();
        let root = e.market.join("plugins").join(name);
        std::fs::create_dir_all(root.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            root.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),
            serde_json::to_string(&serde_json::json!({
                "name": name,
                "version": version,
                "dependencies": dependencies,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn set_dependencies(e: &Env, plugin: &str, version: &str, dependencies: Value) {
        let root = e.market.join("plugins").join(plugin);
        std::fs::write(
            root.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),
            serde_json::to_string(&serde_json::json!({
                "name": plugin,
                "version": version,
                "dependencies": dependencies,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn install_config_persists_nonsensitive_options() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        run_install(
            "hello@mymkt",
            None,
            &["REGION=us-east".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        // Persisted to pluginConfigs[<name@marketplace>].options — the map the
        // loader reads (H-12: keyed by full identity, not the bare manifest name).
        assert_eq!(
            user_settings(&e)["pluginConfigs"]["hello@mymkt"]["options"]["REGION"],
            Value::String("us-east".into())
        );
        // The install itself still succeeded (enabledPlugins set too).
        assert_eq!(
            user_settings(&e)["enabledPlugins"]["hello@mymkt"],
            Value::Bool(true)
        );
    }

    #[test]
    fn install_recursively_records_auto_dependency_and_reverse_owner() {
        let e = env();
        add_market_plugin(&e, "shared", "1.5.0", serde_json::json!([]));
        set_dependencies(
            &e,
            "hello",
            "1.2.3",
            serde_json::json!([{"name":"shared","version":"^1.4"}]),
        );

        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();

        let installed = installed_db(&e);
        let dependency = &installed["plugins"]["shared@mymkt"][0];
        assert_eq!(dependency["auto"], Value::Bool(true));
        assert!(dependency.get("autoInstalled").is_none());
        assert!(dependency.get("auto_installed").is_none());
        assert_eq!(dependency["requiredBy"], serde_json::json!(["hello@mymkt"]));
        assert_eq!(
            user_settings(&e)["enabledPlugins"]["shared@mymkt"],
            Value::Bool(true)
        );
    }

    #[test]
    fn install_rejects_dependency_cycles_before_materializing_root() {
        let e = env();
        add_market_plugin(
            &e,
            "shared",
            "1.0.0",
            serde_json::json!([{"name":"hello","version":"*"}]),
        );
        set_dependencies(
            &e,
            "hello",
            "1.2.3",
            serde_json::json!([{"name":"shared","version":"*"}]),
        );

        let error = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert!(error.contains(
            "Plugin dependency cycle detected: hello@mymkt -> shared@mymkt -> hello@mymkt"
        ));
        assert!(!installed_path(&e.plugins).exists());
    }

    #[test]
    fn install_rejects_unsatisfied_dependency_range() {
        let e = env();
        add_market_plugin(&e, "shared", "2.0.0", serde_json::json!([]));
        set_dependencies(
            &e,
            "hello",
            "1.2.3",
            serde_json::json!([{"name":"shared","version":"^1"}]),
        );

        let error = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert!(error.contains("shared@mymkt\" version 2.0.0 does not satisfy ^1"));
        assert!(!installed_path(&e.plugins).exists());
    }

    #[test]
    fn array_enabled_dependency_keeps_satisfying_installed_version_and_utf16_settings() {
        let e = env();
        add_market_plugin(&e, "shared", "2.1.0", serde_json::json!([]));
        run_install("shared@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();

        // Source offers 3.0.0, while installed 2.1.0 satisfies the owner's
        // ^2 requirement. The array keeps that installed copy from being
        // replaced by the newer listing.
        set_dependencies(&e, "shared", "3.0.0", serde_json::json!([]));
        set_dependencies(
            &e,
            "hello",
            "1.2.3",
            serde_json::json!([{"name":"shared","version":"^2"}]),
        );
        std::fs::write(
            scope_path(WritableScope::User, &e.home, &e.cwd),
            r#"{"enabledPlugins":{"shared@mymkt":["\ud800"],"hello@mymkt":[]}}"#,
        )
        .unwrap();

        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();

        let installed = installed_db(&e);
        assert_eq!(installed["plugins"]["shared@mymkt"][0]["version"], "2.1.0");
        let enabled = read_enabled(WritableScope::User, &e.home, &e.cwd);
        let pinned = enabled.get_property("shared@mymkt").unwrap();
        assert_eq!(pinned.array_len(), Some(1));
        assert_eq!(
            pinned.get_index(0).unwrap().code_units(),
            Some(&[0xd800][..])
        );
        assert_eq!(
            enabled.get_property("hello@mymkt").unwrap().array_len(),
            Some(0)
        );
    }

    #[test]
    fn array_enabled_fresh_dependency_still_checks_available_version() {
        let e = env();
        add_market_plugin(&e, "shared", "2.0.0", serde_json::json!([]));
        set_dependencies(
            &e,
            "hello",
            "1.2.3",
            serde_json::json!([{"name":"shared","version":"^1"}]),
        );
        std::fs::write(
            scope_path(WritableScope::User, &e.home, &e.cwd),
            r#"{"enabledPlugins":{"shared@mymkt":[]}}"#,
        )
        .unwrap();

        let error = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("an array must not waive a fresh dependency's version range");
        assert!(error.contains("shared@mymkt\" version 2.0.0 does not satisfy ^1"));
        assert!(!installed_path(&e.plugins).exists());
    }

    #[test]
    fn array_enabled_installed_dependency_still_checks_installed_version() {
        let e = env();
        add_market_plugin(&e, "shared", "1.0.0", serde_json::json!([]));
        run_install("shared@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        set_dependencies(&e, "shared", "3.0.0", serde_json::json!([]));
        set_dependencies(
            &e,
            "hello",
            "1.2.3",
            serde_json::json!([{"name":"shared","version":"^2"}]),
        );
        std::fs::write(
            scope_path(WritableScope::User, &e.home, &e.cwd),
            r#"{"enabledPlugins":{"shared@mymkt":[]}}"#,
        )
        .unwrap();

        let error = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("an array must not waive the installed dependency's version range");
        assert!(error.contains("shared@mymkt\" version 1.0.0 does not satisfy ^2"));
        let installed = installed_db(&e);
        assert_eq!(installed["plugins"]["shared@mymkt"][0]["version"], "1.0.0");
        assert!(installed["plugins"].get("hello@mymkt").is_none());
    }

    #[test]
    fn install_config_persists_declared_scalar_types() {
        let e = env();
        set_user_config(
            &e,
            r#"{"RETRIES":{"type":"number","title":"Retries","description":""},"ENABLED":{"type":"boolean","title":"Enabled","description":""}}"#,
        );
        run_install(
            "hello@mymkt",
            None,
            &["RETRIES=3.5".to_string(), "ENABLED=true".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let options = &user_settings(&e)["pluginConfigs"]["hello@mymkt"]["options"];
        assert_eq!(options["RETRIES"], serde_json::json!(3.5));
        assert_eq!(options["ENABLED"], Value::Bool(true));
    }

    #[test]
    fn install_config_rejects_invalid_scalar_before_materialization() {
        let e = env();
        set_user_config(
            &e,
            r#"{"ENABLED":{"type":"boolean","title":"Enabled","description":""}}"#,
        );
        let error = run_install(
            "hello@mymkt",
            None,
            &["ENABLED=yes".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "--config ENABLED: expected a boolean (true or false), got \"yes\"."
        );
        assert!(!installed_path(&e.plugins).exists());
    }

    #[test]
    fn install_config_sensitive_not_written_to_plaintext() {
        let e = env();
        set_user_config(
            &e,
            r#"{"API_KEY":{"type":"string","title":"API key","description":"","sensitive":true},"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        run_install(
            "hello@mymkt",
            None,
            &["API_KEY=sk-live".to_string(), "REGION=eu".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let opts = &user_settings(&e)["pluginConfigs"]["hello@mymkt"]["options"];
        // Non-sensitive persisted; the secret is NOT in plaintext settings.
        assert_eq!(opts["REGION"], Value::String("eu".into()));
        assert!(opts.get("API_KEY").is_none());
    }

    #[test]
    fn install_config_malformed_errors() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        let err = run_install(
            "hello@mymkt",
            None,
            &["NOEQ".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "--config expects KEY=VALUE, got \"NOEQ\". Use --config key=value (repeatable)."
        );
    }

    #[test]
    fn install_config_undeclared_key_errors() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        let err = run_install(
            "hello@mymkt",
            None,
            &["NOPE=1".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "--config key \"NOPE\" isn't declared in this plugin's userConfig. Known keys: REGION."
        );
    }

    #[test]
    fn install_config_empty_value_errors() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        let err = run_install(
            "hello@mymkt",
            None,
            &["REGION=".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "--config REGION: value is empty. Omit the flag to leave \"REGION\" unset."
        );
    }

    #[test]
    fn uninstall_clears_plugin_config() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        run_install(
            "hello@mymkt",
            None,
            &["REGION=us-east".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        assert!(user_settings(&e)["pluginConfigs"]
            .get("hello@mymkt")
            .is_some());
        run_uninstall(
            "hello@mymkt",
            None,
            false,
            false,
            false,
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        // deletePluginOptions parity: the pluginConfigs entry is gone.
        assert!(user_settings(&e)["pluginConfigs"]
            .get("hello@mymkt")
            .is_none());
    }

    #[test]
    fn plugin_config_update_and_clear_preserve_exact_sibling_source() {
        let e = env();
        let path = scope_path(WritableScope::User, &e.home, &e.cwd);
        std::fs::write(
            &path,
            r#"{"hooks":{"input":{"value":"\ud800"}},"pluginConfigs":{"hello@mymkt":{"options":{"EXISTING":"\udfff"},"mcpServers":{"server":{"input":"\ud800"}}},"other@mymkt":{"options":{"keep":true}}}}"#,
        )
        .unwrap();
        let mut schema = Map::new();
        schema.insert(
            "REGION".to_string(),
            serde_json::json!({"sensitive": false, "type": "string"}),
        );
        let pairs = vec![ConfigPair {
            key: "REGION".into(),
            value: Value::String("west".into()),
            raw_value: "west".into(),
        }];

        persist_plugin_options(
            WritableScope::User,
            &e.home,
            &e.cwd,
            "hello@mymkt",
            &schema,
            &pairs,
        )
        .unwrap();

        let source = read_settings_source(&path).unwrap();
        assert_eq!(
            source.string_units("/hooks/input/value"),
            Some(vec![0xD800])
        );
        assert_eq!(
            source.string_units("/pluginConfigs/hello@mymkt/options/EXISTING"),
            Some(vec![0xDFFF])
        );
        assert_eq!(
            source.string_at("/pluginConfigs/hello@mymkt/options/REGION"),
            Some("west")
        );
        assert_eq!(
            source.string_units("/pluginConfigs/hello@mymkt/mcpServers/server/input"),
            Some(vec![0xD800])
        );
        assert!(source
            .at_pointer("/pluginConfigs/other@mymkt/options/keep")
            .is_some());

        clear_plugin_config_strict(WritableScope::User, &e.home, &e.cwd, "hello@mymkt").unwrap();
        let source = read_settings_source(&path).unwrap();
        assert_eq!(
            source.string_units("/hooks/input/value"),
            Some(vec![0xD800])
        );
        assert!(source.at_pointer("/pluginConfigs/hello@mymkt").is_none());
        assert!(source
            .at_pointer("/pluginConfigs/other@mymkt/options/keep")
            .is_some());
    }

    #[test]
    fn settings_journal_rollback_preserves_interleaved_nested_plugin_updates() {
        let e = env();
        let path = scope_path(WritableScope::User, &e.home, &e.cwd);
        std::fs::write(
            &path,
            r#"{"enabledPlugins":{"hello@mymkt":true,"other@mymkt":true},"pluginConfigs":{"hello@mymkt":{"options":{"REGION":"old","KEEP":"old"},"mcpServers":{"server":{"input":"\ud800"}}},"other@mymkt":{"options":{"REGION":"old-other"}}},"opaque":{"value":"\udfff"}}"#,
        )
        .unwrap();
        let mut schema = Map::new();
        schema.insert(
            "REGION".to_string(),
            serde_json::json!({"sensitive": false, "type": "string"}),
        );
        let pairs = vec![ConfigPair {
            key: "REGION".into(),
            value: Value::String("west".into()),
            raw_value: "west".into(),
        }];
        let mut journal = SettingsRollbackJournal::default();
        persist_plugin_options_with_journal(
            WritableScope::User,
            &e.home,
            &e.cwd,
            "hello@mymkt",
            &schema,
            &pairs,
            Some(&mut journal),
        )
        .unwrap();

        // A migration/settings writer commits after the plugin mutation but
        // before the enclosing operation fails and rolls back.
        let mut latest = read_settings_source(&path).unwrap();
        let mut enabled = source_object_field(&latest, "enabledPlugins");
        enabled
            .set_field("other@mymkt", source_bool(false))
            .unwrap();
        latest.set_field("enabledPlugins", enabled).unwrap();
        let mut configs = source_object_field(&latest, "pluginConfigs");
        let mut hello = source_object_field(&configs, "hello@mymkt");
        let mut hello_options = source_object_field(&hello, "options");
        hello_options
            .set_field(
                "KEEP",
                Utf16JsonProjection::parse(r#""concurrent-keep""#).unwrap(),
            )
            .unwrap();
        hello_options
            .set_field(
                "NEW_TOKEN",
                Utf16JsonProjection::parse(r#""concurrent-token""#).unwrap(),
            )
            .unwrap();
        hello.set_field("options", hello_options).unwrap();
        configs.set_field("hello@mymkt", hello).unwrap();
        let mut other = source_object_field(&configs, "other@mymkt");
        let mut other_options = source_object_field(&other, "options");
        other_options
            .set_field(
                "REGION",
                Utf16JsonProjection::parse(r#""concurrent-other""#).unwrap(),
            )
            .unwrap();
        other.set_field("options", other_options).unwrap();
        configs.set_field("other@mymkt", other).unwrap();
        latest.set_field("pluginConfigs", configs).unwrap();
        migrations::settings_update::update_settings_source_with_before_publish(
            &path,
            vec![
                (
                    "enabledPlugins".to_string(),
                    Some(source_object_field(&latest, "enabledPlugins")),
                ),
                (
                    "pluginConfigs".to_string(),
                    Some(source_object_field(&latest, "pluginConfigs")),
                ),
            ],
            || {},
        )
        .unwrap();

        journal.rollback();
        let restored = read_settings_source(&path).unwrap();
        assert_eq!(
            restored.string_at("/pluginConfigs/hello@mymkt/options/REGION"),
            Some("old")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/hello@mymkt/options/KEEP"),
            Some("concurrent-keep")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/hello@mymkt/options/NEW_TOKEN"),
            Some("concurrent-token")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/other@mymkt/options/REGION"),
            Some("concurrent-other")
        );
        assert_eq!(
            restored.string_units("/pluginConfigs/hello@mymkt/mcpServers/server/input"),
            Some(vec![0xd800])
        );
        assert_eq!(restored.string_units("/opaque/value"), Some(vec![0xdfff]));
        assert_eq!(
            restored
                .at_pointer("/enabledPlugins/other@mymkt")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn install_bare_name_resolves_marketplace() {
        let e = env();
        let msg = run_install("hello", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Installing plugin \"hello\"...✔ Successfully installed plugin: hello@mymkt (scope: user)"
        );
    }

    #[test]
    fn install_already_installed() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Installing plugin \"hello@mymkt\"...✔ Plugin \"hello@mymkt\" is already installed (scope: user)"
        );
    }

    /// §8: install previously never called the name gate at all — a
    /// marketplace catalog entry declaring a space/control/bidi-laden `name`
    /// materialized without complaint. Reproduces via a bare-relative source
    /// entry, the simplest catalog shape.
    #[test]
    fn install_rejects_a_catalog_entry_with_an_invalid_name() {
        let e = env();
        let manifest = e
            .market
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("marketplace.json");
        std::fs::write(
            &manifest,
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[
                {"name":"hello","source":"./plugins/hello"},
                {"name":"bad name","source":"./plugins/hello"}
            ]}"#,
        )
        .unwrap();

        let err =
            run_install("bad name@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Installing plugin \"bad name@mymkt\"...✘ Failed to install plugin \"bad name@mymkt\": \
             Invalid marketplace entry for \"bad name\": Plugin name cannot contain spaces. \
             Use kebab-case (e.g., \"my-plugin\")"
        );
    }

    #[test]
    fn install_no_marketplace() {
        let e = env();
        std::fs::remove_file(e.plugins.join("known_marketplaces.json")).unwrap();
        let err = run_install("foo", None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Installing plugin \"foo\"...✘ Failed to install plugin \"foo\": Plugin \"foo\" not found in any configured marketplace"
        );
    }

    #[test]
    fn install_unknown_marketplace() {
        let e = env();
        let err = run_install("foo@bar", None, &[], &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Installing plugin \"foo@bar\"...✘ Failed to install plugin \"foo@bar\": Plugin \"foo\" not found in marketplace \"bar\". Your local copy may be out of date — try `lingxi-cli plugin marketplace update bar`."
        );
    }

    #[test]
    fn install_invalid_scope() {
        let e = env();
        let err = run_install(
            "hello@mymkt",
            Some("bogus"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        // Bare scope error — NO "Installing plugin …" prefix (matches the binary).
        assert_eq!(
            err,
            "Invalid scope: bogus. Must be one of: user, project, local."
        );
    }

    #[test]
    fn install_config_rejected_for_project_scope() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        let err = run_install(
            "hello@mymkt",
            Some("project"),
            &["REGION=us-east".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Installing plugin \"hello@mymkt\"...✘ Failed to install plugin \"hello@mymkt\": --config can only be used with user scope."
        );
    }

    #[test]
    fn install_config_rejected_for_local_scope() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        let err = run_install(
            "hello@mymkt",
            Some("local"),
            &["REGION=us-east".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Installing plugin \"hello@mymkt\"...✘ Failed to install plugin \"hello@mymkt\": --config can only be used with user scope."
        );
    }

    #[test]
    fn install_config_persists_with_explicit_user_scope() {
        let e = env();
        set_user_config(
            &e,
            r#"{"REGION":{"type":"string","title":"Region","description":"","sensitive":false}}"#,
        );
        run_install(
            "hello@mymkt",
            Some("user"),
            &["REGION=eu".to_string()],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        assert_eq!(
            user_settings(&e)["pluginConfigs"]["hello@mymkt"]["options"]["REGION"],
            Value::String("eu".into())
        );
    }

    #[test]
    fn uninstall_removes_record_deletes_key_and_orphans() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_uninstall(
            "hello", None, false, false, true, &e.plugins, &e.home, &e.cwd,
        )
        .unwrap();
        assert_eq!(
            msg,
            "✔ Successfully uninstalled plugin: hello (scope: user)"
        );
        // Record gone.
        assert_eq!(installed_db(&e)["plugins"], serde_json::json!({}));
        // enabledPlugins KEY DELETED (not set false).
        assert_eq!(user_settings(&e)["enabledPlugins"], serde_json::json!({}));
        // Cache orphaned (marker written, tree kept).
        assert!(e
            .plugins
            .join("cache/mymkt/hello/1.2.3/.orphaned_at")
            .exists());
        assert!(e
            .plugins
            .join("cache/mymkt/hello/1.2.3/commands/hi.md")
            .exists());
    }

    /// §22 (oracle `QWn`): a non-`--prune` uninstall's message gains a
    /// trailing notice naming any auto-installed dependency the DB shows as
    /// newly unreachable — here modeled directly against an installed DB
    /// carrying only the orphan (nothing manual reaches it).
    #[test]
    fn uninstall_orphan_suffix_reports_a_newly_orphaned_dependency() {
        let e = env();
        let dep_path = e.plugins.join("cache/mymkt/dep/1.0.0");
        std::fs::create_dir_all(dep_path.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            dep_path
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"dep","version":"1.0.0"}"#,
        )
        .unwrap();
        write_installed(
            &e.plugins,
            &serde_json::json!({
                "version": 2,
                "plugins": {
                    "dep@mymkt": [{
                        "scope": "user",
                        "installPath": dep_path.display().to_string(),
                        "version": "1.0.0",
                        "installedAt": "2026-01-01T00:00:00.000Z",
                        "lastUpdated": "2026-01-01T00:00:00.000Z",
                        "auto": true,
                    }]
                }
            }),
        )
        .unwrap();

        let suffix = uninstall_orphan_suffix(&e.plugins, WritableScope::User, &None);
        assert_eq!(
            suffix,
            "\n1 auto-installed dependency no longer needed: dep. Run `lingxi-cli plugin prune` \
             to remove."
        );
    }

    #[test]
    fn uninstall_orphan_suffix_is_empty_with_no_orphans() {
        let e = env();
        assert_eq!(
            uninstall_orphan_suffix(&e.plugins, WritableScope::User, &None),
            ""
        );
    }

    #[test]
    fn uninstall_not_installed() {
        let e = env();
        let err = run_uninstall("foo", None, false, false, true, &e.plugins, &e.home, &e.cwd)
            .unwrap_err();
        assert_eq!(
            err,
            "✘ Failed to uninstall plugin \"foo\": Plugin \"foo\" not found in installed plugins"
        );
    }

    /// Bump the marketplace's `hello` plugin to `version`, adding a marker file.
    fn bump_market_hello(e: &Env, version: &str) {
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            format!(r#"{{"name":"hello","version":"{version}"}}"#),
        )
        .unwrap();
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join("commands")
                .join("new.md"),
            "# new",
        )
        .unwrap();
    }

    #[test]
    fn update_bumps_version_recopies_and_orphans_old() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let installed_at = installed_db(&e)["plugins"]["hello@mymkt"][0]["installedAt"].clone();
        bump_market_hello(&e, "2.0.0");

        let msg = run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Checking for updates for plugin \"hello@mymkt\" at user scope\u{2026}\n\
             \u{2714} Plugin \"hello\" updated from 1.2.3 to 2.0.0 for scope user. Restart to apply changes."
        );

        // New version materialized (with the new component), old version orphaned.
        let new_cache = e.plugins.join("cache/mymkt/hello/2.0.0");
        assert!(new_cache.join(".lingxi-plugin/plugin.json").exists());
        assert!(new_cache.join("commands/new.md").exists());
        assert!(e
            .plugins
            .join("cache/mymkt/hello/1.2.3/.orphaned_at")
            .exists());
        assert!(e
            .plugins
            .join("cache/mymkt/hello/1.2.3/commands/hi.md")
            .exists());

        // Record bumped: version + installPath + lastUpdated changed; installedAt kept.
        let db = installed_db(&e);
        let rec = &db["plugins"]["hello@mymkt"][0];
        assert_eq!(rec["scope"], "user");
        assert_eq!(rec["version"], "2.0.0");
        assert_eq!(rec["installPath"], new_cache.display().to_string());
        assert_eq!(rec["installedAt"], installed_at); // installedAt preserved
        assert!(rec["lastUpdated"].is_string());
        // enabledPlugins untouched by update.
        assert_eq!(
            user_settings(&e)["enabledPlugins"]["hello@mymkt"],
            Value::Bool(true)
        );
    }

    /// §21.9 — a versionless plugin's cache dir (`cache/<market>/<plugin>/unknown`)
    /// is FIXED and shared by every scope. Install the same versionless plugin
    /// at two scopes (both records land on the identical shared cache dir), then
    /// update one scope: the update must defer instead of blowing the shared
    /// Oracle `ice`: the "in use by another session" branch is keyed on a LIVE
    /// session lease (`gK` = `aS(…,{excludeSelf:!0},…)`), and even when it
    /// fires it only writes a DEBUG line and RETURNS the cache path — the
    /// install/update continues and the record is still written. A port that
    /// substitutes "some other installed-plugin RECORD points at this path"
    /// for a live lease deadlocks the ordinary case of one plugin installed at
    /// two scopes: both records share the versionless cache path
    /// `cache/<mkt>/<plugin>/unknown`, so neither scope can ever be updated
    /// again and nothing ever clears the condition.
    #[test]
    fn update_converges_when_a_versionless_cache_is_shared_by_another_scope() {
        let e = env();
        // Strip `version` so the marketplace resolves to "unknown" and both
        // scopes' records land on the identical cache path.
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"hello"}"#,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();

        let shared_cache = e.plugins.join("cache/mymkt/hello/unknown");
        let records = installed_db(&e)["plugins"]["hello@mymkt"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(records.len(), 2, "both scopes share one record set");
        assert!(records
            .iter()
            .all(|r| r["installPath"] == shared_cache.display().to_string()));

        // A new file upstream: the update must actually re-copy.
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join("commands")
                .join("new.md"),
            "# new",
        )
        .unwrap();

        let msg = run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();
        assert!(
            !msg.contains("in use by another session"),
            "a second scope's persisted record is not a live session: {msg}"
        );
        assert!(
            shared_cache.join("commands/new.md").exists(),
            "the update must have re-materialized the cache, got: {msg}"
        );
        assert!(shared_cache.join("commands/hi.md").exists());
    }

    /// The same deadlock in its versioned form (the reviewer's repro): update
    /// the user scope to 2.0.0 first, then the project scope must be able to
    /// reach the very same 2.0.0 cache dir instead of being pinned at 1.2.3.
    #[test]
    fn update_converges_for_a_second_scope_pointing_at_an_existing_version_cache() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"hello","version":"2.0.0"}"#,
        )
        .unwrap();

        run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_update("hello@mymkt", "project", &e.plugins, &e.home, &e.cwd).unwrap();
        assert!(
            msg.contains("updated from 1.2.3 to 2.0.0"),
            "the project scope must converge too, got: {msg}"
        );
        let expected = e
            .plugins
            .join("cache/mymkt/hello/2.0.0")
            .display()
            .to_string();
        let records = installed_db(&e)["plugins"]["hello@mymkt"]
            .as_array()
            .unwrap()
            .clone();
        assert!(
            records
                .iter()
                .all(|r| r["installPath"] == expected && r["version"] == "2.0.0"),
            "both records must land on the 2.0.0 cache: {records:?}"
        );
    }

    #[test]
    fn update_already_latest() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let msg = run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();
        assert_eq!(
            msg,
            "Checking for updates for plugin \"hello@mymkt\" at user scope\u{2026}\n\
             \u{2714} hello is already at the latest version (1.2.3)."
        );
    }

    #[test]
    fn update_bare_name_not_found() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let err = run_update("hello", "user", &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Checking for updates for plugin \"hello\" at user scope\u{2026}\n\
             ✘ Failed to update plugin \"hello\": Plugin \"hello\" not found"
        );
    }

    #[test]
    fn update_unknown_plugin_in_marketplace_not_found() {
        let e = env();
        let err = run_update("foo@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Checking for updates for plugin \"foo@mymkt\" at user scope\u{2026}\n\
             ✘ Failed to update plugin \"foo@mymkt\": Plugin \"foo\" not found"
        );
    }

    #[test]
    fn update_in_marketplace_but_not_installed() {
        let e = env();
        // Add a `world` plugin to the marketplace but never install it.
        std::fs::write(
            e.market.join(branding::PLUGIN_MANIFEST_DIR).join("marketplace.json"),
            r#"{"name":"mymkt","owner":{"name":"me"},"plugins":[{"name":"hello","source":"./plugins/hello"},{"name":"world","source":"./plugins/world"}]}"#,
        )
        .unwrap();
        let err = run_update("world@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Checking for updates for plugin \"world@mymkt\" at user scope\u{2026}\n\
             ✘ Failed to update plugin \"world@mymkt\": Plugin \"world\" is not installed"
        );
    }

    #[test]
    fn update_wrong_scope_managed() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let err = run_update("hello@mymkt", "managed", &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Checking for updates for plugin \"hello@mymkt\" at managed scope\u{2026}\n\
             ✘ Failed to update plugin \"hello@mymkt\": Plugin \"hello\" is not installed at scope managed"
        );
    }

    #[test]
    fn update_wrong_scope_project_shows_cwd() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let err = run_update("hello@mymkt", "project", &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            format!(
                "Checking for updates for plugin \"hello@mymkt\" at project scope\u{2026}\n\
                 ✘ Failed to update plugin \"hello@mymkt\": Plugin \"hello\" is not installed at scope project ({})",
                e.cwd.display()
            )
        );
    }

    #[test]
    fn update_invalid_scope_has_no_header() {
        let e = env();
        let err = run_update("hello@mymkt", "bogus", &e.plugins, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Invalid scope \"bogus\". Valid scopes: user, project, local, managed"
        );
    }

    #[test]
    fn install_second_scope_appends_with_project_path() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let msg = run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        assert!(
            msg.contains("Successfully installed plugin: hello@mymkt (scope: project)"),
            "{msg}"
        );
        let db = installed_db(&e);
        let arr = db["plugins"]["hello@mymkt"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["scope"], "user");
        assert_eq!(arr[1]["scope"], "project");
        // project record carries projectPath; user record does not.
        assert!(arr[0].get("projectPath").is_none());
        assert!(arr[1].get("projectPath").is_some());
    }

    #[test]
    fn install_same_scope_twice_is_already_installed() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let msg = run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        assert!(msg.contains("already installed (scope: user)"), "{msg}");
    }

    #[test]
    fn uninstall_scope_mismatch_names_actual_scope() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let err = run_uninstall(
            "hello@mymkt",
            Some("project"),
            false,
            false,
            true,
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "✘ Failed to uninstall plugin \"hello@mymkt\": Plugin \"hello@mymkt\" is installed in user scope, not project. Use --scope user to uninstall."
        );
        // The user record is untouched.
        assert!(installed_db(&e)["plugins"].get("hello@mymkt").is_some());
    }

    #[test]
    fn uninstall_removes_only_matching_scope() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_uninstall(
            "hello@mymkt",
            Some("project"),
            false,
            false,
            true,
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let db = installed_db(&e);
        let arr = db["plugins"]["hello@mymkt"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["scope"], "user");
    }

    #[test]
    fn uninstall_keeps_shared_cache_and_data_when_another_scope_still_references_it() {
        let e = env();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        let data_dir = e.plugins.join("data").join(sanitize("hello@mymkt", false));
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("sentinel"), "keep").unwrap();

        run_uninstall(
            "hello@mymkt",
            Some("project"),
            false,
            false,
            true,
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();

        let shared_cache = e.plugins.join("cache/mymkt/hello/1.2.3");
        assert!(shared_cache.join("commands/hi.md").exists());
        assert!(!shared_cache.join(".orphaned_at").exists());
        assert!(data_dir.join("sentinel").exists());
    }

    #[test]
    fn uninstall_settings_failure_restores_registry_and_skips_cleanup() {
        let e = env();
        run_install("hello@mymkt", None, &[], &e.plugins, &e.home, &e.cwd).unwrap();
        let data_dir = e.plugins.join("data").join(sanitize("hello@mymkt", false));
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("sentinel"), "keep").unwrap();
        std::fs::remove_file(e.home.join("settings.json")).unwrap();
        std::fs::create_dir_all(e.home.join("settings.json")).unwrap();

        let err = run_uninstall(
            "hello@mymkt",
            None,
            false,
            false,
            true,
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();

        assert!(err.contains("Failed to uninstall plugin"), "{err}");
        assert!(installed_db(&e)["plugins"].get("hello@mymkt").is_some());
        assert!(!e
            .plugins
            .join("cache/mymkt/hello/1.2.3/.orphaned_at")
            .exists());
        assert!(data_dir.join("sentinel").exists());
    }

    #[test]
    fn uninstall_failure_restores_current_registry_bytes() {
        let e = env();
        let current = serde_json::json!({
            "version": 2,
            "plugins": {
                "hello@mymkt": [{
                    "scope": "user",
                    "installPath": e
                        .plugins
                        .join("cache/mymkt/hello/1.2.3")
                        .display()
                        .to_string(),
                    "version": "1.2.3",
                    "installedAt": "2026-01-01T00:00:00.000Z",
                    "lastUpdated": "2026-01-01T00:00:00.000Z"
                }]
            }
        });
        let before = write_current_installed(&e, &current);
        std::fs::create_dir_all(e.home.join("settings.json")).unwrap();

        let err = run_uninstall(
            "hello@mymkt",
            None,
            false,
            false,
            true,
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap_err();

        assert!(err.contains("Failed to uninstall plugin"), "{err}");
        assert_eq!(
            std::fs::read(installed_path(&e.plugins)).unwrap(),
            before,
            "failed uninstall must restore the exact current registry bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_symlinked_cache_root_restores_current_registry_bytes() {
        use std::os::unix::fs::symlink;

        let e = env();
        let current = serde_json::json!({
            "version": 2,
            "plugins": {
                "hello@mymkt": [{
                    "scope": "user",
                    "installPath": e
                        .plugins
                        .join("cache/mymkt/hello/1.2.3")
                        .display()
                        .to_string(),
                    "version": "1.2.3",
                    "installedAt": "2026-01-01T00:00:00.000Z",
                    "lastUpdated": "2026-01-01T00:00:00.000Z"
                }]
            }
        });
        let before = write_current_installed(&e, &current);
        let outside = e._tmp.path().join("outside-cache");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "must stay outside").unwrap();
        symlink(&outside, e.plugins.join("cache")).unwrap();
        bump_market_hello(&e, "2.0.0");

        let err = run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap_err();

        assert!(
            err.contains("Refusing to use a symlinked plugin cache root"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(installed_path(&e.plugins)).unwrap(),
            before,
            "rejected update must leave exact current registry bytes unchanged"
        );
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"must stay outside"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn update_versionless_shared_cache_leaves_no_staging_or_backup_dirs() {
        let e = env();
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"hello"}"#,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("user"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        run_install(
            "hello@mymkt",
            Some("project"),
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
        )
        .unwrap();
        std::fs::write(
            e.market
                .join("plugins")
                .join("hello")
                .join("commands")
                .join("new.md"),
            "# new",
        )
        .unwrap();

        run_update("hello@mymkt", "user", &e.plugins, &e.home, &e.cwd).unwrap();

        let parent = e.plugins.join("cache/mymkt/hello");
        let leftovers: Vec<String> = std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with(".staged-") || name.starts_with(".backup-"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected leftovers: {leftovers:?}");
    }

    /// Marketplace `source:"command"` plugin plus a producer directory whose
    /// path the command prints.
    fn command_source_env() -> (Env, String) {
        let e = env();
        let producer = e._tmp.path().join("produced-from-cmd");
        std::fs::create_dir_all(producer.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::create_dir_all(producer.join("commands")).unwrap();
        std::fs::write(
            producer
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"fromcmd","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(producer.join("commands").join("hi.md"), "# hi").unwrap();
        let command = format!("printf '%s\\n' '{}'", producer.display());
        std::fs::write(
            e.market
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("marketplace.json"),
            serde_json::to_string(&serde_json::json!({
                "name": "mymkt",
                "owner": {"name": "me"},
                "plugins": [{
                    "name": "fromcmd",
                    "source": {
                        "source": "command",
                        "command": command
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        (e, command)
    }

    /// Oracle `ht`: a command-source plugin is not run until the command has
    /// been reviewed. `plugin install` without `-y` must refuse.
    #[test]
    fn command_source_install_without_yes_is_not_run() {
        let (e, _) = command_source_env();
        let err = run_install("fromcmd@mymkt", None, &[], &e.plugins, &e.home, &e.cwd)
            .expect_err("must not install an unreviewed command source");
        assert!(err.contains("has not been reviewed yet"), "got: {err}");
        assert!(
            !installed_path(&e.plugins).exists()
                || installed_db(&e)
                    .get("plugins")
                    .and_then(Value::as_object)
                    .map(|plugins| !plugins.contains_key("fromcmd@mymkt"))
                    .unwrap_or(true),
            "unreviewed command source must not be recorded as installed"
        );
    }

    /// CLI `-y` is the terminal consent for that marketplace-declared command
    /// (oracle `jl({yes})` when not nested inside a Claude session).
    #[test]
    fn command_source_install_with_yes_runs_the_command() {
        let (e, _) = command_source_env();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(run_install_with_credential_factory(
            "fromcmd@mymkt",
            None,
            true,
            &[],
            &e.plugins,
            &e.home,
            &e.cwd,
            None,
            || async {
                Err("unexpected credential initialization for non-sensitive plugin".to_string())
            },
        ));
        assert!(result.is_ok(), "install with -y should succeed: {result:?}");
        let db = installed_db(&e);
        assert!(
            db.get("plugins")
                .and_then(Value::as_object)
                .is_some_and(|plugins| plugins.contains_key("fromcmd@mymkt")),
            "installed registry missing fromcmd@mymkt: {db}"
        );
    }
}
