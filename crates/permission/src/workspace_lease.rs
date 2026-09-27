//! Ephemeral, path-bound permission leases used by local-app build workflows.
//!
//! A lease is intentionally separate from the session-wide permission mode:
//! it grants only calls whose resolved paths stay inside one app workspace and
//! disappears when the workflow drops its guard.

use crate::command_path_containment::check_command_path_containment;
use crate::filesystem::{file_tool_kind, input_path_for_tool, FileToolKind, FsRoots};
use crate::path_constraints::check_path_constraints;
use crate::shell_command::{command_from_input, is_shell_tool};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLeaseInfo {
    pub app_id: String,
    pub root: PathBuf,
}

#[derive(Debug)]
struct ActiveLease {
    info: WorkspaceLeaseInfo,
}

/// Shared registry owned by an engine composition root.
#[derive(Debug, Default)]
pub struct WorkspacePermissionLeaseRegistry {
    next: AtomicU64,
    active: RwLock<HashMap<u64, ActiveLease>>,
}

pub struct WorkspacePermissionLease {
    token: u64,
    registry: Arc<WorkspacePermissionLeaseRegistry>,
}

impl WorkspacePermissionLease {
    #[must_use]
    pub fn token(&self) -> u64 {
        self.token
    }
}

impl WorkspacePermissionLeaseRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[cfg(test)]
    pub(crate) fn begin(
        self: &Arc<Self>,
        app_id: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> WorkspacePermissionLease {
        self.begin_normalized(app_id.into(), root.into())
    }

    /// Begin a lease for a production local-app workspace.
    ///
    /// Unlike the permissive helper used by this module's unit tests, this
    /// production entry point requires the canonical
    /// local-app layout (`apps/<app_id>/workspace` or the matching guest
    /// `local-app-<app_id>` mount). Callers that cannot prove that binding must
    /// fail closed instead of silently granting a lease over a generic cwd.
    pub fn begin_local_app(
        self: &Arc<Self>,
        app_id: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> Result<WorkspacePermissionLease, String> {
        let app_id = app_id.into();
        let root = lexical_normalize(root.into());
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if !is_exact_local_app_root(&canonical_root, &app_id) {
            return Err(format!(
                "workspace lease root {} is not the canonical workspace for app {}",
                canonical_root.display(),
                app_id
            ));
        }
        Ok(self.begin_normalized(app_id, canonical_root))
    }

    fn begin_normalized(
        self: &Arc<Self>,
        app_id: String,
        root: PathBuf,
    ) -> WorkspacePermissionLease {
        let token = self.next.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        // Store the canonical root, not the caller's spelling. This makes the
        // lease boundary stable across relative paths and host symlinks.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        self.active
            .write()
            .expect("workspace lease registry poisoned")
            .insert(
                token,
                ActiveLease {
                    info: WorkspaceLeaseInfo { app_id, root },
                },
            );
        WorkspacePermissionLease {
            token,
            registry: Arc::clone(self),
        }
    }

    pub fn active(&self) -> Vec<WorkspaceLeaseInfo> {
        self.active
            .read()
            .expect("workspace lease registry poisoned")
            .values()
            .map(|lease| lease.info.clone())
            .collect()
    }

    /// Returns true only for a file/shell/local-app MCP call contained by an
    /// active lease. Explicit deny/ask rules are evaluated before this hook in
    /// `PermissionPolicy::authorize_inner`, so the lease cannot override them.
    // Kept private for focused unit tests that exercise the registry in
    // isolation. Production authorization must always use the token-aware
    // entry point below; exposing an unscoped helper would make it too easy
    // for a future caller to accidentally authorize against another active
    // app's lease.
    #[cfg(test)]
    fn allows(&self, tool_name: &str, input: &serde_json::Value, roots: &FsRoots) -> bool {
        let active = self
            .active
            .read()
            .expect("workspace lease registry poisoned");
        (active.len() == 1)
            .then(|| active.values().next())
            .flatten()
            .is_some_and(|lease| allows_for_lease(&lease.info, tool_name, input, roots))
    }

    pub fn allows_for_token(
        &self,
        token: Option<u64>,
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        let Some(token) = token else { return false };
        self.active
            .read()
            .expect("workspace lease registry poisoned")
            .get(&token)
            .is_some_and(|lease| allows_for_lease(&lease.info, tool_name, input, roots))
    }

    /// Returns true when a leased workflow is attempting to modify a
    /// host-owned file. This is a hard deny, not merely a failed lease match,
    /// so a later global `auto`/allow rule cannot re-enable the mutation.
    #[cfg(test)]
    fn denies_host_owned(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        let active = self
            .active
            .read()
            .expect("workspace lease registry poisoned");
        (active.len() == 1)
            .then(|| active.values().next())
            .flatten()
            .is_some_and(|lease| host_owned_for_lease(&lease.info, tool_name, input, roots))
    }

    pub fn denies_host_owned_for_token(
        &self,
        token: Option<u64>,
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        let Some(token) = token else { return false };
        self.active
            .read()
            .expect("workspace lease registry poisoned")
            .get(&token)
            .is_some_and(|lease| host_owned_for_lease(&lease.info, tool_name, input, roots))
    }

    /// The source-editing boundary remains protected for every conversation
    /// rooted at a local-app workspace, even after the temporary build lease is
    /// dropped. The generated local settings file is itself an allow rule, so
    /// this check must not depend on a lease token or host-managed files and
    /// non-inspection shell commands would become writable after a build.
    pub fn denies_host_owned_for_workspace(
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        let root = &roots.cwd;
        is_local_app_workspace_root(root) && host_owned_for_root(root, tool_name, input, roots)
    }

    /// Return true when a file operation resolves outside a local-app's
    /// canonical workspace. This is deliberately evaluated before generic
    /// allow rules: `Edit(./**)` is lexical and cannot prove that a symlink
    /// target remains inside the workspace.
    pub fn escapes_local_app_workspace(
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        if !is_local_app_workspace_root(&roots.cwd) {
            return false;
        }
        match file_tool_kind(tool_name) {
            FileToolKind::Editor | FileToolKind::Reader => {
                let lease_roots = FsRoots {
                    cwd: roots.cwd.clone(),
                    home: roots.home.clone(),
                    lingxi_home: roots.lingxi_home.clone(),
                };
                let Some(raw) = input_path_for_tool(tool_name, input, &lease_roots) else {
                    return false;
                };
                let Some(app_id) = local_app_id(&roots.cwd) else {
                    return false;
                };
                let target = resolve_target(&roots.cwd, &app_id, raw.as_ref());
                !is_workspace_path(&roots.cwd, &target)
            }
            FileToolKind::NonFile if is_shell_tool(tool_name) => {
                let Some(command) = command_from_input(input) else {
                    return false;
                };
                let Some(app_id) = local_app_id(&roots.cwd) else {
                    return false;
                };
                let lease_roots = FsRoots {
                    cwd: roots.cwd.clone(),
                    home: roots.home.clone(),
                    lingxi_home: roots.lingxi_home.clone(),
                };
                let command = command_with_guest_workspace_aliases(command, &roots.cwd, &app_id);
                // These guards are intentionally checked even before the
                // generic Bash allow walk. A global `Bash(...)` exact allow
                // must not authorize `cat /etc/passwd`, `cd /tmp`, or a
                // redirect outside this local-app workspace.
                check_path_constraints(&command, &lease_roots, &[], None).is_some()
                    || check_command_path_containment(&command, &lease_roots, &[], None).is_some()
            }
            _ => false,
        }
    }
}

impl Drop for WorkspacePermissionLease {
    fn drop(&mut self) {
        self.registry
            .active
            .write()
            .expect("workspace lease registry poisoned")
            .remove(&self.token);
    }
}

fn allows_for_lease(
    info: &WorkspaceLeaseInfo,
    tool_name: &str,
    input: &serde_json::Value,
    roots: &FsRoots,
) -> bool {
    // A local-app workspace has the stable on-disk shape
    // `<profile>/apps/<app-id>/workspace`. If a caller supplies a different
    // `app_id` for that root, fail closed instead of granting an app-scoped MCP
    // operation for a sibling app. Test/in-memory roots may use a generic
    // directory name, so only enforce the check when the canonical layout is
    // unambiguously the local-app shape.
    if !workspace_root_matches_app_id(&info.root, &info.app_id) {
        return false;
    }
    // The local-app host operations are BUILTIN tools (`LocalApp*`), not an
    // MCP server — see `harness_runtime::mobile::local_apps_tools` for why they moved.
    // The `app_id` equality check below is what keeps a lease for one app from
    // authorizing an operation aimed at a sibling.
    if tool_name.starts_with("LocalApp") {
        let allowed = matches!(
            tool_name,
            "LocalAppBuild"
                | "LocalAppLogs"
                | "LocalAppRuntime"
                | "LocalAppManifest"
                | "LocalAppQueryData"
        );
        return allowed
            && input.get("app_id").and_then(serde_json::Value::as_str)
                == Some(info.app_id.as_str());
    }

    match file_tool_kind(tool_name) {
        FileToolKind::Editor | FileToolKind::Reader => {
            let lease_roots = FsRoots {
                cwd: info.root.clone(),
                home: roots.home.clone(),
                lingxi_home: roots.lingxi_home.clone(),
            };
            let Some(raw) = input_path_for_tool(tool_name, input, &lease_roots) else {
                return false;
            };
            let target = resolve_target(&info.root, &info.app_id, raw.as_ref());
            is_workspace_path(&info.root, &target)
                && (file_tool_kind(tool_name) == FileToolKind::Reader
                    || !is_host_owned_path_or_container(&info.root, &target))
        }
        FileToolKind::NonFile if is_shell_tool(tool_name) => {
            let Some(command) = command_from_input(input) else {
                return false;
            };
            if !workspace_shell_is_safe(command) {
                // In particular, keep npm/node/networking commands on the
                // normal Shell approval path. The lease is a filesystem
                // boundary, not a network or arbitrary-code capability.
                return false;
            }
            let lease_roots = FsRoots {
                cwd: info.root.clone(),
                home: roots.home.clone(),
                lingxi_home: roots.lingxi_home.clone(),
            };
            let command = command_with_guest_workspace_aliases(command, &info.root, &info.app_id);
            check_path_constraints(&command, &lease_roots, &[], None).is_none()
                && check_command_path_containment(&command, &lease_roots, &[], None).is_none()
        }
        _ => false,
    }
}

fn host_owned_for_lease(
    info: &WorkspaceLeaseInfo,
    tool_name: &str,
    input: &serde_json::Value,
    roots: &FsRoots,
) -> bool {
    if !workspace_root_matches_app_id(&info.root, &info.app_id) {
        return false;
    }
    host_owned_for_root(&info.root, tool_name, input, roots)
}

fn host_owned_for_root(
    root: &Path,
    tool_name: &str,
    input: &serde_json::Value,
    roots: &FsRoots,
) -> bool {
    let lease_roots = FsRoots {
        cwd: root.to_path_buf(),
        home: roots.home.clone(),
        lingxi_home: roots.lingxi_home.clone(),
    };
    let app_id = local_app_id(root);
    match file_tool_kind(tool_name) {
        // Reader access is intentionally not blocked; the lease only protects
        // host-owned files from model mutation.
        FileToolKind::Editor => input_path_for_tool(tool_name, input, &lease_roots)
            .map(|raw| match app_id.as_deref() {
                Some(app_id) => resolve_target(root, app_id, raw.as_ref()),
                None => resolve_target_from_root(root, raw.as_ref()),
            })
            .is_some_and(|target| is_host_owned_path_or_container(root, &target)),
        FileToolKind::NonFile if is_shell_tool(tool_name) => {
            let Some(command) = command_from_input(input) else {
                return true;
            };
            let command = app_id
                .as_deref()
                .map(|app_id| command_with_guest_workspace_aliases(command, root, app_id))
                .unwrap_or_else(|| command.to_string());
            let redirects = crate::path_constraints::write_redirect_targets(&command, &lease_roots);
            // Local-app source mutation goes through structured file tools.
            // Shell redirects are intentionally never lease-authorized: shell
            // expansion makes their final target impossible to prove here
            // (`TARGET=package.json; echo x > "$TARGET"`, braces, globs, ...).
            if !redirects.is_empty() {
                return true;
            }
            // Do not try to enumerate every possible mutator (`env rm`,
            // interpreters, package-manager scripts, and future commands all
            // make that list incomplete). A local-app shell is an inspection
            // surface only; source mutation uses structured file tools whose
            // final target can be checked without evaluating shell expansion.
            !workspace_shell_is_safe(&command)
        }
        _ => false,
    }
}

fn is_local_app_workspace_root(root: &Path) -> bool {
    local_app_id(root).is_some()
}

/// The local-app id a workspace root belongs to, if any.
///
/// Public so the local-app BUILTIN tools can bind `app_id` to the session's
/// own app the way the dynamic MCP tools bind it to their namespace — without
/// it, an auto-allowed read reaches a sibling app.
#[must_use]
pub fn local_app_id_for_root(root: &Path) -> Option<String> {
    if let Some(id) = local_app_id(root) {
        return Some(id);
    }
    // GUEST spelling `/workspace/<app_id>`. The bare-uuid form is what the
    // device actually reports as `model_cwd`, and `guest_workspace_relative`
    // in this same file already accepts it — two derivations of one identity
    // is how the guest/host split forked in the first place.
    let mut components = root.components().filter_map(|component| match component {
        Component::Normal(name) => name.to_str(),
        _ => None,
    });
    match (components.next(), components.next(), components.next()) {
        (Some("workspace"), Some(app_id), None) if !app_id.is_empty() => Some(app_id.to_owned()),
        _ => None,
    }
}

fn local_app_id(root: &Path) -> Option<String> {
    if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
        if let Some(app_id) = name.strip_prefix("local-app-") {
            return (!app_id.is_empty()).then(|| app_id.to_owned());
        }
    }
    let app_root = root.parent()?;
    let apps_root = app_root.parent()?;
    if root.file_name().and_then(|name| name.to_str()) != Some("workspace")
        || apps_root.file_name().and_then(|name| name.to_str()) != Some("apps")
    {
        return None;
    }
    app_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn resolve_target_from_root(root: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        lexical_normalize(path.to_path_buf())
    } else {
        lexical_normalize(root.join(path))
    }
}

fn workspace_root_matches_app_id(root: &Path, app_id: &str) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if let Some(mounted_id) = name.strip_prefix("local-app-") {
        return mounted_id == app_id;
    }
    if name != "workspace" {
        return true;
    }
    let Some(app_root) = root.parent() else {
        return true;
    };
    let Some(apps_root) = app_root.parent() else {
        return true;
    };
    if apps_root.file_name().and_then(|name| name.to_str()) != Some("apps") {
        return true;
    }
    app_root.file_name().and_then(|name| name.to_str()) == Some(app_id)
}

fn is_exact_local_app_root(root: &Path, app_id: &str) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name == format!("local-app-{app_id}") {
        return true;
    }
    if name != "workspace" {
        return false;
    }
    let Some(app_root) = root.parent() else {
        return false;
    };
    let Some(apps_root) = app_root.parent() else {
        return false;
    };
    apps_root.file_name().and_then(|name| name.to_str()) == Some("apps")
        && app_root.file_name().and_then(|name| name.to_str()) == Some(app_id)
}

fn workspace_shell_is_safe(command: &str) -> bool {
    // Lease-authorized shell commands are read-only inspection operations.
    // Source mutation uses structured file tools, whose paths can be checked
    // without shell expansion. Interpreters, package managers, VCS network
    // commands, and unknown binaries remain subject to normal Shell policy.
    const LOCAL_INSPECTION_COMMANDS: &[&str] = &[
        "cat", "cd", "cut", "diff", "false", "find", "grep", "head", "ls", "pwd", "rg", "sort",
        "tail", "tr", "true", "wc",
    ];
    // Use the permission crate's single, quote-aware definition of read-only
    // shell behavior. It rejects redirects, expansion, executable actions,
    // and side-effecting `find` flags such as `-fprint`/`-fprintf`. The second
    // gate below deliberately narrows that general classifier to commands
    // useful for inspecting one local-app workspace (no git/gh/docker/etc.).
    if !crate::read_only_command::command_is_read_only(command) {
        return false;
    }
    let commands = crate::shell_command::split_command(command);
    !commands.is_empty()
        && commands.iter().all(|subcommand| {
            let tokens: Vec<&str> = subcommand.split_whitespace().collect();
            let mut index = 0;
            while index < tokens.len()
                && tokens[index].contains('=')
                && !tokens[index].starts_with('-')
            {
                index += 1;
            }
            let token = tokens.get(index).copied().unwrap_or_default();
            if token.contains('/') || token.contains('\\') {
                return false;
            }
            let basename = Path::new(token)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(token);
            LOCAL_INSPECTION_COMMANDS.contains(&basename)
        })
}

fn resolve_target(root: &Path, app_id: &str, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        if let Some(relative) = guest_workspace_relative(app_id, path) {
            lexical_normalize(root.join(relative))
        } else {
            lexical_normalize(path.to_path_buf())
        }
    } else {
        lexical_normalize(root.join(path))
    }
}

fn guest_workspace_relative<'a>(app_id: &str, path: &'a Path) -> Option<&'a Path> {
    [
        format!("/workspace/local-app-{app_id}"),
        format!("/workspace/{app_id}"),
    ]
    .iter()
    .find_map(|prefix| {
        let prefix = Path::new(prefix);
        if path == prefix {
            Some(Path::new(""))
        } else {
            path.strip_prefix(prefix).ok()
        }
    })
}

fn command_with_guest_workspace_aliases(command: &str, root: &Path, app_id: &str) -> String {
    // Static command containment works in host coordinates. Mobile Linux
    // prompts use `/workspace/<id>` (and older local-app builds used
    // `/workspace/local-app-<id>`), so translate only those exact app-bound
    // aliases before checking cwd, redirects, and positional paths. If the
    // host root contains whitespace, leave the command untouched rather than
    // introducing shell quoting into a security check; it will conservatively
    // remain on the normal approval path.
    let root = root.to_string_lossy();
    if root.chars().any(char::is_whitespace) {
        return command.to_string();
    }
    command
        .replace(&format!("/workspace/local-app-{app_id}"), root.as_ref())
        .replace(&format!("/workspace/{app_id}"), root.as_ref())
}

fn is_workspace_path(root: &Path, target: &Path) -> bool {
    // Canonicalize the deepest existing ancestor and append the non-existing
    // tail. This catches both existing symlink escapes and writes through a
    // symlink whose destination file has not been created yet. A dangling
    // symlink itself is rejected conservatively because its destination cannot
    // be proven to stay inside the lease. Do this before a lexical
    // `starts_with` check: macOS/iOS may spell the same root as `/var` or
    // `/private/var`, and both spellings should resolve to the canonical lease.
    let Some(resolved) = resolve_canonical_target(target) else {
        return false;
    };
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !resolved.starts_with(canonical_root) {
        return false;
    }
    true
}

fn is_host_owned_path_or_container(root: &Path, target: &Path) -> bool {
    let Some(resolved) = resolve_canonical_target(target) else {
        return false;
    };
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Ok(relative) = resolved.strip_prefix(&canonical_root) else {
        return false;
    };
    // Replacing the workspace root or the `lib/` directory would also replace
    // host-managed descendants such as the bridge and platform adapter.
    relative.as_os_str().is_empty() || relative == Path::new("lib") || host_owned_relative(relative)
}

fn host_owned_relative(relative: &Path) -> bool {
    if relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name == ".lingxi" || name == ".lingxi-build-state"
        )
    }) {
        return true;
    }
    // Any BUILD-CONFIG file the toolchain AUTO-DISCOVERS and then executes as
    // Node code. Matched by shape rather than an exact list because the search
    // space belongs to the tool, not to us: Vite walks `DEFAULT_CONFIG_FILES`
    // and PostCSS walks lilconfig's `getDefaultSearchPlaces`, and both have
    // gained spellings across versions — an exact list rots into a hole on the
    // next bump.
    //
    // Bounded on BOTH axes, or it would swallow ordinary source:
    //   - LOCATION: only the workspace root and `.config/`, the only two
    //     directories either tool searches. `src/vite.config.helper.js` is
    //     app-owned and stays writable.
    //   - EXTENSION: only executable/config extensions, so a file merely
    //     BEGINNING with `vite.config.` is not swallowed either.
    {
        let mut components = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            });
        let (dir, file) = match (components.next(), components.next(), components.next()) {
            (Some(file), None, _) => (None, Some(file)),
            (Some(dir), Some(file), None) => (Some(dir), Some(file)),
            _ => (None, None),
        };
        if matches!(dir, None | Some(".config")) {
            if let Some(name) = file {
                let stem = name.strip_prefix('.').unwrap_or(name);
                const CONFIG_EXTENSIONS: &[&str] = &[
                    "js", "cjs", "mjs", "ts", "cts", "mts", "json", "yaml", "yml",
                ];
                let is_config_of = |tool: &str| {
                    let rc = format!("{tool}rc");
                    if stem == rc {
                        return true;
                    }
                    for prefix in [format!("{rc}."), format!("{tool}.config.")] {
                        if let Some(ext) = stem.strip_prefix(&prefix) {
                            if CONFIG_EXTENSIONS.contains(&ext) {
                                return true;
                            }
                        }
                    }
                    false
                };
                if is_config_of("vite") || is_config_of("postcss") || is_config_of("tailwind") {
                    return true;
                }
            }
        }
    }
    relative.file_name().is_some_and(|name| name == "LINGXI.md")
        || relative.starts_with("node_modules")
        || matches!(
            relative.to_str(),
            Some(
                "index.html"
                    // EVERY Vite config spelling, not just the one the template
                    // ships. Vite self-resolves from `DEFAULT_CONFIG_FILES` and
                    // `vite.config.js` sorts FIRST, ahead of the `.mjs` the host
                    // writes, so creating any other spelling shadows the host
                    // config — and a Vite config is executed Node code. `Edit`
                    // creates files on a nonexistent path, so the workspace's
                    // own `Edit(./**)` grant reaches this with no new permission.
                    | "vite.config.js"
                    | "vite.config.mjs"
                    | "vite.config.ts"
                    | "vite.config.cjs"
                    | "vite.config.mts"
                    | "vite.config.cts"
                    | "package.json"
                    | "pnpm-lock.yaml"
                    // Declared host-managed by the generated LINGXI.md. These
                    // were enforced nowhere, so `Edit(./**)` overwrote them and
                    // `restore_host_managed_files` reverted the edit with only
                    // a `tracing::warn!` — the model loops against a file it
                    // cannot change and is never told why.
                    | ".gitignore"
                    | "jsconfig.json"
                    | "lib/lingxi-provider.jsx"
                    | "lib/frame-loop.js"
                    | "lib/phaser-runtime.js"
                    | "lib/babylon-runtime.js"
                    | "styles/foundation.css"
                    // Copied into the install staging tree by
                    // `prepare_dependency_staging`, and the install runs with
                    // the network ENABLED.
                    | "pnpm-workspace.yaml"
                    | "lib/device-context.js"
                    | "lib/lingxi-bridge.js"
                    | "lib/platform-adapter.js"
            )
        )
}

fn resolve_canonical_target(target: &Path) -> Option<PathBuf> {
    let mut cursor = target.to_path_buf();
    let mut tail = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&cursor) {
            return Some(
                tail.iter()
                    .rev()
                    .fold(canonical, |path, part| path.join(part)),
            );
        }
        if std::fs::symlink_metadata(&cursor).is_ok() {
            return None;
        }
        let name = cursor.file_name()?.to_os_string();
        tail.push(name);
        if !cursor.pop() {
            return None;
        }
    }
}

fn lexical_normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn roots(root: &Path) -> FsRoots {
        FsRoots {
            cwd: root.to_path_buf(),
            home: None,
            lingxi_home: root.to_path_buf(),
        }
    }

    #[test]
    fn lease_allows_workspace_file_and_rejects_outside() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app", root.clone());
        let fs = roots(&root);
        assert!(registry.allows(
            "Write",
            &serde_json::json!({"file_path":"src/App.jsx"}),
            &fs
        ));
        assert!(!registry.allows("Write", &serde_json::json!({"file_path":"../secret"}), &fs));
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":".lingxi/source-policy.json"}),
            &fs
        ));
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":"lib/lingxi-bridge.js"}),
            &fs
        ));
        assert!(!registry.allows("Write", &serde_json::json!({"file_path":"LINGXI.md"}), &fs));
        for path in [
            ".",
            "lib",
            "index.html",
            "vite.config.mjs",
            "package.json",
            "pnpm-lock.yaml",
            "lib/device-context.js",
            "lib/platform-adapter.js",
            "node_modules/vite/bin/vite.js",
        ] {
            assert!(
                !registry.allows("Write", &serde_json::json!({"file_path": path}), &fs),
                "host-managed build infrastructure must not be writable: {path}"
            );
        }
        assert!(registry.allows(
            "Write",
            &serde_json::json!({"file_path":"app/main.jsx"}),
            &fs
        ));
    }

    #[test]
    fn host_owned_metadata_is_readable_but_never_writable() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(root.join(".lingxi")).unwrap();
        std::fs::write(root.join(".lingxi/settings.local.json"), b"{}\n").unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app", root.clone());
        let fs = roots(&root);
        let settings = serde_json::json!({"file_path":".lingxi/settings.local.json"});

        assert!(registry.allows("Read", &settings, &fs));
        assert!(!registry.allows("Write", &settings, &fs));
        assert!(registry.denies_host_owned("Write", &settings, &fs));
        assert!(!registry.denies_host_owned("Read", &settings, &fs));
        assert!(registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command":"echo x > .lingxi/settings.local.json"}),
            &fs
        ));
        assert!(registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command":"rm -rf .lingxi"}),
            &fs
        ));
        for command in ["rm -rf .", "rm -rf lib", "mv lib lib.bak", "rm -rf *"] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "destructive parent or glob must not bypass host ownership: {command}"
            );
        }
        assert!(registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command":"printf x > package.json"}),
            &fs
        ));
        for command in ["rm -rf app/old.jsx", "mkdir components"] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "shell mutation must use structured file tools: {command}"
            );
        }
        for command in [
            "TARGET=. rm -rf \"$TARGET\"",
            "env TARGET=. rm -rf \"$TARGET\"",
            "rm -rf {package.json,app}",
            "TARGET=package.json; echo x > \"$TARGET\"",
            "python3 -c 'open(\"package.json\", \"w\").write(\"{}\")'",
            "npm install",
            "find . -fprint package.json",
            "find . -fprintf package.json x",
            "find . -files0-from list",
        ] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "shell expansion must not bypass the structured mutation boundary: {command}"
            );
        }
    }

    #[test]
    fn drop_revokes_lease() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let fs = roots(&root);
        let lease = registry.begin("app", root);
        assert!(!registry.active().is_empty());
        drop(lease);
        assert!(registry.active().is_empty());
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":"src/App.jsx"}),
            &fs
        ));
    }

    #[test]
    fn shell_lease_preserves_network_and_interpreter_approval() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"echo ok > out.txt"}),
            &fs
        ));
        assert!(registry.allows(
            "Bash",
            &serde_json::json!({"command":"cat src/App.jsx"}),
            &fs
        ));
        assert!(!registry.allows("Bash", &serde_json::json!({"command":"npm install"}), &fs));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"curl https://example.com"}),
            &fs
        ));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"node -e 'process.exit(0)'"}),
            &fs
        ));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"echo $(curl https://example.com)"}),
            &fs
        ));
        assert!(!registry.allows(
            "Bash",
            &serde_json::json!({"command":"echo `curl https://example.com`"}),
            &fs
        ));
        for command in [
            "find . -fprint out.txt",
            "find . -fprintf out.txt x",
            "find . -files0-from list",
        ] {
            assert!(
                !registry.allows("Bash", &serde_json::json!({"command": command}), &fs),
                "side-effecting or path-indirect find must not be lease-authorized: {command}"
            );
        }
    }

    #[test]
    fn production_lease_translates_only_the_bound_guest_workspace_alias() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease = registry.begin_local_app("app-a", &root).unwrap();
        let fs = roots(&root);

        assert!(registry.allows_for_token(
            Some(lease.token()),
            "Write",
            &serde_json::json!({"file_path":"/workspace/local-app-app-a/src/App.jsx"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({
                "command":"echo ok > /workspace/local-app-app-a/src/out.txt"
            }),
            &fs
        ));
        assert!(registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({
                "command":"cat /workspace/local-app-app-a/src/out.txt"
            }),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Write",
            &serde_json::json!({"file_path":"/workspace/local-app-app-b/src/App.jsx"}),
            &fs
        ));
    }

    /// The host-owned HARD DENY is reachable only through a forwarded token.
    ///
    /// `denies_host_owned_for_token` had no test at all, and in production the
    /// token never arrived: `DeferredToolInvoker` inherited the trait's
    /// delegating default and dropped it, so the `let Some(token) = token else
    /// { return false }` early return disabled this guard entirely. Forwarding
    /// the token (engine-mobile `workflow_support.rs`, engine-desktop `lib.rs`)
    /// turns it on for the first time — pin both polarities so the activation
    /// is deliberate and a future pass-through wrapper cannot silently undo it.
    #[test]
    fn production_host_owned_deny_requires_the_forwarded_token() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease = registry.begin_local_app("app-a", &root).unwrap();
        let fs = roots(&root);

        // A host-owned file is denied outright when the token is present.
        assert!(
            registry.denies_host_owned_for_token(
                Some(lease.token()),
                "Write",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/package.json"}),
                &fs
            ),
            "a leased workflow must not rewrite host-owned package.json"
        );
        assert!(
            registry.denies_host_owned_for_token(
                Some(lease.token()),
                "Edit",
                &serde_json::json!({
                    "file_path":"/workspace/local-app-app-a/.lingxi-build-state/dependency-update-recovery.json"
                }),
                &fs
            ),
            "a leased workflow must not forge dependency recovery state"
        );

        // Generated source under the same lease stays writable.
        assert!(
            !registry.denies_host_owned_for_token(
                Some(lease.token()),
                "Write",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/src/App.jsx"}),
                &fs
            ),
            "generated source is not host-owned"
        );

        // DOCUMENTS the `let Some(token) = token else { return false }` arm —
        // it is NOT a regression anchor. This assertion stays green precisely
        // WHEN the bug recurs, so it cannot catch a wrapper that drops the
        // token again. The real guards are the forwarding tests in
        // engine-mobile `workflow_support.rs` and engine-desktop `lib.rs`,
        // which assert the token reaches the inner invoker.
        assert!(
            !registry.denies_host_owned_for_token(
                None,
                "Write",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/package.json"}),
                &fs
            ),
            "a dropped token silently disables the host-owned deny"
        );
    }

    #[test]
    fn local_app_shell_path_escape_is_hard_denied_before_global_allow_rules() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let fs = roots(&root);

        assert!(
            !WorkspacePermissionLeaseRegistry::escapes_local_app_workspace(
                "Bash",
                &serde_json::json!({"command":"cat src/App.jsx"}),
                &fs
            )
        );
        assert!(
            WorkspacePermissionLeaseRegistry::escapes_local_app_workspace(
                "Bash",
                &serde_json::json!({"command":"cat /etc/passwd"}),
                &fs
            )
        );
        assert!(
            WorkspacePermissionLeaseRegistry::escapes_local_app_workspace(
                "Bash",
                &serde_json::json!({"command":"cd /tmp && cat src/App.jsx"}),
                &fs
            )
        );
    }

    #[test]
    fn local_app_tool_allowlist_is_app_scoped() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app-a", root.clone());
        let fs = roots(&root);
        assert!(registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-a"}), &fs));
        assert!(!registry.allows(
            "LocalAppDeleteApp",
            &serde_json::json!({"app_id":"app-a"}),
            &fs
        ));
        assert!(!registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-b"}), &fs));
    }

    #[test]
    fn local_app_layout_binds_mcp_lease_to_workspace_app_id() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app-b", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-b"}), &fs));
        assert!(!registry.allows("LocalAppBuild", &serde_json::json!({"app_id":"app-a"}), &fs));
    }

    #[test]
    fn guest_mount_root_binds_lease_to_exact_app_id() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("local-app-app-a");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease = registry.begin("app-b", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "LocalAppBuild",
            &serde_json::json!({"app_id":"app-b"}),
            &fs
        ));
        drop(lease);
        let lease = registry.begin("app-a", root.clone());
        assert!(registry.allows_for_token(
            Some(lease.token()),
            "LocalAppBuild",
            &serde_json::json!({"app_id":"app-a"}),
            &fs
        ));
    }

    #[test]
    fn production_local_app_lease_rejects_generic_or_mismatched_roots() {
        let dir = tempdir().unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let generic = dir.path().join("workspace");
        std::fs::create_dir_all(&generic).unwrap();
        assert!(registry.begin_local_app("app-a", generic).is_err());

        let wrong = dir.path().join("apps/app-b/workspace");
        std::fs::create_dir_all(&wrong).unwrap();
        assert!(registry.begin_local_app("app-a", wrong).is_err());
    }

    #[test]
    fn production_local_app_lease_accepts_canonical_host_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease = registry.begin_local_app("app-a", &root).unwrap();
        assert_eq!(
            registry.active(),
            vec![WorkspaceLeaseInfo {
                app_id: "app-a".into(),
                root: std::fs::canonicalize(root).unwrap(),
            }]
        );
        drop(lease);
        assert!(registry.active().is_empty());
    }

    /// The lease authorizes the local-app host operations under their BUILTIN
    /// names. They used to be `mcp__local_apps__*`; that spelling gave them
    /// third-party-MCP permission semantics they were never meant to have, so
    /// they were moved to ordinary builtin tools.
    ///
    /// The `app_id` equality check is the scoping that makes this safe and
    /// must survive the rename: a lease for app A must not authorize an
    /// operation aimed at app B.
    #[test]
    fn the_lease_authorizes_local_app_tools_by_their_builtin_names() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease = registry.begin_local_app("app-a", &root).unwrap();
        let fs = roots(&root);

        for name in [
            "LocalAppBuild",
            "LocalAppLogs",
            "LocalAppRuntime",
            "LocalAppManifest",
            "LocalAppQueryData",
        ] {
            assert!(
                registry.allows_for_token(
                    Some(lease.token()),
                    name,
                    &serde_json::json!({"app_id": "app-a"}),
                    &fs
                ),
                "{name} must be lease-authorized for its own app"
            );
            assert!(
                !registry.allows_for_token(
                    Some(lease.token()),
                    name,
                    &serde_json::json!({"app_id": "app-b"}),
                    &fs
                ),
                "{name} must NOT reach a sibling app"
            );
        }

        // An operation outside the build loop is not lease-authorized.
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "LocalAppMutateData",
            &serde_json::json!({"app_id": "app-a"}),
            &fs
        ));
    }

    /// `local_app_id_for_root` must recognise the GUEST spelling too.
    ///
    /// The same file's `guest_workspace_relative` already accepts both
    /// `/workspace/local-app-<id>` and `/workspace/<id>`; this derivation
    /// accepted neither bare form, so a caller handed a guest cwd — an
    /// isolated subagent, say — would silently resolve to "no app" and any
    /// binding built on it would be inert while looking correct.
    #[test]
    fn the_app_id_derivation_accepts_host_and_guest_spellings() {
        use std::path::Path;
        // Host spellings.
        assert_eq!(
            local_app_id_for_root(Path::new("/data/apps/abcd1234/workspace")).as_deref(),
            Some("abcd1234")
        );
        assert_eq!(
            local_app_id_for_root(Path::new("/data/local-app-abcd1234")).as_deref(),
            Some("abcd1234")
        );
        // Guest spellings, as measured on device.
        assert_eq!(
            local_app_id_for_root(Path::new("/workspace/local-app-abcd1234")).as_deref(),
            Some("abcd1234")
        );
        assert_eq!(
            local_app_id_for_root(Path::new("/workspace/abcd1234")).as_deref(),
            Some("abcd1234")
        );
        // Not a local-app root.
        assert_eq!(local_app_id_for_root(Path::new("/data/some/project")), None);
        assert_eq!(local_app_id_for_root(Path::new("/workspace")), None);
    }

    /// Every Vite config SPELLING must be host-owned, not just the one the
    /// template ships.
    ///
    /// Vite self-resolves its config from `DEFAULT_CONFIG_FILES`, and
    /// `vite.config.js` is FIRST in that list — ahead of the `vite.config.mjs`
    /// the host writes (verified against vite's own dist). A Vite config is
    /// executed Node code, so a model that creates `vite.config.js` shadows the
    /// host config and runs arbitrary code inside the build. `Edit` creates
    /// files on a nonexistent path, so the existing `Edit(./**)` grant reaches
    /// it without any new permission.
    ///
    /// `pnpm-workspace.yaml` is the same class: `prepare_dependency_staging`
    /// copies it into the install staging directory, and the install runs with
    /// the network ENABLED.
    #[test]
    fn every_host_config_spelling_is_host_owned() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&root).unwrap();
        let fs = roots(&root);

        // Everything the generated LINGXI.md declares host-managed must
        // actually BE host-owned. Four of these were declared to the agent but
        // enforced nowhere, so the workspace's own `Edit(./**)` grant
        // overwrote them and `restore_host_managed_files` silently reverted
        // the edit — the model loops against a file it cannot change.
        for name in [
            ".gitignore",
            "jsconfig.json",
            "lib/lingxi-provider.jsx",
            "lib/frame-loop.js",
            "lib/phaser-runtime.js",
            "lib/babylon-runtime.js",
            "styles/foundation.css",
            "vite.config.js",
            "vite.config.mjs",
            "vite.config.ts",
            "vite.config.cjs",
            "vite.config.mts",
            "vite.config.cts",
            "pnpm-workspace.yaml",
            // PostCSS is the SAME hole as the Vite config: the template sets
            // no `css.postcss` key and imports CSS, so Vite runs
            // `lilconfig("postcss").search(root)` on every build and executes
            // whatever it finds as Node code. `--config` pins only the Vite
            // config and does not affect this search. Filenames read from
            // vite's own `getDefaultSearchPlaces`.
            "postcss.config.js",
            "postcss.config.cjs",
            "postcss.config.mjs",
            ".postcssrc.js",
            ".postcssrc.cjs",
            ".postcssrc.mjs",
            ".postcssrc.json",
            ".config/postcssrc.js",
            ".config/postcssrc.cjs",
            ".config/postcssrc.mjs",
        ] {
            assert!(
                WorkspacePermissionLeaseRegistry::denies_host_owned_for_workspace(
                    "Write",
                    &serde_json::json!({
                        "file_path": format!("/workspace/local-app-app-a/{name}")
                    }),
                    &fs,
                ),
                "{name} must be host-owned"
            );
        }

        // An ordinary source file with a similar name stays writable.
        assert!(
            !WorkspacePermissionLeaseRegistry::denies_host_owned_for_workspace(
                "Write",
                &serde_json::json!({
                    "file_path": "/workspace/local-app-app-a/src/vite.config.helper.js"
                }),
                &fs,
            ),
            "app-owned source must stay writable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_app_settings_and_symlink_escape_stay_denied_without_a_lease() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("apps/app-a/workspace");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(root.join(".lingxi")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join(".lingxi/settings.local.json"), b"{}\n").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let fs = roots(&root);

        assert!(
            WorkspacePermissionLeaseRegistry::denies_host_owned_for_workspace(
                "Edit",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/.lingxi/settings.local.json"}),
                &fs,
            )
        );
        assert!(
            WorkspacePermissionLeaseRegistry::denies_host_owned_for_workspace(
                "Bash",
                &serde_json::json!({"command":"npm install"}),
                &fs,
            )
        );
        assert!(
            !WorkspacePermissionLeaseRegistry::denies_host_owned_for_workspace(
                "Bash",
                &serde_json::json!({"command":"cat src/App.jsx"}),
                &fs,
            )
        );
        assert!(
            WorkspacePermissionLeaseRegistry::escapes_local_app_workspace(
                "Edit",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/link/new.txt"}),
                &fs,
            )
        );
        assert!(
            !WorkspacePermissionLeaseRegistry::escapes_local_app_workspace(
                "Edit",
                &serde_json::json!({"file_path":"/workspace/local-app-app-a/src/new.txt"}),
                &fs,
            )
        );
    }

    #[test]
    fn concurrent_leases_require_the_matching_token() {
        let dir = tempdir().unwrap();
        let root_a = dir.path().join("workspace-a");
        let root_b = dir.path().join("workspace-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease_a = registry.begin("app-a", root_a.clone());
        let lease_b = registry.begin("app-b", root_b.clone());
        let fs_a = roots(&root_a);
        assert!(registry.allows_for_token(
            Some(lease_a.token()),
            "Write",
            &serde_json::json!({"file_path":"src/a.txt"}),
            &fs_a
        ));
        assert!(!registry.allows_for_token(
            Some(lease_b.token()),
            "Write",
            &serde_json::json!({"file_path":root_a.join("src/a.txt").to_string_lossy()}),
            &fs_a
        ));
    }

    #[test]
    fn shell_lease_allows_cd_but_rejects_nested_actions() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let lease = registry.begin("app", root.clone());
        let fs = roots(&root);
        assert!(registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({"command":"cd . && grep -rn foo src/"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({"command":"find . -exec curl https://example.com {} +"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Bash",
            &serde_json::json!({"command":"sed -n 'e curl https://example.com' README.md"}),
            &fs
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_even_when_destination_is_missing() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows(
            "Write",
            &serde_json::json!({"file_path":"link/new.txt"}),
            &fs
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_alias_is_compared_after_canonicalization() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let registry = WorkspacePermissionLeaseRegistry::new();
        let _lease = registry.begin("app", root.clone());
        let fs = roots(&root);
        let canonical = std::fs::canonicalize(&root).unwrap();
        let Some(raw) = canonical
            .to_str()
            .and_then(|path| path.strip_prefix("/private"))
        else {
            return;
        };
        assert!(registry.allows(
            "Write",
            &serde_json::json!({"file_path": format!("{raw}/alias.txt")}),
            &fs
        ));
    }
}
