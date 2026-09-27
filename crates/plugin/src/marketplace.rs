//! Marketplace catalog resolution + plugin materialization (Stage 2 install).
//!
//! A marketplace is a git repo whose `.lingxi-plugin/marketplace.json` lists
//! installable plugins. Installing `plugin@marketplace` clones the catalog into
//! `<plugins>/marketplaces/<name>/`, finds the entry, and materializes that
//! plugin's tree into the versioned cache the discovery loader resolves.
//!
//! Field names mirror claude-code's `marketplace.json` verbatim. HTTP-URL
//! catalogs, `installed_plugins.json` persistence, and non-path plugin sources
//! (git/url sub-sources) are follow-up work; this wires the canonical git-repo,
//! path-based-entry path.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The `.lingxi-plugin/marketplace.json` catalog.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketplaceIndex {
    /// Marketplace name.
    pub name: String,
    /// Installable plugins.
    #[serde(default)]
    pub plugins: Vec<MarketplacePluginEntry>,
    /// Optional marketplace-wide metadata bag.
    #[serde(default)]
    pub metadata: Option<MarketplaceIndexMetadata>,
    /// Marketplaces that dependency declarations may explicitly cross into.
    #[serde(rename = "allowCrossMarketplaceDependenciesOn", default)]
    pub allow_cross_marketplace_dependencies_on: Vec<String>,
}

/// Optional marketplace-wide metadata accepted by marketplace catalogs.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MarketplaceIndexMetadata {
    /// Plugin root prepended to safe bare-string entry sources.
    #[serde(default, alias = "pluginRoot")]
    pub plugin_root: Option<String>,
}

/// Typed source of one plugin catalog entry.
///
/// Deserialization is deliberately LENIENT at the `Structured` arm: the oracle
/// union `ft` transforms each `plugins[]` entry independently and rewrites an
/// unparseable `source` to `{source:"unsupported"}` rather than failing the
/// whole catalog (`detectDelistedPlugins` must not read a schema casualty as a
/// removal). A hard `#[serde(untagged)]`/tagged-enum derive here would instead
/// fail the ENTIRE `Vec<MarketplacePluginEntry>` on one bad entry — seeing the
/// manual `Deserialize` impl below.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MarketplacePluginSource {
    /// A path relative to the marketplace root.
    Relative(String),
    /// A structured external or directory source.
    Structured(MarketplaceExternalSource),
}

impl<'de> Deserialize<'de> for MarketplacePluginSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let serde_json::Value::String(s) = &value {
            return Ok(MarketplacePluginSource::Relative(s.clone()));
        }
        let structured =
            serde_json::from_value::<MarketplaceExternalSource>(value).unwrap_or_else(|error| {
                MarketplaceExternalSource::Unsupported {
                    error: Some(error.to_string()),
                }
            });
        Ok(MarketplacePluginSource::Structured(structured))
    }
}

/// Structured source variants accepted by marketplace catalogs.
///
/// This models the **plugin-entry** union (`plugins[].source`, oracle `ft`),
/// which is a DIFFERENT union from the marketplace-registration source
/// (`extraKnownMarketplaces`/`known_marketplaces.json`, oracle `dYe`; see
/// `apps/cli/src/commands/plugin_marketplace.rs`). Conflating those two was a
/// prior round's biggest error: oracle `ft` has NO `file`/`directory` arm at
/// all (those are registration-only) and its `github`/`url` arms carry `sha`,
/// not `path` beyond what's modeled here. `File`/`Directory`/the `path` on
/// `Github`/`Git` are a port-only superset kept for backward compatibility
/// with existing local-only entries; a real oracle-authored `marketplace.json`
/// never emits them, so accepting them is permissive, not incorrect.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum MarketplaceExternalSource {
    /// GitHub repository source.
    Github {
        repo: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        path: Option<String>,
        /// Pin the checkout to this exact commit (40-hex); the checkout is
        /// refused if HEAD does not match. Port-only superset field `path`
        /// aside, this mirrors oracle `github`'s `sha`.
        #[serde(default)]
        sha: Option<String>,
    },
    /// Arbitrary git repository source (port-only superset — the oracle's
    /// `ft` union has no plain `git` tag; use `url` for a git-repo source).
    Git {
        url: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// A git repository checked out at `ref`/`sha` — oracle: *"Full git
    /// repository URL (https:// or git@)"*. NOT an archive download; that is
    /// the separate `archive` arm below (a prior port version conflated the
    /// two under this same `url` tag and downloaded-and-unpacked an archive
    /// here, silently mis-handling every real oracle `source:"url"` entry).
    Url {
        url: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// A subdirectory of a larger repository (monorepo). The oracle partial-
    /// clones (`--filter=tree:0`) to fetch only that subtree; this port does a
    /// full clone and then confines to `path` (same result, more bandwidth).
    #[serde(rename = "git-subdir")]
    GitSubdir {
        url: String,
        path: String,
        #[serde(rename = "ref", default)]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// A zip archive fetched over HTTPS — the plugin root may be at the
    /// archive's top level or nested one directory deep.
    Archive {
        url: String,
        /// SHA-256 digest (64-hex); when set every download is verified
        /// against it and the install is refused on mismatch.
        #[serde(default)]
        sha256: Option<String>,
    },
    /// npm package source.
    Npm {
        package: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        registry: Option<String>,
    },
    /// Run a shell command that prints exactly one absolute plugin directory
    /// (oracle plugin command source). `mode:"link"` uses that directory in
    /// place; omitted/`copy` copies it into the plugin cache.
    Command {
        /// Shell command whose stdout is a single absolute plugin path.
        command: String,
        /// Timeout in seconds (oracle default 60 when unset).
        #[serde(default)]
        timeout: Option<u64>,
        /// How the printed directory is consumed.
        #[serde(default)]
        mode: Option<MarketplaceCommandMode>,
    },
    /// File source (port-only superset; see the enum doc comment).
    File { path: String },
    /// Directory source (port-only superset; see the enum doc comment).
    Directory { path: String },
    /// Parse-time placeholder for a source type this port does not recognize,
    /// or a known type whose fields failed validation. Never authored by
    /// hand — `MarketplacePluginSource::deserialize` rewrites an unparseable
    /// `source` to this so the entry stays listed (delisting detection must
    /// not read a schema casualty as a removal). Install attempts on an
    /// `unsupported` source fail with an actionable message.
    Unsupported {
        #[serde(default)]
        error: Option<String>,
    },
}

/// How a marketplace `source:"command"` result is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketplaceCommandMode {
    /// Copy the command's output directory into the plugin cache.
    Copy,
    /// Use the command's output directory in place (linked, not copied).
    /// Unsupported on Windows (oracle `c6e`).
    Link,
}

/// One plugin entry in a marketplace catalog.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketplacePluginEntry {
    /// Plugin name (the `<plugin>` in `plugin@marketplace`).
    pub name: String,
    /// Declared version (informational; the real version comes from the
    /// plugin's own `plugin.json` at materialize time).
    #[serde(default)]
    pub version: Option<String>,
    /// Plugin root within the marketplace repo (a.k.a. `pluginRoot`). `None` ⇒
    /// the plugin lives at the repo root.
    #[serde(default, alias = "pluginRoot")]
    pub path: Option<String>,
    /// Canonical source; a relative string remains inside the marketplace.
    #[serde(default)]
    pub source: Option<MarketplacePluginSource>,
    /// A plugin hosted OUTSIDE the marketplace repo (git/url sub-source). Not
    /// yet materialized (follow-up); flagged so the arm can error clearly.
    #[serde(default)]
    pub external: bool,
    /// Dependencies contributed by the marketplace entry.
    #[serde(default)]
    pub dependencies: Option<serde_json::Value>,
}

/// Errors resolving / materializing from a marketplace (the string is the
/// byte-faithful `marketplace:`-error detail).
pub type MarketplaceError = String;

/// Resolves marketplace catalogs and materializes plugins from them.
pub struct MarketplaceManager {
    /// The plugins root (`~/.lingxi/plugins`).
    install_dir: PathBuf,
}

impl MarketplaceManager {
    /// Construct over the plugins root directory.
    #[must_use]
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }

    /// Clone (or refresh) the marketplace git repo at `url` into
    /// `marketplaces/<name>/` and parse its `.lingxi-plugin/marketplace.json`.
    /// Returns the parsed index + the clone directory.
    pub async fn resolve_index_via_git(
        &self,
        url: &str,
        name: &str,
    ) -> Result<(MarketplaceIndex, PathBuf), MarketplaceError> {
        self.resolve_index_via_git_ref(url, name, None).await
    }

    /// Clone a marketplace at an optional branch or tag.
    pub async fn resolve_index_via_git_ref(
        &self,
        url: &str,
        name: &str,
        git_ref: Option<&str>,
    ) -> Result<(MarketplaceIndex, PathBuf), MarketplaceError> {
        let clone_dir = self
            .install_dir
            .join("marketplaces")
            .join(crate::discovery::sanitize_segment(name, false));
        if clone_dir.exists() {
            tokio::fs::remove_dir_all(&clone_dir).await.ok();
        }
        if let Some(parent) = clone_dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to clone marketplace repository: {e}"))?;
        }
        let (u, r, cd) = (
            url.to_string(),
            git_ref.unwrap_or_default().to_string(),
            clone_dir.clone(),
        );
        tokio::task::spawn_blocking(move || crate::git::clone_plugin_git(&u, &r, &cd))
            .await
            .map_err(|e| format!("Failed to clone marketplace repository: {e}"))?
            .map_err(|e| format!("Failed to clone marketplace repository: {e}"))?;

        let index_path = clone_dir
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("marketplace.json");
        let raw = tokio::fs::read_to_string(&index_path)
            .await
            .map_err(|_| format!("Marketplace file not found at {}", index_path.display()))?;
        let index: MarketplaceIndex = serde_json::from_str(&raw)
            .map(Self::normalize_index_plugin_root)
            .map_err(|e| format!("Invalid marketplace schema: {e}"))?;
        Ok((index, clone_dir))
    }

    fn normalize_index_plugin_root(mut index: MarketplaceIndex) -> MarketplaceIndex {
        let Some(plugin_root) = index
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.plugin_root.as_deref())
            .map(str::trim)
            .filter(|root| !root.is_empty())
        else {
            return index;
        };

        for entry in &mut index.plugins {
            let Some(MarketplacePluginSource::Relative(source)) = entry.source.as_mut() else {
                continue;
            };
            if !is_safe_bare_relative_source(source) {
                continue;
            }
            *source = normalized_source_with_root(plugin_root, source);
        }
        index
    }

    /// Resolve a catalog entry's plugin directory WITHIN the marketplace clone,
    /// with a path-safety guard: an attacker-controlled `path`/`pluginRoot`
    /// (e.g. `../../x`) that would escape the clone is rejected.
    pub fn plugin_dir_in_clone(
        clone_dir: &Path,
        entry: &MarketplacePluginEntry,
    ) -> Result<PathBuf, MarketplaceError> {
        if entry.external {
            return Err(format!(
                "Plugin '{}' is hosted outside the marketplace repo (external sources not yet supported)",
                entry.name
            ));
        }
        let source_path = match entry.source.as_ref() {
            Some(MarketplacePluginSource::Relative(path)) => Some(path.as_str()),
            Some(MarketplacePluginSource::Structured(
                MarketplaceExternalSource::Directory { path }
                | MarketplaceExternalSource::File { path },
            )) => Some(path.as_str()),
            Some(MarketplacePluginSource::Structured(MarketplaceExternalSource::Unsupported {
                error,
            })) => {
                return Err(format!(
                    "This plugin's marketplace entry is invalid: '{}'{}",
                    entry.name,
                    error
                        .as_deref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ));
            }
            Some(MarketplacePluginSource::Structured(_)) => {
                return Err(format!(
                    "Plugin '{}' is hosted outside the marketplace repo",
                    entry.name
                ));
            }
            None => None,
        };
        let rel = PathBuf::from(
            source_path
                .or(entry.path.as_deref())
                .filter(|s| !s.is_empty())
                .unwrap_or("."),
        );
        // A catalog must point only inside its own repo — reject traversal / root.
        if rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(format!(
                "Marketplace name '{}' resolves to a path outside the cache directory",
                entry.name
            ));
        }
        Ok(clone_dir.join(rel))
    }
}

fn is_safe_bare_relative_source(source: &str) -> bool {
    if source.is_empty() || source.starts_with('.') {
        return false;
    }
    !Path::new(source).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn normalized_source_with_root(plugin_root: &str, source: &str) -> String {
    let mut rel = PathBuf::from(".");
    if plugin_root != "." {
        rel.push(plugin_root);
    }
    rel.push(source);
    rel.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_metadata_plugin_root_rewrites_safe_bare_source() {
        let index: MarketplaceIndex = serde_json::from_value(serde_json::json!({
            "name": "demo-market",
            "metadata": { "pluginRoot": "plugins/demo" },
            "plugins": [{ "name": "demo", "source": "bundle" }]
        }))
        .map(MarketplaceManager::normalize_index_plugin_root)
        .expect("parse index");

        let dir =
            MarketplaceManager::plugin_dir_in_clone(Path::new("/tmp/clone"), &index.plugins[0])
                .expect("resolve inside clone");
        assert_eq!(dir, Path::new("/tmp/clone/./plugins/demo/bundle"));
    }

    #[test]
    fn top_level_plugin_root_stays_compatible() {
        let entry: MarketplacePluginEntry = serde_json::from_value(serde_json::json!({
            "name": "demo",
            "pluginRoot": "plugins/demo"
        }))
        .expect("parse entry");

        let dir = MarketplaceManager::plugin_dir_in_clone(Path::new("/tmp/clone"), &entry)
            .expect("resolve legacy plugin root");
        assert_eq!(dir, Path::new("/tmp/clone/plugins/demo"));
    }

    #[test]
    fn index_metadata_plugin_root_does_not_rewrite_explicit_dot_path() {
        let index: MarketplaceIndex = serde_json::from_value(serde_json::json!({
            "name": "demo-market",
            "metadata": { "pluginRoot": "plugins/demo" },
            "plugins": [{ "name": "demo", "source": "./bundle" }]
        }))
        .map(MarketplaceManager::normalize_index_plugin_root)
        .expect("parse index");

        match index.plugins[0].source.as_ref() {
            Some(MarketplacePluginSource::Relative(source)) => assert_eq!(source, "./bundle"),
            other => panic!("expected relative source, got {other:?}"),
        }
    }

    #[test]
    fn index_metadata_plugin_root_does_not_mask_escape() {
        let index: MarketplaceIndex = serde_json::from_value(serde_json::json!({
            "name": "demo-market",
            "metadata": { "pluginRoot": "plugins/demo" },
            "plugins": [{ "name": "demo", "source": "../secret" }]
        }))
        .map(MarketplaceManager::normalize_index_plugin_root)
        .expect("parse index");

        let err =
            MarketplaceManager::plugin_dir_in_clone(Path::new("/tmp/clone"), &index.plugins[0])
                .expect_err("unsafe source must still be rejected");
        assert!(err.contains("outside the cache directory"), "got: {err}");
    }

    /// The oracle: one entry with an unrecognized/invalid `source` becomes an
    /// `unsupported` placeholder — it must NOT fail the whole catalog parse
    /// (`Vec<MarketplacePluginEntry>` is not allowed to go strict on one bad
    /// element; `detectDelistedPlugins` must not read this as a removal).
    #[test]
    fn unknown_plugin_entry_source_becomes_unsupported_placeholder_not_a_parse_failure() {
        let index: MarketplaceIndex = serde_json::from_value(serde_json::json!({
            "name": "demo-market",
            "plugins": [
                { "name": "good", "source": "./bundle" },
                { "name": "bad", "source": { "source": "totally-unknown-type", "foo": "bar" } }
            ]
        }))
        .expect("the whole index must still parse despite one bad entry");

        assert_eq!(index.plugins.len(), 2, "the bad entry must stay listed");
        match index.plugins[1].source.as_ref() {
            Some(MarketplacePluginSource::Structured(MarketplaceExternalSource::Unsupported {
                error,
            })) => {
                assert!(
                    error.is_some(),
                    "the placeholder should carry the parse reason"
                );
            }
            other => panic!("expected an Unsupported placeholder, got {other:?}"),
        }
    }

    /// Oracle `ft`: `source:"url"` on a plugin entry names a GIT REPOSITORY
    /// ("Full git repository URL (https:// or git@)"), carrying `ref`/`sha` —
    /// NOT the same shape as the separate `archive` (HTTPS zip + `sha256`)
    /// arm. A prior port version conflated these under the same `url` tag.
    #[test]
    fn url_source_is_a_git_repo_shape_distinct_from_archive() {
        let url_source: MarketplaceExternalSource = serde_json::from_value(serde_json::json!({
            "source": "url",
            "url": "https://example.test/repo.git",
            "ref": "v1.0.0",
            "sha": "a".repeat(40)
        }))
        .expect("parse url source");
        match url_source {
            MarketplaceExternalSource::Url { url, git_ref, sha } => {
                assert_eq!(url, "https://example.test/repo.git");
                assert_eq!(git_ref.as_deref(), Some("v1.0.0"));
                assert_eq!(sha.as_deref(), Some("a".repeat(40)).as_deref());
            }
            other => panic!("expected Url, got {other:?}"),
        }

        let archive_source: MarketplaceExternalSource = serde_json::from_value(serde_json::json!({
            "source": "archive",
            "url": "https://example.test/plugin.zip",
            "sha256": "b".repeat(64)
        }))
        .expect("parse archive source");
        match archive_source {
            MarketplaceExternalSource::Archive { url, sha256 } => {
                assert_eq!(url, "https://example.test/plugin.zip");
                assert_eq!(sha256.as_deref(), Some("b".repeat(64)).as_deref());
            }
            other => panic!("expected Archive, got {other:?}"),
        }
    }

    /// Oracle plugin command source: `{source:"command", command, timeout?,
    /// mode?: "link"|"copy"}`. `mode:"link"` uses the command's output
    /// directory in place; omitted mode is copy-into-cache.
    #[test]
    fn command_source_parses_command_timeout_and_link_mode() {
        let source: MarketplaceExternalSource = serde_json::from_value(serde_json::json!({
            "source": "command",
            "command": "printf '%s\\n' /tmp/produced-plugin",
            "timeout": 15,
            "mode": "link"
        }))
        .expect("parse command source");
        match source {
            MarketplaceExternalSource::Command {
                command,
                timeout,
                mode,
            } => {
                assert_eq!(command, "printf '%s\\n' /tmp/produced-plugin");
                assert_eq!(timeout, Some(15));
                assert_eq!(mode, Some(MarketplaceCommandMode::Link));
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn command_source_default_mode_is_copy_when_omitted() {
        let source: MarketplaceExternalSource = serde_json::from_value(serde_json::json!({
            "source": "command",
            "command": "echo /tmp/plugin"
        }))
        .expect("parse command source");
        match source {
            MarketplaceExternalSource::Command {
                command,
                timeout,
                mode,
            } => {
                assert_eq!(command, "echo /tmp/plugin");
                assert_eq!(timeout, None);
                assert_eq!(mode, None);
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }
}
