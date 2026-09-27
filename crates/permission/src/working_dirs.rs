//! The session's additional working directories, with the SOURCE that
//! contributed each one.
//!
//! 1:1 with claude-code's `ToolPermissionContext.additionalWorkingDirectories`,
//! which is a `Map<path, {path, source}>` — keyed by path, carrying the source
//! that added it:
//!
//! ```js
//! kEn(ctx, dirs, source) {                       // addDirectories
//!   let m = new Map(ctx.additionalWorkingDirectories);
//!   for (let d of dirs) m.set(d, {path: d, source});
//!   return {...ctx, additionalWorkingDirectories: m};
//! }
//! rb(ctx)  = new Set([cwd, ...ctx.additionalWorkingDirectories.keys()]);
//! mEt(ctx) = new Set([cwd, ...[...ctx.additionalWorkingDirectories.values()]
//!                              .filter(t => t.source !== "projectSettings")
//!                              .map(t => t.path)]);
//! ```
//!
//! The source is not decoration: [`Self::read_block_paths`] (`mEt`) drops
//! `projectSettings`-sourced entries, so a directory a checked-in settings file
//! adds cannot widen `permissions.blockReadsOutsideWorkingDirectories`. The
//! oracle also partitions on source elsewhere — `Kt(s)` (`cliArg`/`command`/
//! `session`) marks the ephemeral sources that are never persisted back to a
//! settings file, and the background-session snapshot keeps only `session`
//! entries — so the source must live on the entry, not in a side table.

use crate::rule::PermissionRuleSource;
use std::path::{Path, PathBuf};

/// Help copy for a refused network working directory (claude-code `nZ`
/// `networkPath`, minus terminal bold). Shared by `/add-dir`, `--add-dir`,
/// and `permissions.additionalDirectories`.
#[must_use]
pub fn network_working_directory_message(directory_path: &str) -> String {
    format!(
        "{directory_path} is a network path, which cannot be added as a working directory. On Windows, map the share to a drive letter and pass it at launch with --add-dir (a drive letter added mid-session does not yet carry remote-read trust)."
    )
}

/// Lexical network-path detector used before `stat` (claude-code `as` + the
/// non-walk arms of `epe`): UNC except WSL localhost, `/net/<host>` and
/// `/Network/Servers/<host>` automounts whose host does not match `cwd`, and
/// the bare `/net` map root.
#[must_use]
pub fn is_network_working_directory(path: &str) -> bool {
    let cwd = std::env::current_dir().ok();
    is_network_working_directory_against(path, cwd.as_deref().and_then(Path::to_str))
}

/// [`is_network_working_directory`] with an explicit cwd for automount-host
/// matching (tests, and callers that already resolved the session cwd).
#[must_use]
pub fn is_network_working_directory_against(path: &str, cwd: Option<&str>) -> bool {
    if is_unc_except_wsl(path) {
        return true;
    }
    if is_net_map_root(path) {
        return true;
    }
    if let Some(host) = automount_host(path) {
        match cwd.and_then(automount_host) {
            Some(cwd_host) if cwd_host == host => {}
            _ => return true,
        }
    }
    false
}

fn is_double_slash(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('/' | '\\'), Some('/' | '\\'))
    )
}

/// Oracle `N3`: `\\??\` / `//??/` NT namespace prefix.
fn is_nt_namespace(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 4
        && matches!(bytes[0], b'/' | b'\\')
        && bytes[1] == b'?'
        && bytes[2] == b'?'
        && matches!(bytes[3], b'/' | b'\\')
}

/// Oracle `Ji`: `//wsl.localhost/` / `//wsl$/` (any slash style).
fn is_wsl_unc(path: &str) -> bool {
    let rest = path.get(2..).unwrap_or("");
    let lower = rest.to_ascii_lowercase();
    lower.starts_with("wsl.localhost/")
        || lower.starts_with("wsl.localhost\\")
        || lower.starts_with("wsl$/")
        || lower.starts_with("wsl$\\")
}

/// Oracle `as` = `Rn && !Ji`.
fn is_unc_except_wsl(path: &str) -> bool {
    if !(is_double_slash(path) || is_nt_namespace(path)) {
        return false;
    }
    !is_wsl_unc(path)
}

fn posix_components(path: &str) -> Option<Vec<&str>> {
    if !path.starts_with('/') {
        return None;
    }
    let mut out = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            out.pop();
            continue;
        }
        out.push(part);
    }
    Some(out)
}

fn is_automount_prefix(components: &[&str]) -> bool {
    match components {
        [first, _] if first.eq_ignore_ascii_case("net") => true,
        [first, second, _]
            if first.eq_ignore_ascii_case("network") && second.eq_ignore_ascii_case("servers") =>
        {
            true
        }
        _ => false,
    }
}

fn automount_host(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut components = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            components.pop();
            continue;
        }
        components.push(part);
        if is_automount_prefix(&components) {
            return components.last().map(|host| host.to_ascii_lowercase());
        }
    }
    None
}

fn is_net_map_root(path: &str) -> bool {
    posix_components(path).is_some_and(|components| {
        components.len() == 1 && components[0].eq_ignore_ascii_case("net")
    })
}

/// One additional working directory and the source that contributed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingDirectory {
    /// The directory itself (the map KEY in claude-code).
    pub path: PathBuf,
    /// Which settings tier / runtime action added it.
    pub source: PermissionRuleSource,
}

/// Insertion-ordered, path-keyed set of [`WorkingDirectory`] — the port of the
/// `additionalWorkingDirectories` Map. Iteration order is insertion order, which
/// keeps the `${[...dirs].join(", ")}` confinement message deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdditionalWorkingDirs {
    entries: Vec<WorkingDirectory>,
}

impl AdditionalWorkingDirs {
    /// Empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `Map.set(path, {path, source})` — a repeated path keeps its position and
    /// takes the NEW source, exactly like re-setting a JS Map key.
    pub fn insert(&mut self, path: impl Into<PathBuf>, source: PermissionRuleSource) {
        let path = path.into();
        let displayed = path.to_string_lossy();
        if is_network_working_directory(&displayed) {
            tracing::warn!("{}", network_working_directory_message(&displayed));
            return;
        }
        if let Some(existing) = self.entries.iter_mut().find(|entry| entry.path == path) {
            existing.source = source;
            return;
        }
        self.entries.push(WorkingDirectory { path, source });
    }

    /// `kEn(ctx, dirs, source)` — add every directory under one source.
    pub fn extend_from_source<P: Into<PathBuf>>(
        &mut self,
        dirs: impl IntoIterator<Item = P>,
        source: PermissionRuleSource,
    ) {
        for dir in dirs {
            self.insert(dir, source);
        }
    }

    /// Build from `(dirs, source)` groups, applied in order.
    #[must_use]
    pub fn from_sources<P: Into<PathBuf>>(
        groups: impl IntoIterator<Item = (Vec<P>, PermissionRuleSource)>,
    ) -> Self {
        let mut out = Self::new();
        for (dirs, source) in groups {
            out.extend_from_source(dirs, source);
        }
        out
    }

    /// `Map.delete(path)`.
    pub fn remove(&mut self, path: &Path) {
        self.entries.retain(|entry| entry.path != path);
    }

    /// `Map.has(path)`.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.entries.iter().any(|entry| entry.path == path)
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many directories are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &WorkingDirectory> {
        self.entries.iter()
    }

    /// `Map.keys()` — every path regardless of source. This is the set every
    /// ordinary working-dir consumer wants (the `rb` union minus cwd).
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// `mEt(ctx)` minus cwd — the paths the READ BLOCK honours: everything
    /// EXCEPT what `projectSettings` contributed.
    #[must_use]
    pub fn read_block_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.source != PermissionRuleSource::Settings(protocol::SettingsScope::Project)
            })
            .map(|entry| entry.path.clone())
            .collect()
    }
}

impl FromIterator<WorkingDirectory> for AdditionalWorkingDirs {
    fn from_iter<T: IntoIterator<Item = WorkingDirectory>>(iter: T) -> Self {
        let mut out = Self::new();
        for entry in iter {
            out.insert(entry.path, entry.source);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_semantics_key_by_path_last_source_wins() {
        let mut dirs = AdditionalWorkingDirs::new();
        dirs.insert(
            "/a",
            PermissionRuleSource::Settings(protocol::SettingsScope::Project),
        );
        dirs.insert("/b", PermissionRuleSource::CliArg);
        // Re-setting an existing key keeps its POSITION and takes the new source.
        dirs.insert("/a", PermissionRuleSource::Session);
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs.paths(), vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(
            dirs.iter().next().unwrap().source,
            PermissionRuleSource::Session
        );
        dirs.remove(Path::new("/a"));
        assert!(!dirs.contains(Path::new("/a")));
    }

    /// 🚨 `mEt` drops `projectSettings` entries; `rb` keeps everything.
    #[test]
    fn read_block_paths_drop_project_settings_only() {
        let dirs = AdditionalWorkingDirs::from_sources([
            (
                vec!["/from-project"],
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            ),
            (vec!["/from-cli"], PermissionRuleSource::CliArg),
            (
                vec!["/from-local"],
                PermissionRuleSource::Settings(protocol::SettingsScope::Local),
            ),
        ]);
        assert_eq!(dirs.paths().len(), 3);
        assert_eq!(
            dirs.read_block_paths(),
            vec![PathBuf::from("/from-cli"), PathBuf::from("/from-local")]
        );
    }

    #[test]
    fn unc_and_automount_paths_are_network() {
        assert!(is_network_working_directory_against(
            "//fileserver/share",
            Some("/proj")
        ));
        assert!(is_network_working_directory_against(
            r"\\fileserver\share",
            Some("/proj")
        ));
        assert!(is_network_working_directory_against(
            "/net/host/data",
            Some("/proj")
        ));
        assert!(is_network_working_directory_against(
            "/Network/Servers/host/data",
            Some("/proj")
        ));
        assert!(is_network_working_directory_against("/net", Some("/proj")));
        assert!(!is_network_working_directory_against(
            "//wsl.localhost/Ubuntu/home",
            Some("/proj")
        ));
        assert!(!is_network_working_directory_against(
            "/tmp/work",
            Some("/proj")
        ));
        assert!(!is_network_working_directory_against(
            "/net/host/other",
            Some("/net/host/project")
        ));
    }

    #[test]
    fn insert_drops_network_paths() {
        let mut dirs = AdditionalWorkingDirs::new();
        dirs.insert("//fileserver/share", PermissionRuleSource::CliArg);
        dirs.insert("/tmp/ok", PermissionRuleSource::CliArg);
        assert_eq!(dirs.paths(), vec![PathBuf::from("/tmp/ok")]);
    }

    #[test]
    fn network_message_matches_oracle_wording() {
        assert_eq!(
            network_working_directory_message("//fileserver/share"),
            "//fileserver/share is a network path, which cannot be added as a working directory. On Windows, map the share to a drive letter and pass it at launch with --add-dir (a drive letter added mid-session does not yet carry remote-read trust)."
        );
    }
}
