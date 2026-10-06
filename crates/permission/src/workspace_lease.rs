//! Ephemeral, path-bound permission leases for host-managed workspaces.
//!
//! A lease is intentionally separate from the session-wide permission mode:
//! it grants only calls whose resolved paths stay inside one workspace and
//! disappears when the workflow drops its guard.
//!
//! What a workspace looks like on disk, which of its files the host owns, and
//! which host operations a lease may authorize is the business of whoever
//! manages the workspaces. That is a [`WorkspaceProfile`], supplied when the
//! registry is built; this module holds the lifecycle and the generic checks
//! (canonical containment, symlink escapes, read-only shell inspection).

use crate::command_path_containment::check_command_path_containment;
use crate::filesystem::{file_tool_kind, input_path_for_tool, FileToolKind, FsRoots};
use crate::path_constraints::check_path_constraints;
use crate::shell_command::{command_from_input, is_shell_tool};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// How one kind of host-managed workspace is laid out and protected.
///
/// The registry asks the profile and never guesses: a profile that answers
/// "no" or `None` makes the corresponding check fail closed.
pub trait WorkspaceProfile: Send + Sync + std::fmt::Debug {
    /// The id of the workspace `root` belongs to, if `root` has this profile's
    /// on-disk shape (in the host's spelling).
    fn workspace_id(&self, root: &Path) -> Option<String>;

    /// Whether `root` and `id` agree. Roots that do not have the profile's
    /// shape (test and in-memory roots) may answer `true`; roots that do have
    /// it must name the same id.
    fn root_matches_id(&self, root: &Path, id: &str) -> bool;

    /// Whether `root` is exactly the canonical workspace of `id`. This is the
    /// production gate for starting a lease.
    fn is_exact_root(&self, root: &Path, id: &str) -> bool;

    /// Absolute path prefixes under which a sandboxed guest sees the
    /// workspace of `id`. Paths and shell commands using them are translated
    /// to the host root before they are checked.
    fn guest_prefixes(&self, id: &str) -> Vec<String>;

    /// A decision for a tool this profile owns, keyed by the lease's id:
    /// `Some(allowed)` ends the check, `None` leaves the tool to the generic
    /// file and shell rules.
    fn tool_decision(&self, tool_name: &str, input: &serde_json::Value, id: &str) -> Option<bool>;

    /// Whether `relative`, a path below the canonical workspace root, is
    /// owned by the host and must not be written by a leased workflow. The
    /// empty path is the root itself.
    fn is_host_owned(&self, relative: &Path) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLeaseInfo {
    pub workspace_id: String,
    pub root: PathBuf,
}

#[derive(Debug)]
struct ActiveLease {
    info: WorkspaceLeaseInfo,
}

/// Shared registry owned by an engine composition root.
#[derive(Debug)]
pub struct WorkspacePermissionLeaseRegistry {
    next: AtomicU64,
    active: RwLock<HashMap<u64, ActiveLease>>,
    profile: Arc<dyn WorkspaceProfile>,
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
    pub fn new(profile: Arc<dyn WorkspaceProfile>) -> Arc<Self> {
        Arc::new(Self {
            next: AtomicU64::new(0),
            active: RwLock::new(HashMap::new()),
            profile,
        })
    }

    /// Begin a lease without proving that `root` is the profile's canonical
    /// workspace for `id`. For tests that exercise the registry with roots of
    /// their own; production code uses [`Self::begin_bound`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn begin_unchecked(
        self: &Arc<Self>,
        workspace_id: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> WorkspacePermissionLease {
        self.begin_normalized(workspace_id.into(), root.into())
    }

    /// Begin a lease for a production workspace.
    ///
    /// Unlike the permissive helper the unit tests use, this entry point
    /// requires the profile's canonical layout for `id`. Callers that cannot
    /// prove that binding must fail closed instead of silently granting a
    /// lease over a generic cwd.
    pub fn begin_bound(
        self: &Arc<Self>,
        workspace_id: impl Into<String>,
        root: impl Into<PathBuf>,
    ) -> Result<WorkspacePermissionLease, String> {
        let workspace_id = workspace_id.into();
        let root = lexical_normalize(root.into());
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if !self.profile.is_exact_root(&canonical_root, &workspace_id) {
            return Err(format!(
                "workspace lease root {} is not the canonical workspace for {}",
                canonical_root.display(),
                workspace_id
            ));
        }
        Ok(self.begin_normalized(workspace_id, canonical_root))
    }

    fn begin_normalized(
        self: &Arc<Self>,
        workspace_id: String,
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
                    info: WorkspaceLeaseInfo { workspace_id, root },
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

    /// Returns true only for a file/shell/profile-owned call contained by the
    /// sole active lease. Kept out of production builds: production
    /// authorization must always use the token-aware entry point below;
    /// exposing an unscoped helper would make it too easy for a future caller
    /// to accidentally authorize against another active workspace's lease.
    #[cfg(any(test, feature = "test-support"))]
    pub fn allows(&self, tool_name: &str, input: &serde_json::Value, roots: &FsRoots) -> bool {
        let active = self
            .active
            .read()
            .expect("workspace lease registry poisoned");
        (active.len() == 1)
            .then(|| active.values().next())
            .flatten()
            .is_some_and(|lease| {
                allows_for_lease(&*self.profile, &lease.info, tool_name, input, roots)
            })
    }

    /// Returns true only for a file/shell/profile-owned call contained by the
    /// lease `token` names. Explicit deny/ask rules are evaluated before this
    /// hook in `PermissionPolicy::authorize_inner`, so the lease cannot
    /// override them.
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
            .is_some_and(|lease| {
                allows_for_lease(&*self.profile, &lease.info, tool_name, input, roots)
            })
    }

    /// Returns true when the sole active lease's workflow is attempting to
    /// modify a host-owned file. Test-only for the same reason as
    /// [`Self::allows`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn denies_host_owned(
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
            .is_some_and(|lease| {
                host_owned_for_lease(&*self.profile, &lease.info, tool_name, input, roots)
            })
    }

    /// Returns true when a leased workflow is attempting to modify a
    /// host-owned file. This is a hard deny, not merely a failed lease match,
    /// so a later global `auto`/allow rule cannot re-enable the mutation.
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
            .is_some_and(|lease| {
                host_owned_for_lease(&*self.profile, &lease.info, tool_name, input, roots)
            })
    }

    /// The source-editing boundary remains protected for every conversation
    /// rooted at a managed workspace, even after the temporary build lease is
    /// dropped. The generated local settings file is itself an allow rule, so
    /// this check must not depend on a lease token or host-managed files and
    /// non-inspection shell commands would become writable after a build.
    pub fn denies_host_owned_for_workspace(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        let root = &roots.cwd;
        self.is_workspace_root(root)
            && host_owned_for_root(&*self.profile, root, tool_name, input, roots)
    }

    /// Return true when a file operation resolves outside a managed
    /// workspace's canonical root. This is deliberately evaluated before
    /// generic allow rules: `Edit(./**)` is lexical and cannot prove that a
    /// symlink target remains inside the workspace.
    pub fn escapes_workspace(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        roots: &FsRoots,
    ) -> bool {
        let profile = &*self.profile;
        let Some(workspace_id) = profile.workspace_id(&roots.cwd) else {
            return false;
        };
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
                let target = resolve_target(profile, &roots.cwd, &workspace_id, raw.as_ref());
                !is_workspace_path(&roots.cwd, &target)
            }
            FileToolKind::NonFile if is_shell_tool(tool_name) => {
                let Some(command) = command_from_input(input) else {
                    return false;
                };
                let lease_roots = FsRoots {
                    cwd: roots.cwd.clone(),
                    home: roots.home.clone(),
                    lingxi_home: roots.lingxi_home.clone(),
                };
                let command = command_with_guest_workspace_aliases(
                    profile,
                    command,
                    &roots.cwd,
                    &workspace_id,
                );
                // These guards are intentionally checked even before the
                // generic Bash allow walk. A global `Bash(...)` exact allow
                // must not authorize `cat /etc/passwd`, `cd /tmp`, or a
                // redirect outside this workspace.
                check_path_constraints(&command, &lease_roots, &[], None).is_some()
                    || check_command_path_containment(&command, &lease_roots, &[], None).is_some()
            }
            _ => false,
        }
    }

    fn is_workspace_root(&self, root: &Path) -> bool {
        self.profile.workspace_id(root).is_some()
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
    profile: &dyn WorkspaceProfile,
    info: &WorkspaceLeaseInfo,
    tool_name: &str,
    input: &serde_json::Value,
    roots: &FsRoots,
) -> bool {
    // A managed workspace has a stable on-disk shape. If a caller supplies a
    // different id for that root, fail closed instead of granting an
    // id-scoped operation for a sibling workspace. Test/in-memory roots may
    // use a generic directory name, so the profile only enforces the check
    // when the layout is unambiguously its own.
    if !profile.root_matches_id(&info.root, &info.workspace_id) {
        return false;
    }
    // Tools the profile owns are decided by it. Its id equality check is what
    // keeps a lease for one workspace from authorizing an operation aimed at
    // a sibling.
    if let Some(allowed) = profile.tool_decision(tool_name, input, &info.workspace_id) {
        return allowed;
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
            let target = resolve_target(profile, &info.root, &info.workspace_id, raw.as_ref());
            is_workspace_path(&info.root, &target)
                && (file_tool_kind(tool_name) == FileToolKind::Reader
                    || !is_host_owned_path_or_container(profile, &info.root, &target))
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
            let command = command_with_guest_workspace_aliases(
                profile,
                command,
                &info.root,
                &info.workspace_id,
            );
            check_path_constraints(&command, &lease_roots, &[], None).is_none()
                && check_command_path_containment(&command, &lease_roots, &[], None).is_none()
        }
        _ => false,
    }
}

fn host_owned_for_lease(
    profile: &dyn WorkspaceProfile,
    info: &WorkspaceLeaseInfo,
    tool_name: &str,
    input: &serde_json::Value,
    roots: &FsRoots,
) -> bool {
    if !profile.root_matches_id(&info.root, &info.workspace_id) {
        return false;
    }
    host_owned_for_root(profile, &info.root, tool_name, input, roots)
}

fn host_owned_for_root(
    profile: &dyn WorkspaceProfile,
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
    let workspace_id = profile.workspace_id(root);
    match file_tool_kind(tool_name) {
        // Reader access is intentionally not blocked; the lease only protects
        // host-owned files from model mutation.
        FileToolKind::Editor => input_path_for_tool(tool_name, input, &lease_roots)
            .map(|raw| match workspace_id.as_deref() {
                Some(id) => resolve_target(profile, root, id, raw.as_ref()),
                None => resolve_target_from_root(root, raw.as_ref()),
            })
            .is_some_and(|target| is_host_owned_path_or_container(profile, root, &target)),
        FileToolKind::NonFile if is_shell_tool(tool_name) => {
            let Some(command) = command_from_input(input) else {
                return true;
            };
            let command = workspace_id
                .as_deref()
                .map(|id| command_with_guest_workspace_aliases(profile, command, root, id))
                .unwrap_or_else(|| command.to_string());
            let redirects = crate::path_constraints::write_redirect_targets(&command, &lease_roots);
            // Workspace source mutation goes through structured file tools.
            // Shell redirects are intentionally never lease-authorized: shell
            // expansion makes their final target impossible to prove here
            // (`TARGET=package.json; echo x > "$TARGET"`, braces, globs, ...).
            if !redirects.is_empty() {
                return true;
            }
            // Do not try to enumerate every possible mutator (`env rm`,
            // interpreters, package-manager scripts, and future commands all
            // make that list incomplete). A managed-workspace shell is an
            // inspection surface only; source mutation uses structured file
            // tools whose final target can be checked without evaluating
            // shell expansion.
            !workspace_shell_is_safe(&command)
        }
        _ => false,
    }
}

fn resolve_target_from_root(root: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        lexical_normalize(path.to_path_buf())
    } else {
        lexical_normalize(root.join(path))
    }
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
    // useful for inspecting one workspace (no git/gh/docker/etc.).
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

fn resolve_target(
    profile: &dyn WorkspaceProfile,
    root: &Path,
    workspace_id: &str,
    raw: &str,
) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        if let Some(relative) = guest_workspace_relative(profile, workspace_id, path) {
            lexical_normalize(root.join(relative))
        } else {
            lexical_normalize(path.to_path_buf())
        }
    } else {
        lexical_normalize(root.join(path))
    }
}

fn guest_workspace_relative<'a>(
    profile: &dyn WorkspaceProfile,
    workspace_id: &str,
    path: &'a Path,
) -> Option<&'a Path> {
    profile
        .guest_prefixes(workspace_id)
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

fn command_with_guest_workspace_aliases(
    profile: &dyn WorkspaceProfile,
    command: &str,
    root: &Path,
    workspace_id: &str,
) -> String {
    // Static command containment works in host coordinates. A sandboxed guest
    // sees the workspace under its own prefixes, so translate only those
    // exact workspace-bound aliases before checking cwd, redirects, and
    // positional paths. If the host root contains whitespace, leave the
    // command untouched rather than introducing shell quoting into a security
    // check; it will conservatively remain on the normal approval path.
    let root = root.to_string_lossy();
    if root.chars().any(char::is_whitespace) {
        return command.to_string();
    }
    profile
        .guest_prefixes(workspace_id)
        .iter()
        .fold(command.to_string(), |command, prefix| {
            command.replace(prefix.as_str(), root.as_ref())
        })
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

fn is_host_owned_path_or_container(
    profile: &dyn WorkspaceProfile,
    root: &Path,
    target: &Path,
) -> bool {
    let Some(resolved) = resolve_canonical_target(target) else {
        return false;
    };
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let Ok(relative) = resolved.strip_prefix(&canonical_root) else {
        return false;
    };
    profile.is_host_owned(relative)
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

    /// A workspace layout of this module's own, so the generic checks are
    /// tested without any product's rules: roots are `…/ws-<id>`, the guest
    /// sees them as `/guest/<id>`, `host.lock` and everything under `.host/`
    /// belong to the host, and the tool `TestOp` is decided by its `id`.
    #[derive(Debug)]
    struct TestProfile;

    impl WorkspaceProfile for TestProfile {
        fn workspace_id(&self, root: &Path) -> Option<String> {
            root.file_name()?
                .to_str()?
                .strip_prefix("ws-")
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        }
        fn root_matches_id(&self, root: &Path, id: &str) -> bool {
            self.workspace_id(root).is_none_or(|own| own == id)
        }
        fn is_exact_root(&self, root: &Path, id: &str) -> bool {
            self.workspace_id(root).as_deref() == Some(id)
        }
        fn guest_prefixes(&self, id: &str) -> Vec<String> {
            vec![format!("/guest/{id}")]
        }
        fn tool_decision(&self, name: &str, input: &serde_json::Value, id: &str) -> Option<bool> {
            (name == "TestOp")
                .then(|| input.get("id").and_then(serde_json::Value::as_str) == Some(id))
        }
        fn is_host_owned(&self, relative: &Path) -> bool {
            relative.as_os_str().is_empty()
                || relative == Path::new("host.lock")
                || relative.starts_with(".host")
        }
    }

    fn registry() -> Arc<WorkspacePermissionLeaseRegistry> {
        WorkspacePermissionLeaseRegistry::new(Arc::new(TestProfile))
    }

    fn roots(root: &Path) -> FsRoots {
        FsRoots {
            cwd: root.to_path_buf(),
            home: None,
            lingxi_home: root.to_path_buf(),
        }
    }

    fn workspace(dir: &Path, id: &str) -> PathBuf {
        let root = dir.join(format!("ws-{id}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn lease_allows_workspace_file_and_rejects_outside_and_host_owned() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let _lease = registry.begin_unchecked("a", root.clone());
        let fs = roots(&root);
        let write = |path: &str| {
            registry.allows("Write", &serde_json::json!({"file_path": path}), &fs)
        };
        assert!(write("src/main.txt"));
        assert!(!write("../secret"));
        assert!(!write("host.lock"));
        assert!(!write(".host/state.json"));
        assert!(!write("."), "the root itself is host-owned");
    }

    #[test]
    fn host_owned_files_are_readable_but_never_writable() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        std::fs::create_dir_all(root.join(".host")).unwrap();
        std::fs::write(root.join(".host/state.json"), b"{}\n").unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("a", root.clone());
        let fs = roots(&root);
        let state = serde_json::json!({"file_path": ".host/state.json"});

        assert!(registry.allows("Read", &state, &fs));
        assert!(!registry.allows("Write", &state, &fs));
        assert!(registry.denies_host_owned("Write", &state, &fs));
        assert!(!registry.denies_host_owned("Read", &state, &fs));
        // Shell redirects and anything that is not read-only inspection are
        // never lease-authorized, whatever they touch.
        for command in [
            "echo x > host.lock",
            "TARGET=host.lock; echo x > \"$TARGET\"",
            "rm -rf src",
            "find . -fprint host.lock",
        ] {
            assert!(
                registry.denies_host_owned("Bash", &serde_json::json!({"command": command}), &fs),
                "{command}"
            );
        }
        assert!(!registry.denies_host_owned(
            "Bash",
            &serde_json::json!({"command": "cat src/main.txt"}),
            &fs
        ));
    }

    #[test]
    fn drop_revokes_lease() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let fs = roots(&root);
        let lease = registry.begin_unchecked("a", root);
        assert!(!registry.active().is_empty());
        drop(lease);
        assert!(registry.active().is_empty());
        assert!(!registry.allows("Write", &serde_json::json!({"file_path":"src/x"}), &fs));
    }

    #[test]
    fn shell_lease_is_read_only_inspection() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let _lease = registry.begin_unchecked("a", root.clone());
        let fs = roots(&root);
        let shell = |command: &str| {
            registry.allows("Bash", &serde_json::json!({"command": command}), &fs)
        };
        assert!(shell("cat src/main.txt"));
        assert!(shell("cd . && grep -rn foo src/"));
        for command in [
            "echo ok > out.txt",
            "npm install",
            "curl https://example.com",
            "node -e 'process.exit(0)'",
            "echo $(curl https://example.com)",
            "echo `curl https://example.com`",
            "find . -fprint out.txt",
            "find . -fprintf out.txt x",
            "find . -files0-from list",
            "find . -exec curl https://example.com {} +",
            "sed -n 'e curl https://example.com' README.md",
        ] {
            assert!(!shell(command), "{command}");
        }
    }

    #[test]
    fn production_lease_translates_only_the_bound_guest_alias() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let lease = registry.begin_bound("a", &root).unwrap();
        let fs = roots(&root);
        let allows = |tool: &str, input: serde_json::Value| {
            registry.allows_for_token(Some(lease.token()), tool, &input, &fs)
        };
        assert!(allows("Write", serde_json::json!({"file_path":"/guest/a/src/x.txt"})));
        assert!(!allows("Write", serde_json::json!({"file_path":"/guest/b/src/x.txt"})));
        assert!(allows("Bash", serde_json::json!({"command":"cat /guest/a/src/x.txt"})));
        assert!(!allows("Bash", serde_json::json!({"command":"echo ok > /guest/a/src/x.txt"})));
    }

    #[test]
    fn production_lease_rejects_generic_or_mismatched_roots() {
        let dir = tempdir().unwrap();
        let registry = registry();
        let generic = dir.path().join("plain");
        std::fs::create_dir_all(&generic).unwrap();
        assert!(registry.begin_bound("a", generic).is_err());
        let wrong = workspace(dir.path(), "b");
        assert!(registry.begin_bound("a", wrong).is_err());
    }

    #[test]
    fn production_lease_accepts_the_canonical_root_and_records_it() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let lease = registry.begin_bound("a", &root).unwrap();
        assert_eq!(
            registry.active(),
            vec![WorkspaceLeaseInfo {
                workspace_id: "a".into(),
                root: std::fs::canonicalize(root).unwrap(),
            }]
        );
        drop(lease);
        assert!(registry.active().is_empty());
    }

    #[test]
    fn a_profile_owned_tool_is_decided_by_the_profile_and_scoped_to_the_lease_id() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let lease = registry.begin_bound("a", &root).unwrap();
        let fs = roots(&root);
        let allows = |input: serde_json::Value| {
            registry.allows_for_token(Some(lease.token()), "TestOp", &input, &fs)
        };
        assert!(allows(serde_json::json!({"id": "a"})));
        assert!(!allows(serde_json::json!({"id": "b"})), "a sibling is out of scope");
        assert!(!allows(serde_json::json!({})));
    }

    #[test]
    fn a_lease_whose_id_disagrees_with_the_root_authorizes_nothing() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let lease = registry.begin_unchecked("b", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "TestOp",
            &serde_json::json!({"id": "b"}),
            &fs
        ));
        assert!(!registry.allows_for_token(
            Some(lease.token()),
            "Write",
            &serde_json::json!({"file_path": "src/x"}),
            &fs
        ));
    }

    #[test]
    fn the_host_owned_deny_requires_the_forwarded_token() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let lease = registry.begin_bound("a", &root).unwrap();
        let fs = roots(&root);
        let input = serde_json::json!({"file_path": "host.lock"});
        assert!(registry.denies_host_owned_for_token(Some(lease.token()), "Write", &input, &fs));
        assert!(!registry.denies_host_owned_for_token(
            Some(lease.token()),
            "Write",
            &serde_json::json!({"file_path": "src/x"}),
            &fs
        ));
        // Documents the `let Some(token) = token else { return false }` arm. It
        // is not a regression anchor: it stays green precisely when a wrapper
        // drops the token. The forwarding tests in the runtime guard that.
        assert!(!registry.denies_host_owned_for_token(None, "Write", &input, &fs));
    }

    #[test]
    fn concurrent_leases_require_the_matching_token() {
        let dir = tempdir().unwrap();
        let root_a = workspace(dir.path(), "a");
        let root_b = workspace(dir.path(), "b");
        let registry = registry();
        let lease_a = registry.begin_unchecked("a", root_a.clone());
        let lease_b = registry.begin_unchecked("b", root_b);
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
    fn the_static_boundary_needs_no_lease() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        std::fs::create_dir_all(root.join(".host")).unwrap();
        let registry = registry();
        let fs = roots(&root);
        let denies = |tool: &str, input: serde_json::Value| {
            registry.denies_host_owned_for_workspace(tool, &input, &fs)
        };
        assert!(denies("Edit", serde_json::json!({"file_path":"/guest/a/.host/state.json"})));
        assert!(denies("Bash", serde_json::json!({"command":"npm install"})));
        assert!(!denies("Bash", serde_json::json!({"command":"cat src/x"})));
        let escapes = |tool: &str, input: serde_json::Value| {
            registry.escapes_workspace(tool, &input, &fs)
        };
        assert!(!escapes("Bash", serde_json::json!({"command":"cat src/x"})));
        assert!(escapes("Bash", serde_json::json!({"command":"cat /etc/passwd"})));
        assert!(escapes("Bash", serde_json::json!({"command":"cd /tmp && cat src/x"})));
        // A root the profile does not recognise is not a managed workspace.
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!registry.denies_host_owned_for_workspace(
            "Write",
            &serde_json::json!({"file_path":"host.lock"}),
            &roots(&plain)
        ));
        assert!(!registry.escapes_workspace(
            "Bash",
            &serde_json::json!({"command":"cat /etc/passwd"}),
            &roots(&plain)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_even_when_destination_is_missing() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let registry = registry();
        let _lease = registry.begin_unchecked("a", root.clone());
        let fs = roots(&root);
        assert!(!registry.allows("Write", &serde_json::json!({"file_path":"link/new.txt"}), &fs));
        // The same hole without a lease: the static boundary names it too.
        assert!(registry.escapes_workspace(
            "Edit",
            &serde_json::json!({"file_path":"/guest/a/link/new.txt"}),
            &fs
        ));
        assert!(!registry.escapes_workspace(
            "Edit",
            &serde_json::json!({"file_path":"/guest/a/src/new.txt"}),
            &fs
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_alias_is_compared_after_canonicalization() {
        let dir = tempdir().unwrap();
        let root = workspace(dir.path(), "a");
        let registry = registry();
        let _lease = registry.begin_unchecked("a", root.clone());
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
